//! What a target tells selection about its instructions: a rule's semantic
//! pattern, its operand and result requirements, and how it is emitted.

use tir::{
    BlockId, Context, OpId, Operation, OperationRef, PassError, Terms, TypeArg, TypeId, ValueId,
    sem::SemGraph,
};
use tir_adt::APInt;
use tir_symbolic::term::ctor;

use super::rules::{EmitSpec, OperandConstraint, emit_with};

pub struct RuleMatch {
    int_bindings: Vec<(u32, APInt)>,
    value_bindings: Vec<(u32, ValueId)>,
    /// Block operands (branch targets), bound by conditional-branch selection.
    block_bindings: Vec<(u32, BlockId)>,
}

impl RuleMatch {
    pub fn new(
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

    pub fn with_block_binding(mut self, symbol: u32, block: BlockId) -> Self {
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
    /// Destination values, in result order. `None` for a result nothing reads.
    pub results: &'a [Option<ValueId>],
    /// The type of the first result, when known.
    pub result_ty: Option<TypeId>,
    /// Resource chains taken over from the covered operation.
    pub states: &'a [StatePorts],
}

/// The dependencies an emitted instruction takes over from the IR access it
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

pub type RuleEmitFn =
    fn(&Context, &EmitRequest, &RuleMatch) -> Result<Box<dyn Operation>, PassError>;

/// An immediate operand's encoding range: the field's bit width, whether the
/// instruction sign-extends it, and the `#[align]`/`#[nonzero]` constraints the
/// operand declares. A constant outside the range must not bind — its encoding
/// would silently truncate to a different value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    defined: Option<u32>,
    view_offset: u32,
}

impl RegisterRequirement {
    pub(super) fn exclusive_float_type(&self, terms: &Terms) -> Result<Option<TypeId>, ()> {
        if !self.capability.float || self.capability.integer {
            return Ok(None);
        }
        let (exponent, mantissa) = match self.capability.width {
            16 => (5, 10),
            32 => (8, 23),
            64 => (11, 52),
            128 => (15, 112),
            _ => return Err(()),
        };
        Ok(Some(terms.float(exponent, mantissa)))
    }

    pub fn low_bits(capability: RegisterCapability) -> Self {
        Self {
            capability,
            whole: false,
            defined: None,
            view_offset: 0,
        }
    }

    pub fn whole(capability: RegisterCapability) -> Self {
        Self {
            capability,
            whole: true,
            defined: None,
            view_offset: 0,
        }
    }

