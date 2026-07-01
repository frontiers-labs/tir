//! Instruction selection over semantic e-graphs.
//!
//! Each block's operations are lowered into an e-graph of semantic expressions
//! ([`builder`]), saturated with proved algebraic rewrites ([`rewrites`]), and
//! covered by the target's instruction patterns ([`pattern`]) via a PBQP
//! instance over e-classes ([`cover`]). The solved cover becomes an emission
//! plan ([`emit`]) the pass commits through the rewriter.

mod builder;
mod cover;
mod ematch;
mod emit;
mod node;
mod pattern;
mod rewrites;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use tir::{
    Block, BlockId, Context, OpId, Operation, OperationRef, Pass, PassError, PassTarget, Rewriter,
    TypeId, ValueId,
    graph::{NodeId, OperandConstraint, PatternExpr},
    sem::{SemGraph, SymKind},
};
use tir_adt::APInt;
use tir_symbolic::egraph::{ENode, Id};

pub use ematch::EMatch;
pub use node::{SemEGraph, SemNode, SemPayload};
pub use rewrites::{IselRewrite, SaturationLimits};

use builder::SemDagBuilder;
use cover::{
    CaptureBindings, FullMatchBindings, PatternNodeBinding, PbqpIselAlternative, PbqpIselMatch,
    build_eclass_cover, completeness_error, prune_dominated_matches,
};
use emit::{BlockDecision, BlockPlan, DefinerEmit, EmissionBuilder};
use node::{Binding, class_binding};
use pattern::{CompiledIselPattern, compile_isel_pattern};
use rewrites::discover_rewrites;
#[cfg(test)]
use {node::template_node, rewrites::extension_rewrite};

#[derive(Debug, Clone)]
pub struct RuleMatch {
    int_bindings: Vec<(u32, APInt)>,
    value_bindings: Vec<(u32, ValueId)>,
}

impl RuleMatch {
    pub(crate) fn new(
        mut int_bindings: Vec<(u32, APInt)>,
        mut value_bindings: Vec<(u32, ValueId)>,
    ) -> Self {
        int_bindings.sort_by_key(|(sym, _)| *sym);
        value_bindings.sort_by_key(|(sym, _)| *sym);
        Self {
            int_bindings,
            value_bindings,
        }
    }

    pub fn value_binding(&self, symbol: u32) -> Option<ValueId> {
        self.value_bindings
            .iter()
            .find(|(sym, _)| *sym == symbol)
            .map(|(_, v)| *v)
    }

    pub fn int_binding(&self, symbol: u32) -> Option<i64> {
        self.int_bindings
            .iter()
            .find(|(sym, _)| *sym == symbol)
            .map(|(_, v)| v.to_u64() as i64)
    }
}

/// The destination an emitter writes into: the original op being replaced, or
/// just fresh destination values for a rewrite-introduced instruction that has
/// no backing IR op.
pub struct EmitRequest<'a> {
    /// The op being replaced; `None` for an introduced instruction.
    pub op: Option<&'a OperationRef>,
    /// Destination values, in result order.
    pub results: &'a [ValueId],
    /// The type of the first result, when known.
    pub result_ty: Option<TypeId>,
}

impl<'a> EmitRequest<'a> {
    fn for_op(op: &'a OperationRef, context: &Context) -> Self {
        Self {
            op: Some(op),
            results: &op.op().results,
            result_ty: op.op().results.first().map(|v| context.get_value(*v).ty()),
        }
    }

    /// The op id for diagnostics; invalid for an introduced instruction.
    pub fn op_id(&self) -> OpId {
        self.op.map(|op| op.op().id).unwrap_or_default()
    }
}

/// The optimization objective the PBQP builder minimizes: the cost placed on
/// the *root* alternative of a pattern match (non-root alternatives carry zero,
/// per the paper). The default is the rule's TMDL-derived `base_cost`.
pub trait IselCostModel: Send + Sync {
    fn node_cost(
        &self,
        _context: &Context,
        _op: &OperationRef,
        rule: &Rule,
        _m: &RuleMatch,
    ) -> u64 {
        rule.base_cost as u64
    }
}

