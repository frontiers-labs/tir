//! Instruction selection over semantic e-graphs.
//!
//! The whole function's operations are lowered into one shared e-graph of
//! semantic expressions ([`builder`]), saturated with the proved algebraic
//! rewrites the vocabulary owns ([`tir::sem::rewrites`]), and then covered *per
//! block* while traversing the
//! dominating-edge assumption scopes — by the target's instruction patterns
//! ([`pattern`]), e-matched by the shared [`tir_relational`] engine, via a
//! PBQP instance over e-classes ([`cover`]). The solved cover becomes an emission
//! plan ([`emit`]) the pass commits through the rewriter.

mod builder;
mod cover;
mod destruct;
mod emit;
mod matches;
mod node;
mod pattern;
mod rules;

use std::collections::{HashMap, HashSet};

use tir::BlockHandle;
use tir::{
    AnalysisManager, BlockId, Conditional, Context, EntryGuard, GuardedLoop, OpHandle, OpId,
    Operation, OperationRef, Pass, PassError, PassTarget, RegionId, Rewriter, TypeId, ValueId,
    analysis::{DefUse, DominatorTree, scopes},
    builtin::{trailing_state_operand, trailing_state_result},
    graph::{Dag, MutDag, NodeId, OperandConstraint},
    sem::{
        EquivalenceOracle, SemGraph, SmtOracle, SymKind, SymPayload, canonicalize_for_selection,
        definedness_condition,
        egraph::{class_int_binding, class_width, complement_comparison},
        infer_widths, template_node,
    },
};
use tir_adt::APInt;
use tir_relational::{ClassId as Id, Label as ENode};

pub use rules::{
    CapabilityKind, EmitAttr, EmitSpec, PatternRef, RegOperandSpec, ResultRegSpec, RuleSpec,
    build_rules, emit_with,
};
pub use tir::sem::{SaturationLimits, SemEGraph, SemNode, SemPayload, Theory};
pub use tir_relational::Match as IselMatch;

use builder::{AuxSlot, SemDagBuilder};
use cover::{
    BoundaryDemand, CaptureBindings, FullMatchBindings, PatternNodeBinding, PbqpIselAlternative,
    PbqpIselMatch, build_eclass_cover, completeness_error, prune_dominated_matches,
};
use emit::{AuxEmit, BlockPlan, GuardBranch, ScheduledEmit, order_tiles, resolve_match};
use matches::{MatchRef, Matches};
use node::{is_low_extract_view, low_extract_source};
use pattern::{CompiledIselPattern, PatternNode, compile_isel_pattern};
use tir::sem::axioms::{self, verify_axioms};
use tir::sem::rewrites::{self, discover_rewrites};

/// A conditional-branch rule chosen for a destruction's test: the rule, its
/// operand bindings (the taken target bound as a block), the boundary classes the
/// branch reads as registers, and the operand symbols whose register the cover has
/// yet to mint.
struct FusedGuard {
    rule_index: usize,
    m: RuleMatch,
    boundaries: Vec<Id>,
    deferred: Vec<(u32, Id)>,
}

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

    pub(crate) fn bind_value(&mut self, symbol: u32, value: ValueId) {
        self.value_bindings.push((symbol, value));
        self.value_bindings.sort_by_key(|(sym, _)| *sym);
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
    /// The memory chain the covered access reads and the state it publishes,
    /// taken from the IR operation the tile covers. An opcode that touches
    /// memory carries them as its trailing ports.
    pub state: Option<StatePorts>,
}

/// The `!state` values an emitted instruction takes over from the IR access it
/// covers: memory order is the mid-end's, and selection only keeps the edges.
#[derive(Clone, Copy, Debug)]
pub struct StatePorts {
    pub observed: ValueId,
    pub published: Option<ValueId>,
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

/// An immediate operand's encoding range: the field's bit width, whether the
/// instruction sign-extends it, and the `#[align]`/`#[nonzero]` constraints the
/// operand declares. A constant outside the range must not bind — its encoding
/// would silently truncate to a different value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImmRange {
    pub width: u32,
    pub signed: bool,
    /// The declared alignment; 1 when the operand declares none.
    pub align: u32,
    pub nonzero: bool,
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

/// A register operand's storage capability, whether its instruction reads the
/// value's full architectural width rather than only its defined low bits, and
/// where its register class views the storage element (`view_offset`, x86 `ah`
/// at bit 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterRequirement {
    capability: RegisterCapability,
    whole: bool,
    view_offset: u32,
}

impl RegisterRequirement {
    pub fn low_bits(capability: RegisterCapability) -> Self {
        Self {
            capability,
            whole: false,
            view_offset: 0,
        }
    }

    pub fn whole(capability: RegisterCapability) -> Self {
        Self {
            capability,
            whole: true,
            view_offset: 0,
        }
    }

    /// Place this operand's class at `offset` bits into its storage element (a
    /// TMDL `BIT_OFFSET` view, x86 `ah`). A value crosses between a producer and
    /// a consumer for free only when both view their storage at the same offset:
    /// no instruction moves bits across offsets implicitly.
    pub fn at_view_offset(mut self, offset: u32) -> Self {
        self.view_offset = offset;
        self
    }

