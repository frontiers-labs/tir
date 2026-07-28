//! Instruction selection over semantic e-graphs.
//!
//! The whole function's operations are lowered into one shared e-graph of
//! semantic expressions ([`builder`]), saturated with proved algebraic rewrites
//! ([`rewrites`]), and then covered *per block* — each inside its own
//! dominating-edge assumption scope — by the target's instruction patterns
//! ([`pattern`]), e-matched by the shared [`tir_symbolic::egraph`] engine, via a
//! PBQP instance over e-classes ([`cover`]). The solved cover becomes an emission
//! plan ([`emit`]) the pass commits through the rewriter.

mod axioms;
mod builder;
mod cover;
mod emit;
mod node;
mod pattern;
mod rewrites;
#[cfg(test)]
mod tests;
mod theory;

use std::collections::{HashMap, HashSet};

use tir::{
    AnalysisManager, Block, BlockId, BranchGuard, BranchTerminator, Context, OpId, Operation,
    OperationRef, Pass, PassError, PassTarget, PreservedAnalyses, Rewriter, Terminator, TypeId,
    ValueId,
    analysis::{DominatingEdgeFacts, DominatorTree, EdgeFact, GSA, GateNode},
    graph::{Dag, MutDag, NodeId, OperandConstraint},
    sem::{
        EquivalenceOracle, SemGraph, SmtOracle, SymKind, SymPayload, canonicalize_for_selection,
        definedness_condition, infer_widths,
    },
};
use tir_adt::APInt;
use tir_symbolic::egraph::{ENode, Id, PatternNode, Var};

pub use node::{SemEGraph, SemNode, SemPayload};
pub use rewrites::{IselRewrite, SaturationLimits};
pub use tir_symbolic::egraph::EMatch;

use builder::SemDagBuilder;
use cover::{
    BoundaryDemand, CaptureBindings, FullMatchBindings, PatternNodeBinding, PbqpIselAlternative,
    PbqpIselMatch, build_eclass_cover, completeness_error, prune_dominated_matches,
};
use emit::{BlockPlan, GuardBranch, ScheduledEmit, TerminatorPlan, resolve_match, schedule_tiles};
use node::{
    class_int_binding, class_width, is_comparison, is_low_extract_view, kind_is_pure,
    low_extract_source, template_node,
};
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

    pub(crate) fn rebind_block(&mut self, symbol: u32, block: BlockId) {
        for (sym, dest) in &mut self.block_bindings {
            if *sym == symbol {
                *dest = block;
            }
        }
    }

    fn remap_values(&mut self, remaps: &HashMap<ValueId, ValueId>) {
        for (_, value) in &mut self.value_bindings {
            if let Some(replacement) = remaps.get(value) {
                *value = *replacement;
            }
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
            .map(|(_, v)| {
                // Boolean values occupy registers and immediates as 0/1; reading
                // signed i1 through APInt would turn true into -1.
                if v.width() == 1 {
                    v.to_u64() as i64
                } else if v.is_signed() {
                    v.to_i64()
                } else {
                    v.to_u64() as i64
                }
            })
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

/// The semantic value representations a physical register class can store.
/// This is deliberately separate from the value's type: overlapping banks may
/// accept both integer and floating-point interpretations of the same bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterCapability {
    width: u32,
    integer: bool,
    float: bool,
}

/// A register operand's storage capability and whether its instruction reads
/// the value's full architectural width rather than only its defined low bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterRequirement {
    capability: RegisterCapability,
    whole: bool,
}

impl RegisterRequirement {
    pub fn low_bits(capability: RegisterCapability) -> Self {
        Self {
            capability,
            whole: false,
        }
    }

    pub fn whole(capability: RegisterCapability) -> Self {
        Self {
            capability,
            whole: true,
        }
    }

    pub fn accepts(&self, ty: &tir::sem::SemType) -> bool {
        use tir::sem::{SemType, Width};
        if !self.capability.accepts(ty) {
            return false;
        }
        !self.whole
            || !matches!(
                ty,
                SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width))
                    if *width != self.capability.width
            )
    }

    fn accepts_low_view_source(&self, ty: &tir::sem::SemType) -> bool {
        use tir::sem::{SemType, Width};
        matches!(
            ty,
            SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width))
                if self.capability.integer && *width >= self.capability.width
        )
    }
}

impl RegisterCapability {
    pub fn integer(width: u32) -> Self {
        Self {
            width,
            integer: true,
            float: false,
        }
    }

    pub fn float(width: u32) -> Self {
        Self {
            width,
            integer: false,
            float: true,
        }
    }

    pub fn any(width: u32) -> Self {
        Self {
            width,
            integer: true,
            float: true,
        }
    }