pub struct DefaultIselCostModel;

impl IselCostModel for DefaultIselCostModel {}

pub type RuleEmitFn =
    fn(&Context, &EmitRequest, &RuleMatch) -> Result<Box<dyn Operation>, PassError>;

/// A register an instruction reads implicitly — declared in its behavior but not
/// among its encoded operands (e.g. `vadd` reading `VCSR::vl`). The read is a real
/// dependency: selection introduces the register's definer ahead of the reader,
/// passing the value bound to `symbol`.
#[derive(Clone, Debug)]
pub struct ImplicitUse {
    pub symbol: u32,
    pub register_class: &'static str,
    pub register_index: u32,
}

/// An instruction that defines a register implicitly (writes it in its behavior
/// with no encoded result, e.g. `vsetvli`/`vsetivli` defining `VCSR::vl`). It is
/// never selected by value matching; selection introduces it ahead of a reader of
/// `register_index`, binding `value_symbol` to the value that read bound to (an
/// immediate when `value_is_immediate`, else a register value). Its `emit_fn`
/// hardwires the destination to `x0`. Nothing here is target-specific: the
/// definer's input is exactly the value flowing across the register def/use.
pub struct RegisterDefiner {
    pub register_class: &'static str,
    pub register_index: u32,
    pub value_is_immediate: bool,
    pub value_symbol: u32,
    pub emit_fn: RuleEmitFn,
}

pub struct Rule {
    pub name: &'static str,
    pub pattern: SemGraph,
    pub base_cost: u32,
    /// Per-operand-symbol constraint (register vs immediate). Symbols absent here
    /// are unconstrained, so hand-written and synthesized rules keep matching any
    /// value.
    pub operand_constraints: Vec<(u32, OperandConstraint)>,
    /// Registers this instruction reads implicitly (from its behavior, not its
    /// encoded operands); selection introduces each one's definer ahead of this op.
    pub implicit_uses: Vec<ImplicitUse>,
    pub emit_fn: RuleEmitFn,
}

impl Rule {
    pub fn new(name: &'static str, pattern: SemGraph, base_cost: u32, emit_fn: RuleEmitFn) -> Self {
        Self {
            name,
            pattern,
            base_cost,
            operand_constraints: Vec::new(),
            implicit_uses: Vec::new(),
            emit_fn,
        }
    }

    /// Constrain operand symbols to register or immediate operands, so e.g. an
    /// immediate-shift pattern only matches a constant shift amount.
    pub fn with_operand_constraints(mut self, constraints: Vec<(u32, OperandConstraint)>) -> Self {
        self.operand_constraints = constraints;
        self
    }

    /// Declare the registers this instruction reads implicitly, so selection
    /// introduces their definers ahead of it.
    pub fn with_implicit_uses(mut self, uses: Vec<ImplicitUse>) -> Self {
        self.implicit_uses = uses;
        self
    }
}
struct BlockSelectionCache {
    egraph: SemEGraph,
    /// The earliest op whose result each (canonical) e-class produces.
    op_by_root: HashMap<Id, OpId>,
    /// The canonical e-class of every op's root (total over the block's lowered
    /// ops, unlike `op_by_root`, which keeps one op per merged class).
    op_root: HashMap<OpId, Id>,
    /// The IR value each (canonical) e-class computes, so an operand resolving to an
    /// intermediate result can be materialized as that register value at emit time.
    class_value: HashMap<Id, ValueId>,
    /// E-classes used as an operand by more than one consumer. A memory effect in
    /// such a class cannot be internalized into a match; a pure class still can —
    /// each fused instruction recomputes it (duplication).
    shared_classes: HashSet<Id>,
    /// Op-root e-classes whose value some consumer can never internalize — a use
    /// by an op no match reaches (return, branch, an un-lowerable op) or by an op
    /// outside this block — so the defining op must never be consumed.
    must_materialize: HashSet<Id>,
    /// The solved emission plan, or the completeness error explaining why the block
    /// cannot be selected with this rule set.
    plan: Option<Result<BlockPlan, String>>,
}
pub type OpLowering = fn(&Context, &OperationRef, &mut Rewriter) -> Result<bool, PassError>;