    pub fn view_offset(&self) -> u32 {
        self.view_offset
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
    /// Whether `value` is representable in the field: its register pattern
    /// must survive the encode/decode roundtrip (truncate to the field, extend
    /// back per the field's signedness). A signed field compares the
    /// sign-extended pattern, so `4096` is rejected by a signed 12-bit field
    /// (it would decode as `-2048`) while the all-ones register constant fits
    /// any signed field as `-1`. An unsigned field compares the value's raw
    /// bits at its own width: i16 `-32640` is the pattern `0x8080`, which a
    /// 16-bit unsigned field encodes exactly.
    pub fn contains(&self, value: &APInt) -> bool {
        let bits = if value.is_signed() {
            value.to_i64() as u64
        } else {
            value.to_u64()
        };
        // The operand's declared constraints hold whatever the field's width:
        // the encoding drops the bits `#[align]` promises are zero.
        if !bits.is_multiple_of(u64::from(self.align)) || (self.nonzero && bits == 0) {
            return false;
        }
        if self.width >= 64 {
            return true;
        }
        if self.signed {
            let shift = 64 - self.width;
            (((bits << shift) as i64) >> shift) as u64 == bits
        } else {
            value.to_u64() >> self.width == 0
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

/// The latency weight of a rule cost: `cost = latency * LATENCY_COST_SCALE +
/// encoding_bytes`. x86's longest instruction is 15 bytes, so the byte term
/// never reaches a whole latency unit — latency decides the cost, and encoding
/// size only breaks ties between equally fast instructions (a REX-free form
/// against its REX twin).
pub const LATENCY_COST_SCALE: u32 = 16;

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
    pointer_width: Option<u32>,
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
    /// The block each block argument belongs to. Its register is written by the
    /// incoming edges, so it holds the argument only in blocks that block
    /// dominates. Entry inputs are absent (available everywhere).
    arg_block: HashMap<ValueId, BlockId>,
    /// The earliest region-carrying operation of a value's own block under whose
    /// regions the value is read. What a region reads has to have run before the
    /// region does, so a spelling bound for the value's class must precede it.
    region_use: HashMap<ValueId, OpId>,
    /// E-classes used as an operand by more than one consumer (function-wide). A
    /// memory effect in such a class cannot be internalized into a match.
    shared_classes: HashSet<Id>,
    /// Classes selected at their defining block because a surviving reader needs
    /// their register value.
    demand: HashSet<(Id, BlockId)>,
    /// Each region-entry condition prepared against the base graph: the
    /// condition's class and, when its definer is a comparison, the comparison
    /// class with its kind and operand classes. Keyed by the condition value; the
    /// per-block truth (`holds`) is applied when the scope asserts it.
    prepared: HashMap<ValueId, ConditionExpr>,
    /// The assumption a region's entry block is entered under, read off the
    /// region-carrying operation's own interfaces (see [`region_entry_facts`]).
    region_facts: HashMap<BlockId, (ValueId, bool)>,
    /// What each block must materialize for a destruction to branch on it.
    region_aux: HashMap<BlockId, Vec<(OpId, AuxSlot, Id)>>,
}

/// A boundary class resolved to concrete operands for a consumer: the proven
/// constant it folds to as an immediate, and/or the register value legal under
/// the dominance rule. A class can carry both (an assumption proves a value equal to
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

    /// The classes `block` must materialize for a destruction to read them, in the
    /// order the seeder recorded.
    fn aux_classes(&self, block: BlockId) -> impl Iterator<Item = Id> + '_ {
        self.region_aux
            .get(&block)
            .into_iter()
            .flatten()
            .map(|&(.., class)| self.egraph.find(class))
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

    /// Whether `block` holds an operation selection must emit for `class` — a
    /// definition of the class, as opposed to a name adopted from it.
    fn defined_in(&self, context: &Context, class: Id, block: BlockId) -> bool {
        self.any_class_value(class, |value| {
            self.value_to_def.get(value).is_some_and(|def| {
                self.op_root.contains_key(def) && context.parent_block(*def) == Some(block)
            })
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
                // An argument is written by the edges entering its own block, so
                // it holds the class only where that block has run — never in a
                // block it does not dominate, and never in a sibling arm.
                return self
                    .arg_block
                    .get(value)
                    .is_none_or(|&owner| dom.dominates(owner, block));
            };
            let op = context.get_op(def);
            let Some(def_block) = context.parent_block(def) else {
                return true;
            };
            if !self.op_root.contains_key(&def) {
                if op.is::<crate::builtin::ConstantOp>() || op.is::<crate::builtin::ConstantFOp>() {
                    return false;
                }
                // A region-carrying operation's result is a name adopted from the
                // class when its arms all publish the same one. The class holds
                // that name only once the region has run, which is after this
                // block's own definition of it — so the definition still has to
                // be selected, and the adopted name witnesses nothing.
                if def_block == block
                    && !op.regions().is_empty()
                    && self.defined_in(context, class, block)
                {
                    return false;
                }
                return def_block == block || self.has_run_at(context, dom, def, def_block, block);
            }
            def_block != block
                && self.has_run_at(context, dom, def, def_block, block)
                && self.placed_at(class, def_block)
        })
    }

    /// The point in `block` a spelling of `class` must reach: the earliest
    /// operation whose regions read a value the class holds here. A definition
    /// placed after that operation spells the class only for what follows the
    /// region, never for the region itself — the arms of a gate cannot read the
    /// name the gate publishes.
    fn region_ask(&self, context: &Context, block: BlockId, class: Id) -> Option<OpId> {
        self.base_members(class)
            .filter_map(|member| self.class_values.get(&member))
            .flatten()
            .filter_map(|value| match self.value_block.get(value) {
                Some(&Some(held)) if held == block => self.region_use.get(value).copied(),
                // A member of the class inside a region hanging off this block is
                // read where the operation holding that region is: a name that
                // operation publishes cannot answer it, so the ask goes ahead of it.
                Some(&Some(held)) => scopes::holder_in(context, block, held),
                _ => None,
            })
            .min_by_key(|op| self.op_position.get(op).copied().unwrap_or(usize::MAX))
    }

    /// Whether a definition of `def` in `def_block` has run wherever `block` runs.
    /// Dominance orders the blocks, but a block holding a region-carrying
    /// operation is only partly ordered against that region: what follows the
    /// operation runs after the region does, so a definition is visible inside
    /// only when it precedes the operation the region hangs from.
    fn has_run_at(
        &self,
        context: &Context,
        dom: &DominatorTree,
        def: OpId,
        def_block: BlockId,
        block: BlockId,
    ) -> bool {
        if !dom.dominates(def_block, block) {
            return false;
        }
        let mut current = block;
        while current != def_block {
            let Some(region) = context.parent_region(current) else {
                return true;
            };
            let Some(carrier) = context.get_region(region).parent_op() else {
                return true;
            };
            let Some(parent) = context.parent_block(carrier) else {
                return true;
            };
            if parent == def_block {
                return context.get_block(def_block).is_before(def, carrier);
            }
            if dom.node_of(parent).is_none() {
                return true;
            }
            current = parent;
        }
        true
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
    /// in `block`, under the dominance rule: an entry input or an argument of a
    /// dominating block; a same-block def preceding the consumer; or a value
    /// defined in a strict dominator that the original IR already used across
    /// blocks (so it is guaranteed materialized). `None` when no such value
    /// exists (the class may still bind as an immediate, or be materialized as
    /// an introduced instruction). Preference order — same-block earliest, then
    /// closest dominator — is deterministic.
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
        // A class a fact proves equal to a literal may read a register already
        // holding that literal, and a literal may read the register of a value
        // proven equal to it: the union used to make them one class, and this is
        // the one place that congruence was worth a register.
        let literal = self
            .egraph
            .assumed_const(class)
            .and_then(|node| self.egraph.const_class(node))
            .map(|id| self.egraph.find(id));
        let equal: Vec<Id> = self
            .egraph
            .nodes(class)
            .find(|node| node.sym() == Some(SymKind::Constant) && node.int().is_some())
            .map(|node| self.egraph.classes_assumed_const(node).collect())
            .unwrap_or_default();
        let mut best: Option<((u8, usize, u32), ValueId)> = None;
        for member in self.base_members(class).chain(literal).chain(equal) {
            let Some(candidates) = self.class_values.get(&member) else {
                continue;
            };
            for &v in candidates {
                let key = match self.value_block.get(&v).copied().flatten() {
                    None => {
                        // A block argument lives in a register only where its own
                        // block has run: mutually exclusive blocks may hold equal
                        // arguments, but only one of them was written.
                        if self
                            .arg_block
                            .get(&v)
                            .is_some_and(|&owner| !dom.dominates(owner, block))
                        {
                            continue;
                        }
                        (1u8, 0usize, v.number())
                    }
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
                        let def = self.value_to_def[&v];
                        if !self.has_run_at(context, dom, def, def_block, block) {
                            continue;
                        }
                        // A def the extraction places dominates by construction;
                        // an op selection never touches (an alloca, a call)
                        // survives with its original value.
                        let survives = !self.op_root.contains_key(&def)
                            && !context.get_op(def).is::<crate::builtin::ConstantOp>()
                            && !context.get_op(def).is::<crate::builtin::ConstantFOp>();
                        // Demand is what says a tile was placed for the value —
                        // and it is asked of the member the value belongs to,
                        // not of the whole class: a scope may merge a class the
                        // block materialized with one it folded into an
                        // encoding, and only the first leaves a register behind.
                        if !survives && !self.demand.contains(&(member, def_block)) {
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

/// A region-entry condition prepared against the base graph (see
/// [`FunctionSelection::prepared`]).
struct ConditionExpr {
    condition: Id,
    compare: Option<(Id, SymKind, Id, Id)>,
}

pub type OpLowering = fn(&Context, &OperationRef, &mut Rewriter) -> Result<bool, PassError>;

pub struct InstructionSelectPass {
    rules: Vec<Rule>,
    compiled_patterns: Vec<CompiledIselPattern>,
    /// How type-constrained each compiled pattern is: the tie-break the cover
    /// prunes dominated matches by.
    specificity: Vec<usize>,
    /// Value patterns rooted on a concrete operator, by that operator's key, and
    /// those that root on anything (a bare symbol, or a copy rule). Rule data, so
    /// it is built once rather than per function: a class is only ever searched
    /// against the patterns that can root at it.
    value_patterns_by_op: HashMap<u64, Vec<usize>>,
    value_patterns_anywhere: Vec<usize>,
    /// Immediate ranges of every formal constant materializer
    /// (see [`pattern::constant_materializer_ranges`]). Empty means bare
    /// constants stay with the target's pre-RA materialization hook.
    constant_materializer_ranges: Vec<ImmRange>,
    /// Floating-point widths target instructions can materialize from integer bits.
    float_constant_materializer_widths: HashSet<u32>,
    /// The target's own data layout, applying where the IR declares none.
    default_layout: Option<crate::attributes::AttributeValue>,
    /// Semantic invariants the program e-graph is saturated with before covering.
    theory: Theory,
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
    /// Where each region-carrying operation's destruction reads its tests and its
    /// counter, filled as the blocks holding them commit.
    region_values: HashMap<(OpId, AuxSlot), AuxEmit>,
    /// Function roots already solved, so a re-visit does not rebuild the graph.
    solved: HashSet<OpId>,
}

/// Prove, for every rule carrying [`Rule::guarded_semantics`], that relaxing the
/// guarded behavior to its pure [`Rule::pattern`] is sound: the pure op equals the
/// guarded behavior wherever the IR op is defined. An unprovable rule is reported
/// as [`PassError::InvalidRuleSet`] naming the rule and the failed obligation.
///
/// Pass construction runs this only under [`verify_axioms`]; each backend's test
/// suite calls it directly over its full generated ruleset, so the obligation stays
/// enforced per commit without re-proving on every compile.
pub fn prove_guarded_relaxations(rules: &[Rule]) -> Result<(), PassError> {
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
    /// `D(pattern) => guarded_semantics == pattern` does not hold — checked only
    /// under [`verify_axioms`].
    pub fn try_new(rules: Vec<Rule>) -> Result<Self, PassError> {
        if verify_axioms() {
            prove_guarded_relaxations(&rules)?;
        }
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

        let theory = discover_rewrites();
        let specificity = compiled_patterns
            .iter()
            .map(|pattern| pattern.specificity)
            .collect();
        let mut value_patterns_by_op: HashMap<u64, Vec<usize>> = HashMap::new();
        let mut value_patterns_anywhere = Vec::new();
        for (index, compiled) in compiled_patterns.iter().enumerate() {
            if rules[compiled.rule_index].kind != RuleKind::Value {
                continue;
            }
            match &compiled.nodes[compiled.root()] {
                PatternNode::Template(node) if !compiled.is_copy() => value_patterns_by_op
                    .entry(node.op_key())
                    .or_default()
                    .push(index),
                _ => value_patterns_anywhere.push(index),
            }
        }
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

        Self {
            rules,
            compiled_patterns,
            specificity,
            value_patterns_by_op,
            value_patterns_anywhere,
            constant_materializer_ranges,
            float_constant_materializer_widths,
            default_layout: None,
            theory,
            branch_emitters: None,
            cost_model: Box::new(DefaultIselCostModel),
            op_lowerings: vec![],
            call_lowering: None,
            plans: HashMap::new(),
            emitted_blocks: HashSet::new(),
            emitted_values: HashMap::new(),
            region_values: HashMap::new(),
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
    pub fn with_theory(mut self, theory: Theory) -> Self {
        self.theory = theory;
        self
    }

    /// Install the target's own data layout, which the pass reads where the IR
    /// declares none (see [`DataLayout::for_op_with_default`](crate::DataLayout)).
    pub fn with_data_layout(mut self, spec: Option<crate::attributes::AttributeValue>) -> Self {
        self.default_layout = spec;
        self
    }

    /// Install additional semantic invariants.
    pub fn with_axioms(mut self, file: &str) -> Self {
        for form in axioms::axiom_forms(file) {
            let axiom = axioms::parse_axiom(&form)
                .unwrap_or_else(|e| panic!("invalid axiom `{form}`: {e}"));
            self.theory.push(axiom);
        }
        if self.theory.materializes_constants() && !self.constant_materializer_ranges.is_empty() {
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
    /// when the pass first visits the function op — a region's entry fact reads
    /// its condition's *defining op*, which an enclosing block's commit would
    /// replace by the time the guarded region solves.
    fn solve_function(
        &mut self,
        context: &Context,
        op: &OperationRef,
        analyses: &AnalysisManager,
    ) -> bool {
        let root = op.op().id;
        if !self.solved.insert(root) {
            return false;
        }
        let dom = analyses.get::<DominatorTree>(context, root);
        let def_use = analyses.get::<DefUse>(context, root);

        let mut fs = self.build_function_selection(context, op, &def_use);
        // A fact-free block sees exactly the base graph, so every value pattern's
        // e-match is block-independent: search once here and reuse for all such
        // blocks (fact-bearing blocks re-search under their scope).
        let mut matches = self.base_value_matches(&fs, context);
        let mut visited = HashSet::new();
        if let Some(root) = dom.root() {
            self.solve_dominator_subtree(context, &mut fs, &dom, root, &mut matches, &mut visited);
        }
        // Unreachable blocks are absent from the dominator tree. A region's
        // blocks are absent for the same reason — the tree orders the function's
        // own blocks, and nothing yet orders sibling arms.
        for block_id in function_blocks(context, op, true) {
            let block = context.get_block(block_id);
            if block.is_empty() || visited.contains(&block_id) {
                continue;
            }
            let plan = self.solve_block(context, &block, &fs, &dom, &mut matches);
            self.plans.insert(block_id, plan);
        }
        telemetry::report(
            &op.op()
                .clone()
                .as_interface::<dyn tir::Symbol>()
                .map_or_else(|| format!("{root:?}"), |symbol| symbol.symbol_name()),
            visited.len(),
            fs.egraph.num_classes(),
        );
        // The regions were solved here, so the operations carrying them must not
        // solve graphs of their own when the walk reaches them.
        for block_id in function_blocks(context, op, true) {
            for op_id in context.get_block(block_id).op_ids() {
                if !context.get_op(op_id).regions().is_empty() {
                    self.solved.insert(op_id);
                }
            }
        }
        true
    }

    fn solve_dominator_subtree(
        &mut self,
        context: &Context,
        fs: &mut FunctionSelection,
        dom: &DominatorTree,
        node: NodeId,
        matches: &mut Matches,
        visited: &mut HashSet<BlockId>,
    ) {
        let Some(block_id) = dom.block(node) else {
            return;
        };
        visited.insert(block_id);

        // A block is covered under whatever its entry proves: for a region's
        // entry block, the assumption the region-carrying operation enters that
        // region under.
        let own_fact = fs.region_facts.get(&block_id).copied();
        if let Some((condition, holds)) = own_fact {
            fs.egraph.push_context();
            if let Some(expr) = fs.prepared.get(&condition) {
                assert_fact(context, &mut fs.egraph, expr, holds);
            }
            fs.egraph.rebuild();
            self.open_scope_matches(context, fs, matches);
        }

        let block = context.get_block(block_id);
        if !block.is_empty() {
            let plan = self.solve_block(context, &block, fs, dom, matches);
            self.plans.insert(block_id, plan);
        }

        let children: Vec<_> = dom.children(node).collect();
        for child in children {
            self.solve_dominator_subtree(context, fs, dom, child, matches, visited);
        }
        if own_fact.is_some() {
            fs.egraph.pop_context();
            matches.close_scope();
        }
    }

    /// Search every value pattern over the base graph once, honoring the same
    /// legality a fact-free block's solve applies (boundary constraints, and
    /// interior nodes restricted to pure or function-wide op-root classes). A
    /// block narrows this superset to the classes its cover reaches.
    fn base_value_matches(&self, fs: &FunctionSelection, context: &Context) -> Matches {
        let mut found = Vec::new();
        for (pattern_index, compiled) in self.compiled_patterns.iter().enumerate() {
            if self.rules[compiled.rule_index].kind != RuleKind::Value {
                continue;
            }
            let roots = compiled.roots(&fs.egraph);
            self.search_pattern(fs, context, pattern_index, roots, &mut found);
        }
        Matches::base(fs.egraph.class_count(), found)
    }

    /// The value matches rooted at one class, in the order the cover reads them:
    /// ascending pattern index, then production order. Only the patterns whose
    /// root can bind at the class are searched, including those rooted on the
    /// constant an open assumption proved it to be, which adds no row of its own.
    fn value_matches_at(
        &self,
        fs: &FunctionSelection,
        context: &Context,
        class: Id,
    ) -> Vec<(usize, IselMatch)> {
        let mut indices = self.value_patterns_anywhere.clone();
        let assumed = fs.egraph.const_of(class).into_iter();
        for node in fs.egraph.nodes(class).chain(assumed) {
            indices.extend(
                self.value_patterns_by_op
                    .get(&node.op_key())
                    .into_iter()
                    .flatten(),
            );
        }
        indices.sort_unstable();
        indices.dedup();
        let mut found = Vec::new();
        for pattern_index in indices {
            self.search_pattern(fs, context, pattern_index, [class], &mut found);
        }
        found
    }

    fn search_pattern(
        &self,
        fs: &FunctionSelection,
        context: &Context,
        pattern_index: usize,
        roots: impl IntoIterator<Item = Id>,
        found: &mut Vec<(usize, IselMatch)>,
    ) {
        let compiled = &self.compiled_patterns[pattern_index];
        let pattern_root = Id::from_raw(compiled.root() as u32);
        let matched = compiled.search_roots_with_legality(
            &fs.egraph,
            context,
            roots,
            fs.pointer_width,
            &|node, class| value_match_allowed(fs, context, compiled, pattern_root, node, class),
        );
        found.extend(matched.into_iter().map(|mut m| {
            m.root = fs.egraph.find(m.root);
            (pattern_index, m)
        }));
    }

    /// Saturate the assumption just pushed and open a match frame over what it
    /// changed. Once per scope, not once per block: the engine's log is a single
    /// consumable stream, so a second saturation under the same assumption would
    /// find the assertion already drained, and the frame's changed set would stop
    /// being a fixed point for the blocks still to be solved under it.
    fn open_scope_matches(
        &self,
        context: &Context,
        fs: &mut FunctionSelection,
        matches: &mut Matches,
    ) {
        rewrites::saturate(context, &mut fs.egraph, &self.theory, Default::default());
        let changed = fs.egraph.innermost_dirty();
        telemetry::record_scope(changed.len());
        matches.open_scope(changed);
    }

    /// Lower every block of the function into one shared, base-saturated e-graph
    /// and compute the canonical side tables (see [`FunctionSelection`]).
    fn build_function_selection(
        &self,
        context: &Context,
        op: &OperationRef,
        def_use: &DefUse,
    ) -> FunctionSelection {
        // Function-wide value/op layout: with a single `value_to_def` a cross-block
        // operand expands to its defining expression naturally (no remat special
        // case), and a block argument / entry input stays an `Input` leaf.
        let mut value_to_def = HashMap::new();
        let mut op_block = HashMap::new();
        let mut op_position = HashMap::new();
        let block_ids = function_blocks(context, op, false);
        for &block_id in &function_blocks(context, op, true) {
            for (position, op_id) in context.get_block(block_id).op_ids().into_iter().enumerate() {
                op_block.insert(op_id, block_id);
                op_position.insert(op_id, position);
                for result in context.get_op(op_id).results() {
                    value_to_def.insert(result, op_id);
                }
            }
        }

        // Pointer width is a data layout fact, so it comes from the layout in
        // scope at this function, over the target's own — the same layout the
        // view's probes carry, since a detached probe resolves none itself.
        let layout = crate::DataLayout::for_op_with_default(
            context,
            op.op().id,
            self.default_layout.as_ref(),
        );
        let pointer_width = layout.as_ref().and_then(crate::DataLayout::pointer_size);

        // Lower every block's ops through one builder so its `value_to_class`
        // memoization unifies classes across blocks (cross-block CSE). Class ids
        // are resolved through `find` afterwards because saturation may merge them.
        let mut egraph = SemEGraph::new();
        let mut prepared: HashMap<ValueId, ConditionExpr> = HashMap::new();
        let mut region_facts: HashMap<BlockId, (ValueId, bool)> = HashMap::new();
        let mut region_control = builder::RegionControl::default();
        let (value_to_class, mut roots_by_op, mut constant_candidates) = {
            let mut builder =
                SemDagBuilder::new(context, &value_to_def, &mut egraph, pointer_width);
            let seeds = builder.build_blocks(&block_ids, &self.float_constant_materializer_widths);

            // The structured operations' own control: the tests a destruction
            // branches on and the counter recurrences it advances, seeded before
            // saturation so the cover selects them like any other class.
            for &block_id in &function_blocks(context, op, true) {
                for op_id in context.get_block(block_id).op_ids() {
                    let inner = context.get_op(op_id);
                    if inner.regions().is_empty() {
                        continue;
                    }
                    builder.build_region_control(&inner, block_id, &mut region_control);
                    for (region, condition, holds) in region_entry_facts(&inner) {
                        let Some(entry) = context.get_region(region).iter(context.clone()).next()
                        else {
                            continue;
                        };
                        region_facts.insert(entry.id(), (condition, holds));
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            prepared.entry(condition)
                        {
                            slot.insert(ConditionExpr {
                                condition: builder.build_from_value(condition),
                                compare: builder.build_defining_compare(condition),
                            });
                        }
                    }
                }
            }

            (
                builder.value_to_class,
                seeds.roots_by_op,
                seeds.constant_candidates,
            )
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

        rewrites::saturate(context, &mut egraph, &self.theory, Default::default());

        crate::memstats::egraph_census("isel", &egraph);

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
        //
        // A `!state` value names memory, not a register. It is interned like any
        // other operand — a memory term is a term over the chain it reads — but
        // no tile ever materializes it, so it is not one of the values a class
        // can be bound to or remapped onto.
        let state_ty = crate::builtin::StateType::new(context);
        let mut class_values: HashMap<Id, Vec<ValueId>> = HashMap::new();
        for (&value, &class) in &value_to_class {
            if context.get_value(value).ty() == state_ty {
                continue;
            }
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
        // Every block a region holds counts here too: its arguments are written by
        // the edges entering it, so they hold their value only where it has run,
        // and a use inside a region is a use.
        let mut arg_block: HashMap<ValueId, BlockId> = HashMap::new();
        let mut operand_uses: HashMap<ValueId, usize> = HashMap::new();
        for &block_id in &function_blocks(context, op, true) {
            for argument in context.get_block(block_id).arguments() {
                arg_block.insert(argument.id(), block_id);
            }
            for op_id in context.get_block(block_id).op_ids() {
                for operand in context.get_op(op_id).operands() {
                    *operand_uses.entry(operand).or_insert(0) += 1;
                }
            }
        }
        // Where a value is read from inside a region hanging off its own block:
        // the operation carrying that region. The read happens before that
        // operation finishes, so what spells the value has to precede it.
        let mut region_use: HashMap<ValueId, OpId> = HashMap::new();
        for &block_id in &function_blocks(context, op, true) {
            for op_id in context.get_block(block_id).op_ids() {
                for operand in context.get_op(op_id).operands() {
                    let Some(def_block) = value_to_def
                        .get(&operand)
                        .map(|def| op_block[def])
                        .or_else(|| arg_block.get(&operand).copied())
                    else {
                        continue;
                    };
                    if def_block == block_id {
                        continue;
                    }
                    let Some(carrier) = enclosing_carrier(context, block_id, def_block) else {
                        continue;
                    };
                    let earlier = region_use
                        .get(&operand)
                        .is_none_or(|held| op_position[&carrier] < op_position[held]);
                    if earlier {
                        region_use.insert(operand, carrier);
                    }
                }
            }
        }

        let mut shared_classes = HashSet::new();
        for (&op, &root) in &roots_by_op {
            if context
                .get_op(op)
                .results()
                .iter()
                .any(|r| operand_uses.get(r).copied().unwrap_or(0) > 1)
            {
                shared_classes.insert(egraph.find(root));
            }
        }

        let needs_register = |result: ValueId, class: Id, def_block: BlockId| {
            let unselected_use = def_use.users_of(result.number()).iter().any(|user| {
                if roots_by_op.contains_key(user) {
                    return false;
                }
                // A destruction's branch recomputes the test it reads.
                !region_control.test_conditions.contains(&(*user, result))
            });
            let cross_block = def_use
                .users_of(result.number())
                .iter()
                .any(|user| op_block.get(user).copied() != Some(def_block));
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
            for result in context.get_op(op_id).results() {
                if needs_register(result, class, def_block) {
                    demand.insert((chase(&egraph, class), def_block));
                }
            }
        }
        for &(op_id, class) in &constant_candidates {
            let def_block = op_block[&op_id];
            for result in context.get_op(op_id).results() {
                if needs_register(result, class, def_block) {
                    demand.insert((chase(&egraph, class), def_block));
                }
            }
        }
        for aux in region_control.aux.values_mut() {
            for (.., class) in aux.iter_mut() {
                *class = egraph.find(*class);
            }
        }
        FunctionSelection {
            egraph,
            pointer_width,
            ops_by_root,
            op_root,
            class_values,
            op_position,
            value_to_def,
            value_block,
            arg_block,
            region_use,
            shared_classes,
            demand,
            prepared,
            region_facts,
            region_aux: region_control.aux,
        }
    }

    /// Commit every block of the function and then destruct what carries regions:
    /// the whole function is emitted from its own visit, because a region's blocks
    /// become blocks of the function and neither the walk nor a per-block commit
    /// can own that.
    fn commit_function(
        &mut self,
        context: &Context,
        op: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        for block_id in function_blocks(context, op, true) {
            let block = context.get_block(block_id);
            self.commit_block_solution(context, &block, rewriter)?;
        }
        let Some(emitters) = self.branch_emitters.as_ref() else {
            return Ok(());
        };
        let Some(&region) = op.op().regions().first() else {
            return Ok(());
        };
        destruct::Destructor::new(
            context,
            emitters,
            &self.emitted_values,
            &self.region_values,
            &self.rules,
            region,
        )
        .run(rewriter)
    }

    fn commit_block_solution(
        &mut self,
        context: &Context,
        block: &BlockHandle,
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
        // The place each surviving operation holds, and — as each tile is
        // emitted — the place it inherits: its root operation's, clamped so the
        // plan's own order is never broken, since that is the order the values
        // the tiles pass each other admit. A tile rooted at no operation of this
        // block is a pure value and takes the head.
        let position: HashMap<OpId, usize> = block_arc
            .op_ids()
            .into_iter()
            .enumerate()
            .map(|(position, op)| (op, position))
            .collect();
        let mut emitted: Vec<(OpId, usize)> = Vec::with_capacity(plan.schedule.len());
        let mut cursor = 0;
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
                state: scheduled.state,
            };
            let rule = &self.rules[scheduled.rule_index];
            cursor = cursor.max(
                scheduled
                    .source_op
                    .and_then(|op| position.get(&op).copied())
                    .unwrap_or(0),
            );
            if let Some(prelude) = rule.prelude_emit {
                let op = prelude(context, &request, &m)?;
                block_arc.append(op.id());
                emitted.push((op.id(), cursor));
            }
            let op = (rule.emit_fn)(context, &request, &m)?;
            block_arc.append(op.id());
            emitted.push((op.id(), cursor));
            // The tile's results are born register-class-typed; the mid-end
            // values they stand for are replaced by them.
            for (&old, new) in scheduled
                .results
                .iter()
                .zip(context.get_op(op.id()).results().iter())
            {
                self.emitted_values.insert(old, *new);
                context.replace_value_uses(old, *new);
            }
        }
        for (old, new) in &plan.value_remaps {
            let new = self.emitted_values.get(new).copied().unwrap_or(*new);
            if *old != new {
                self.emitted_values.insert(*old, new);
                context.replace_value_uses(*old, new);
            }
        }

        for (op, slot, emit) in &mut plan.aux {
            match emit {
                AuxEmit::Branch(GuardBranch::Fused { m, .. }) => {
                    m.remap_values(&self.emitted_values)
                }
                AuxEmit::Branch(GuardBranch::Nonzero { condition }) => {
                    if let Some(replacement) = self.emitted_values.get(condition) {
                        *condition = *replacement;
                    }
                }
                AuxEmit::Value(value) => {
                    if let Some(replacement) = self.emitted_values.get(value) {
                        *value = *replacement;
                    }
                }
                AuxEmit::Decided(_) => {}
            }
            self.region_values.insert((*op, *slot), emit.clone());
        }

        // The chains the emitted instructions took over. One tile answers a
        // whole group of accesses, so the group's other members publish no
        // state of their own.
        let claimed: HashSet<ValueId> = plan
            .schedule
            .iter()
            .filter_map(|scheduled| scheduled.state?.published)
            .collect();

        // The cover replaces a whole group of operations at once, and region
        // destruction still reads the values it consumed — a region terminator
        // forwards them across an edge whether or not a tile re-produced them.
        // The ops go; the values they defined stay readable.
        for op in plan.erase_ops.into_iter().rev() {
            let instance = context.get_op(op);
            // A read the cover answered from another access leaves memory where
            // that one left it: its readers take the state it observed. Only a
            // read is ever answered this way — a write's term is a state
            // nothing before it names, so no other access can stand for it.
            if let (Some(published), Some(observed)) = (
                trailing_state_result(context, &instance),
                trailing_state_operand(context, &instance),
            ) && !claimed.contains(&published)
                && instance
                    .clone()
                    .as_interface::<dyn tir::MemoryWrite>()
                    .is_none()
            {
                context.replace_value_uses(published, observed);
            }
            let op = OperationRef::new(instance, Some(block_arc.clone()), None);
            rewriter.erase_op_keeping_results(&op)?;
        }

        // Order is derived. What the block holds now is the survivors and the
        // tiles; every dependence between them is an edge, so any topological
        // order of the graph is a legal block. The one handed in is the order
        // selection meant: the tiles as the cover ordered them, each at the
        // place its root operation held, and the survivors between them. With no
        // scheduler yet, that is the order that comes back.
        let tiles: HashSet<OpId> = emitted.iter().map(|&(op, _)| op).collect();
        let survivors: Vec<OpId> = block_arc
            .op_ids()
            .into_iter()
            .filter(|op| !tiles.contains(op))
            .collect();
        let mut reference = Vec::with_capacity(survivors.len() + emitted.len());
        let mut next = 0;
        for (op, at) in emitted {
            while next < survivors.len() && position[&survivors[next]] < at {
                reference.push(survivors[next]);
                next += 1;
            }
            reference.push(op);
        }
        reference.extend(survivors[next..].iter().copied());
        let graph = crate::backend::Dependences::of_ops(
            context,
            &reference,
            &crate::backend::RegAssignment::default(),
        );
        let order = graph.linearize().ok_or_else(|| {
            PassError::InvalidRuleSet(format!(
                "cyclic instruction dependences in {block:?}",
                block = block.id()
            ))
        })?;
        block_arc.set_ops(order);

        Ok(())
    }

    /// Solve `block` against the (already scoped) shared graph, restricting
    /// matching and the cover to what `block` computes.
    fn solve_block(
        &self,
        context: &Context,
        block: &BlockHandle,
        fs: &FunctionSelection,
        dom: &DominatorTree,
        value_matches: &mut Matches,
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

        let guard_classes: HashSet<Id> = fs.aux_classes(block_id).collect();

        let (matches, covered) = self.collect_block_matches(
            context,
            fs,
            &op_refs,
            &block_op_by_root,
            &guard_classes,
            value_matches,
        );

        // Search the branch rules once for the whole block, indexed by condition
        // class, so each guard just looks up its hits.
        let guard_branch_hits = if guard_classes.is_empty() {
            HashMap::new()
        } else {
            self.guard_branch_hits(context, fs, &guard_classes)
        };

        // A destruction branches on its tests: fuse each into a branch rule where
        // one matches, and otherwise demand its register for the target's
        // branch-if-nonzero (which needs the condition materialized). A counter's
        // advance is a value, not a branch, and is always demanded — so it names
        // no value of the IR (the seeder mints it) and yet this block is bound to
        // materialize it. A branch may therefore read the register its tile
        // defines, bound once the cover has chosen that tile.
        let deferred_classes: HashSet<Id> = fs
            .region_aux
            .get(&block_id)
            .into_iter()
            .flatten()
            .filter(|(_, slot, _)| *slot == AuxSlot::Advance)
            .map(|&(.., class)| fs.egraph.find(class))
            .collect();
        let mut mm_overlay: HashSet<Id> = HashSet::new();
        let mut aux_branches: Vec<(OpId, AuxSlot, Option<AuxEmit>)> = Vec::new();
        let mut aux_deferred: HashMap<(OpId, AuxSlot), Vec<(u32, Id)>> = HashMap::new();
        for &(op, slot, class) in fs.region_aux.get(&block_id).into_iter().flatten() {
            let class = fs.egraph.find(class);
            // The scope this block solves under may already decide the test — an
            // enclosing region's entry fact proves a re-tested condition equal to its
            // truth. Then no branch is selected and nothing is demanded: the
            // destruction takes the edge the decision picks.
            if slot != AuxSlot::Advance
                && let Some(known) = class_int_binding(&fs.egraph, class)
            {
                aux_branches.push((op, slot, Some(AuxEmit::Decided(!known.is_zero()))));
                continue;
            }
            let fused = (slot != AuxSlot::Advance)
                .then(|| {
                    let candidates = guard_branch_hits
                        .get(&class)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    self.best_guard_branch(
                        context,
                        fs,
                        dom,
                        (block_id, op, block_id),
                        candidates,
                        &deferred_classes,
                    )
                })
                .flatten();
            match fused {
                Some(guard) => {
                    for boundary in guard.boundaries {
                        mm_overlay.insert(fs.chase_low_extract(boundary));
                    }
                    if !guard.deferred.is_empty() {
                        aux_deferred.insert((op, slot), guard.deferred);
                    }
                    aux_branches.push((
                        op,
                        slot,
                        Some(AuxEmit::Branch(GuardBranch::Fused {
                            rule_index: guard.rule_index,
                            m: guard.m,
                        })),
                    ));
                }
                None => {
                    mm_overlay.insert(fs.chase_low_extract(class));
                    aux_branches.push((op, slot, None));
                }
            }
        }
        let demanded: HashSet<Id> = covered
            .iter()
            .copied()
            .filter(|class| {
                fs.demanded_at(*class, block_id, &mm_overlay)
                    || (block_roots.contains(class) && !node::class_is_pure(&fs.egraph, *class))
            })
            .collect();
        let available = |class| {
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
        if let Some(message) = completeness_error(&fs.egraph, &demanded, &matches, &available) {
            return Err(message);
        }

        let cover = build_eclass_cover(
            &fs.egraph,
            &covered,
            &cover::ClassPolicies {
                demanded: &|class| demanded.contains(&fs.egraph.find(class)),
                available: &available,
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
        for (node, choice) in cover.choices.iter().enumerate() {
            if let PbqpIselAlternative::Tile { match_id } = choice {
                root_match.insert(cover.classes[node], *match_id);
            }
        }
        let required_available: HashSet<Id> = root_match
            .values()
            .flat_map(|match_id| &matches[*match_id].bindings.pattern_nodes)
            .filter(|binding| binding.is_boundary && binding.demand == BoundaryDemand::Register)
            .map(|binding| fs.egraph.find(binding.class))
            .filter(|class| !root_match.contains_key(class))
            .collect();
        let tiles = order_tiles(&fs.egraph, &matches, &root_match, |class| {
            block_op_by_root
                .get(&class)
                .and_then(|op| op_ids.iter().position(|candidate| candidate == op))
        })
        .ok_or_else(|| format!("cyclic instruction cover for {block_id:?}"))?;

        // The state ports of every access this block holds, by the class the
        // access is rooted at: what a tile covering it must read and publish.
        // Block order visits the earliest op of a class first, as `source_op` does.
        let mut state_by_class: HashMap<Id, StatePorts> = HashMap::new();
        for &op_id in &op_ids {
            let Some(&root) = fs.op_root.get(&op_id) else {
                continue;
            };
            let op = context.get_op(op_id);
            let Some(observed) = trailing_state_operand(context, &op) else {
                continue;
            };
            state_by_class
                .entry(fs.egraph.find(root))
                .or_insert(StatePorts {
                    observed,
                    published: trailing_state_result(context, &op),
                });
        }

        let state_ty = tir::builtin::StateType::new(context);
        let mut destinations = HashMap::new();
        let mut tile_results = HashMap::new();
        for &(class, _) in &tiles {
            let source_op = block_op_by_root.get(&class).copied();
            // A state result is a port, not a destination: the emitted
            // instruction takes it over as its own, so it is not one of the
            // registers a tile defines.
            let mut results: Vec<ValueId> = source_op
                .map(|op| context.get_op(op).results().to_vec())
                .unwrap_or_default()
                .into_iter()
                .filter(|value| context.get_value(*value).ty() != state_ty)
                .collect();
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
                // A tile covering an access — at its root, or fused into it —
                // carries that access's chain: the instruction reads the memory
                // the IR said it does and publishes the state the IR named.
                let state = matches[match_id]
                    .bindings
                    .pattern_nodes
                    .iter()
                    .filter(|binding| !binding.is_boundary && !binding.is_state)
                    .find_map(|binding| state_by_class.get(&fs.egraph.find(binding.class)))
                    .copied();
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
                    state,
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
        // block-local values must still resolve to the available register. The
        // register is asked for where the values are read — before the region
        // reading them, where one does, so a name published by that very region
        // cannot be the answer.
        for &class in &demanded {
            if destinations.contains_key(&class) {
                continue;
            }
            let ask = fs.region_ask(context, block_id, class).unwrap_or(consumer);
            if let Some(destination) = fs
                .resolve_binding(dom, context, class, block_id, ask, false)
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
                        .results()
                        .iter()
                        .copied()
                        .map(|value| (value, source)),
                );
            }
        }
        let erase_ops = op_ids
            .into_iter()
            .filter(|op| fs.op_root.contains_key(op))
            .filter(|op| {
                fs.op_root.get(op).is_none_or(|class| {
                    let class = fs.egraph.find(*class);
                    !required_available.contains(&class)
                })
            })
            .collect();

        let aux_class: HashMap<(OpId, AuxSlot), Id> = fs
            .region_aux
            .get(&block_id)
            .into_iter()
            .flatten()
            .map(|&(op, slot, class)| ((op, slot), fs.chase_low_extract(fs.egraph.find(class))))
            .collect();
        let resolve_class = |class: Id| {
            destinations.get(&class).copied().or_else(|| {
                fs.resolve_binding(dom, context, class, block_id, consumer, true)
                    .value
            })
        };
        let aux = aux_branches
            .into_iter()
            .filter_map(|(op, slot, selected)| {
                let branch = match selected {
                    Some(AuxEmit::Branch(GuardBranch::Fused { rule_index, mut m }))
                        if aux_deferred.contains_key(&(op, slot)) =>
                    {
                        // The tiles are chosen now, so a class the branch reads but
                        // this block only mints has its register.
                        for &(symbol, class) in &aux_deferred[&(op, slot)] {
                            m.bind_value(symbol, resolve_class(fs.chase_low_extract(class))?);
                        }
                        AuxEmit::Branch(GuardBranch::Fused { rule_index, m })
                    }
                    Some(emit) => emit,
                    None => {
                        let value = resolve_class(*aux_class.get(&(op, slot))?)?;
                        match slot {
                            AuxSlot::Advance => AuxEmit::Value(value),
                            _ => AuxEmit::Branch(GuardBranch::Nonzero { condition: value }),
                        }
                    }
                };
                Some((op, slot, branch))
            })
            .collect();

        Ok(BlockPlan {
            schedule,
            erase_ops,
            value_remaps,
            aux,
        })
    }

    /// Every conditional-branch rule match over the block's (scoped) graph,
    /// indexed by condition class, so each guard resolves against its own hits
    /// without re-searching per guard.
    fn guard_branch_hits(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        guard_classes: &HashSet<Id>,
    ) -> HashMap<Id, Vec<(usize, IselMatch)>> {
        let mut hits: HashMap<Id, Vec<(usize, IselMatch)>> = HashMap::new();
        for (pattern_index, compiled) in self.compiled_patterns.iter().enumerate() {
            if !matches!(
                self.rules[compiled.rule_index].kind,
                RuleKind::CondBranch { .. }
            ) {
                continue;
            }
            for m in compiled.search_roots(
                &fs.egraph,
                context,
                guard_classes.iter().copied(),
                fs.pointer_width,
            ) {
                hits.entry(fs.egraph.find(m.root))
                    .or_default()
                    .push((pattern_index, m));
            }
        }
        hits
    }

    /// The best conditional-branch rule among a guard's condition-class hits.
    /// `None` when none matches or an operand is unresolvable at the branching
    /// block.
    ///
    /// `at` is where the branch goes: the block holding it, the operation it
    /// replaces (whose position the operands resolve against), and the block the
    /// taken edge reaches. A destruction that has not minted its blocks yet binds
    /// any block and rebinds the target when it emits. `deferred_classes` are the
    /// classes this block materializes for the destruction itself, which resolve
    /// only once the cover has minted their tiles.
    #[allow(clippy::too_many_arguments)]
    fn best_guard_branch(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        dom: &DominatorTree,
        at: (BlockId, OpId, BlockId),
        candidates: &[(usize, IselMatch)],
        deferred_classes: &HashSet<Id>,
    ) -> Option<FusedGuard> {
        let (block, consumer, taken) = at;
        let mut best: Option<(u64, usize, FusedGuard)> = None;
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
            for &node in &compiled.captures {
                let symbol = compiled.nodes[node as usize]
                    .symbol()
                    .expect("a capture names an operand");
                if compiled.is_state_symbol(symbol) {
                    continue;
                }
                let class = CompiledIselPattern::binding(m, node as usize);
                captures.bind(symbol, fs.egraph.find(class));
            }

            // Every operand must resolve at B. A class carrying an immediate folds
            // it into the encoding (and still records its register form so a
            // register-reading emitter finds it) without pinning materialization;
            // a class with only a register value binds under the dominance rule and
            // joins the materialization set. An unresolvable boundary disqualifies.
            let mut boundary_classes = Vec::new();
            let mut int_bindings = Vec::new();
            let mut value_bindings = Vec::new();
            let mut deferred = Vec::new();
            let mut resolvable = true;
            // A register operand may read a bare constant only when another use
            // already forces that constant to be materialized.
            for (symbol, class) in &captures.entries {
                // Prefer a surviving/available value; bind a same-block pending
                // tile only when none exists — then the overlay demand forces
                // that tile (the class cannot be available, so NotDemanded is
                // never offered) and the bound value is defined.
                let mut binding = fs.resolve_binding(dom, context, *class, block, consumer, false);
                if binding.value.is_none() {
                    binding = fs.resolve_binding(dom, context, *class, block, consumer, true);
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
                        None if deferred_classes.contains(class) => {
                            deferred.push((*symbol, *class));
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
                let m = RuleMatch::new(int_bindings, value_bindings)
                    .with_block_binding(target_symbol, taken);
                best = Some((
                    cost,
                    specificity,
                    FusedGuard {
                        rule_index: compiled.rule_index,
                        m,
                        boundaries: boundary_classes,
                        deferred,
                    },
                ));
            }
        }
        best.map(|(_, _, guard)| guard)
    }

    /// Every value match the cover can reach from what the block computes, and the
    /// classes it reached. Demand-driven: a class is searched only once something
    /// already covered binds it, which is the fixpoint the cover's class set is
    /// anyway — searching the block's whole reachable cone instead generates two
    /// orders of magnitude more matches than the cover has any use for.
    #[allow(clippy::too_many_arguments)]
    fn collect_block_matches(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        op_refs: &HashMap<OpId, OperationRef>,
        block_op_by_root: &HashMap<Id, OpId>,
        guard_classes: &HashSet<Id>,
        value_matches: &mut Matches,
    ) -> (Vec<PbqpIselMatch>, Vec<Id>) {
        let mut covered: HashSet<Id> = block_op_by_root.keys().copied().collect();
        covered.extend(guard_classes.iter().copied());
        let mut work: Vec<Id> = covered.iter().copied().collect();
        work.sort_unstable();
        let mut matches: Vec<PbqpIselMatch> = Vec::new();
        while let Some(class) = work.pop() {
            // Domination groups a match with the others at its own root, so
            // pruning per class is the same verdict as pruning the whole block at
            // once — and it keeps what feeds the closure below down to survivors.
            let mut at_class = self.root_matches(
                context,
                fs,
                op_refs,
                block_op_by_root,
                guard_classes,
                value_matches,
                class,
            );
            prune_dominated_matches(&self.specificity, &mut at_class);
            for matched in &at_class {
                for binding in &matched.bindings.pattern_nodes {
                    if binding.is_state {
                        continue;
                    }
                    let bound = fs.egraph.find(binding.class);
                    if covered.insert(bound) {
                        work.push(bound);
                    }
                }
            }
            matches.append(&mut at_class);
        }
        let mut covered: Vec<Id> = covered.into_iter().collect();
        covered.sort();
        (matches, covered)
    }

    /// The value matches rooted at one class, narrowed to what this block may
    /// select. The index answers from the function-wide search where the open
    /// assumption left the class alone, and from a re-search under the assumption
    /// where it did not.
    #[allow(clippy::too_many_arguments)]
    fn root_matches(
        &self,
        context: &Context,
        fs: &FunctionSelection,
        op_refs: &HashMap<OpId, OperationRef>,
        block_op_by_root: &HashMap<Id, OpId>,
        guard_classes: &HashSet<Id>,
        value_matches: &mut Matches,
        class: Id,
    ) -> Vec<PbqpIselMatch> {
        value_matches.ensure(class, || {
            let found = self.value_matches_at(fs, context, class);
            telemetry::record_research(found.len());
            found
        });
        let at_class: Vec<MatchRef<'_>> = value_matches.at(class).collect();
        telemetry::record_root_matches(at_class.len());

        let mut matches = Vec::new();
        for m in at_class {
            let pattern_index = m.pattern;

            let compiled = &self.compiled_patterns[pattern_index];
            let rule = &self.rules[compiled.rule_index];
            let pattern_root = Id::from_raw(compiled.root() as u32);
            let root = fs.egraph.find(m.root);
            if compiled.is_copy() && fs.has_values(m.bindings[pattern_root.index()]) {
                continue;
            }
            let block_op = block_op_by_root.get(&root).copied();
            let is_guard_class = guard_classes.contains(&root);
            // A match roots an instruction only if it produces a value B
            // computes: an op of B, a guard condition of B, a
            // rewrite-introduced intermediate, or a terminal constant covered
            // by a real target materializer instruction.
            let is_computed = fs.egraph.nodes(root).any(|n| !n.children().is_empty());
            let synthetic = is_computed || compiled.constant_materializer_range().is_some();
            if block_op.is_none() && !is_guard_class && !synthetic {
                continue;
            }

            // Narrow the function-wide legality to B: a non-pure interior class
            // is legal only when its backing op is in B and it is not shared
            // (boundary constraints were already enforced during the search).
            let interior_ok = (0..compiled.nodes.len()).all(|index| {
                let node = Id::from_raw(index as u32);
                if node == pattern_root || compiled.node_meta[node.index()].duplicable {
                    return true;
                }
                let class = fs.egraph.find(m.bindings[node.index()]);
                node::class_is_pure(&fs.egraph, class)
                    || (block_op_by_root.contains_key(&class) && !fs.is_shared(class))
            });
            if !interior_ok {
                continue;
            }

            let mut captures = CaptureBindings::new();
            for &node in &compiled.captures {
                let symbol = compiled.nodes[node as usize]
                    .symbol()
                    .expect("a capture names an operand");
                if compiled.is_state_symbol(symbol) {
                    continue;
                }
                let class = m.bindings[node as usize];
                captures.bind(symbol, fs.egraph.find(class));
            }

            let mut structural_boundaries = HashSet::new();
            let mut value_boundaries = HashSet::new();
            for index in 0..compiled.nodes.len() {
                let PatternNode::Template(node) = &compiled.nodes[index] else {
                    continue;
                };
                for (operand, &child) in node.children.iter().enumerate() {
                    if !compiled.node_meta[child.index()].is_boundary {
                        continue;
                    }
                    if matches!(node.sym(), Some(SymKind::SExt | SymKind::ZExt)) && operand == 1 {
                        structural_boundaries.insert(child);
                    } else {
                        value_boundaries.insert(child);
                    }
                }
            }
            structural_boundaries.retain(|node| !value_boundaries.contains(node));
            let pattern_nodes: Vec<PatternNodeBinding> = (0..compiled.nodes.len())
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
                    let mut class = fs.egraph.find(m.bindings[pattern_node.index()]);
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
                        is_state: meta.is_state,
                        demand,
                        view_offset: meta
                            .register
                            .map_or(0, |requirement| requirement.view_offset()),
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
            // The virtual register a match defines for an IR value leaves
            // selection's control: copies, spills and ABI pinning treat the
            // registers of a file as freely interchangeable, which only holds
            // for views of one bit offset. A rule writing a shifted view (x86
            // `ah`) may therefore only cover a rewrite-introduced class, whose
            // consumers are tiles this cover checks.
            let result_view_offset = rule
                .result_register
                .map_or(0, |requirement| requirement.view_offset());
            if result_view_offset != 0 && fs.has_values(root) {
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
                result_view_offset,
            });
        }
        matches
    }
}

/// Whether the per-scope match index is paying: how much of the graph each
/// assumption changed, how many matches re-searching those classes produced, and
/// how many the cover read in total. Printed as `tir-isel:` lines on stderr under
/// `TIR_TIME_PASSES`, alongside the pass-timing table.
mod telemetry {
    use std::cell::Cell;

    const SCOPED: usize = 0;
    const CHANGED_SUM: usize = 1;
    const CHANGED_MAX: usize = 2;
    const READ: usize = 3;
    const RESEARCHED: usize = 4;
    const COVERED: usize = 5;

    thread_local! {
        static COUNTS: Cell<[usize; 6]> = const { Cell::new([0; 6]) };
    }

    fn bump(update: impl Fn(&mut [usize; 6])) {
        if !crate::pass::timing::enabled() {
            return;
        }
        COUNTS.with(|counts| {
            let mut current = counts.get();
            update(&mut current);
            counts.set(current);
        });
    }

    pub(super) fn record_scope(changed: usize) {
        bump(|counts| {
            counts[SCOPED] += 1;
            counts[CHANGED_SUM] += changed;
            counts[CHANGED_MAX] = counts[CHANGED_MAX].max(changed);
        });
    }

    pub(super) fn record_research(found: usize) {
        bump(|counts| counts[RESEARCHED] += found);
    }

    pub(super) fn record_root_matches(read: usize) {
        bump(|counts| {
            counts[COVERED] += 1;
            counts[READ] += read;
        });
    }

    pub(super) fn report(function: &str, blocks: usize, classes: usize) {
        if !crate::pass::timing::enabled() {
            return;
        }
        tir_relational::report_saturation("isel");
        let c = COUNTS.replace([0; 6]);
        let changed_avg = c[CHANGED_SUM].checked_div(c[SCOPED]).unwrap_or(0);
        eprintln!(
            "tir-isel: fn={function} blocks={blocks} classes={classes} scopes={} \
             changed_avg={changed_avg} changed_max={} covered={} read={} researched={}",
            c[SCOPED], c[CHANGED_MAX], c[COVERED], c[READ], c[RESEARCHED]
        );
    }
}

/// The function's own blocks in region order and, under the region path, every
/// block its operations nest — the structural order the walk reads them in.
fn function_blocks(context: &Context, op: &OperationRef, nested: bool) -> Vec<BlockId> {
    fn walk(context: &Context, regions: &[RegionId], nested: bool, out: &mut Vec<BlockId>) {
        for &region_id in regions {
            for block in context.get_region(region_id).iter(context.clone()) {
                out.push(block.id());
                if nested {
                    for op_id in block.op_ids() {
                        walk(context, &context.get_op(op_id).regions(), true, out);
                    }
                }
            }
        }
    }
    let mut blocks = Vec::new();
    walk(context, &op.op().regions(), nested, &mut blocks);
    blocks
}

/// The operation of `def_block` whose regions hold `from`, following the chain of
/// regions out of it. `None` when `from` is not nested inside `def_block`.
fn enclosing_carrier(context: &Context, from: BlockId, def_block: BlockId) -> Option<OpId> {
    let mut current = from;
    loop {
        let region = context.parent_region(current)?;
        let carrier = context.get_region(region).parent_op()?;
        let parent = context.parent_block(carrier)?;
        if parent == def_block {
            return Some(carrier);
        }
        current = parent;
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
    if !compiled.boundary_ok(&fs.egraph, context, pattern_node, class, fs.pointer_width) {
        return false;
    }
    if pattern_node == pattern_root || compiled.node_meta[pattern_node.index()].duplicable {
        return true;
    }
    let class = fs.egraph.find(class);
    node::class_is_pure(&fs.egraph, class) || (fs.is_op_root(class) && !fs.is_shared(class))
}

/// The assumption each of `op`'s regions runs under, read off the operation's own
/// interfaces: a [`Conditional`]'s guarded arm runs on its decision holding, and a
/// tested loop's body runs on the condition its test region yields — which holds on
/// every iteration, since the condition is spelled over the ports' per-iteration
/// heads. Regions a structured operation states nothing about (a switch case, a
/// loop's own test) carry no fact.
fn region_entry_facts(op: &OpHandle) -> Vec<(RegionId, ValueId, bool)> {
    if let Some(conditional) = op.clone().as_interface::<dyn Conditional>() {
        return conditional.guarded_regions();
    }
    let Some(guard) = op.clone().as_interface::<dyn GuardedLoop>() else {
        return Vec::new();
    };
    let EntryGuard::Region {
        region: test,
        condition,
        ..
    } = guard.entry_guard()
    else {
        return Vec::new();
    };
    op.regions()
        .iter()
        .filter(|&&region| region != test)
        .map(|&region| (region, condition, true))
        .collect()
}

/// Assert one entry fact in the current scope: the condition (and its defining
/// comparison, when there is one) is assumed to equal its known truth value, the
/// complement comparison the opposite, and an `eq`/`ne` guard makes its operands
/// congruent. Facts, not unions into the constant class: the literal's own class
/// and its users stay untouched, so the scope dirties only the condition's users.
fn assert_fact(context: &Context, egraph: &mut SemEGraph, expr: &ConditionExpr, holds: bool) {
    let truth = |holds: bool| {
        template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(1, holds as u64))),
            None,
        )
    };
    egraph.assume_const(expr.condition, truth(holds));
    if let Some((compare, kind, lhs, rhs)) = expr.compare {
        egraph.assume_const(compare, truth(holds));
        if let Some(complement) = complement_comparison(kind) {
            let mut node = template_node(
                complement,
                None,
                Some(tir::builtin::IntegerType::new(context, 1)),
            );
            node.children = vec![lhs, rhs];
            let complement_class = egraph.add(node);
            egraph.assume_const(complement_class, truth(!holds));
        }
        if (kind == SymKind::Eq && holds) || (kind == SymKind::Ne && !holds) {
            assert_equal(egraph, lhs, rhs);
        }
    }
}

/// Assert `lhs ≡ rhs` in the current scope. A side that is a literal becomes a
/// fact on the other side's class rather than a union with the literal's own
/// class: that class is hash-consed function-wide, so merging into it would dirty
/// every user of the literal instead of every user of the compared value.
fn assert_equal(egraph: &mut SemEGraph, lhs: Id, rhs: Id) {
    let literal = |class: Id| {
        egraph
            .nodes(egraph.find(class))
            .find(|node| node.sym() == Some(SymKind::Constant) && node.int().is_some())
            .cloned()
    };
    match (literal(lhs), literal(rhs)) {
        (Some(_), Some(_)) => {}
        (None, Some(node)) => egraph.assume_const(lhs, node),
        (Some(node), None) => egraph.assume_const(rhs, node),
        (None, None) => {
            egraph.union(lhs, rhs);
        }
    }
}

/// The closure of B's op-root and guard-condition classes under the bindings of
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
    ) -> Result<(), PassError> {
        // The function op is visited before any of its blocks' ops: build the
        // shared graph and solve every block up front — a dominating-edge fact
        // reads the guard condition's *defining op*, which a dominator's commit
        // would otherwise have replaced by the time the dominated block solves.
        if !op.op().regions().is_empty() {
            if let Some(lowering) = &mut self.call_lowering {
                lowering.prepare_function(context, op, rewriter)?;
            }
            if self.solve_function(context, op, analyses) {
                self.commit_function(context, op, rewriter)?;
            }
        }

        for lowering in &self.op_lowerings {
            if lowering(context, op, rewriter)? {
                return Ok(());
            }
        }

        if let Some(lowering) = &mut self.call_lowering
            && lowering.lower(context, op, rewriter)?
        {
            return Ok(());
        }

        // Result-less ops still participate: a store must trigger its block's
        // selection even when no value-producing op precedes it.
        let Some(block) = op.block() else {
            return Ok(());
        };

        self.commit_block_solution(context, block, rewriter)?;
        Ok(())
    }
}