    pub fn accepts(&self, ty: &tir::sem::SemType) -> bool {
        use tir::sem::{SemType, Width};
        match ty {
            SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width)) => {
                self.integer && *width <= self.width
            }
            SemType::Float(format) => {
                let (Width::Const(exponent), Width::Const(mantissa)) =
                    (&format.exponent, &format.mantissa)
                else {
                    return self.float;
                };
                self.float && 1 + exponent + mantissa == self.width
            }
            SemType::Var(_) => true,
            SemType::Iterator(_) | SemType::Pair(_, _) | SemType::State | SemType::Unit => false,
            SemType::Bits(_) | SemType::RawBits(_) => self.integer,
        }
    }
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
    /// Per-register-operand storage and bit-demand requirements.
    pub operand_registers: Vec<(u32, RegisterRequirement)>,
    /// Storage capability of the register receiving this rule's result.
    pub result_register: Option<RegisterRequirement>,
    /// Width of a floating-point value this target instruction can materialize
    /// from its integer bit pattern.
    pub float_constant_width: Option<u32>,
    /// Per-operand-symbol immediate encoding range. A constant outside the field's
    /// representable range must not bind (its encoding would truncate). Symbols
    /// absent here accept any constant.
    pub operand_imm_ranges: Vec<(u32, ImmRange)>,
    /// The destination's full guarded semantics as an `If` tree, when the behavior
    /// assigns the result under a guard (e.g. riscv `div`'s divide-by-zero case).
    /// [`Rule::pattern`] is the guard-relaxed pure op that actually selects; this
    /// companion lets pass construction *prove* the relaxation sound (the pure op
    /// equals the guarded behavior wherever the IR op is defined). `None` for plain
    /// unguarded or sequential multi-assignment behaviors.
    pub guarded_semantics: Option<SemGraph>,
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
            operand_registers: Vec::new(),
            result_register: None,
            float_constant_width: None,
            operand_imm_ranges: Vec::new(),
            guarded_semantics: None,
            emit_fn,
        }
    }

    /// Constrain operand symbols to register or immediate operands, so e.g. an
    /// immediate-shift pattern only matches a constant shift amount.
    pub fn with_operand_constraints(mut self, constraints: Vec<(u32, OperandConstraint)>) -> Self {
        self.operand_constraints = constraints;
        self
    }

    /// Describe which semantic values each physical register operand can store
    /// and whether the instruction consumes all of their architectural bits.
    pub fn with_operand_registers(mut self, registers: Vec<(u32, RegisterRequirement)>) -> Self {
        self.operand_registers = registers;
        self
    }

    pub fn with_result_register(mut self, register: RegisterRequirement) -> Self {
        self.result_register = Some(register);
        self
    }

    pub fn with_optional_result_register(mut self, register: Option<RegisterRequirement>) -> Self {
        self.result_register = register;
        self
    }

    /// Record that this target instruction bridges integer bits into a scalar
    /// floating-point register, so floating constants can be rooted by selection.
    pub fn with_float_constant_materializer(mut self, width: u32) -> Self {
        self.float_constant_width = Some(width);
        self
    }

    /// Restrict immediate operand symbols to constants their encoding field can
    /// represent (see [`Rule::operand_imm_ranges`]).
    pub fn with_operand_imm_ranges(mut self, ranges: Vec<(u32, ImmRange)>) -> Self {
        self.operand_imm_ranges = ranges;
        self
    }

    /// Mark this rule as a conditional branch (see [`RuleKind::CondBranch`]).
    pub fn with_kind(mut self, kind: RuleKind) -> Self {
        self.kind = kind;
        self
    }

    /// Attach the destination's full guarded semantics (see
    /// [`Rule::guarded_semantics`]), so pass construction proves that relaxing to
    /// [`Rule::pattern`] is sound.
    pub fn with_guarded_semantics(mut self, guarded: SemGraph) -> Self {
        self.guarded_semantics = Some(guarded);
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
    /// E-classes used as an operand by more than one consumer (function-wide). A
    /// memory effect in such a class cannot be internalized into a match.
    shared_classes: HashSet<Id>,
    /// Classes selected at their defining block because a surviving reader needs
    /// their register value.
    demand: HashSet<(Id, BlockId)>,
    /// Control-flow edges retained when gate values are reified.
    control: HashMap<BlockId, Vec<ControlReification>>,
    /// Canonical classes seeded from reducible γ/μ gates. An algebraic `If`
    /// introduced by an axiom is not control flow and must not gain reification.
    reifiable_gates: HashSet<Id>,
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

    fn is_reifiable_gate(&self, class: Id) -> bool {
        self.base_members(class)
            .any(|member| self.reifiable_gates.contains(&member))
    }

    fn demanded_at(&self, class: Id, block: BlockId, overlay: &HashSet<Id>) -> bool {
        overlay.contains(&class) || self.placed_at(class, block)
    }

    fn placed_at(&self, class: Id, block: BlockId) -> bool {
        self.base_members(class)
            .any(|member| self.demand.contains(&(member, block)))
    }

    /// Whether any base member of `class` computes an IR value (a candidate for a
    /// register binding). A class with none is pure / rewrite-introduced.
    fn has_values(&self, class: Id) -> bool {
        self.base_members(class)
            .any(|m| self.class_values.contains_key(&m))
    }

    /// Whether any IR value carried by a base member of `class` satisfies `pred`.
    fn any_class_value(&self, class: Id, pred: impl Fn(&ValueId) -> bool) -> bool {
        self.base_members(class).any(|m| {
            self.class_values
                .get(&m)
                .is_some_and(|values| values.iter().any(&pred))
        })
    }

    fn available_at(
        &self,
        context: &Context,
        dom: &DominatorTree,
        class: Id,
        block: BlockId,
    ) -> bool {
        self.any_class_value(class, |value| {
            let Some(&def) = self.value_to_def.get(value) else {
                return true;
            };
            let op = context.get_op(def);
            let Some(def_block) = context.parent_block(def) else {
                return true;
            };
            if !self.op_root.contains_key(&def) {
                if op.is::<crate::builtin::ConstantOp>() || op.is::<crate::builtin::ConstantFOp>() {
                    return false;
                }
                return def_block == block || dom.dominates(def_block, block);
            }
            def_block != block
                && dom.dominates(def_block, block)
                && self.placed_at(class, def_block)
        })
    }

    /// Resolve `class` to operands for consumer op `consumer` in `block`: the
    /// proven constant (folds to an immediate) and/or a register value legal under
    /// the dominance rule. The single resolver behind boundary filtering, guard
    /// selection, and emission, so collect-time acceptance implies emit-time
    /// success. A valueless class yields neither — resolvable only as an
    /// introduced dest the caller expects the cover to materialize.
    ///
    /// `bind_pending_tiles` lets a caller that is about to demand the class in
    /// this block (guard selection: fused-branch boundary operands join the
    /// overlay) bind a same-block op-rooted value whose tile will define it.
    /// Every other caller passes `false`: an op-rooted def with no tile is
    /// erased by the extraction, so only surviving values may bind.
    fn resolve_binding(
        &self,
        dom: &DominatorTree,
        context: &Context,
        class: Id,
        block: BlockId,
        consumer: OpId,
        bind_pending_tiles: bool,
    ) -> Binding {
        Binding {
            int: class_int_binding(&self.egraph, class),
            value: self.register_value(dom, context, class, block, consumer, bind_pending_tiles),
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
        bind_pending_tiles: bool,
    ) -> Option<ValueId> {
        // A low-bit truncation re-views its operand's register: bind the operand
        // (chasing a chain of truncations), never the erased truncation itself.
        let mut class = self.egraph.find(class);
        while let Some(source) = low_extract_source(&self.egraph, class) {
            class = source;
        }
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
                        if !bind_pending_tiles && self.op_root.contains_key(&def) {
                            continue;
                        }
                        (0, self.op_position[&def], v.number())
                    }
                    Some(def_block) => {
                        if !dom.dominates(def_block, block) {
                            continue;
                        }
                        // A def the extraction places dominates by construction;
                        // an op selection never touches (an alloca, a call)
                        // survives with its original value.
                        let def = self.value_to_def[&v];
                        let survives = !self.op_root.contains_key(&def)
                            && !context.get_op(def).is::<crate::builtin::ConstantOp>()
                            && !context.get_op(def).is::<crate::builtin::ConstantFOp>();
                        if !survives && !self.placed_at(class, def_block) {
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

    /// The class whose tile defines the register a low-extract view re-reads:
    /// `class` itself unless it is a chain of low-bit truncations.
    fn chase_low_extract(&self, class: Id) -> Id {
        let mut class = self.egraph.find(class);
        while let Some(source) = low_extract_source(&self.egraph, class) {
            class = source;
        }
        class
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
    /// Values forwarded to each successor's block arguments.
    true_args: Vec<ValueId>,
    false_args: Vec<ValueId>,
}

/// An unconditional branch terminator and its forwarded block arguments.
struct BlockJump {
    op: OpId,
    dest: BlockId,
    args: Vec<ValueId>,
}

/// Existing CFG edges that implement an unselected γ/θ gate. Both guarded and
/// unconditional edges use the same forwarded-value representation.
enum ControlReification {
    Guarded(BlockGuard),
    Unconditional(BlockJump),
}

impl ControlReification {
    fn guard(&self) -> Option<&BlockGuard> {
        match self {
            ControlReification::Guarded(guard) => Some(guard),
            ControlReification::Unconditional(_) => None,
        }
    }
}

pub type OpLowering = fn(&Context, &OperationRef, &mut Rewriter) -> Result<bool, PassError>;

pub struct InstructionSelectPass {
    rules: Vec<Rule>,
    compiled_patterns: Vec<CompiledIselPattern>,
    /// Immediate ranges of every formal constant materializer
    /// (see [`pattern::constant_materializer_ranges`]). Empty means bare
    /// constants stay with the target's pre-RA materialization hook.
    constant_materializer_ranges: Vec<ImmRange>,
    /// Floating-point widths target instructions can materialize from integer bits.
    float_constant_materializer_widths: HashSet<u32>,
    /// Address width inferred from the target's natural integer load rules.
    pointer_width: Option<u32>,
    /// Semantic invariants the program e-graph is saturated with before covering.
    rewrites: Vec<IselRewrite>,
    /// Instructions that define a register implicitly; selection introduces one
    /// ahead of any op whose `implicit_uses` name a matching register.
    /// Target hooks for terminator lowering; branch selection is off without them
    /// (terminators are then left to the target's op lowerings).
    branch_emitters: Option<BranchEmitters>,
    cost_model: Box<dyn IselCostModel>,
    op_lowerings: Vec<OpLowering>,
    call_lowering: Option<crate::backend::call_lowering::CallLowering>,
    /// The solved emission plan of every block (or the error explaining why it
    /// cannot be selected), populated up front when the pass visits each function.
    plans: HashMap<BlockId, Result<BlockPlan, String>>,
    emitted_blocks: HashSet<BlockId>,
    emitted_values: HashMap<ValueId, ValueId>,
    /// Function roots already solved, so a re-visit does not rebuild the graph.
    solved: HashSet<OpId>,
}

/// Prove, for every rule carrying [`Rule::guarded_semantics`], that relaxing the
/// guarded behavior to its pure [`Rule::pattern`] is sound: the pure op equals the
/// guarded behavior wherever the IR op is defined. An unprovable rule is reported
/// as [`PassError::InvalidRuleSet`] naming the rule and the failed obligation.
fn prove_guarded_relaxations(rules: &[Rule]) -> Result<(), PassError> {
    for rule in rules {
        let Some(guarded) = &rule.guarded_semantics else {
            continue;
        };
        prove_relaxation(rule, guarded).map_err(PassError::InvalidRuleSet)?;
    }
    Ok(())
}

fn relaxation_error(rule: &Rule, why: &str) -> String {
    format!("rule `{}`: {why}", rule.name)
}

/// Prove `D(pattern) => guarded_semantics == pattern` for one guarded rule.
fn prove_relaxation(rule: &Rule, guarded: &SemGraph) -> Result<(), String> {
    let if_root = guarded
        .root()
        .ok_or_else(|| relaxation_error(rule, "empty guarded semantics"))?;
    if *guarded.get_node(if_root) != SymKind::If {
        return Err(relaxation_error(
            rule,
            "guarded semantics must be rooted at an `if`",
        ));
    }
    let else_arm = guarded
        .children(if_root)
        .nth(2)
        .ok_or_else(|| relaxation_error(rule, "guarded `if` lacks an else arm"))?;

    // The else arm, canonicalized exactly as the selection pattern is, must *be*
    // the selection pattern: only then is the proved relaxation the one selection
    // performs. This is what rejects an else arm that computes a different op.
    let immediate_symbols: HashSet<u32> = rule
        .operand_constraints
        .iter()
        .filter(|(_, c)| matches!(c, OperandConstraint::Immediate))
        .map(|(symbol, _)| *symbol)
        .collect();
    let (canon_else, canon_root, _) =
        canonicalize_for_selection(guarded, else_arm, &immediate_symbols);
    let pattern_root = rule
        .pattern
        .root()
        .ok_or_else(|| relaxation_error(rule, "empty selection pattern"))?;
    if !subgraphs_equal(&canon_else, canon_root, &rule.pattern, pattern_root) {
        return Err(relaxation_error(
            rule,
            "guarded else arm does not match the selection pattern",
        ));
    }

    // Prove the relaxation at the target register width baked into the behavior
    // (the `if`'s arm width).
    let register_width = infer_widths(guarded, |_| None)[if_root.index()].unwrap_or(64);
    if !relaxation_holds(guarded, if_root, else_arm, register_width) {
        return Err(relaxation_error(
            rule,
            &format!(
                "guard relaxation `D(pattern) => guarded == pattern` is not valid \
                 at register width {register_width}"
            ),
        ));
    }
    Ok(())
}

/// Whether the subgraphs rooted at `r1`/`r2` are structurally identical (same
/// kinds, leaf payloads and children, in order). Generic over the graph backend so
/// the canonicalizer's [`GenericDag`](tir::graph::GenericDag) output compares
/// against the [`SemGraph`] selection pattern.
fn subgraphs_equal<L: PartialEq>(
    g1: &impl Dag<Node = SymKind, Leaf = L>,
    r1: NodeId,
    g2: &impl Dag<Node = SymKind, Leaf = L>,
    r2: NodeId,
) -> bool {
    if g1.get_node(r1) != g2.get_node(r2) || g1.get_leaf_data(r1) != g2.get_leaf_data(r2) {
        return false;
    }
    let c1: Vec<NodeId> = g1.children(r1).collect();
    let c2: Vec<NodeId> = g2.children(r2).collect();
    c1.len() == c2.len()
        && c1
            .iter()
            .zip(&c2)
            .all(|(&a, &b)| subgraphs_equal(g1, a, g2, b))
}

/// Copy the subgraph under `node` into `dst`, preserving DAG sharing through `memo`.
fn copy_subgraph(
    src: &SemGraph,
    node: NodeId,
    dst: &mut SemGraph,
    memo: &mut HashMap<usize, NodeId>,
) -> NodeId {
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<NodeId> = src
        .children(node)
        .map(|c| copy_subgraph(src, c, dst, memo))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        dst.set_leaf_data(copied, data.clone());
    }
    for child in children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// The conjunction of definedness conditions of every partial-kind node reachable
/// from `root`, appended to `g`; `None` when the subgraph holds no partial op.
fn definedness_of(g: &mut SemGraph, root: NodeId, widths: &[Option<u32>]) -> Option<NodeId> {
    let mut conditions = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !seen.insert(node.index()) {
            continue;
        }
        stack.extend(g.children(node).collect::<Vec<_>>());
        if let Some(cond) = definedness_condition(g, node, widths) {
            conditions.push(cond);
        }
    }
    conditions.into_iter().reduce(|a, b| {
        let and = g.add_node(SymKind::And);
        g.add_edge(and, a);
        g.add_edge(and, b);
        and
    })
}

/// Whether relaxing `guarded` to its (guard-dropped) else arm is sound: the guard
/// region must lie inside the region where the pure op is undefined. Concretely,
/// prove `D(else) AND guard_cond` is unsatisfiable — the guard fires only where the
/// IR op is undefined, so outside the guard the instruction already computes the
/// pure op (the shared else arm) and the drop loses nothing.
///
/// This is the tractable form of `D(pattern) => guarded == pattern`: since the else
/// arm *is* the pattern, that obligation is `(D AND guard_cond) => (then == else)`,
/// discharged here by refuting its antecedent. `D`'s divisor-nonzero test and the
/// guard's divisor-zero test share the divisor symbol, so the conflict closes by
/// unit propagation without the SAT solver ever entering the divider circuit (the
/// full-equality form forces it through a divider miter — seconds, not
/// milliseconds).
fn relaxation_holds(
    guarded: &SemGraph,
    if_root: NodeId,
    else_arm: NodeId,
    register_width: u32,
) -> bool {
    let guard_cond = match guarded.children(if_root).next() {
        Some(cond) => cond,
        None => return false,
    };

    let mut g = SemGraph::new();
    let mut memo = HashMap::new();
    copy_subgraph(guarded, if_root, &mut g, &mut memo);
    let cond = memo[&guard_cond.index()];
    let else_copy = memo[&else_arm.index()];

    let widths = infer_widths(&g, |id| match g.get_leaf_data(id) {
        Some(SymPayload::SymbolId(_)) => Some(register_width),
        _ => None,
    });
    // A total else arm (no partial op) has no definedness slack; the antecedent is
    // then the guard alone, so relaxing is sound only if the guard is never taken.
    let defined = definedness_of(&mut g, else_copy, &widths).unwrap_or_else(|| {
        let always = g.add_node(SymKind::Constant);
        g.set_leaf_data(always, SymPayload::Int(APInt::new(1, 1)));
        always
    });
    let conflict = g.add_node(SymKind::And);
    g.add_edge(conflict, defined);
    g.add_edge(conflict, cond);
    debug_assert_eq!(g.root(), Some(conflict));

    let mut never = SemGraph::new();
    let zero = never.add_node(SymKind::Constant);
    never.set_leaf_data(zero, SymPayload::Int(APInt::new(1, 0)));

    let symbol_count = symbol_ids(&g).into_iter().max().map_or(0, |m| m + 1) as usize;
    let symbol_widths = vec![register_width; symbol_count];
    SmtOracle.equivalent(&g, &never, &symbol_widths)
}

fn symbol_ids(g: &SemGraph) -> Vec<u32> {
    (0..g.len())
        .filter_map(|i| match g.get_leaf_data(NodeId::from_index(i)) {
            Some(SymPayload::SymbolId(id)) => Some(*id),
            _ => None,
        })
        .collect()
}

impl InstructionSelectPass {
    /// Build the pass, panicking if a guarded rule's relaxation cannot be proved.
    /// The generated backends call this: an unprovable rule is a target-definition
    /// bug that must fail loudly, not at runtime.
    pub fn new(rules: Vec<Rule>) -> Self {
        Self::try_new(rules).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Build the pass, returning [`PassError::InvalidRuleSet`] naming the offending
    /// rule when a guarded rule's guard-relaxation obligation
    /// `D(pattern) => guarded_semantics == pattern` does not hold.
    pub fn try_new(rules: Vec<Rule>) -> Result<Self, PassError> {
        prove_guarded_relaxations(&rules)?;
        Ok(Self::build(rules))
    }

    fn build(rules: Vec<Rule>) -> Self {
        let declared_float_constant_materializer_widths: HashSet<_> = rules
            .iter()
            .filter_map(|rule| rule.float_constant_width)
            .collect();
        let compiled_patterns: Vec<_> = rules
            .iter()
            .enumerate()
            .filter_map(|(rule_index, rule)| {
                compile_isel_pattern(
                    rule_index,
                    &rule.pattern,
                    &rule.operand_constraints,
                    &rule.operand_registers,
                    &rule.operand_imm_ranges,
                    rule.result_register,
                )
            })
            .collect();

        let rewrites = discover_rewrites();
        let constant_materializer_ranges: Vec<_> = compiled_patterns
            .iter()
            .filter_map(CompiledIselPattern::constant_materializer_range)
            .collect();
        let float_constant_materializer_widths = declared_float_constant_materializer_widths
            .into_iter()
            .filter(|width| {
                constant_materializer_ranges
                    .iter()
                    .any(|range| range.width >= *width)
            })
            .collect();
        let pointer_width = pattern::natural_pointer_width(&compiled_patterns);

        Self {
            rules,
            compiled_patterns,
            constant_materializer_ranges,
            float_constant_materializer_widths,
            pointer_width,
            rewrites,
            branch_emitters: None,
            cost_model: Box::new(DefaultIselCostModel),
            op_lowerings: vec![],
            call_lowering: None,
            plans: HashMap::new(),
            emitted_blocks: HashSet::new(),
            emitted_values: HashMap::new(),
            solved: HashSet::new(),
        }
    }

    /// Install the target's terminator emitters, enabling rule-driven selection
    /// of conditional branches (and generic lowering of unconditional ones).
    pub fn with_branch_emitters(mut self, emitters: BranchEmitters) -> Self {
        self.branch_emitters = Some(emitters);
        self
    }

    /// Install semantic invariants used to saturate the program e-graph.
    pub fn with_rewrites(mut self, rewrites: Vec<IselRewrite>) -> Self {
        self.rewrites = rewrites;
        self
    }

    /// Install additional semantic invariants.
    pub fn with_axioms(mut self, file: &str) -> Self {
        let mut materializes_constants = false;
        for form in axioms::axiom_forms(file) {
            let axiom = axioms::parse_axiom(&form)
                .unwrap_or_else(|e| panic!("invalid axiom `{form}`: {e}"));
            materializes_constants |= axiom.materializes_constants();
            self.rewrites.push(axiom.compile());
        }
        if materializes_constants && !self.constant_materializer_ranges.is_empty() {
            self.float_constant_materializer_widths.extend(
                self.rules
                    .iter()
                    .filter_map(|rule| rule.float_constant_width),
            );
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

    pub fn with_call_lowering(
        mut self,
        abi: &'static crate::backend::abi::AbiInfo,
        emitter: Box<dyn crate::backend::call_lowering::CallEmitter>,
    ) -> Self {
        self.call_lowering = Some(crate::backend::call_lowering::CallLowering::new(
            abi, emitter,
        ));
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
        let gsa = analyses.get::<GSA>(context, root);

        let mut fs = self.build_function_selection(context, op, &facts, &gsa);
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
                compiled.search_with_legality(&fs.egraph, context, &|node, class| {
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
        gsa: &GSA,
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
        let mut control: HashMap<BlockId, Vec<ControlReification>> = HashMap::new();
        let mut prepared: HashMap<ValueId, ConditionExpr> = HashMap::new();
        let mut constant_candidates: Vec<(OpId, Id)> = Vec::new();
        let value_to_class = {
            let mut builder =
                SemDagBuilder::new(context, &value_to_def, gsa, &mut egraph, self.pointer_width);
            for &block_id in &block_ids {
                for op_id in context.get_block(block_id).op_ids() {
                    let op = context.get_op(op_id);
                    if let Some(root) = builder.build_for_op(&op).or_else(|| {
                        op.is::<crate::builtin::ConstantOp>()
                            .then(|| builder.build_from_value(op.results[0]))
                    }) {
                        roots_by_op.insert(op_id, root);
                    } else if (op.is::<crate::builtin::ConstantFOp>()
                        && op.results.first().is_some_and(|result| {
                            let ty = context.get_value(*result).ty();
                            let data = context.get_type_data(ty);
                            (data.as_ref() as &dyn std::any::Any)
                                .downcast_ref::<tir::builtin::FloatType>()
                                .is_some_and(|ty| {
                                    self.float_constant_materializer_widths
                                        .contains(&ty.bit_width())
                                })
                        }))
                        && let Some(&result) = op.results.first()
                    {
                        constant_candidates.push((op_id, builder.build_from_value(result)));
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
                            let (true_dest, false_dest) = if *a_taken {
                                (*a_dest, *b_dest)
                            } else {
                                (*b_dest, *a_dest)
                            };
                            let operands = op
                                .clone()
                                .as_interface::<dyn BranchTerminator>()
                                .map(|branch| branch.successor_operands())
                                .unwrap_or_default();
                            let edge_args = |dest| {
                                operands
                                    .iter()
                                    .find(|(succ, _)| *succ == dest)
                                    .map(|(_, args)| args.clone())
                                    .unwrap_or_default()
                            };
                            let true_args = edge_args(true_dest);
                            let false_args = edge_args(false_dest);
                            let class = builder.build_from_value(*a_cond);
                            control
                                .entry(block_id)
                                .or_default()
                                .push(ControlReification::Guarded(BlockGuard {
                                    op: op_id,
                                    condition: *a_cond,
                                    class,
                                    true_dest,
                                    false_dest,
                                    true_args,
                                    false_args,
                                }));
                        } else if let Some(branch) =
                            op.clone().as_interface::<dyn BranchTerminator>()
                        {
                            let successors = branch.successor_operands();
                            let [(dest, args)] = successors.as_slice() else {
                                continue;
                            };
                            control.entry(block_id).or_default().push(
                                ControlReification::Unconditional(BlockJump {
                                    op: op_id,
                                    dest: *dest,
                                    args: args.clone(),
                                }),
                            );
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

        // Standalone constants have no semantic root. When the target patterns
        // describe a matching materializer, their operand-built class becomes
        // the op root so the cover can select the materializing instructions.
        constant_candidates.retain(|(op, class)| {
            context.get_op(*op).is::<crate::builtin::ConstantFOp>()
                || class_int_binding(&egraph, *class).is_some()
        });
        for &(op_id, class) in &constant_candidates {
            roots_by_op.entry(op_id).or_insert(class);
        }

        rewrites::saturate(context, &mut egraph, &self.rewrites, Default::default());

        seed_bare_condition_terms(context, &mut egraph, &control);

        // Canonicalize the side tables through `find`: saturation may merge classes,
        // so every id recorded against the pre-saturation graph is re-resolved here.
        let mut ops_by_root: HashMap<Id, Vec<OpId>> = HashMap::new();
        let mut op_root: HashMap<OpId, Id> = HashMap::new();
        for (&op, &root) in &roots_by_op {
            let class = egraph.find(root);
            ops_by_root.entry(class).or_default().push(op);
            op_root.insert(op, class);
        }
        // `roots_by_op` iterates in hash order; the per-class lists decide
        // emission order, so they are sorted into program order.
        for ops in ops_by_root.values_mut() {
            ops.sort_unstable();
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

        let guards = || {
            control
                .values()
                .flatten()
                .filter_map(ControlReification::guard)
        };
        let guard_by_op: HashMap<OpId, &BlockGuard> =
            guards().map(|guard| (guard.op, guard)).collect();
        let guard_condition_ops: HashSet<OpId> = guards()
            .filter_map(|guard| value_to_def.get(&guard.condition).copied())
            .collect();
        let needs_register = |result: ValueId, class: Id, def_block: BlockId| {
            let unselected_use = context.get_value(result).uses().iter().any(|u| {
                if roots_by_op.contains_key(&u.op()) || guard_condition_ops.contains(&u.op()) {
                    return false;
                }
                // A guard's condition is recomputed by branch selection, but an
                // edge argument the same terminator forwards needs the register.
                match guard_by_op.get(&u.op()) {
                    Some(guard) => {
                        result != guard.condition
                            || guard.true_args.contains(&result)
                            || guard.false_args.contains(&result)
                    }
                    None => true,
                }
            });
            let cross_block = context
                .get_value(result)
                .uses()
                .iter()
                .any(|u| op_block.get(&u.op()).copied() != Some(def_block));
            let cross_block_register = cross_block
                && class_int_binding(&egraph, class).is_some_and(|value| {
                    !self
                        .constant_materializer_ranges
                        .iter()
                        .any(|range| range.contains(&value))
                })
                || cross_block && class_int_binding(&egraph, class).is_none();
            unselected_use || cross_block_register
        };
        // A low-bit truncation re-views its source's register, so demand lands
        // on the chased source class — the one a tile can define.
        let chase = |egraph: &SemEGraph, class: Id| {
            let mut class = egraph.find(class);
            while let Some(source) = low_extract_source(egraph, class) {
                class = source;
            }
            class
        };
        let mut demand = HashSet::new();
        for (&op_id, &class) in &roots_by_op {
            let def_block = op_block[&op_id];
            for &result in &context.get_op(op_id).results {
                if needs_register(result, class, def_block) {
                    demand.insert((chase(&egraph, class), def_block));
                }
            }
        }
        for &(op_id, class) in &constant_candidates {
            let def_block = op_block[&op_id];
            for &result in &context.get_op(op_id).results {
                if needs_register(result, class, def_block) {
                    demand.insert((chase(&egraph, class), def_block));
                }
            }
        }
        for edge in control.values_mut().flatten() {
            if let ControlReification::Guarded(guard) = edge {
                guard.class = egraph.find(guard.class);
            }
        }
        let reifiable_gates = value_to_class
            .iter()
            .filter_map(|(&value, &class)| {
                let node = gsa.node_of(value)?;
                matches!(gsa.gate(node), GateNode::Gamma { .. } | GateNode::Mu { .. })
                    .then(|| egraph.find(class))
            })
            .collect();
        FunctionSelection {
            egraph,
            ops_by_root,
            op_root,
            class_values,
            op_position,
            value_to_def,
            value_block,
            shared_classes,
            demand,
            control,
            reifiable_gates,
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

    /// Create a trampoline block forwarding `args` to `dest` through the target's
    /// unconditional-branch emitter, appended to the branching block's region.
    /// Returns the trampoline's id, which a conditional branch targets in place
    /// of `dest`. The solver has already run over the (unchanged) block set, so
    /// the new block is never visited for selection.
    fn emit_edge_trampoline(
        &self,
        context: &Context,
        source: BlockId,
        dest: BlockId,
        args: &[ValueId],
        emitters: &BranchEmitters,
    ) -> BlockId {
        let trampoline = context.create_block(vec![]);
        let vbr = (emitters.uncond)(context, dest, args);
        trampoline.insert(trampoline.len(), vbr.id());
        if let Some(region) = context.parent_region(source) {
            context.get_region(region).add_block(trampoline.id());
        }
        trampoline.id()
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

        let mut plan = match self.plans.get(&block.id()) {
            Some(Ok(plan)) => plan.clone(),
            Some(Err(message)) => return Err(PassError::InvalidRuleSet(message.clone())),
            None => return Ok(()),
        };

        let block_arc = context.get_block(block.id());
        let anchor = block_arc
            .op_ids()
            .into_iter()
            .find(|op| {
                context
                    .get_op(*op)
                    .as_interface::<dyn Terminator>()
                    .is_some()
            })
            .map(|op| OperationRef::new(context.get_op(op), Some(block_arc.clone()), None));
        for scheduled in &plan.schedule {
            let source = scheduled
                .source_op
                .map(|op| OperationRef::new(context.get_op(op), Some(block_arc.clone()), None));
            let mut m = scheduled.m.clone();
            m.remap_values(&self.emitted_values);
            let request = EmitRequest {
                op: source.as_ref(),
                results: &scheduled.results,
                result_ty: scheduled.result_ty,
            };
            let rule = &self.rules[scheduled.rule_index];
            let tile_anchor = scheduled
                .anchor
                .map(|op| OperationRef::new(context.get_op(op), Some(block_arc.clone()), None))
                .or_else(|| anchor.clone());
            if let Some(prelude) = rule.prelude_emit {
                let op = prelude(context, &request, &m)?;
                if let Some(anchor) = &tile_anchor {
                    rewriter.insert_op_before(anchor, op.as_ref())?;
                } else {
                    block_arc.insert(block_arc.len(), op.id());
                }
            }
            let op = (rule.emit_fn)(context, &request, &m)?;
            if let Some(anchor) = &tile_anchor {
                rewriter.insert_op_before(anchor, op.as_ref())?;
            } else {
                block_arc.insert(block_arc.len(), op.id());
            }
            for (&old, new) in scheduled
                .results
                .iter()
                .zip(context.get_op(op.id()).results.iter())
            {
                self.emitted_values.insert(old, *new);
            }
        }
        for (old, new) in &plan.value_remaps {
            let new = self.emitted_values.get(new).copied().unwrap_or(*new);
            if *old != new {
                self.emitted_values.insert(*old, new);
                context.replace_value_uses(*old, new);
            }
        }

        for terminator in &mut plan.terminators {
            match terminator {
                TerminatorPlan::Guard {
                    branch,
                    true_args,
                    false_args,
                    ..
                } => {
                    if let GuardBranch::Fused { m, .. } = branch {
                        m.remap_values(&self.emitted_values);
                    } else if let GuardBranch::Nonzero { condition } = branch
                        && let Some(replacement) = self.emitted_values.get(condition)
                    {
                        *condition = *replacement;
                    }
                    for value in true_args.iter_mut().chain(false_args) {
                        if let Some(replacement) = self.emitted_values.get(value) {
                            *value = *replacement;
                        }
                    }
                }
                TerminatorPlan::Jump { args, .. } => {
                    for value in args {
                        if let Some(replacement) = self.emitted_values.get(value) {
                            *value = *replacement;
                        }
                    }
                }
            }
        }

        if let Some(emitters) = &self.branch_emitters {
            for terminator in &plan.terminators {
                match terminator {
                    TerminatorPlan::Guard {
                        op,
                        branch,
                        true_dest,
                        true_args,
                        false_dest,
                        false_args,
                    } => {
                        let op_ref =
                            OperationRef::new(context.get_op(*op), Some(block_arc.clone()), None);
                        // A true edge carrying arguments cannot ride the
                        // conditional branch, so it forwards them through a
                        // trampoline block the branch targets instead.
                        let true_target = if true_args.is_empty() {
                            *true_dest
                        } else {
                            self.emit_edge_trampoline(
                                context,
                                block.id(),
                                *true_dest,
                                true_args,
                                emitters,
                            )
                        };
                        let branch_ops: Vec<Box<dyn Operation>> = match branch {
                            GuardBranch::Fused { rule_index, m } => {
                                let request = EmitRequest {
                                    op: None,
                                    results: &[],
                                    result_ty: None,
                                };
                                let rule = &self.rules[*rule_index];
                                let RuleKind::CondBranch { target_symbol } = rule.kind else {
                                    unreachable!("fused guard rule is a conditional branch")
                                };
                                let mut m = m.clone();
                                m.rebind_block(target_symbol, true_target);
                                // A flag-mediated branch rule emits its
                                // flag-setting definer right before the branch
                                // instruction that reads the flags.
                                let mut ops = Vec::new();
                                if let Some(prelude) = rule.prelude_emit {
                                    ops.push(prelude(context, &request, &m)?);
                                }
                                ops.push((rule.emit_fn)(context, &request, &m)?);
                                ops
                            }
                            GuardBranch::Nonzero { condition } => {
                                (emitters.cond_nonzero)(context, *condition, true_target)
                            }
                        };
                        for branch_op in &branch_ops {
                            rewriter.insert_op_before(&op_ref, branch_op.as_ref())?;
                        }
                        let fallthrough = (emitters.uncond)(context, *false_dest, false_args);
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

        for op in plan.erase_ops.into_iter().rev() {
            let op = OperationRef::new(context.get_op(op), Some(block_arc.clone()), None);
            rewriter.erase_op(&op)?;
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

        let control = fs.control.get(&block_id).map(Vec::as_slice).unwrap_or(&[]);
        let guard_classes: HashSet<Id> = control
            .iter()
            .filter_map(ControlReification::guard)
            .map(|guard| fs.egraph.find(guard.class))
            .collect();

        let matches = self.collect_block_matches(
            context,
            fs,
            &op_refs,
            &block_op_by_root,
            &guard_classes,
            base_matches,
        );

        // Search the branch rules once for the whole block, indexed by condition
        // class, so each guard just looks up its hits.
        let guard_branch_hits = if guard_classes.is_empty() {
            HashMap::new()
        } else {
            self.guard_branch_hits(context, fs)
        };

        // Resolve each guarded terminator: fuse its condition into a branch-rule
        // instruction when one matches, else fall back to the target's
        // branch-if-nonzero (which needs the condition materialized).
        let mut mm_overlay: HashSet<Id> = HashSet::new();
        let mut terminators = Vec::new();
        for edge in control {
            let guard = match edge {
                ControlReification::Unconditional(jump) => {
                    terminators.push(TerminatorPlan::Jump {
                        op: jump.op,
                        dest: jump.dest,
                        args: jump.args.clone(),
                    });
                    continue;
                }
                ControlReification::Guarded(guard) => guard,
            };
            let class = fs.egraph.find(guard.class);
            // A condition proven constant (a dominating-edge assumption) folds
            // the guard to an unconditional branch to the known successor.
            if let Some(known) = class_int_binding(&fs.egraph, class) {
                let (dest, args) = if known.to_u64() != 0 {
                    (guard.true_dest, guard.true_args.clone())
                } else {
                    (guard.false_dest, guard.false_args.clone())
                };
                terminators.push(TerminatorPlan::Jump {
                    op: guard.op,
                    dest,
                    args,
                });
                continue;
            }
            let candidates = guard_branch_hits
                .get(&class)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let branch = match self.best_guard_branch(context, fs, dom, block_id, guard, candidates)
            {
                Some((rule_index, m, boundary_classes)) => {
                    // A low-extract boundary re-views its source's register, so
                    // the demand lands on the chased source class.
                    for boundary in boundary_classes {
                        mm_overlay.insert(fs.chase_low_extract(boundary));
                    }
                    GuardBranch::Fused { rule_index, m }
                }
                None => {
                    mm_overlay.insert(fs.chase_low_extract(class));
                    GuardBranch::Nonzero {
                        condition: guard.condition,
                    }
                }
            };
            terminators.push(TerminatorPlan::Guard {
                op: guard.op,
                branch,
                true_dest: guard.true_dest,
                true_args: guard.true_args.clone(),
                false_dest: guard.false_dest,
                false_args: guard.false_args.clone(),
            });
        }

        // Restrict the cover to the closure of B's op-root and guard-condition
        // classes under the surviving matches' bindings (so rewrite-introduced
        // intermediates reached from B are covered, but nothing from other blocks).
        let covered = closure_classes(&fs.egraph, &block_roots, &guard_classes, &matches);
        let demanded: HashSet<Id> = covered
            .iter()
            .copied()
            .filter(|class| {
                fs.demanded_at(*class, block_id, &mm_overlay)
                    || (block_roots.contains(class) && !node::class_is_pure(&fs.egraph, *class))
            })
            .collect();
        let available = |class| {
            if fs.is_reifiable_gate(class) {
                return false;
            }
            // A low-extract view owns no register of its own: it re-views its
            // source's. So it is available exactly when that source is — either
            // already in a register, or extracted here (the source's tile
            // defines the register the view reads). Treating the view itself as
            // unconditionally available would leave a demanded class with no
            // tile and its erased value dangling.
            let source = fs.chase_low_extract(class);
            let source_available = fs.available_at(context, dom, source, block_id)
                || (self.constant_materializer_ranges.is_empty()
                    && class_int_binding(&fs.egraph, source).is_some());
            if source == class {
                source_available
            } else {
                source_available || demanded.contains(&source)
            }
        };
        if let Some(message) =
            completeness_error(&fs.egraph, &demanded, &matches, &available, &|class| {
                fs.is_reifiable_gate(class)
            })
        {
            return Err(message);
        }

        let cover = build_eclass_cover(
            &fs.egraph,
            &covered,
            &cover::ClassPolicies {
                demanded: &|class| demanded.contains(&fs.egraph.find(class)),
                available: &available,
                reifiable_gate: &|class| fs.is_reifiable_gate(class),
            },
            &matches,
        )
        .ok_or_else(|| {
            let ops = op_ids
                .iter()
                .map(|op| context.get_op(*op).name().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("no feasible instruction cover for {block_id:?}: {ops}")
        })?;

        let mut root_match: HashMap<Id, usize> = HashMap::new();
        let mut reified = HashSet::new();
        for (node, choice) in cover.choices.iter().enumerate() {
            match choice {
                PbqpIselAlternative::Tile { match_id } => {
                    root_match.insert(cover.classes[node], *match_id);
                }
                PbqpIselAlternative::Reify => {
                    reified.insert(cover.classes[node]);
                }
                PbqpIselAlternative::NotDemanded => {}
            }
        }
        let required_available: HashSet<Id> = root_match
            .values()
            .flat_map(|match_id| &matches[*match_id].bindings.pattern_nodes)
            .filter(|binding| binding.is_boundary && binding.demand == BoundaryDemand::Register)
            .map(|binding| fs.egraph.find(binding.class))
            .filter(|class| !root_match.contains_key(class) && !reified.contains(class))
            .collect();
        let mut positions: HashMap<Id, usize> = block_op_by_root
            .iter()
            .filter_map(|(&class, op)| {
                op_ids
                    .iter()
                    .position(|candidate| candidate == op)
                    .map(|position| (class, position))
            })
            .collect();
        // Pull each pure tile up to its earliest consumer's position: surviving
        // mid-block ops (calls) and other tiles read their operands in block
        // order, so a tile must precede every consumer even when its original op
        // (a constant merged into an earlier class) sits after one.
        loop {
            let mut changed = false;
            for (&class, &match_id) in &root_match {
                let Some(&consumer_pos) = positions.get(&class) else {
                    continue;
                };
                for binding in &matches[match_id].bindings.pattern_nodes {
                    let leaf = fs.egraph.find(binding.class);
                    if leaf == class
                        || !binding.is_boundary
                        || binding.demand != BoundaryDemand::Register
                        || !root_match.contains_key(&leaf)
                        || !node::class_is_pure(&fs.egraph, leaf)
                    {
                        continue;
                    }
                    if positions.get(&leaf).is_none_or(|p| *p > consumer_pos) {
                        positions.insert(leaf, consumer_pos);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let tiles = schedule_tiles(&fs.egraph, &matches, &root_match, &positions)
            .ok_or_else(|| format!("cyclic instruction schedule for {block_id:?}"))?;

        let mut destinations = HashMap::new();
        let mut tile_results = HashMap::new();
        for &(class, _) in &tiles {
            let source_op = block_op_by_root.get(&class).copied();
            let mut results = source_op
                .map(|op| context.get_op(op).results.clone())
                .unwrap_or_default();
            let mut result_ty = results.first().map(|value| context.get_value(*value).ty());
            if results.is_empty() && node::class_is_pure(&fs.egraph, class) {
                let ty = class_width(context, &fs.egraph, class)
                    .map(|width| tir::builtin::IntegerType::new(context, width))
                    .unwrap_or_else(|| tir::builtin::IntegerType::new(context, 64));
                results.push(context.create_value(ty, None).id());
                result_ty = Some(ty);
            }
            if let Some(&result) = results.first() {
                destinations.insert(class, result);
            }
            tile_results.insert(class, (source_op, results, result_ty));
        }

        let consumer = op_ids.last().copied().unwrap();
        let schedule = tiles
            .into_iter()
            .map(|(class, match_id)| {
                let (source_op, results, result_ty) = tile_results.remove(&class).unwrap();
                ScheduledEmit {
                    rule_index: matches[match_id].rule_index,
                    m: resolve_match(
                        fs,
                        dom,
                        context,
                        block_id,
                        source_op.unwrap_or(consumer),
                        &matches[match_id],
                        &destinations,
                    ),
                    source_op,
                    anchor: positions.get(&class).map(|&position| op_ids[position]),
                    results,
                    result_ty,
                }
            })
            .collect();

        let mut value_remaps = Vec::new();
        let mut remap_class_values = |class: Id, destination: ValueId| {
            for member in fs.base_members(class) {
                if let Some(values) = fs.class_values.get(&member) {
                    value_remaps.extend(
                        values
                            .iter()
                            .copied()
                            .filter(|value| {
                                *value != destination
                                    && fs.value_block.get(value) == Some(&Some(block_id))
                            })
                            .map(|value| (value, destination)),
                    );
                }
            }
        };
        for (&class, &destination) in &destinations {
            remap_class_values(class, destination);
        }
        // A demanded class satisfied by availability (a dominating placement, or
        // a value the scope's facts merged in) schedules no tile, but its
        // block-local values must still resolve to the available register.
        for &class in &demanded {
            if destinations.contains_key(&class) || reified.contains(&class) {
                continue;
            }
            if let Some(destination) = fs
                .resolve_binding(dom, context, class, block_id, consumer, false)
                .value
            {
                remap_class_values(class, destination);
            }
        }
        for &op in &op_ids {
            let Some(mut class) = fs.op_root.get(&op).map(|class| fs.egraph.find(*class)) else {
                continue;
            };
            // A view the extraction tiled in its own right already defines the
            // op's result; only an untiled view forwards its source's register.
            if !is_low_extract_view(&fs.egraph, class) || destinations.contains_key(&class) {
                continue;
            }
            while let Some(source) = low_extract_source(&fs.egraph, class) {
                class = source;
            }
            if let Some(source) = destinations.get(&class).copied().or_else(|| {
                fs.resolve_binding(dom, context, class, block_id, consumer, false)
                    .value
            }) {
                value_remaps.extend(
                    context
                        .get_op(op)
                        .results
                        .iter()
                        .copied()
                        .map(|value| (value, source)),
                );
            }
        }
        let terminator_ops: HashSet<OpId> = terminators
            .iter()
            .map(|terminator| match terminator {
                TerminatorPlan::Guard { op, .. } | TerminatorPlan::Jump { op, .. } => *op,
            })
            .collect();
        let erase_ops = op_ids
            .into_iter()
            .filter(|op| fs.op_root.contains_key(op))
            .filter(|op| !terminator_ops.contains(op))
            .filter(|op| {
                fs.op_root.get(op).is_none_or(|class| {
                    let class = fs.egraph.find(*class);
                    !reified.contains(&class) && !required_available.contains(&class)
                })
            })
            .collect();

        Ok(BlockPlan {
            schedule,
            erase_ops,
            value_remaps,
            terminators,
        })
    }

    /// Every conditional-branch rule match over the block's (scoped) graph,
    /// indexed by condition class, so each guard resolves against its own hits
    /// without re-searching per guard.
    fn guard_branch_hits(
        &self,
        context: &Context,
        fs: &FunctionSelection,
    ) -> HashMap<Id, Vec<(usize, EMatch<u32>)>> {
        let mut hits: HashMap<Id, Vec<(usize, EMatch<u32>)>> = HashMap::new();
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
        let mut register_symbols_by_pattern: HashMap<usize, HashSet<u32>> = HashMap::new();
        for (pattern_index, m) in candidates {
            let compiled = &self.compiled_patterns[*pattern_index];
            let RuleKind::CondBranch { target_symbol } = self.rules[compiled.rule_index].kind
            else {
                continue;
            };

            let register_symbols = register_symbols_by_pattern
                .entry(*pattern_index)
                .or_insert_with(|| compiled.register_symbols());

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
            // A register operand may read a bare constant only when another use
            // already forces that constant to be materialized.
            for (symbol, class) in &captures.entries {
                // Prefer a surviving/available value; bind a same-block pending
                // tile only when none exists — then the overlay demand forces
                // that tile (the class cannot be available, so NotDemanded is
                // never offered) and the bound value is defined.
                let mut binding = fs.resolve_binding(dom, context, *class, block, guard.op, false);
                if binding.value.is_none() {
                    binding = fs.resolve_binding(dom, context, *class, block, guard.op, true);
                }
                match binding.int {
                    Some(v) => {
                        if register_symbols.contains(symbol)
                            && !fs.available_at(context, dom, *class, block)
                            && !fs.placed_at(*class, block)
                        {
                            resolvable = false;
                            break;
                        }
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
                    (cost, std::cmp::Reverse(specificity))
                        < (*best_cost, std::cmp::Reverse(*best_specificity))
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
                fresh = compiled.search_with_legality(&fs.egraph, context, &|node, class| {
                    value_match_allowed(fs, context, compiled, pattern_root, node, class)
                });
                &fresh
            };
            for m in raw {
                let root = fs.egraph.find(m.root);
                if compiled.is_copy() && fs.has_values(m.binding(pattern_root)) {
                    continue;
                }
                if fs.is_reifiable_gate(root)
                    && !gate_match_is_speculatable(compiled, m, &fs.egraph)
                {
                    continue;
                }
                let block_op = block_op_by_root.get(&root).copied();
                let is_guard_class = guard_classes.contains(&root);
                // A match roots an instruction only if it produces a value B
                // computes: an op of B, a guard condition of B, a
                // rewrite-introduced intermediate, or a terminal constant covered
                // by a real target materializer instruction.
                let is_computed = fs
                    .egraph
                    .nodes(root)
                    .iter()
                    .any(|n| !n.children().is_empty());
                let synthetic = !fs.is_op_root(root)
                    && (is_computed || compiled.constant_materializer_range().is_some());
                if block_op.is_none() && !is_guard_class && !synthetic {
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

                let mut structural_boundaries = HashSet::new();
                let mut value_boundaries = HashSet::new();
                for index in 0..compiled.pattern.len() {
                    let PatternNode::Node(node) = compiled.pattern.node(Id::from_raw(index as u32))
                    else {
                        continue;
                    };
                    for (operand, &child) in node.children.iter().enumerate() {
                        if !compiled.node_meta[child.index()].is_boundary {
                            continue;
                        }
                        if matches!(node.kind, SymKind::SExt | SymKind::ZExt) && operand == 1 {
                            structural_boundaries.insert(child);
                        } else {
                            value_boundaries.insert(child);
                        }
                    }
                }
                structural_boundaries.retain(|node| !value_boundaries.contains(node));
                let pattern_nodes: Vec<PatternNodeBinding> = (0..compiled.pattern.len())
                    .map(|index| Id::from_raw(index as u32))
                    .map(|pattern_node| {
                        let meta = &compiled.node_meta[pattern_node.index()];
                        // Constants are boundary-like: pure, folded into the
                        // encoding, never consumed by the match — so the same
                        // constant class (e.g. the literal 0) can sit inside one
                        // match and under a boundary of another without making
                        // the cover infeasible.
                        let is_boundary = meta.is_boundary || meta.is_constant;
                        let demand = if meta.demands_register()
                            || (meta.is_boundary
                                && meta.constraint.is_none()
                                && meta.register.is_none()
                                && !structural_boundaries.contains(&pattern_node))
                        {
                            cover::BoundaryDemand::Register
                        } else if meta.constraint == Some(tir::graph::OperandConstraint::Immediate)
                            || meta.is_constant
                        {
                            cover::BoundaryDemand::Immediate
                        } else {
                            cover::BoundaryDemand::Structural
                        };
                        let mut class = fs.egraph.find(m.binding(pattern_node));
                        // A register boundary on a low-extract view reads the
                        // chased source's register, so the cover's edges, the
                        // schedule's dependencies, and availability all target
                        // the class a tile can actually define.
                        if is_boundary && demand == cover::BoundaryDemand::Register {
                            class = fs.chase_low_extract(class);
                        }
                        PatternNodeBinding {
                            pattern_node,
                            class,
                            is_boundary,
                            demand,
                        }
                    })
                    .collect();
                // Extraction is acyclic: a tile may not register-read its own
                // root class (it would compute the value from itself). Identity
                // members put e.g. `add(x, 0)` inside `x`'s class, so an
                // add-with-immediate rule roots on `x` while register-binding
                // `x` — a zero-progress tile, never selectable.
                if pattern_nodes.iter().any(|binding| {
                    binding.is_boundary
                        && binding.demand == cover::BoundaryDemand::Register
                        && binding.class == root
                }) {
                    continue;
                }
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

/// Whether selecting `matched` as a value instruction is sound for a gate class:
/// the instruction computes both arms unconditionally, so every node it covers —
/// in the pattern and in the classes its arms bind — must be speculatable. A
/// pattern not rooted at an `If` selects no gate arm, so it is unconstrained.
fn gate_match_is_speculatable(
    pattern: &CompiledIselPattern,
    matched: &EMatch<u32>,
    egraph: &SemEGraph,
) -> bool {
    let PatternNode::Node(root) = pattern.pattern.node(pattern.pattern.root()) else {
        return true;
    };
    if root.kind != SymKind::If {
        return true;
    }
    root.children
        .iter()
        .all(|&child| gate_pattern_is_speculatable(pattern, child))
        && root.children[1..].iter().all(|&arm| {
            gate_class_is_speculatable(egraph, matched.binding(arm), &mut HashSet::new())
        })
}

fn gate_pattern_is_speculatable(pattern: &CompiledIselPattern, node: Id) -> bool {
    match pattern.pattern.node(node) {
        PatternNode::Var(_) => true,
        PatternNode::Node(node) => {
            gate_kind_is_speculatable(node.kind)
                && node
                    .children
                    .iter()
                    .all(|&child| gate_pattern_is_speculatable(pattern, child))
        }
    }
}

fn gate_class_is_speculatable(egraph: &SemEGraph, class: Id, visited: &mut HashSet<Id>) -> bool {
    let class = egraph.find(class);
    if !visited.insert(class) {
        return true;
    }
    egraph.nodes(class).iter().all(|node| {
        gate_kind_is_speculatable(node.kind)
            && node
                .children
                .iter()
                .all(|&child| gate_class_is_speculatable(egraph, child, visited))
    })
}

/// Whether the kind may be evaluated on a path that does not need its value.
fn gate_kind_is_speculatable(kind: SymKind) -> bool {
    kind_is_pure(kind)
        && !matches!(
            kind,
            // Division may trap, and a Theta has no value outside its loop.
            SymKind::Div
                | SymKind::UDiv
                | SymKind::SRem
                | SymKind::URem
                | SymKind::Theta
                | SymKind::StateAssign
                | SymKind::StateStore
                | SymKind::StateStoreConditional
                | SymKind::StateFence
                | SymKind::StateTrap
                | SymKind::StateBlock
                | SymKind::StateIf
                | SymKind::StateTry
                | SymKind::StateHandler
        )
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

/// Give a bare one-bit guard the same nonzero comparison term used by target
/// zero-register branch rules. Comparison guards gain their zero-register forms
/// from the target-independent axioms; only a leaf condition needs this seed
/// because a self-referential global axiom interacts pathologically with the
/// boolean `*-via-if` saturation rules.
fn seed_bare_condition_terms(
    context: &Context,
    egraph: &mut SemEGraph,
    control: &HashMap<BlockId, Vec<ControlReification>>,
) {
    let i1 = tir::builtin::IntegerType::new(context, 1);
    for guard in control
        .values()
        .flatten()
        .filter_map(ControlReification::guard)
    {
        let class = egraph.find(guard.class);
        if class_width(context, egraph, class) != Some(1)
            || egraph
                .nodes(class)
                .iter()
                .any(|node| is_comparison(node.kind))
        {
            continue;
        }
        let zero = egraph.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(1, 0))),
            None,
        ));
        // The zext width operand is a wildcard in the matching rules, so any
        // constant serves as its placeholder.
        let width = egraph.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(1, 1))),
            None,
        ));
        let mut zext = template_node(SymKind::ZExt, None, None);
        zext.children = vec![zero, width];
        let zext = egraph.add(zext);
        let mut ne = template_node(SymKind::Ne, None, Some(i1));
        ne.children = vec![class, zext];
        let ne = egraph.add(ne);
        egraph.union(class, ne);
    }
    egraph.rebuild();
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
            if let Some(lowering) = &mut self.call_lowering {
                lowering.prepare_function(context, op, rewriter)?;
            }
            self.solve_function(context, op, analyses);
        }

        for lowering in &self.op_lowerings {
            if lowering(context, op, rewriter)? {
                return Ok(PreservedAnalyses::none());
            }
        }

        if let Some(lowering) = &mut self.call_lowering
            && lowering.lower(context, op, rewriter)?
        {
            return Ok(PreservedAnalyses::none());
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