pub struct InstructionSelectPass {
    rules: Vec<Rule>,
    compiled_patterns: Vec<CompiledIselPattern>,
    /// Target-independent algebraic identities the program e-graph is saturated
    /// with before covering (e.g. discovered `sext`/shift bridges). Populated by
    /// rewrite discovery; empty means selection is purely syntactic tiling.
    rewrites: Vec<IselRewrite>,
    /// Instructions that define a register implicitly; selection introduces one
    /// ahead of any op whose `implicit_uses` name a matching register.
    definers: Vec<RegisterDefiner>,
    cost_model: Box<dyn IselCostModel>,
    op_lowerings: Vec<OpLowering>,
    block_cache: HashMap<BlockId, BlockSelectionCache>,
    emitted_blocks: HashSet<BlockId>,
}

impl InstructionSelectPass {
    pub fn new(rules: Vec<Rule>) -> Self {
        let compiled_patterns: Vec<_> = rules
            .iter()
            .enumerate()
            .filter_map(|(rule_index, rule)| {
                compile_isel_pattern(rule_index, &rule.pattern, &rule.operand_constraints)
            })
            .collect();

        let rewrites = discover_rewrites(&compiled_patterns);

        Self {
            rules,
            compiled_patterns,
            rewrites,
            definers: Vec::new(),
            cost_model: Box::new(DefaultIselCostModel),
            op_lowerings: vec![],
            block_cache: HashMap::new(),
            emitted_blocks: HashSet::new(),
        }
    }

    /// Install the instructions that define registers implicitly (e.g.
    /// `vsetvli`/`vsetivli`), introduced ahead of ops that read those registers.
    pub fn with_register_definers(mut self, definers: Vec<RegisterDefiner>) -> Self {
        self.definers = definers;
        self
    }

    /// Install the algebraic identities used to saturate the program e-graph before
    /// covering. These are proved equivalences (target-independent bit-vector
    /// lemmas, or sequences discovered against the target's own instructions), so
    /// the rule set stays free of hand-written selection rules.
    pub fn with_rewrites(mut self, rewrites: Vec<IselRewrite>) -> Self {
        self.rewrites = rewrites;
        self
    }

    pub fn with_cost_model(mut self, cost_model: Box<dyn IselCostModel>) -> Self {
        self.cost_model = cost_model;
        self
    }

    pub fn with_op_lowering(mut self, lowering: OpLowering) -> Self {
        self.op_lowerings.push(lowering);
        self
    }

