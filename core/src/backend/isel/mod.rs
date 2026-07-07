//! Instruction selection over semantic e-graphs.
//!
//! Each block's operations are lowered into an e-graph of semantic expressions
//! ([`builder`]), saturated with proved algebraic rewrites ([`rewrites`]), and
//! covered by the target's instruction patterns ([`pattern`]) — e-matched by
//! the shared [`tir_symbolic::egraph`] engine — via a PBQP instance over
//! e-classes ([`cover`]). The solved cover becomes an emission plan ([`emit`])
//! the pass commits through the rewriter.

mod axioms;
mod builder;
mod cover;
mod emit;
mod node;
mod pattern;
mod rewrites;
mod synthesis;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use tir::{
    AnalysisManager, Block, BlockId, BranchGuard, BranchTerminator, Context, OpId, Operation,
    OperationRef, Pass, PassError, PassTarget, PreservedAnalyses, Rewriter, TypeId, ValueId,
    analysis::{DominatingEdgeFacts, DominatorTree, EdgeFact},
    graph::OperandConstraint,
    sem::{SemGraph, SymKind, SymPayload},
};
use tir_adt::APInt;
use tir_symbolic::egraph::{ENode, Id, Var};

pub use node::{SemEGraph, SemNode, SemPayload};
pub use rewrites::{IselRewrite, SaturationLimits};
pub use synthesis::{discover_axioms, render_axioms_file};
pub use tir_symbolic::egraph::EMatch;

use builder::SemDagBuilder;
use cover::{
    CaptureBindings, FullMatchBindings, PatternNodeBinding, PbqpIselAlternative, PbqpIselMatch,
    build_eclass_cover, completeness_error, prune_dominated_matches,
};
use emit::{BlockDecision, BlockPlan, EmissionBuilder, GuardBranch, TerminatorPlan};
use node::{class_int_binding, template_node};
use pattern::{CompiledIselPattern, compile_isel_pattern};
use rewrites::discover_rewrites;

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

/// An immediate operand's encoding range: the field's bit width and whether the
/// instruction sign-extends it. A constant outside the range must not bind — its
/// encoding would silently truncate to a different value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImmRange {
    pub width: u32,
    pub signed: bool,
}

impl ImmRange {
    /// Whether `value` is representable in the field: its 64-bit register
    /// pattern must survive the encode/decode roundtrip (truncate to the
    /// field, extend back per the field's signedness). So `4096` is rejected
    /// by a signed 12-bit field (it would decode as `-2048`), while the
    /// all-ones register constant fits any signed field as `-1`.
    pub fn contains(&self, value: &APInt) -> bool {
        let bits = if value.is_signed() {
            value.to_i64() as u64
        } else {
            value.to_u64()
        };
        if self.width >= 64 {
            return true;
        }
        if self.signed {
            let shift = 64 - self.width;
            (((bits << shift) as i64) >> shift) as u64 == bits
        } else {
            bits >> self.width == 0
        }
    }
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
    /// A companion instruction emitted immediately before the rule's own — a
    /// flag-setting compare (`cmp`, x86 `cmp`/`test`) whose status-register
    /// writes the branch instruction's condition reads. TMDL derives such rules
    /// by composing the definer's flag semantics into the branch guard, so the
    /// pair selects as one condition pattern but emits two real instructions.
    pub prelude_emit: Option<RuleEmitFn>,
    /// Per-operand-symbol constraint (register vs immediate). Symbols absent here
    /// are unconstrained, so hand-written and synthesized rules keep matching any
    /// value.
    pub operand_constraints: Vec<(u32, OperandConstraint)>,
    /// Per-operand-symbol required value width, for operands the instruction is
    /// width-sensitive in (comparisons, right shifts, division): the operand's
    /// upper bits reach the result, so a narrower value must not bind. Symbols
    /// absent here match any width.
    pub operand_widths: Vec<(u32, u32)>,
    /// Per-operand-symbol immediate encoding range. A constant outside the field's
    /// representable range must not bind (its encoding would truncate). Symbols
    /// absent here accept any constant.
    pub operand_imm_ranges: Vec<(u32, ImmRange)>,
    /// Per-operand-symbol float requirement: `true` for operands living in a
    /// float register class, `false` for integer ones. A value whose IR type is
    /// known to be of the other kind must not bind — an integer store must not
    /// consume a float value and vice versa. Symbols absent here (and values of
    /// unknown type) match either.
    pub operand_floats: Vec<(u32, bool)>,
    pub emit_fn: RuleEmitFn,
}