    /// The same whole read of a value defined at `width` bits only: the
    /// rule's immediate field keeps the register's higher bits out of what
    /// the instruction computes.
    pub fn defined_at(mut self, width: u32) -> Self {
        self.defined = Some(width);
        self
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

    /// The width of the register class this operand names, which is the width a
    /// tile defines when the requirement is a result.
    pub fn width(&self) -> u32 {
        self.capability.width
    }

    /// The width the operand's instruction reads its register at, when it reads
    /// it whole. A value the producer defined at any other width does not answer
    /// it: the bits outside what the producer wrote are undefined, and the bits
    /// beyond what this reads are dropped.
    pub fn whole_width(&self) -> Option<u32> {
        self.whole
            .then_some(self.defined.unwrap_or(self.capability.width))
    }

    pub fn accepts(&self, terms: &Terms, ty: &TypeArg) -> bool {
        self.capability.accepts(terms, ty) && self.reads_whole(bits_width(terms, ty))
    }

    pub(super) fn accepts_int(&self, width: u64) -> bool {
        self.capability.holds_int(Some(width)) && self.reads_whole(Some(width))
    }

    fn reads_whole(&self, width: Option<u64>) -> bool {
        !self.whole
            || width.is_none_or(|width| {
                width == u64::from(self.defined.unwrap_or(self.capability.width))
            })
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

    pub fn accepts(&self, terms: &Terms, ty: &TypeArg) -> bool {
        let TypeArg::Type(ty) = *ty else {
            return matches!(ty, TypeArg::Var(_));
        };
        let width = u64::from(self.width);
        match terms.ctor(ty) {
            ctor::INT | ctor::RAW => self.holds_int(terms.nat(ty, 0)),
            ctor::FLOAT => match terms.float_format(ty) {
                Some((exponent, mantissa)) => self.float && 1 + exponent + mantissa == width,
                None => self.float,
            },
            _ => false,
        }
    }

    fn holds_int(&self, bits: Option<u64>) -> bool {
        match bits {
            Some(bits) => self.integer && bits <= u64::from(self.width),
            None => self.integer,
        }
    }
}

pub(super) fn bits_width(terms: &Terms, ty: &TypeArg) -> Option<u64> {
    let TypeArg::Type(ty) = *ty else {
        return None;
    };
    matches!(terms.ctor(ty), ctor::INT | ctor::RAW)
        .then(|| terms.nat(ty, 0))
        .flatten()
}

impl ImmRange {
    pub(super) fn holds(&self, other: &ImmRange) -> bool {
        let wide = self.width >= 64
            || (self.signed == other.signed && other.width <= self.width)
            || (self.signed && !other.signed && other.width < self.width);
        wide && other.align.is_multiple_of(self.align) && (other.nonzero || !self.nonzero)
    }

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

/// The instruction's declared update to hardware floating-point flags.
#[derive(Clone)]
pub enum FpFlags<T> {
    None,
    Clobber,
    Exact(T),
}

pub struct Rule {
    pub name: &'static str,
    /// The semantics of the first value the rule's instructions define.
    pub pattern: SemGraph,
    /// The semantics of each further value the same instructions define, over
    /// the same operands. Each roots an instance of its own class; instances
    /// binding the same operands are one emission, whose cost is paid once.
    pub secondary: Vec<SemGraph>,
    pub base_cost: u32,
    pub kind: RuleKind,
    pub fp_flags: FpFlags<SemGraph>,
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
    /// The full instruction result before pattern generalization. Rule validation
    /// proves that it refines [`Rule::pattern`] on the source's defined domain,
    /// including the permitted NaN results of floating-point arithmetic.
    pub guarded_semantics: Option<SemGraph>,
    /// The emission plan: the machine instructions the rule emits, in the
    /// order they run. Each but the last defines only machine state the next
    /// one reads — the flags a compare sets for its branch, the register a
    /// division's setup extends into. TMDL derives such rules by composing
    /// the instructions' semantics, so the sequence selects as one pattern but
    /// emits every real instruction. The last one defines the rule's results.
    pub emit: Vec<Emitter>,
}

/// One machine instruction of an emission plan.
#[derive(Clone, Copy)]
pub enum Emitter {
    /// Built from the instruction's own table, which says what it reads,
    /// writes and costs.
    Spec(&'static EmitSpec),
    /// A hand-written emitter.
    Custom(RuleEmitFn),
}

impl Emitter {
    /// Emit the instruction, returning it with the value it defines for each
    /// of the request's results.
    pub fn emit(
        &self,
        context: &Context,
        req: &EmitRequest,
        m: &RuleMatch,
    ) -> Result<(OpId, Vec<(usize, ValueId)>), PassError> {
        match self {
            Emitter::Spec(spec) => {
                let (op, defined) = emit_with(context, req, m, spec)?;
                Ok((op.id(), defined))
            }
            Emitter::Custom(emit) => {
                let op = emit(context, req, m)?.id();
                let results = context.get_op(op).results();
                Ok((op, results.iter().copied().enumerate().collect()))
            }
        }
    }

    pub(super) fn accesses_memory(&self) -> bool {
        match self {
            Emitter::Spec(spec) => spec.info.effects.reads || spec.info.effects.writes,
            Emitter::Custom(_) => false,
        }
    }
}

impl Rule {
    pub fn new(name: &'static str, pattern: SemGraph, base_cost: u32, emit_fn: RuleEmitFn) -> Self {
        Self {
            name,
            pattern,
            secondary: Vec::new(),
            base_cost,
            kind: RuleKind::Value,
            fp_flags: FpFlags::None,
            operand_constraints: Vec::new(),
            operand_registers: Vec::new(),
            result_register: None,
            float_constant_width: None,
            operand_imm_ranges: Vec::new(),
            guarded_semantics: None,
            emit: vec![Emitter::Custom(emit_fn)],
        }
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
pub type OpLowering = Box<dyn Fn(&Context, &OperationRef) -> Result<bool, PassError> + Send + Sync>;

/// Validate a function before instruction selection changes its operations.
pub type FunctionCheck = fn(&Context, &OperationRef) -> Result<(), PassError>;
