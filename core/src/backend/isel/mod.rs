//! Instruction selection over semantic e-graphs.
//!
//! Each block's operations are lowered into an e-graph of semantic expressions
//! ([`builder`]), saturated with proved algebraic rewrites ([`rewrites`]), and
//! covered by the target's instruction patterns ([`pattern`]) — e-matched by
//! the shared [`tir_symbolic::egraph`] engine — via a PBQP instance over
//! e-classes ([`cover`]). The solved cover becomes an emission plan ([`emit`])
//! the pass commits through the rewriter.

mod builder;
mod cover;
mod emit;
mod node;
mod pattern;
mod rewrites;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use tir::{
    Block, BlockId, BranchGuard, BranchTerminator, Context, OpId, Operation, OperationRef, Pass,
    PassError, PassTarget, Rewriter, Terminator, TypeId, ValueId,
    graph::OperandConstraint,
    sem::{SemGraph, SymKind},
};
use tir_adt::APInt;
use tir_symbolic::egraph::{ENode, Id, Var};

pub use node::{SemEGraph, SemNode, SemPayload};
pub use rewrites::{IselRewrite, SaturationLimits};
pub use tir_symbolic::egraph::EMatch;

use builder::SemDagBuilder;
use cover::{
    CaptureBindings, FullMatchBindings, PatternNodeBinding, PbqpIselAlternative, PbqpIselMatch,
    build_eclass_cover, completeness_error, prune_dominated_matches,
};
use emit::{BlockDecision, BlockPlan, DefinerEmit, EmissionBuilder, GuardBranch, TerminatorPlan};
use node::{Binding, class_binding, class_int_binding, template_node};
use pattern::{CompiledIselPattern, compile_isel_pattern};
use rewrites::discover_rewrites;
#[cfg(test)]
use rewrites::extension_rewrite;