    fn ensure_block_cache(&mut self, context: &Context, block: &Block) {
        if self.block_cache.contains_key(&block.id()) {
            return;
        }

        let mut value_to_def = HashMap::new();
        for op_id in block.op_ids() {
            let op = context.get_op(op_id);
            for result in &op.results {
                value_to_def.insert(*result, op_id);
            }
        }

        // Build every op's semantic expression directly into the e-graph (it
        // hash-conses, so it is itself the interned DAG), then saturate with the
        // algebraic identities. Class ids are resolved through `find` afterwards
        // because saturation may have merged classes.
        let mut egraph = SemEGraph::new();
        let mut roots_by_op = HashMap::new();
        let op_ids = block.op_ids();
        let class_value = {
            let mut builder = SemDagBuilder::new(context, &value_to_def, &mut egraph);
            for op_id in &op_ids {
                let op = context.get_op(*op_id);
                if let Some(root) = builder.build_for_op(&op) {
                    roots_by_op.insert(*op_id, root);
                }
            }
            builder.class_value
        };
        rewrites::saturate(context, &mut egraph, &self.rewrites, Default::default());

        // Saturation may merge classes, so canonicalize both maps through `find`.
        // When two value-carrying classes merge (the values are provably equal),
        // the earliest-defined op wins: it is deterministic and its result
        // dominates every later use in the block.
        let op_position: HashMap<OpId, usize> = op_ids
            .iter()
            .enumerate()
            .map(|(position, op)| (*op, position))
            .collect();

        let mut op_by_root: HashMap<Id, OpId> = HashMap::new();
        for (op, root) in &roots_by_op {
            op_by_root
                .entry(egraph.find(*root))
                .and_modify(|existing| {
                    if op_position[op] < op_position[existing] {
                        *existing = *op;
                    }
                })
                .or_insert(*op);
        }

        let value_position =
            |v: ValueId| value_to_def.get(&v).map(|op| op_position[op]).unwrap_or(0);
        let mut canon_class_value: HashMap<Id, ValueId> = HashMap::new();
        for (class, value) in class_value {
            canon_class_value
                .entry(egraph.find(class))
                .and_modify(|existing| {
                    if value_position(value) < value_position(*existing) {
                        *existing = value;
                    }
                })
                .or_insert(value);
        }

        let op_root: HashMap<OpId, Id> = roots_by_op
            .iter()
            .map(|(op, root)| (*op, egraph.find(*root)))
            .collect();

        // A value used as an operand by more than one consumer must stay a register.
        let mut operand_uses: HashMap<ValueId, usize> = HashMap::new();
        for op_id in &op_ids {
            for operand in &context.get_op(*op_id).operands {
                *operand_uses.entry(*operand).or_insert(0) += 1;
            }
        }
        let mut shared_classes = HashSet::new();
        for (op_id, root) in &roots_by_op {
            let op = context.get_op(*op_id);
            if op
                .results
                .iter()
                .any(|r| operand_uses.get(r).copied().unwrap_or(0) > 1)
            {
                shared_classes.insert(egraph.find(*root));
            }
        }

        // A value used by an op no match can reach (it lowered to no e-graph root)
        // or by an op outside this block can never be recomputed inside a fused
        // instruction, so its class must keep a materializing alternative.
        let block_ops: HashSet<OpId> = op_ids.iter().copied().collect();
        let mut must_materialize = HashSet::new();
        for (op_id, root) in &roots_by_op {
            let op = context.get_op(*op_id);
            let escapes = op.results.iter().any(|result| {
                context
                    .get_value(*result)
                    .uses()
                    .iter()
                    .any(|u| !block_ops.contains(&u.op()) || !roots_by_op.contains_key(&u.op()))
            });
            if escapes {
                must_materialize.insert(egraph.find(*root));
            }
        }

        self.block_cache.insert(
            block.id(),
            BlockSelectionCache {
                egraph,
                op_by_root,
                op_root,
                class_value: canon_class_value,
                shared_classes,
                must_materialize,
                plan: None,
            },
        );
    }

    fn ensure_block_solution(&mut self, context: &Context, block: &Block) {
        self.ensure_block_cache(context, block);
        let Some(cache) = self.block_cache.get(&block.id()) else {
            return;
        };
        if cache.plan.is_some() {
            return;
        }

        let plan = self.solve_block(context, block, cache);
        if let Some(cache) = self.block_cache.get_mut(&block.id()) {
            cache.plan = Some(plan);
        }
    }