impl Rule {
    pub fn new(name: &'static str, pattern: SemGraph, base_cost: u32, emit_fn: RuleEmitFn) -> Self {
        Self {
            name,
            pattern,
            base_cost,
            kind: RuleKind::Value,
            prelude_emit: None,
            operand_constraints: Vec::new(),
            operand_widths: Vec::new(),
            operand_imm_ranges: Vec::new(),
            operand_floats: Vec::new(),
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

    /// Restrict immediate operand symbols to constants their encoding field can
    /// represent (see [`Rule::operand_imm_ranges`]).
    pub fn with_operand_imm_ranges(mut self, ranges: Vec<(u32, ImmRange)>) -> Self {
        self.operand_imm_ranges = ranges;
        self
    }

    /// Require operand symbols to bind float (or non-float) values (see
    /// [`Rule::operand_floats`]).
    pub fn with_operand_floats(mut self, floats: Vec<(u32, bool)>) -> Self {
        self.operand_floats = floats;
        self
    }

    /// Mark this rule as a conditional branch (see [`RuleKind::CondBranch`]).
    pub fn with_kind(mut self, kind: RuleKind) -> Self {
        self.kind = kind;
        self
    }

    /// Emit a companion instruction ahead of the rule's own (see
    /// [`Rule::prelude_emit`]). The prelude emitter reads the same [`RuleMatch`]
    /// bindings as the rule's emitter.
    pub fn with_prelude_emitter(mut self, emit_fn: RuleEmitFn) -> Self {
        self.prelude_emit = Some(emit_fn);
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
    /// Emit the instruction(s) branching to `dest` when `condition` (an i1 in a
    /// register) is nonzero — the fallback when no branch rule matches the
    /// guard condition. One instruction on targets that compare against a zero
    /// register (`bne cond, x0`); a flag-setting test plus the conditional
    /// branch on flag targets (`test cond, cond` + `jne`, `cmp cond, xzr` +
    /// `b.ne`).
    pub cond_nonzero: fn(&Context, ValueId, BlockId) -> Vec<Box<dyn Operation>>,
}
/// The whole function lowered into one shared, base-saturated e-graph, with the
/// canonical side tables every block's solve reads. Built once when the pass
/// visits the function op; each block then solves against it inside its own
/// assumption scope (the dominating-edge facts).
struct FunctionSelection {
    egraph: SemEGraph,
    /// Every op whose (canonical) root is the class, across all blocks.
    ops_by_root: HashMap<Id, Vec<OpId>>,
    /// The canonical e-class of every lowered op's root (total over all ops).
    op_root: HashMap<OpId, Id>,
    /// Every IR value a (canonical) class computes, so a boundary can resolve to a
    /// register value under the dominance rule at emit time.
    class_values: HashMap<Id, Vec<ValueId>>,
    /// The position of each lowered op within its own block.
    op_position: HashMap<OpId, usize>,
    /// The op defining each IR value (function-wide).
    value_to_def: HashMap<ValueId, OpId>,
    /// The block defining each value, or `None` for a block argument / entry input
    /// (always available in a register).
    value_block: HashMap<ValueId, Option<BlockId>>,
    /// Values with at least one original use outside their defining block: these are
    /// guaranteed materialized in a register, so a dominated block may bind them.
    externally_bound: HashSet<ValueId>,
    /// E-classes used as an operand by more than one consumer (function-wide). A
    /// memory effect in such a class cannot be internalized into a match.
    shared_classes: HashSet<Id>,
    /// Op-root e-classes whose value some consumer can never internalize — a use by
    /// an op no match reaches, or by an op in a different block — so the defining op
    /// must never be consumed.
    must_materialize: HashSet<Id>,
    /// The guarded terminators of each block, each with its condition's e-class.
    guards: HashMap<BlockId, Vec<BlockGuard>>,
    /// Plain unconditional branch terminators per block.
    jumps: HashMap<BlockId, Vec<BlockJump>>,
    /// Each dominating-edge condition prepared against the base graph: the
    /// condition's class and, when its definer is a comparison, the comparison
    /// class with its kind and operand classes. Keyed by the condition value; the
    /// per-block truth (`holds`) is applied when the scope asserts it.
    prepared: HashMap<ValueId, ConditionExpr>,
}

/// A boundary class resolved to concrete operands for a consumer: the proven
/// constant it folds to as an immediate, and/or the register value legal under
/// the dominance rule. A class can carry both (an assumption merges a value with
/// its truth constant); a valueless (pure or rewrite-introduced) class neither.
struct Binding {
    int: Option<APInt>,
    value: Option<ValueId>,
}

impl FunctionSelection {
    /// The base class ids a (scoped-canonical) class covers: the fact scope's
    /// partition members, or the class itself when no scope is open. The side
    /// tables are keyed by base reps, so every per-block query aggregates over
    /// these — an assumption may merge a scoped class over several base keys, and
    /// a query through the scoped rep must see all of them.
    fn base_members(&self, class: Id) -> impl Iterator<Item = Id> + '_ {
        let canon = self.egraph.find(class);
        let members = self.egraph.scope_members(canon);
        members
            .is_empty()
            .then_some(canon)
            .into_iter()
            .chain(members.iter().copied())
    }

    /// Whether any base member of `class` roots a lowered op (function-wide).
    fn is_op_root(&self, class: Id) -> bool {
        self.base_members(class)
            .any(|m| self.ops_by_root.contains_key(&m))
    }

    /// Whether any base member of `class` is used as an operand by more than one
    /// consumer (so a memory effect in it cannot be internalized).
    fn is_shared(&self, class: Id) -> bool {
        self.base_members(class)
            .any(|m| self.shared_classes.contains(&m))
    }

    /// Whether `class` must keep a materializing alternative: a function-wide
    /// requirement (any base member) or the block-local `overlay` (fused-branch
    /// boundaries and materialized guard conditions, keyed by scoped rep).
    fn requires_materialization(&self, class: Id, overlay: &HashSet<Id>) -> bool {
        overlay.contains(&class)
            || self
                .base_members(class)
                .any(|m| self.must_materialize.contains(&m))
    }

    /// Whether any base member of `class` computes an IR value (a candidate for a
    /// register binding). A class with none is pure / rewrite-introduced.
    fn has_values(&self, class: Id) -> bool {
        self.base_members(class)
            .any(|m| self.class_values.contains_key(&m))
    }

    /// Resolve `class` to operands for consumer op `consumer` in `block`: the
    /// proven constant (folds to an immediate) and/or a register value legal under
    /// the dominance rule. The single resolver behind boundary filtering, guard
    /// selection, and emission, so collect-time acceptance implies emit-time
    /// success. A valueless class yields neither — resolvable only as an
    /// introduced dest the caller expects the cover to materialize.
    fn resolve_binding(
        &self,
        dom: &DominatorTree,
        context: &Context,
        class: Id,
        block: BlockId,
        consumer: OpId,
    ) -> Binding {
        Binding {
            int: class_int_binding(&self.egraph, class),
            value: self.register_value(dom, context, class, block, consumer),
        }
    }

    /// The register value to bind `class` as an operand of consumer op `consumer`
    /// in `block`, under the dominance rule: a block argument / entry input; a
    /// same-block def preceding the consumer; or a value defined in a strict
    /// dominator that the original IR already used across blocks (so it is
    /// guaranteed materialized). `None` when no such value exists (the class may
    /// still bind as an immediate, or be materialized as an introduced instruction).
    /// Preference order — same-block earliest, then closest dominator — is
    /// deterministic.
    fn register_value(
        &self,
        dom: &DominatorTree,
        context: &Context,
        class: Id,
        block: BlockId,
        consumer: OpId,
    ) -> Option<ValueId> {
        let mut best: Option<((u8, usize, u32), ValueId)> = None;
        for member in self.base_members(class) {
            let Some(candidates) = self.class_values.get(&member) else {
                continue;
            };
            for &v in candidates {
                let key = match self.value_block.get(&v).copied().flatten() {
                    None => (1u8, 0usize, v.number()),
                    Some(def_block) if def_block == block => {
                        let def = self.value_to_def[&v];
                        if !context.get_block(block).is_before(def, consumer) {
                            continue;
                        }
                        (0, self.op_position[&def], v.number())
                    }
                    Some(def_block) => {
                        if !dom.dominates(def_block, block) || !self.externally_bound.contains(&v) {
                            continue;
                        }
                        (2, self.dom_distance(dom, block, def_block), v.number())
                    }
                };
                if best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
                    best = Some((key, v));
                }
            }
        }
        best.map(|(_, v)| v)
    }

    /// Steps up the dominator chain from `from` to `to` (closer dominators rank
    /// first). `usize::MAX` when `to` is not on the chain. The tree exposes no
    /// depth, so ranking dominators by closeness needs this walk.
    fn dom_distance(&self, dom: &DominatorTree, from: BlockId, to: BlockId) -> usize {
        let mut distance = 0;
        let mut current = Some(from);
        while let Some(block) = current {
            if block == to {
                return distance;
            }
            distance += 1;
            current = dom.idom(block);
        }
        usize::MAX
    }

    /// Whether `class` resolves to an operand for `consumer` in `block`: an
    /// immediate constant; a valueless class (a pure or rewrite-introduced
    /// intermediate the cover materializes); or a class with a candidate value
    /// legal under the dominance rule. A class whose only register candidates are
    /// cross-block non-escaping values is unresolvable.
    fn boundary_resolvable(
        &self,
        dom: &DominatorTree,
        context: &Context,
        class: Id,
        block: BlockId,
        consumer: OpId,
    ) -> bool {
        let binding = self.resolve_binding(dom, context, class, block, consumer);
        binding.int.is_some() || binding.value.is_some() || !self.has_values(class)
    }
}

/// A dominating-edge condition prepared against the base graph (see
/// [`FunctionSelection::prepared`]).
struct ConditionExpr {
    condition: Id,
    compare: Option<(Id, SymKind, Id, Id)>,
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
    /// Target hooks for terminator lowering; branch selection is off without them
    /// (terminators are then left to the target's op lowerings).
    branch_emitters: Option<BranchEmitters>,
    cost_model: Box<dyn IselCostModel>,
    op_lowerings: Vec<OpLowering>,
    /// The solved emission plan of every block (or the error explaining why it
    /// cannot be selected), populated up front when the pass visits each function.
    plans: HashMap<BlockId, Result<BlockPlan, String>>,
    emitted_blocks: HashSet<BlockId>,
    /// Function roots already solved, so a re-visit does not rebuild the graph.
    solved: HashSet<OpId>,
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
                    &rule.operand_imm_ranges,
                    &rule.operand_floats,
                )
            })
            .collect();

        let rewrites = discover_rewrites(&compiled_patterns);

        Self {
            rules,
            compiled_patterns,
            rewrites,
            branch_emitters: None,
            cost_model: Box::new(DefaultIselCostModel),
            op_lowerings: vec![],
            plans: HashMap::new(),
            emitted_blocks: HashSet::new(),
            solved: HashSet::new(),
        }
    }

    /// Install the target's terminator emitters, enabling rule-driven selection
    /// of conditional branches (and generic lowering of unconditional ones).
    pub fn with_branch_emitters(mut self, emitters: BranchEmitters) -> Self {
        self.branch_emitters = Some(emitters);
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

    /// Install the target's discovered bridge axioms (the committed
    /// `isel.axioms` file the `tir axioms` utility generates). An axiom whose
    /// RHS needs a kind the rule set has no atomic instruction for is dropped —
    /// a stale file degrades coverage, never correctness — and every applied
    /// width instantiation is still proved first (see [`axioms`](self)).
    pub fn with_axioms(mut self, file: &str) -> Self {
        let atomics = pattern::atomic_kinds(&self.compiled_patterns);
        for form in axioms::axiom_forms(file) {
            let axiom = axioms::parse_axiom(&form)
                .unwrap_or_else(|e| panic!("invalid axiom `{form}`: {e}"));
            if axiom.rhs_kinds().is_subset(&atomics) {
                self.rewrites.push(axiom.compile());
            }
        }
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

    /// Build the shared function e-graph and solve every block up front. Called
    /// when the pass first visits the function op — a dominating-edge fact reads a
    /// guard condition's *defining op*, which a dominator's commit would replace
    /// by the time the dominated block solves.
    fn solve_function(&mut self, context: &Context, op: &OperationRef, analyses: &AnalysisManager) {
        let root = op.op().id;
        if !self.solved.insert(root) {
            return;
        }
        let dom = analyses.get::<DominatorTree>(context, root);
        let facts = analyses.get::<DominatingEdgeFacts>(context, root);

        let mut fs = self.build_function_selection(context, op, &facts);
        // A fact-free block sees exactly the base graph, so every value pattern's
        // e-match is block-independent: search once here and reuse for all such
        // blocks (fact-bearing blocks re-search under their scope).
        let base_matches = self.base_value_matches(&fs, context);
        for region_id in &op.op().regions {
            let region = context.get_region(*region_id);
            for block in region.iter(context.clone()) {
                if block.is_empty() {
                    continue;
                }
                let plan = self.solve_block(
                    context,
                    &block,
                    &mut fs,
                    &dom,
                    facts.facts(block.id()),
                    &base_matches,
                );
                self.plans.insert(block.id(), plan);
            }
        }
    }

    /// Search every value pattern over the base graph once, honoring the same
    /// legality a fact-free block's solve applies (boundary constraints, and
    /// interior nodes restricted to pure or function-wide op-root classes). A
    /// block narrows this superset to its own op-roots. Non-value patterns get an
    /// empty slot so indices line up with `compiled_patterns`.
    fn base_value_matches(
        &self,
        fs: &FunctionSelection,
        context: &Context,
    ) -> Vec<Vec<EMatch<u32>>> {
        self.compiled_patterns
            .iter()
            .map(|compiled| {
                if self.rules[compiled.rule_index].kind != RuleKind::Value {
                    return Vec::new();
                }
                let pattern_root = compiled.pattern.root();
                compiled
                    .pattern
                    .search_with_legality(&fs.egraph, &|node, class| {
                        value_match_allowed(fs, context, compiled, pattern_root, node, class)
                    })
            })
            .collect()
    }

    /// Lower every block of the function into one shared, base-saturated e-graph
    /// and compute the canonical side tables (see [`FunctionSelection`]).
    fn build_function_selection(
        &self,
        context: &Context,
        op: &OperationRef,
        facts: &DominatingEdgeFacts,
    ) -> FunctionSelection {
        // Function-wide value/op layout: with a single `value_to_def` a cross-block
        // operand expands to its defining expression naturally (no remat special
        // case), and a block argument / entry input stays an `Input` leaf.
        let mut value_to_def = HashMap::new();
        let mut op_block = HashMap::new();
        let mut op_position = HashMap::new();
        let mut block_ids = Vec::new();
        for region_id in &op.op().regions {
            let region = context.get_region(*region_id);
            for block in region.iter(context.clone()) {
                block_ids.push(block.id());
                for (position, op_id) in block.op_ids().into_iter().enumerate() {
                    op_block.insert(op_id, block.id());
                    op_position.insert(op_id, position);
                    for result in &context.get_op(op_id).results {
                        value_to_def.insert(*result, op_id);
                    }
                }
            }
        }

        // Lower every block's ops through one builder so its `value_to_class`
        // memoization unifies classes across blocks (cross-block CSE). Class ids
        // are resolved through `find` afterwards because saturation may merge them.
        let mut egraph = SemEGraph::new();
        let mut roots_by_op: HashMap<OpId, Id> = HashMap::new();
        let mut guards: HashMap<BlockId, Vec<BlockGuard>> = HashMap::new();
        let mut jumps: HashMap<BlockId, Vec<BlockJump>> = HashMap::new();
        let mut prepared: HashMap<ValueId, ConditionExpr> = HashMap::new();
        let value_to_class = {
            let mut builder = SemDagBuilder::new(context, &value_to_def, &mut egraph);
            for &block_id in &block_ids {
                for op_id in context.get_block(block_id).op_ids() {
                    let op = context.get_op(op_id);
                    if let Some(root) = builder.build_for_op(&op) {
                        roots_by_op.insert(op_id, root);
                    }
                }
            }

            // With branch emitters installed, terminators select here too: a
            // guarded two-way terminator's condition is lowered so branch rules can
            // match it; a plain branch is recorded for the `uncond` emitter.
            if self.branch_emitters.is_some() {
                for &block_id in &block_ids {
                    for op_id in context.get_block(block_id).op_ids() {
                        let op = context.get_op(op_id);
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
                            guards.entry(block_id).or_default().push(BlockGuard {
                                op: op_id,
                                condition: *a_cond,
                                class,
                                true_dest,
                                false_dest,
                                has_edge_args,
                            });
                        } else if let Some(branch) =
                            op.clone().as_interface::<dyn BranchTerminator>()
                        {
                            let successors = branch.successor_operands();
                            let [(dest, args)] = successors.as_slice() else {
                                continue;
                            };
                            jumps.entry(block_id).or_default().push(BlockJump {
                                op: op_id,
                                dest: *dest,
                                args: args.clone(),
                            });
                        }
                    }
                }
            }

            // Prepare each dominating-edge condition against the base graph, so its
            // scope can assert it while the graph is otherwise assumption-free.
            for &block_id in &block_ids {
                for fact in facts.facts(block_id) {
                    prepared
                        .entry(fact.condition)
                        .or_insert_with(|| ConditionExpr {
                            condition: builder.build_from_value(fact.condition),
                            compare: builder.build_defining_compare(fact.condition),
                        });
                }
            }

            builder.value_to_class
        };

        rewrites::saturate(context, &mut egraph, &self.rewrites, Default::default());

        // Canonicalize the side tables through `find`: saturation may merge classes,
        // so every id recorded against the pre-saturation graph is re-resolved here.
        let mut ops_by_root: HashMap<Id, Vec<OpId>> = HashMap::new();
        let mut op_root: HashMap<OpId, Id> = HashMap::new();
        for (&op, &root) in &roots_by_op {
            let class = egraph.find(root);
            ops_by_root.entry(class).or_default().push(op);
            op_root.insert(op, class);
        }

        // Every value a class computes: the input leaves it interned plus every op
        // result rooting it (a result never used as an operand is absent from
        // `value_to_class`). Sorted and deduped for a deterministic binding order.
        let mut class_values: HashMap<Id, Vec<ValueId>> = HashMap::new();
        for (&value, &class) in &value_to_class {
            class_values
                .entry(egraph.find(class))
                .or_default()
                .push(value);
        }
        for (&op, &root) in &roots_by_op {
            let class = egraph.find(root);
            for result in &context.get_op(op).results {
                class_values.entry(class).or_default().push(*result);
            }
        }
        for values in class_values.values_mut() {
            values.sort_by_key(|v| v.number());
            values.dedup();
        }

        let mut value_block: HashMap<ValueId, Option<BlockId>> = HashMap::new();
        for values in class_values.values() {
            for &value in values {
                value_block
                    .entry(value)
                    .or_insert_with(|| value_to_def.get(&value).map(|op| op_block[op]));
            }
        }

        // A value with an original use outside its defining block is guaranteed
        // materialized in a register, so a dominated block may bind it.
        let mut externally_bound = HashSet::new();
        for (&value, &def_op) in &value_to_def {
            let def_block = op_block[&def_op];
            if context
                .get_value(value)
                .uses()
                .iter()
                .any(|u| op_block.get(&u.op()).copied() != Some(def_block))
            {
                externally_bound.insert(value);
            }
        }

        // A value used as an operand by more than one consumer must stay a register.
        let mut operand_uses: HashMap<ValueId, usize> = HashMap::new();
        for &block_id in &block_ids {
            for op_id in context.get_block(block_id).op_ids() {
                for operand in &context.get_op(op_id).operands {
                    *operand_uses.entry(*operand).or_insert(0) += 1;
                }
            }
        }
        let mut shared_classes = HashSet::new();
        for (&op, &root) in &roots_by_op {
            if context
                .get_op(op)
                .results
                .iter()
                .any(|r| operand_uses.get(r).copied().unwrap_or(0) > 1)
            {
                shared_classes.insert(egraph.find(root));
            }
        }

        // A value used by an op no match can reach (it lowered to no e-graph root)
        // or by an op in a different block can never be recomputed inside a fused
        // instruction, so its class must keep a materializing alternative. A use by
        // a guarded terminator is exempt (branch selection recomputes the condition
        // inside the branch, or re-adds the materialization requirement itself).
        let guard_ops: HashSet<OpId> = guards.values().flatten().map(|g| g.op).collect();
        let mut must_materialize = HashSet::new();
        for (&op, &root) in &roots_by_op {
            let escapes = context.get_op(op).results.iter().any(|result| {
                // (a) a use in another block — captured exactly by `externally_bound`.
                externally_bound.contains(result)
                    // (b) a same-block use no match reaches and that is not a guard.
                    || context.get_value(*result).uses().iter().any(|u| {
                        !roots_by_op.contains_key(&u.op()) && !guard_ops.contains(&u.op())
                    })
            });
            if escapes {
                must_materialize.insert(egraph.find(root));
            }
        }

        for guard in guards.values_mut().flatten() {
            guard.class = egraph.find(guard.class);
        }

        FunctionSelection {
            egraph,
            ops_by_root,
            op_root,
            class_values,
            op_position,
            value_to_def,
            value_block,
            externally_bound,
            shared_classes,
            must_materialize,
            guards,
            jumps,
            prepared,
        }
    }

    /// Solve one block against the shared graph inside its assumption scope: assert
    /// every dominating-edge fact (generalizing the former single-fact path to a
    /// vector), scoped-saturate, solve, and pop.
    fn solve_block(
        &self,
        context: &Context,
        block: &Block,
        fs: &mut FunctionSelection,
        dom: &DominatorTree,
        facts: &[EdgeFact],
        base_matches: &[Vec<EMatch<u32>>],
    ) -> Result<BlockPlan, String> {
        let scoped = !facts.is_empty();
        if scoped {
            let egraph = &mut fs.egraph;
            let prepared = &fs.prepared;
            egraph.push_context();
            for fact in facts {
                if let Some(expr) = prepared.get(&fact.condition) {
                    assert_fact(context, egraph, expr, fact.holds);
                }
            }
            egraph.rebuild();
            rewrites::saturate(context, egraph, &self.rewrites, Default::default());
        }
        // Under a scope the graph differs from the base, so re-search; a fact-free
        // block reuses the cached base matches.
        let cached = (!scoped).then_some(base_matches);
        let plan = self.solve_block_inner(context, block, fs, dom, cached);
        if scoped {
            fs.egraph.pop_context();
        }
        plan
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

        let plan = match self.plans.get(&block.id()) {
            Some(Ok(plan)) => plan.clone(),
            Some(Err(message)) => return Err(PassError::InvalidRuleSet(message.clone())),
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
                        let branch_ops: Vec<Box<dyn Operation>> = match branch {
                            GuardBranch::Fused { rule_index, m } => {
                                let request = EmitRequest {
                                    op: None,
                                    results: &[],
                                    result_ty: None,
                                };
                                let rule = &self.rules[*rule_index];
                                // A flag-mediated branch rule emits its
                                // flag-setting definer right before the branch
                                // instruction that reads the flags.
                                let mut ops = Vec::new();
                                if let Some(prelude) = rule.prelude_emit {
                                    ops.push(prelude(context, &request, m)?);
                                }
                                ops.push((rule.emit_fn)(context, &request, m)?);
                                ops
                            }
                            GuardBranch::Nonzero { condition, dest } => {
                                (emitters.cond_nonzero)(context, *condition, *dest)
                            }
                        };
                        for branch_op in &branch_ops {
                            rewriter.insert_op_before(&op_ref, branch_op.as_ref())?;
                        }
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

    /// Solve `block` against the (already scoped) shared graph, restricting
    /// matching and the cover to what `block` computes.
    fn solve_block_inner(
        &self,
        context: &Context,
        block: &Block,
        fs: &FunctionSelection,
        dom: &DominatorTree,
        base_matches: Option<&[Vec<EMatch<u32>>]>,
    ) -> Result<BlockPlan, String> {
        let block_id = block.id();
        let op_ids = block.op_ids();
        let mut op_refs = HashMap::new();
        for (position, op_id) in op_ids.iter().copied().enumerate() {
            let op = context.get_op(op_id);
            op_refs.insert(
                op_id,
                OperationRef::new(op, Some(context.get_block(block_id)), Some(position)),
            );
        }

        // The earliest op of B rooting each class (for costing / the Emit anchor);
        // its keys are B's op-root classes. Block order visits earliest first, so
        // the first insertion per class already wins.
        let mut block_op_by_root: HashMap<Id, OpId> = HashMap::new();
        for &op_id in &op_ids {
            let Some(&root) = fs.op_root.get(&op_id) else {
                continue;
            };
            block_op_by_root
                .entry(fs.egraph.find(root))
                .or_insert(op_id);
        }
        let block_roots: HashSet<Id> = block_op_by_root.keys().copied().collect();

        let guards = fs.guards.get(&block_id).map(Vec::as_slice).unwrap_or(&[]);
        let jumps = fs.jumps.get(&block_id).map(Vec::as_slice).unwrap_or(&[]);
        let guard_classes: HashSet<Id> = guards.iter().map(|g| fs.egraph.find(g.class)).collect();

        let matches = self.collect_block_matches(
            context,
            fs,
            dom,
            block_id,
            &op_refs,
            &block_op_by_root,
            &guard_classes,
            base_matches,
        );

        // Search the branch rules once for the whole block, indexed by condition
        // class, so each guard just looks up its hits.
        let guard_branch_hits = self.guard_branch_hits(context, fs, guards);

        // Resolve each guarded terminator: fuse its condition into a branch-rule
        // instruction when one matches, else fall back to the target's
        // branch-if-nonzero (which needs the condition materialized). Fused
        // branches read their operands as registers, so those classes join the
        // block's materialization overlay; a condition consumed only by its fused
        // branch may instead go Dead (its defining op is erased).
        let mut mm_overlay: HashSet<Id> = HashSet::new();
        let mut fused_conditions = HashSet::new();
        let mut terminators = Vec::new();
        for guard in guards {
            if guard.has_edge_args {
                return Err(
                    "block arguments on conditional branch edges are not supported by codegen yet"
                        .to_string(),
                );
            }
            let class = fs.egraph.find(guard.class);
            // A condition proven constant (a dominating-edge assumption) folds
            // the guard to an unconditional branch to the known successor.
            if let Some(known) = class_int_binding(&fs.egraph, class) {
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
            let candidates = guard_branch_hits
                .get(&class)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            match self.best_guard_branch(context, fs, dom, block_id, guard, candidates) {
                Some((rule_index, m, boundary_classes)) => {
                    for boundary in boundary_classes {
                        mm_overlay.insert(fs.egraph.find(boundary));
                    }
                    fused_conditions.insert(class);
                    terminators.push(TerminatorPlan::Guard {
                        op: guard.op,
                        branch: GuardBranch::Fused { rule_index, m },
                        false_dest: guard.false_dest,
                    });
                }
                None => {
                    mm_overlay.insert(class);
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
        for jump in jumps {
            terminators.push(TerminatorPlan::Jump {
                op: jump.op,
                dest: jump.dest,
                args: jump.args.clone(),
            });
        }

        let dead_allowed: HashSet<Id> = fused_conditions
            .iter()
            .copied()
            .filter(|class| !fs.requires_materialization(*class, &mm_overlay))
            .collect();

        if let Some(message) = completeness_error(&fs.egraph, &block_roots, &matches, &dead_allowed)
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
            for &op_id in &op_ids {
                let Some(class) = fs.op_root.get(&op_id).map(|c| fs.egraph.find(*c)) else {
                    continue;
                };
                if !fs.requires_materialization(class, &mm_overlay)
                    && class_int_binding(&fs.egraph, class).is_some()
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

        // Restrict the cover to the closure of B's op-root and guard-condition
        // classes under the surviving matches' bindings (so rewrite-introduced
        // intermediates reached from B are covered, but nothing from other blocks).
        let covered = closure_classes(&fs.egraph, &block_roots, &guard_classes, &matches);

        let Some(cover) = build_eclass_cover(
            &fs.egraph,
            &block_roots,
            &covered,
            |class| fs.requires_materialization(class, &mm_overlay),
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
            fs,
            dom,
            block: block_id,
            matches: &matches,
            root_match: &root_match,
            context,
            introduced_dest: HashMap::new(),
            introduced: Vec::new(),
        };

        let mut op_decisions = HashMap::new();
        for &op_id in &op_ids {
            let Some(class) = fs.op_root.get(&op_id).map(|c| fs.egraph.find(*c)) else {
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
            } else if !fs.requires_materialization(class, &mm_overlay)
                && class_int_binding(&fs.egraph, class).is_some()
            {
                // The class is proven constant under the block's assumption:
                // consumers fold the immediate (or read the merged input value's
                // register), so the defining op is erased.
                op_decisions.insert(op_id, BlockDecision::Consume);
            }
        }

        Ok(BlockPlan {
            op_decisions,
            introduced: emit.introduced,
            terminators,
        })
    }

    /// The best conditional-branch rule match rooted at a guard's condition
    /// class: the rule, the operand bindings (with the taken target bound as a
    /// block), and the boundary classes the branch reads as registers. `None`
    /// when no branch rule matches or an operand is unresolvable at `block`.
    /// Every conditional-branch rule match over the block's (scoped) graph,
    /// indexed by condition class, so each guard resolves against its own hits
    /// without re-searching per guard. Empty when the block has no guards.
    fn guard_branch_hits(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        guards: &[BlockGuard],
    ) -> HashMap<Id, Vec<(usize, EMatch<u32>)>> {
        let mut hits: HashMap<Id, Vec<(usize, EMatch<u32>)>> = HashMap::new();
        if guards.is_empty() {
            return hits;
        }
        for (pattern_index, compiled) in self.compiled_patterns.iter().enumerate() {
            if !matches!(
                self.rules[compiled.rule_index].kind,
                RuleKind::CondBranch { .. }
            ) {
                continue;
            }
            for m in compiled.search(&fs.egraph, context) {
                hits.entry(fs.egraph.find(m.root))
                    .or_default()
                    .push((pattern_index, m));
            }
        }
        hits
    }

    /// The best conditional-branch rule among a guard's condition-class hits: the
    /// rule, the operand bindings (taken target bound as a block), and the
    /// boundary classes the branch reads as registers. `None` when none matches or
    /// an operand is unresolvable at `block`.
    fn best_guard_branch(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        dom: &DominatorTree,
        block: BlockId,
        guard: &BlockGuard,
        candidates: &[(usize, EMatch<u32>)],
    ) -> Option<(usize, RuleMatch, Vec<Id>)> {
        let mut best: Option<(u64, usize, usize, RuleMatch, Vec<Id>)> = None;
        for (pattern_index, m) in candidates {
            let compiled = &self.compiled_patterns[*pattern_index];
            let RuleKind::CondBranch { target_symbol } = self.rules[compiled.rule_index].kind
            else {
                continue;
            };

            let mut captures = CaptureBindings::new();
            for (var, class) in m.subst.entries() {
                let Var::Symbol(symbol) = var else { continue };
                captures.bind(*symbol, fs.egraph.find(class));
            }

            // Every operand must resolve at B. A class carrying an immediate folds
            // it into the encoding (and still records its register form so a
            // register-reading emitter finds it) without pinning materialization;
            // a class with only a register value binds under the dominance rule and
            // joins the materialization set. An unresolvable boundary disqualifies.
            let mut boundary_classes = Vec::new();
            let mut int_bindings = Vec::new();
            let mut value_bindings = Vec::new();
            let mut resolvable = true;
            for (symbol, class) in &captures.entries {
                let binding = fs.resolve_binding(dom, context, *class, block, guard.op);
                match binding.int {
                    Some(v) => {
                        int_bindings.push((*symbol, v));
                        if let Some(reg) = binding.value {
                            value_bindings.push((*symbol, reg));
                        }
                    }
                    None => match binding.value {
                        Some(reg) => {
                            value_bindings.push((*symbol, reg));
                            boundary_classes.push(*class);
                        }
                        None => {
                            resolvable = false;
                            break;
                        }
                    },
                }
            }
            if !resolvable {
                continue;
            }

            let cost = self.rules[compiled.rule_index].base_cost as u64;
            let specificity = compiled.specificity;
            let better = match &best {
                None => true,
                Some((best_cost, best_specificity, ..)) => {
                    cost < *best_cost || (cost == *best_cost && specificity > *best_specificity)
                }
            };
            if better {
                let rule_match = RuleMatch::new(int_bindings, value_bindings)
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
        best.map(|(_, _, rule_index, m, boundaries)| (rule_index, m, boundaries))
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_block_matches(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        dom: &DominatorTree,
        block: BlockId,
        op_refs: &HashMap<OpId, OperationRef>,
        block_op_by_root: &HashMap<Id, OpId>,
        guard_classes: &HashSet<Id>,
        base_matches: Option<&[Vec<EMatch<u32>>]>,
    ) -> Vec<PbqpIselMatch> {
        let mut matches = Vec::new();
        for (pattern_index, compiled) in self.compiled_patterns.iter().enumerate() {
            let rule = &self.rules[compiled.rule_index];
            // Branch rules select terminators, not values (see `best_guard_branch`).
            if rule.kind != RuleKind::Value {
                continue;
            }
            let pattern_root = compiled.pattern.root();

            // A fact-free block reuses the base search; a scoped one re-searches its
            // own graph. Both apply the function-wide legality, then the per-block
            // filter below narrows interior classes to B's own op-roots.
            let fresh: Vec<EMatch<u32>>;
            let raw: &[EMatch<u32>] = if let Some(cache) = base_matches {
                &cache[pattern_index]
            } else {
                fresh = compiled
                    .pattern
                    .search_with_legality(&fs.egraph, &|node, class| {
                        value_match_allowed(fs, context, compiled, pattern_root, node, class)
                    });
                &fresh
            };

            for m in raw {
                let root = fs.egraph.find(m.root);
                let block_op = block_op_by_root.get(&root).copied();
                let is_guard_class = guard_classes.contains(&root);
                // A match roots an instruction only if it produces a value B
                // computes: an op of B, a guard condition of B, or a
                // rewrite-introduced intermediate (a computed class with no op).
                let is_computed = fs
                    .egraph
                    .nodes(root)
                    .iter()
                    .any(|n| !n.children().is_empty());
                let introduced = is_computed && !fs.is_op_root(root);
                if block_op.is_none() && !is_guard_class && !introduced {
                    continue;
                }

                // Narrow the function-wide legality to B: a non-pure interior class
                // is legal only when its backing op is in B and it is not shared
                // (boundary constraints were already enforced during the search).
                let interior_ok = (0..compiled.pattern.len()).all(|index| {
                    let node = Id::from_raw(index as u32);
                    if node == pattern_root || compiled.node_meta[node.index()].duplicable {
                        return true;
                    }
                    let class = fs.egraph.find(m.binding(node));
                    node::class_is_pure(&fs.egraph, class)
                        || (block_op_by_root.contains_key(&class) && !fs.is_shared(class))
                });
                if !interior_ok {
                    continue;
                }

                let mut captures = CaptureBindings::new();
                for (var, class) in m.subst.entries() {
                    let Var::Symbol(symbol) = var else { continue };
                    captures.bind(*symbol, fs.egraph.find(class));
                }

                // Discard the match if a boundary operand cannot be resolved to an
                // operand at B (a cross-block register read of a non-escaping
                // value). Only when there is a concrete consumer op in B.
                if let Some(consumer) = block_op
                    && !captures.entries.iter().all(|(_, class)| {
                        fs.boundary_resolvable(dom, context, *class, block, consumer)
                    })
                {
                    continue;
                }

                let pattern_nodes = (0..compiled.pattern.len())
                    .map(|index| Id::from_raw(index as u32))
                    .map(|pattern_node| {
                        let meta = &compiled.node_meta[pattern_node.index()];
                        PatternNodeBinding {
                            pattern_node,
                            class: fs.egraph.find(m.binding(pattern_node)),
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

                // Cost is op-relative when there is a backing op in B; a
                // rewrite-introduced root has no op, so it takes the rule's
                // target-independent base cost.
                let rule_match = bindings
                    .captures
                    .to_rule_match(&fs.egraph, &fs.class_values);
                let cost = if let Some(op_ref) = block_op.and_then(|id| op_refs.get(&id)) {
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

/// Whether `class` may bind under `pattern_node` in a value match, before the
/// per-block narrowing: boundary constraints (register / immediate / width), and
/// interior nodes restricted to pure or function-wide op-root, non-shared classes
/// (a memory effect recomputed inside a fused instruction must have its backing
/// op reachable). The root and duplicable nodes are always allowed.
fn value_match_allowed(
    fs: &FunctionSelection,
    context: &Context,
    compiled: &CompiledIselPattern,
    pattern_root: Id,
    pattern_node: Id,
    class: Id,
) -> bool {
    if !compiled.boundary_ok(&fs.egraph, context, pattern_node, class) {
        return false;
    }
    if pattern_node == pattern_root || compiled.node_meta[pattern_node.index()].duplicable {
        return true;
    }
    let class = fs.egraph.find(class);
    node::class_is_pure(&fs.egraph, class) || (fs.is_op_root(class) && !fs.is_shared(class))
}

/// Assert one dominating-edge fact in the current scope: the condition (and its
/// defining comparison, when there is one) equals its known truth value, the
/// complement comparison equals the opposite, and an `eq`/`ne` guard makes its
/// operands congruent.
fn assert_fact(context: &Context, egraph: &mut SemEGraph, expr: &ConditionExpr, holds: bool) {
    let truth = |egraph: &mut SemEGraph, holds: bool| {
        egraph.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(1, holds as u64))),
            None,
        ))
    };
    let known = truth(egraph, holds);
    egraph.union(expr.condition, known);
    if let Some((compare, kind, lhs, rhs)) = expr.compare {
        egraph.union(compare, known);
        if let Some(complement) = node::complement_comparison(kind) {
            let mut node = template_node(
                complement,
                None,
                Some(tir::builtin::IntegerType::new(context, 1)),
            );
            node.children = vec![lhs, rhs];
            let complement_class = egraph.add(node);
            let opposite = truth(egraph, !holds);
            egraph.union(complement_class, opposite);
        }
        if (kind == SymKind::Eq && holds) || (kind == SymKind::Ne && !holds) {
            egraph.union(lhs, rhs);
        }
    }
}

/// The closure of B's op-root and guard-condition classes under the bindings of
/// matches rooted in that set — the classes the PBQP cover ranges over.
fn closure_classes(
    egraph: &SemEGraph,
    block_roots: &HashSet<Id>,
    guard_classes: &HashSet<Id>,
    matches: &[PbqpIselMatch],
) -> Vec<Id> {
    let mut by_root: HashMap<Id, Vec<usize>> = HashMap::new();
    for (i, m) in matches.iter().enumerate() {
        by_root.entry(egraph.find(m.root)).or_default().push(i);
    }

    let mut covered: HashSet<Id> = block_roots.iter().copied().collect();
    covered.extend(guard_classes.iter().copied());
    let mut work: Vec<Id> = covered.iter().copied().collect();
    while let Some(class) = work.pop() {
        let Some(indices) = by_root.get(&class) else {
            continue;
        };
        for &i in indices {
            for binding in &matches[i].bindings.pattern_nodes {
                let bound = egraph.find(binding.class);
                if covered.insert(bound) {
                    work.push(bound);
                }
            }
        }
    }

    let mut classes: Vec<Id> = covered.into_iter().collect();
    classes.sort();
    classes
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
        analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        // The function op is visited before any of its blocks' ops: build the
        // shared graph and solve every block up front — a dominating-edge fact
        // reads the guard condition's *defining op*, which a dominator's commit
        // would otherwise have replaced by the time the dominated block solves.
        if !op.op().regions.is_empty() {
            self.solve_function(context, op, analyses);
        }

        for lowering in &self.op_lowerings {
            if lowering(context, op, rewriter)? {
                return Ok(PreservedAnalyses::none());
            }
        }

        // Result-less ops still participate: a store must trigger its block's
        // selection even when no value-producing op precedes it.
        let Some(block) = op.block() else {
            return Ok(PreservedAnalyses::all());
        };

        self.commit_block_solution(context, block, rewriter)?;
        Ok(PreservedAnalyses::none())
    }
}