#[derive(Debug, Clone)]
pub struct RuleMatch {
    int_bindings: Vec<(u32, APInt)>,
    value_bindings: Vec<(u32, ValueId)>,
    /// Block operands (branch targets), bound by conditional-branch selection.
    block_bindings: Vec<(u32, BlockId)>,
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
            block_bindings: Vec::new(),
        }
    }

    pub(crate) fn with_block_binding(mut self, symbol: u32, block: BlockId) -> Self {
        self.block_bindings.push((symbol, block));
        self
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

    pub fn block_binding(&self, symbol: u32) -> Option<BlockId> {
        self.block_bindings
            .iter()
            .find(|(sym, _)| *sym == symbol)
            .map(|(_, b)| *b)
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

/// What a rule selects. A `Value` rule computes its pattern's value into a
/// destination register. A `CondBranch` rule is a conditional branch whose
/// pattern is the *branch condition* (from the instruction's guarded PC write);
/// its taken target is bound to `target_symbol` as a block operand at emit time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleKind {
    Value,
    CondBranch { target_symbol: u32 },
}

pub struct Rule {
    pub name: &'static str,
    pub pattern: SemGraph,
    pub base_cost: u32,
    pub kind: RuleKind,
    /// Per-operand-symbol constraint (register vs immediate). Symbols absent here
    /// are unconstrained, so hand-written and synthesized rules keep matching any
    /// value.
    pub operand_constraints: Vec<(u32, OperandConstraint)>,
    /// Per-operand-symbol required value width, for operands the instruction is
    /// width-sensitive in (comparisons, right shifts, division): the operand's
    /// upper bits reach the result, so a narrower value must not bind. Symbols
    /// absent here match any width.
    pub operand_widths: Vec<(u32, u32)>,
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
            kind: RuleKind::Value,
            operand_constraints: Vec::new(),
            operand_widths: Vec::new(),
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

    /// Require operand symbols to bind values of exactly the given width (see
    /// [`Rule::operand_widths`]). Values of unknown width — rewrite-introduced
    /// intermediates carrying no IR type — still match.
    pub fn with_operand_widths(mut self, widths: Vec<(u32, u32)>) -> Self {
        self.operand_widths = widths;
        self
    }

    /// Declare the registers this instruction reads implicitly, so selection
    /// introduces their definers ahead of it.
    pub fn with_implicit_uses(mut self, uses: Vec<ImplicitUse>) -> Self {
        self.implicit_uses = uses;
        self
    }

    /// Mark this rule as a conditional branch (see [`RuleKind::CondBranch`]).
    pub fn with_kind(mut self, kind: RuleKind) -> Self {
        self.kind = kind;
        self
    }
}

/// Target hooks for lowering control-flow terminators, enabling rule-driven
/// conditional-branch selection: `builtin.br` lowers through `uncond`, and
/// `builtin.cond_br` becomes a selected [`RuleKind::CondBranch`] instruction
/// (or `cond_nonzero` when no branch rule fuses the condition) followed by an
/// `uncond` to the false successor.
#[derive(Clone, Copy)]
pub struct BranchEmitters {
    /// Emit an unconditional branch to `dest`, forwarding `args` to its block
    /// arguments (typically a virtual branch finalized after regalloc).
    pub uncond: fn(&Context, BlockId, &[ValueId]) -> Box<dyn Operation>,
    /// Emit a branch to `dest` taken when `condition` (an i1 in a register) is
    /// nonzero — the fallback when no branch rule matches the guard condition.
    pub cond_nonzero: fn(&Context, ValueId, BlockId) -> Box<dyn Operation>,
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
    /// The guarded terminators of the block (e.g. `cond_br`), each with its
    /// condition's e-class, when the target supplies branch emitters.
    guards: Vec<BlockGuard>,
    /// Plain unconditional branch terminators, lowered through the target's
    /// `uncond` emitter.
    jumps: Vec<BlockJump>,
    /// Whether the e-graph currently holds an open assumption scope (popped
    /// once the plan is solved).
    scoped: bool,
    /// The solved emission plan, or the completeness error explaining why the block
    /// cannot be selected with this rule set.
    plan: Option<Result<BlockPlan, String>>,
}

/// A guarded two-way terminator: branch to `true_dest` when `condition` is
/// nonzero, else to `false_dest`.
struct BlockGuard {
    op: OpId,
    condition: ValueId,
    /// The canonical e-class holding the condition's semantic expression.
    class: Id,
    true_dest: BlockId,
    false_dest: BlockId,
    /// Whether any edge forwards block arguments (unsupported by codegen).
    has_edge_args: bool,
}

/// An unconditional branch terminator and its forwarded block arguments.
struct BlockJump {
    op: OpId,
    dest: BlockId,
    args: Vec<ValueId>,
}

/// The fact a guarded CFG edge carries: on this edge, `condition` is known to
/// equal `holds`.
#[derive(Clone, Copy)]
struct EdgeFact {
    condition: ValueId,
    holds: bool,
}

/// An edge fact prepared against a block's e-graph: the condition's class and,
/// when its definer is a comparison, the comparison class with its kind and
/// operand classes.
struct PreparedAssumption {
    holds: bool,
    condition: Id,
    compare: Option<(Id, SymKind, Id, Id)>,
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
    /// Target hooks for terminator lowering; branch selection is off without them
    /// (terminators are then left to the target's op lowerings).
    branch_emitters: Option<BranchEmitters>,
    cost_model: Box<dyn IselCostModel>,
    op_lowerings: Vec<OpLowering>,
    block_cache: HashMap<BlockId, BlockSelectionCache>,
    emitted_blocks: HashSet<BlockId>,
    /// Incoming CFG edges per block — the fact carried when the edge is guarded
    /// — recorded when the pass visits the enclosing function.
    cfg_in_edges: HashMap<BlockId, Vec<Option<EdgeFact>>>,
    /// Region entry blocks: they carry an implicit edge from the function
    /// boundary, so no single CFG edge dominates them.
    entry_blocks: HashSet<BlockId>,
}

impl InstructionSelectPass {
    pub fn new(rules: Vec<Rule>) -> Self {
        let compiled_patterns: Vec<_> = rules
            .iter()
            .enumerate()
            .filter_map(|(rule_index, rule)| {
                compile_isel_pattern(
                    rule_index,
                    &rule.pattern,
                    &rule.operand_constraints,
                    &rule.operand_widths,
                )
            })
            .collect();

        let rewrites = discover_rewrites(&compiled_patterns);

        Self {
            rules,
            compiled_patterns,
            rewrites,
            definers: Vec::new(),
            branch_emitters: None,
            cost_model: Box::new(DefaultIselCostModel),
            op_lowerings: vec![],
            block_cache: HashMap::new(),
            emitted_blocks: HashSet::new(),
            cfg_in_edges: HashMap::new(),
            entry_blocks: HashSet::new(),
        }
    }

    /// Install the target's terminator emitters, enabling rule-driven selection
    /// of conditional branches (and generic lowering of unconditional ones).
    pub fn with_branch_emitters(mut self, emitters: BranchEmitters) -> Self {
        self.branch_emitters = Some(emitters);
        self
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

    /// Record the CFG edges of every block in `op`'s regions (called when the
    /// pass visits the enclosing function, before any of its blocks commit).
    /// Guarded edges carry the guard's fact; region entries are excluded from
    /// assumptions (they have an implicit incoming edge).
    fn record_cfg(&mut self, context: &Context, op: &OperationRef) {
        for region_id in &op.op().regions {
            let region = context.get_region(*region_id);
            let mut blocks = region.iter(context.clone());
            if let Some(entry) = blocks.next() {
                self.entry_blocks.insert(entry.id());
            }
            for block in region.iter(context.clone()) {
                for op_id in block.op_ids() {
                    let inst = context.get_op(op_id);
                    if let Some(guard) = inst.clone().as_interface::<dyn BranchGuard>() {
                        for (dest, condition, holds) in guard.guarded_successors() {
                            self.cfg_in_edges
                                .entry(dest)
                                .or_default()
                                .push(Some(EdgeFact { condition, holds }));
                        }
                    } else if let Some(terminator) = inst.clone().as_interface::<dyn Terminator>() {
                        for dest in terminator.successors() {
                            self.cfg_in_edges.entry(dest).or_default().push(None);
                        }
                    }
                }
            }
        }
    }

    /// The fact known to hold throughout `block`: it is entered through exactly
    /// one CFG edge, and that edge is guarded.
    fn edge_fact(&self, block: BlockId) -> Option<EdgeFact> {
        if self.entry_blocks.contains(&block) {
            return None;
        }
        match self.cfg_in_edges.get(&block)?.as_slice() {
            [Some(fact)] => Some(*fact),
            _ => None,
        }
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
        let mut guards = Vec::new();
        let mut jumps = Vec::new();
        let mut assumption = None;
        let class_value = {
            let mut builder = SemDagBuilder::new(context, &value_to_def, &mut egraph);
            for op_id in &op_ids {
                let op = context.get_op(*op_id);
                if let Some(root) = builder.build_for_op(&op) {
                    roots_by_op.insert(*op_id, root);
                }
            }

            // A block entered through exactly one guarded CFG edge inherits the
            // guard's fact: the condition holds (or not) throughout. The related
            // classes are built in the base graph; the equalities they imply are
            // asserted in an assumption scope popped once the plan is solved.
            if let Some(fact) = self.edge_fact(block.id()) {
                assumption = Some(PreparedAssumption {
                    holds: fact.holds,
                    condition: builder.build_from_value(fact.condition),
                    compare: builder.build_defining_compare(fact.condition),
                });
            }
            // With branch emitters installed, terminators are selected here too:
            // a guarded two-way terminator's condition is lowered into the
            // e-graph so branch rules can match (and fuse) it; a plain branch is
            // recorded for the target's unconditional emitter.
            if self.branch_emitters.is_some() {
                for op_id in &op_ids {
                    let op = context.get_op(*op_id);
                    if let Some(guard) = op.clone().as_interface::<dyn BranchGuard>() {
                        let successors = guard.guarded_successors();
                        let [(a_dest, a_cond, a_taken), (b_dest, b_cond, _)] =
                            successors.as_slice()
                        else {
                            continue;
                        };
                        if a_cond != b_cond {
                            continue;
                        }
                        let has_edge_args = op
                            .clone()
                            .as_interface::<dyn BranchTerminator>()
                            .is_some_and(|branch| {
                                branch
                                    .successor_operands()
                                    .iter()
                                    .any(|(_, args)| !args.is_empty())
                            });
                        let (true_dest, false_dest) = if *a_taken {
                            (*a_dest, *b_dest)
                        } else {
                            (*b_dest, *a_dest)
                        };
                        let class = builder.build_from_value(*a_cond);
                        guards.push(BlockGuard {
                            op: *op_id,
                            condition: *a_cond,
                            class,
                            true_dest,
                            false_dest,
                            has_edge_args,
                        });
                    } else if let Some(branch) = op.clone().as_interface::<dyn BranchTerminator>() {
                        let successors = branch.successor_operands();
                        let [(dest, args)] = successors.as_slice() else {
                            continue;
                        };
                        jumps.push(BlockJump {
                            op: *op_id,
                            dest: *dest,
                            args: args.clone(),
                        });
                    }
                }
            }
            builder.class_value
        };

        // Assert the dominating edge's fact inside an assumption scope: the
        // condition (and its defining comparison, when there is one) equals its
        // known truth value, the complement comparison equals the opposite, and
        // an `eq`/`ne` guard makes its operands congruent. Saturation and the
        // solve below run inside the scope; it is popped once the plan is
        // stored, leaving the cached e-graph assumption-free.
        let scoped = assumption.is_some();
        if let Some(assumption) = assumption {
            egraph.push_context();
            let truth = |egraph: &mut SemEGraph, holds: bool| {
                egraph.add(template_node(
                    SymKind::Constant,
                    Some(tir::sem::SymPayload::Int(APInt::new(1, holds as u64))),
                    None,
                ))
            };
            let known = truth(&mut egraph, assumption.holds);
            egraph.union(assumption.condition, known);
            if let Some((compare, kind, lhs, rhs)) = assumption.compare {
                egraph.union(compare, known);
                if let Some(complement) = node::complement_comparison(kind) {
                    let mut node = template_node(
                        complement,
                        None,
                        Some(tir::builtin::IntegerType::new(context, 1)),
                    );
                    node.children = vec![lhs, rhs];
                    let complement_class = egraph.add(node);
                    let opposite = truth(&mut egraph, !assumption.holds);
                    egraph.union(complement_class, opposite);
                }
                if (kind == SymKind::Eq && assumption.holds)
                    || (kind == SymKind::Ne && !assumption.holds)
                {
                    egraph.union(lhs, rhs);
                }
            }
            egraph.rebuild();
        }

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
        // instruction, so its class must keep a materializing alternative. A use
        // by a guarded terminator is exempt: branch selection either fuses the
        // condition (recomputing it inside the branch instruction) or re-adds the
        // materialization requirement itself (see `solve_block`).
        let block_ops: HashSet<OpId> = op_ids.iter().copied().collect();
        let guard_ops: HashSet<OpId> = guards.iter().map(|guard| guard.op).collect();
        let mut must_materialize = HashSet::new();
        for (op_id, root) in &roots_by_op {
            let op = context.get_op(*op_id);
            let escapes = op.results.iter().any(|result| {
                context.get_value(*result).uses().iter().any(|u| {
                    !block_ops.contains(&u.op())
                        || (!roots_by_op.contains_key(&u.op()) && !guard_ops.contains(&u.op()))
                })
            });
            if escapes {
                must_materialize.insert(egraph.find(*root));
            }
        }

        for guard in &mut guards {
            guard.class = egraph.find(guard.class);
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
                guards,
                jumps,
                scoped,
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
            // The plan is concrete (rules, values, blocks); drop the assumption
            // scope so the cached e-graph reverts to unassumed facts.
            if cache.scoped {
                cache.egraph.pop_context();
                cache.scoped = false;
            }
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

        // Lower the terminators first: a fused conditional branch reads its
        // operand *values* (not the condition register), so the condition's
        // defining op — possibly erased as Dead below — must lose its last use
        // before the main loop runs.
        if let Some(emitters) = &self.branch_emitters {
            for terminator in &plan.terminators {
                match terminator {
                    TerminatorPlan::Guard {
                        op,
                        branch,
                        false_dest,
                    } => {
                        let op_ref =
                            OperationRef::new(context.get_op(*op), Some(block_arc.clone()), None);
                        let branch_op: Box<dyn Operation> = match branch {
                            GuardBranch::Fused { rule_index, m } => {
                                let request = EmitRequest {
                                    op: None,
                                    results: &[],
                                    result_ty: None,
                                };
                                (self.rules[*rule_index].emit_fn)(context, &request, m)?
                            }
                            GuardBranch::Nonzero { condition, dest } => {
                                (emitters.cond_nonzero)(context, *condition, *dest)
                            }
                        };
                        rewriter.insert_op_before(&op_ref, branch_op.as_ref())?;
                        let fallthrough = (emitters.uncond)(context, *false_dest, &[]);
                        rewriter.replace_op(&op_ref, fallthrough.as_ref())?;
                    }
                    TerminatorPlan::Jump { op, dest, args } => {
                        let op_ref =
                            OperationRef::new(context.get_op(*op), Some(block_arc.clone()), None);
                        let jump = (emitters.uncond)(context, *dest, args);
                        rewriter.replace_op(&op_ref, jump.as_ref())?;
                    }
                }
            }
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

        // Resolve each guarded terminator: fuse its condition into a branch-rule
        // instruction when one matches, else fall back to the target's
        // branch-if-nonzero (which needs the condition materialized). Fused
        // branches read their operands as registers, so those classes join the
        // materialization set; a condition consumed only by its fused branch may
        // instead go Dead (its defining op is erased).
        let mut must_materialize = cache.must_materialize.clone();
        let mut fused_conditions = HashSet::new();
        let mut terminators = Vec::new();
        for guard in &cache.guards {
            if guard.has_edge_args {
                return Err(
                    "block arguments on conditional branch edges are not supported by codegen yet"
                        .to_string(),
                );
            }
            // A condition proven constant (a dominating-edge assumption) folds
            // the guard to an unconditional branch to the known successor.
            if let Some(known) = class_int_binding(&cache.egraph, guard.class) {
                let dest = if known.to_u64() != 0 {
                    guard.true_dest
                } else {
                    guard.false_dest
                };
                terminators.push(TerminatorPlan::Jump {
                    op: guard.op,
                    dest,
                    args: Vec::new(),
                });
                continue;
            }
            match self.select_guard_branch(context, cache, guard) {
                Some((rule_index, m, boundary_classes)) => {
                    for class in boundary_classes {
                        must_materialize.insert(cache.egraph.find(class));
                    }
                    fused_conditions.insert(guard.class);
                    terminators.push(TerminatorPlan::Guard {
                        op: guard.op,
                        branch: GuardBranch::Fused { rule_index, m },
                        false_dest: guard.false_dest,
                    });
                }
                None => {
                    must_materialize.insert(guard.class);
                    terminators.push(TerminatorPlan::Guard {
                        op: guard.op,
                        branch: GuardBranch::Nonzero {
                            condition: guard.condition,
                            dest: guard.true_dest,
                        },
                        false_dest: guard.false_dest,
                    });
                }
            }
        }
        for jump in &cache.jumps {
            terminators.push(TerminatorPlan::Jump {
                op: jump.op,
                dest: jump.dest,
                args: jump.args.clone(),
            });
        }

        let dead_allowed: HashSet<Id> = fused_conditions
            .iter()
            .copied()
            .filter(|class| !must_materialize.contains(class))
            .collect();

        if let Some(message) =
            completeness_error(&cache.egraph, &cache.op_by_root, &matches, &dead_allowed)
        {
            return Err(message);
        }
        // The cover still runs with no value matches when a fused condition can
        // go Dead: its defining op must receive the Consume decision. Without
        // either, only constant-proven op roots (a dominating-edge assumption)
        // need decisions — their consumers fold the immediate, so they are
        // erased.
        if matches.is_empty() && dead_allowed.is_empty() {
            let mut op_decisions = HashMap::new();
            for op_id in block.op_ids() {
                let Some(class) = cache.op_root.get(&op_id).map(|c| cache.egraph.find(*c)) else {
                    continue;
                };
                if !must_materialize.contains(&class)
                    && class_int_binding(&cache.egraph, class).is_some()
                {
                    op_decisions.insert(op_id, BlockDecision::Consume);
                }
            }
            return Ok(BlockPlan {
                op_decisions,
                terminators,
                ..BlockPlan::default()
            });
        }

        let Some(cover) = build_eclass_cover(
            &cache.egraph,
            &cache.op_by_root,
            &must_materialize,
            &dead_allowed,
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
                // A Dead condition's defining op is erased like a consumed
                // internal: the fused branch recomputes the value.
                PbqpIselAlternative::Internal { .. } | PbqpIselAlternative::Dead => {
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
            } else if !must_materialize.contains(&class)
                && class_int_binding(&cache.egraph, class).is_some()
            {
                // The class is proven constant under the block's assumption:
                // consumers fold the immediate (or read the merged input value's
                // register), so the defining op is erased.
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
            terminators,
        })
    }

    /// The best conditional-branch rule match rooted at a guard's condition
    /// class: the rule, the operand bindings (with the taken target bound as a
    /// block), and the boundary classes the branch reads as registers. `None`
    /// when no branch rule matches (the fallback path).
    fn select_guard_branch(
        &self,
        context: &Context,
        cache: &BlockSelectionCache,
        guard: &BlockGuard,
    ) -> Option<(usize, RuleMatch, Vec<Id>)> {
        let mut best: Option<(u64, usize, usize, RuleMatch, Vec<Id>)> = None;
        for compiled in &self.compiled_patterns {
            let rule = &self.rules[compiled.rule_index];
            let RuleKind::CondBranch { target_symbol } = rule.kind else {
                continue;
            };
            for m in compiled.search(&cache.egraph, context) {
                if cache.egraph.find(m.root) != guard.class {
                    continue;
                }

                let mut captures = CaptureBindings::new();
                for (var, class) in m.subst.entries() {
                    let Var::Symbol(symbol) = var else { continue };
                    captures.bind(*symbol, cache.egraph.find(class));
                }

                // Every operand must resolve: immediates fold into the encoding;
                // register operands make their classes materialization
                // requirements. An unresolvable boundary (e.g. a rewrite-introduced
                // class with no backing value) disqualifies the match.
                let mut boundary_classes = Vec::new();
                let resolvable = captures.entries.iter().all(|(_, class)| {
                    match class_binding(&cache.egraph, &cache.class_value, *class) {
                        Some(Binding::Int(_)) => true,
                        Some(Binding::Value(_)) => {
                            boundary_classes.push(*class);
                            true
                        }
                        None => false,
                    }
                });
                if !resolvable {
                    continue;
                }

                let cost = rule.base_cost as u64;
                let specificity = compiled.specificity;
                let better = match &best {
                    None => true,
                    Some((best_cost, best_specificity, ..)) => {
                        cost < *best_cost || (cost == *best_cost && specificity > *best_specificity)
                    }
                };
                if better {
                    let rule_match = captures
                        .to_rule_match(&cache.egraph, &cache.class_value)
                        .with_block_binding(target_symbol, guard.true_dest);
                    best = Some((
                        cost,
                        specificity,
                        compiled.rule_index,
                        rule_match,
                        boundary_classes,
                    ));
                }
            }
        }
        best.map(|(_, _, rule_index, m, boundaries)| (rule_index, m, boundaries))
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
            // Branch rules select terminators, not values (see `select_guard_branch`).
            if rule.kind != RuleKind::Value {
                continue;
            }
            let pattern_root = compiled.pattern.root();

            // A pure class may sit interior to any number of matches: each fused
            // instruction recomputes it, and whether the defining op is erased is
            // the solver's separate Consume decision. A shared *memory effect*
            // must stay materialized — it may be a match root or a boundary
            // operand, never an interior node a larger match would consume.
            // Boundaries additionally honor the rule's register/immediate/width
            // requirements.
            let allowed = |pattern_node: Id, class: Id| {
                if !compiled.boundary_ok(&cache.egraph, context, pattern_node, class) {
                    return false;
                }
                pattern_node == pattern_root
                    || compiled.node_meta[pattern_node.index()].duplicable
                    || node::class_is_pure(&cache.egraph, class)
                    || !cache.shared_classes.contains(&cache.egraph.find(class))
            };

            for m in compiled
                .pattern
                .search_with_legality(&cache.egraph, &allowed)
            {
                let root = cache.egraph.find(m.root);
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
                for (var, class) in m.subst.entries() {
                    let Var::Symbol(symbol) = var else { continue };
                    captures.bind(*symbol, cache.egraph.find(class));
                }

                let pattern_nodes = (0..compiled.pattern.len())
                    .map(|index| Id::from_raw(index as u32))
                    .map(|pattern_node| {
                        let meta = &compiled.node_meta[pattern_node.index()];
                        PatternNodeBinding {
                            pattern_node,
                            class: cache.egraph.find(m.binding(pattern_node)),
                            // Constants are boundary-like: pure, folded into the
                            // encoding, never consumed by the match — so the same
                            // constant class (e.g. the literal 0) can sit inside one
                            // match and under a boundary of another without making
                            // the cover infeasible.
                            is_boundary: meta.is_boundary || meta.is_constant,
                        }
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
        // The function op is visited before any of its blocks' ops: record its
        // CFG, then solve every block up front — a dominating-edge assumption
        // reads the guard condition's *defining op*, which a dominator's commit
        // would otherwise have replaced by the time the dominated block solves.
        if !op.op().regions.is_empty() {
            self.record_cfg(context, op);
            for region_id in &op.op().regions {
                let region = context.get_region(*region_id);
                for block in region.iter(context.clone()) {
                    if !block.is_empty() {
                        self.ensure_block_solution(context, &block);
                    }
                }
            }
        }

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