    fn commit_block_solution(
        &mut self,
        context: &Context,
        block: &Block,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        if !self.emitted_blocks.insert(block.id()) {
            return Ok(());
        }

        self.ensure_block_solution(context, block);
        let plan = match self
            .block_cache
            .get(&block.id())
            .and_then(|cache| cache.plan.clone())
        {
            Some(Ok(plan)) => plan,
            Some(Err(message)) => return Err(PassError::InvalidRuleSet(message)),
            None => return Ok(()),
        };

        let block_arc = context.get_block(block.id());

        // Insert the rewrite-introduced instructions first, in operand-first order,
        // each ahead of its anchor op. The request carries only the fresh
        // destination value: there is no backing IR op.
        for intro in &plan.introduced {
            let request = EmitRequest {
                op: None,
                results: std::slice::from_ref(&intro.dest),
                result_ty: Some(intro.dest_ty),
            };
            let rule = &self.rules[intro.rule_index];
            let new_op = (rule.emit_fn)(context, &request, &intro.m)?;
            let anchor =
                OperationRef::new(context.get_op(intro.anchor), Some(block_arc.clone()), None);
            rewriter.insert_op_before(&anchor, new_op.as_ref())?;
        }

        // Insert the definer of each implicit register use just before the op that
        // reads it. The definer has no destination value (its emitter hardwires one).
        for definer in &plan.definers {
            let request = EmitRequest {
                op: None,
                results: &[],
                result_ty: None,
            };
            let emit_fn = self.definers[definer.definer_index].emit_fn;
            let new_op = emit_fn(context, &request, &definer.m)?;
            let anchor = OperationRef::new(
                context.get_op(definer.anchor),
                Some(block_arc.clone()),
                None,
            );
            rewriter.insert_op_before(&anchor, new_op.as_ref())?;
        }

        // Rewrite the original ops in reverse block order — consumers before
        // defs — so when a def's replacement remaps SSA uses of its results
        // (`replace_op`), every already-emitted consumer is visible. Positions
        // are resolved by id, so the insertions above do not invalidate this.
        let commit_order: Vec<OpId> = block_arc
            .op_ids()
            .into_iter()
            .rev()
            .filter(|op_id| plan.op_decisions.contains_key(op_id))
            .collect();
        for op_id in &commit_order {
            let decision = &plan.op_decisions[op_id];
            let op_ref = OperationRef::new(context.get_op(*op_id), Some(block_arc.clone()), None);
            match decision {
                BlockDecision::Emit { rule_index, m } => {
                    let rule = &self.rules[*rule_index];
                    let request = EmitRequest::for_op(&op_ref, context);
                    let new_op = (rule.emit_fn)(context, &request, m)?;
                    rewriter.replace_op(&op_ref, new_op.as_ref())?;
                }
                BlockDecision::Consume => {
                    rewriter.erase_op(&op_ref)?;
                }
            }
        }

        // Drop constants left dead by selection: an immediate operand folds its
        // constant into the instruction's attribute (e.g. `slliw`'s `imm`), so the
        // defining `constant` op no longer feeds anything. It binds to an *immediate
        // boundary*, never an interior node, so the cover gives it neither Emit nor
        // Consume and it lingers as dead code. Replacing the consumer detached the
        // constant's operand use, and the folded immediate is an `Int` attribute (not
        // a register use), so the maintained def-use chain now reports zero uses.
        for op_id in block_arc.op_ids() {
            let op = context.get_op(op_id);
            if op.name != "constant" {
                continue;
            }
            if op.results.iter().all(|v| !context.is_value_used(*v)) {
                let op_ref = OperationRef::new(op, Some(block_arc.clone()), None);
                rewriter.erase_op(&op_ref)?;
            }
        }

        Ok(())
    }

    fn solve_block(
        &self,
        context: &Context,
        block: &Block,
        cache: &BlockSelectionCache,
    ) -> Result<BlockPlan, String> {
        let mut op_refs = HashMap::new();
        for (position, op_id) in block.op_ids().into_iter().enumerate() {
            let op = context.get_op(op_id);
            op_refs.insert(
                op_id,
                OperationRef::new(op, Some(context.get_block(block.id())), Some(position)),
            );
        }

        let matches = self.collect_block_matches(context, cache, &op_refs);

        if let Some(message) = completeness_error(&cache.egraph, &cache.op_by_root, &matches) {
            return Err(message);
        }
        if matches.is_empty() {
            return Ok(BlockPlan::default());
        }

        let Some(cover) = build_eclass_cover(
            &cache.egraph,
            &cache.op_by_root,
            &cache.must_materialize,
            &matches,
        ) else {
            return Ok(BlockPlan::default());
        };

        // The match chosen as Root for each e-class, and the classes consumed as an
        // interior node of some selected match.
        let mut root_match: HashMap<Id, usize> = HashMap::new();
        let mut internal_classes: HashSet<Id> = HashSet::new();
        for (node, choice) in cover.choices.iter().enumerate() {
            match choice {
                PbqpIselAlternative::Root { match_id } => {
                    root_match.insert(cover.classes[node], *match_id);
                }
                PbqpIselAlternative::Internal { .. } => {
                    internal_classes.insert(cover.classes[node]);
                }
                PbqpIselAlternative::External => {}
            }
        }

        let mut emit = EmissionBuilder {
            egraph: &cache.egraph,
            class_value: &cache.class_value,
            op_by_root: &cache.op_by_root,
            matches: &matches,
            root_match: &root_match,
            context,
            introduced_dest: HashMap::new(),
            introduced: Vec::new(),
        };

        let mut op_decisions = HashMap::new();
        for op_id in block.op_ids() {
            let Some(class) = cache.op_root.get(&op_id).map(|c| cache.egraph.find(*c)) else {
                continue;
            };
            if let Some(&match_id) = root_match.get(&class) {
                let result_ty = context
                    .get_op(op_id)
                    .results
                    .first()
                    .map(|v| context.get_value(*v).ty());
                let m = emit.resolve_match(match_id, op_id, result_ty);
                op_decisions.insert(
                    op_id,
                    BlockDecision::Emit {
                        rule_index: matches[match_id].rule_index,
                        m,
                    },
                );
            } else if internal_classes.contains(&class) {
                op_decisions.insert(op_id, BlockDecision::Consume);
            }
        }

        // Honor each selected op's implicit register reads: introduce the register's
        // definer ahead of it, passing exactly the value that read bound to. This is
        // the register def/use edge declared in the instruction's behavior; the
        // value crossing it is the only thing threaded — nothing here inspects the
        // operands' types.
        let mut definers = Vec::new();
        for op_id in block.op_ids() {
            let Some(BlockDecision::Emit { rule_index, .. }) = op_decisions.get(&op_id) else {
                continue;
            };
            let rule = &self.rules[*rule_index];
            if rule.implicit_uses.is_empty() {
                continue;
            }
            let Some(class) = cache.op_root.get(&op_id).map(|c| cache.egraph.find(*c)) else {
                continue;
            };
            let Some(&match_id) = root_match.get(&class) else {
                continue;
            };
            for implicit_use in &rule.implicit_uses {
                let Some((_, bound)) = matches[match_id]
                    .bindings
                    .captures
                    .entries
                    .iter()
                    .find(|(sym, _)| *sym == implicit_use.symbol)
                else {
                    continue;
                };
                let bound = cache.egraph.find(*bound);
                let Some(binding) = class_binding(&cache.egraph, &cache.class_value, bound) else {
                    continue;
                };
                let value_immediate = matches!(binding, Binding::Int(_));
                let Some((definer_index, definer)) =
                    self.definers.iter().enumerate().find(|(_, d)| {
                        d.register_class == implicit_use.register_class
                            && d.register_index == implicit_use.register_index
                            && d.value_is_immediate == value_immediate
                    })
                else {
                    continue;
                };
                let m = match binding {
                    Binding::Int(v) => RuleMatch::new(vec![(definer.value_symbol, v)], vec![]),
                    Binding::Value(v) => RuleMatch::new(vec![], vec![(definer.value_symbol, v)]),
                };
                definers.push(DefinerEmit {
                    definer_index,
                    m,
                    anchor: op_id,
                });
            }
        }

        Ok(BlockPlan {
            op_decisions,
            introduced: emit.introduced,
            definers,
        })
    }

    fn collect_block_matches(
        &self,
        context: &Context,
        cache: &BlockSelectionCache,
        op_refs: &HashMap<OpId, OperationRef>,
    ) -> Vec<PbqpIselMatch> {
        let mut matches = Vec::new();
        for (pattern_index, compiled) in self.compiled_patterns.iter().enumerate() {
            let rule = &self.rules[compiled.rule_index];
            let Some(pattern_root) = compiled.pattern.root() else {
                continue;
            };
            let pattern = &compiled.pattern;

            // A pure class may sit interior to any number of matches: each fused
            // instruction recomputes it, and whether the defining op is erased is
            // the solver's separate Consume decision. A shared *memory effect*
            // must stay materialized — it may be a match root or a boundary
            // operand, never an interior node a larger match would consume.
            let allowed = |pattern_node: NodeId, class: Id| {
                pattern_node == pattern_root
                    || pattern.is_duplicable(pattern_node)
                    || node::class_is_pure(&cache.egraph, class)
                    || !cache.shared_classes.contains(&cache.egraph.find(class))
            };

            for m in ematch::ematch_with_legality(&cache.egraph, context, pattern, &allowed) {
                let root = cache.egraph.find(m.root());
                let op_id = cache.op_by_root.get(&root).copied();
                // Instructions root at computed values: an original op result, or a
                // rewrite-introduced intermediate (which has no op). Matches rooted at
                // a pure operand (leaf/constant) are not instruction candidates.
                let is_computed = cache
                    .egraph
                    .nodes(root)
                    .iter()
                    .any(|n| !n.children().is_empty());
                if op_id.is_none() && !is_computed {
                    continue;
                }

                let mut captures = CaptureBindings::new();
                for (pattern_node, symbol) in &compiled.boundary_symbols {
                    captures.bind(*symbol, cache.egraph.find(m.binding(*pattern_node)));
                }

                let pattern_nodes = (0..pattern.len())
                    .map(NodeId::from_index)
                    .map(|pattern_node| PatternNodeBinding {
                        pattern_node,
                        class: cache.egraph.find(m.binding(pattern_node)),
                        // Constants are boundary-like: pure, folded into the
                        // encoding, never consumed by the match — so the same
                        // constant class (e.g. the literal 0) can sit inside one
                        // match and under a boundary of another without making
                        // the cover infeasible.
                        is_boundary: match pattern.get_node(pattern_node) {
                            PatternExpr::Boundary => true,
                            PatternExpr::Node(node) => node.kind == SymKind::Constant,
                            _ => false,
                        },
                    })
                    .collect();
                let bindings = FullMatchBindings {
                    captures,
                    pattern_nodes,
                };

                // Cost is op-relative when there is a backing op; a
                // rewrite-introduced root has no op, so it takes the rule's
                // target-independent base cost.
                let rule_match = bindings
                    .captures
                    .to_rule_match(&cache.egraph, &cache.class_value);
                let cost = if let Some(op_ref) = op_id.and_then(|id| op_refs.get(&id)) {
                    self.cost_model
                        .node_cost(context, op_ref, rule, &rule_match)
                } else {
                    rule.base_cost as u64
                };

                matches.push(PbqpIselMatch {
                    pattern_index,
                    rule_index: compiled.rule_index,
                    root,
                    pattern_root,
                    bindings,
                    cost,
                });
            }
        }
        prune_dominated_matches(&self.compiled_patterns, &mut matches);
        matches
    }
}
impl Pass for InstructionSelectPass {
    fn name(&self) -> &'static str {
        "instruction-select"
    }

    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        for lowering in &self.op_lowerings {
            if lowering(context, op, rewriter)? {
                return Ok(());
            }
        }

        // Result-less ops still participate: a store must trigger its block's
        // selection even when no value-producing op precedes it.
        let Some(block) = op.block() else {
            return Ok(());
        };

        self.commit_block_solution(context, block, rewriter)
    }
}
