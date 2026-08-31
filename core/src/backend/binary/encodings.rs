//! Table-driven instruction encoders and decoders.
//!
//! TMDL emits one spec per instruction instead of a generated function; these
//! engines interpret the spec. An instruction's encoding is a list of *shapes*:
//! fixed bit maps, each with the guard over the operands that selects it. Bit
//! placement within a shape is described as runs: operand bits
//! `[op_lo, op_lo + width)` map to word bits `[word_lo, word_lo + width)`.
//!
//! A fixed-width ISA has one shape per instruction, guarded by [`Guard::True`].
//! A prefix ISA has several — an x86 `mov` is two or three bytes depending on
//! the register indices — and the guards are what the hand-written
//! encoding-only twins used to be.

use tir::attributes::{AttributeValue, NamedAttribute, RegisterAttr};
use tir::backend::{RegAssignment, reg_slot, slot_register};
use tir::{Context, OpHandle, OpId, OpInstance};

use crate::backend::binary::{EncodedFixup, EncodedInst, FixupTarget};
use crate::backend::regalloc::RegClassId;

/// One contiguous run of an operand's bits placed into the encoded word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldRun {
    pub op_lo: u16,
    pub word_lo: u16,
    pub width: u16,
}

fn run_mask(width: u16) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

/// Encode-side scatter: place `value`'s runs into `word`.
fn scatter(word: &mut u128, value: u128, runs: &[FieldRun]) {
    for run in runs {
        *word |= ((value >> run.op_lo as u32) & run_mask(run.width)) << run.word_lo as u32;
    }
}

/// Decode-side gather: rebuild an operand value from its runs in `word`.
fn gather(word: u32, runs: &[FieldRun]) -> u64 {
    let mut value: u64 = 0;
    for run in runs {
        value |= (((word >> run.word_lo as u32) as u64) & run_mask(run.width) as u64)
            << run.op_lo as u32;
    }
    value
}

/// How a [`Guard::Cmp`] reads its operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    ULt,
    ULe,
    UGt,
    UGe,
}

/// The condition under which an [`EncodeShape`] is the encoding, as a test over
/// the instruction's operands.
///
/// A leaf reads its operand as the `width`-bit pattern the instruction word
/// holds, which is what TMDL decided the shapes over: the two spellings of one
/// pattern (`-1` and `0xFF` for a `bits<8>` operand) pick the same shape.
///
/// An operand with no value yet — an unplaced register, or a symbol or branch
/// target resolved only at layout time — leaves a test over it undecided, and a
/// shape whose guard is undecided is not selected. The exception is the fit
/// tests, which a fixup answers "no": a symbol needs the widest immediate the
/// instruction spells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    True,
    Not(&'static Guard),
    And(&'static [Guard]),
    Or(&'static [Guard]),
    /// Bit `bit` of the operand's encoded value is set.
    Bit {
        op: &'static str,
        bit: u16,
    },
    /// The operand's bits `[lo, hi]` equal `value`.
    SliceEq {
        op: &'static str,
        lo: u16,
        hi: u16,
        value: u128,
    },
    /// The `width`-bit operand is representable in `bits` bits, two's
    /// complement.
    SignedFits {
        op: &'static str,
        width: u16,
        bits: u16,
    },
    /// The `width`-bit operand is representable in `bits` bits, unsigned.
    UnsignedFits {
        op: &'static str,
        width: u16,
        bits: u16,
    },
    /// The `width`-bit operand compares to `value`. Both are read at
    /// `cmp_width` bits, the wider of the two as the condition spelled them,
    /// so the operand zero-extends into the comparison.
    Cmp {
        op: &'static str,
        width: u16,
        cmp_width: u16,
        cmp: CmpOp,
        value: i128,
    },
}

/// What a guard evaluates to. Undecided when it reads an operand that has no
/// value yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    No,
    Yes,
    Undecided,
}

impl Truth {
    fn of(value: bool) -> Truth {
        match value {
            true => Truth::Yes,
            false => Truth::No,
        }
    }

    fn not(self) -> Truth {
        match self {
            Truth::No => Truth::Yes,
            Truth::Yes => Truth::No,
            Truth::Undecided => Truth::Undecided,
        }
    }
}

/// What one operand of the instruction being encoded holds.
enum Operand {
    /// The bits to scatter, and the same value read as a signed integer.
    Known { bits: u128, signed: i128 },
    /// A symbol or branch target: its bits stay zero until layout resolves it.
    Fixup(FixupTarget),
    /// No value at all: an unplaced register, or an absent attribute.
    Missing,
}

/// The operand values of one instruction, resolved once and shared by the
/// guards that select a shape and the fields that spell it.
struct Operands<'a> {
    op: &'a OpHandle,
    assignment: &'a RegAssignment,
    resolved: Vec<(&'static str, Operand)>,
}

impl<'a> Operands<'a> {
    fn new(op: &'a OpHandle, assignment: &'a RegAssignment) -> Self {
        Operands {
            op,
            assignment,
            resolved: Vec::new(),
        }
    }

    /// The operand named `name`: a register slot's allocated index, or the
    /// attribute of that name.
    fn get(&mut self, name: &'static str) -> &Operand {
        if let Some(index) = self.resolved.iter().position(|(held, _)| *held == name) {
            return &self.resolved[index].1;
        }
        let value = match reg_slot(self.op, name) {
            Some(slot) => match slot_register(slot, self.assignment) {
                Some((_, index)) => Operand::Known {
                    bits: u128::from(index),
                    signed: i128::from(index),
                },
                None => Operand::Missing,
            },
            None => match self.op.attr(name) {
                Some(AttributeValue::Int(v)) => Operand::Known {
                    bits: v as u128,
                    signed: i128::from(v),
                },
                Some(AttributeValue::UInt(v)) => Operand::Known {
                    bits: u128::from(v),
                    signed: i128::from(v),
                },
                Some(AttributeValue::Str(s)) => Operand::Fixup(FixupTarget::Symbol(s.to_string())),
                Some(AttributeValue::Block(b)) => Operand::Fixup(FixupTarget::Block(b)),
                _ => Operand::Missing,
            },
        };
        self.resolved.push((name, value));
        &self.resolved.last().expect("just pushed").1
    }

    /// The operand's value as the `width`-bit pattern the encoding holds.
    fn pattern(&mut self, name: &'static str, width: u16) -> Option<u128> {
        match self.get(name) {
            Operand::Known { bits, .. } => Some(bits & run_mask(width)),
            _ => None,
        }
    }

    /// Whether the operand is a fixup, whose value layout has yet to resolve.
    fn is_fixup(&mut self, name: &'static str) -> bool {
        matches!(self.get(name), Operand::Fixup(_))
    }
}

/// What a fit test says about an operand with no value. A fixup does not fit:
/// it takes the widest immediate the instruction spells, and layout checks the
/// resolved value against that field. Anything else is undecided.
fn fixup_fits(operands: &mut Operands, op: &'static str) -> Truth {
    match operands.is_fixup(op) {
        true => Truth::No,
        false => Truth::Undecided,
    }
}

/// A `width`-bit pattern read as a two's-complement integer.
fn signed(pattern: u128, width: u16) -> i128 {
    match width > 0 && width < 128 && pattern >> (width - 1) & 1 == 1 {
        true => pattern as i128 | !(run_mask(width) as i128),
        false => pattern as i128,
    }
}

impl Guard {
    /// Whether `operands` select the shape this guard belongs to.
    fn holds(&self, operands: &mut Operands) -> Truth {
        match self {
            Guard::True => Truth::Yes,
            Guard::Not(guard) => guard.holds(operands).not(),
            Guard::And(guards) => guards.iter().fold(Truth::Yes, |acc, guard| {
                match (acc, guard.holds(operands)) {
                    (Truth::No, _) | (_, Truth::No) => Truth::No,
                    (Truth::Undecided, _) | (_, Truth::Undecided) => Truth::Undecided,
                    _ => Truth::Yes,
                }
            }),
            Guard::Or(guards) => {
                guards
                    .iter()
                    .fold(Truth::No, |acc, guard| match (acc, guard.holds(operands)) {
                        (Truth::Yes, _) | (_, Truth::Yes) => Truth::Yes,
                        (Truth::Undecided, _) | (_, Truth::Undecided) => Truth::Undecided,
                        _ => Truth::No,
                    })
            }
            Guard::Bit { op, bit } => match operands.pattern(op, bit + 1) {
                Some(pattern) => Truth::of(pattern >> u32::from(*bit) & 1 == 1),
                None => Truth::Undecided,
            },
            Guard::SliceEq { op, lo, hi, value } => match operands.pattern(op, hi + 1) {
                Some(pattern) => Truth::of((pattern >> u32::from(*lo)) == *value),
                None => Truth::Undecided,
            },
            Guard::SignedFits { op, width, bits } => match operands.pattern(op, *width) {
                Some(pattern) => {
                    let limit = 1i128 << (bits - 1);
                    Truth::of((-limit..limit).contains(&signed(pattern, *width)))
                }
                None => fixup_fits(operands, op),
            },
            Guard::UnsignedFits { op, width, bits } => match operands.pattern(op, *width) {
                Some(pattern) => Truth::of(pattern < 1u128 << bits),
                None => fixup_fits(operands, op),
            },
            Guard::Cmp {
                op,
                width,
                cmp_width,
                cmp,
                value,
            } => match operands.pattern(op, *width) {
                Some(pattern) => {
                    let rhs = *value as u128 & run_mask(*cmp_width);
                    let at = *cmp_width;
                    Truth::of(match cmp {
                        CmpOp::Eq => pattern == rhs,
                        CmpOp::Ne => pattern != rhs,
                        CmpOp::Lt => signed(pattern, at) < signed(rhs, at),
                        CmpOp::Le => signed(pattern, at) <= signed(rhs, at),
                        CmpOp::Gt => signed(pattern, at) > signed(rhs, at),
                        CmpOp::Ge => signed(pattern, at) >= signed(rhs, at),
                        CmpOp::ULt => pattern < rhs,
                        CmpOp::ULe => pattern <= rhs,
                        CmpOp::UGt => pattern > rhs,
                        CmpOp::UGe => pattern >= rhs,
                    })
                }
                None => Truth::Undecided,
            },
        }
    }
}

/// One operand's contribution to an instruction encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeField {
    pub attr: &'static str,
    /// Immediate fit check for a field narrower than 64 bits: `(min, max)` for
    /// signed spellings, `umax` (exclusive) for unsigned. `None` for full-width
    /// fields and register operands.
    pub int_range: Option<(i64, i64, u64)>,
    /// Low bits the operand's `#[align]` declares zero; a value with any of
    /// them set has no encoding.
    pub align_mask: u128,
    /// Whether the operand declares `#[nonzero]`.
    pub nonzero: bool,
    pub runs: &'static [FieldRun],
}

/// Re-scatter of a resolved fixup value into one immediate field of a shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchField {
    pub attr: &'static str,
    /// Signed fit check for the value; `None` for a full-width field.
    pub range: Option<(i64, i64)>,
    /// Operand bits below the lowest encoded bit are silently dropped by the
    /// scatter (e.g. bit 0 of RISC-V branch offsets); a value with any of them
    /// set cannot be represented.
    pub dropped_mask: u128,
    pub runs: &'static [FieldRun],
}

/// One fixed bit map of an instruction: the fixed bits, the operand fields
/// scattered into them, and the guard that selects this shape over the others.
pub struct EncodeShape {
    pub guard: Guard,
    pub const_word: u128,
    pub width_bytes: u8,
    pub fields: &'static [EncodeField],
    /// How to re-scatter a resolved fixup, per immediate field of this shape.
    pub patch: &'static [PatchField],
}

/// The encoding of one instruction: the shapes its operands choose between,
/// in the order the guards are tried.
pub struct EncodeSpec {
    pub shapes: &'static [EncodeShape],
    /// Encoded size in bytes over all shapes: `(min, max)`.
    pub width_bytes: (u8, u8),
}

/// The shape `op`'s operands select, or `None` when no guard holds.
fn select<'a>(operands: &mut Operands, spec: &'a EncodeSpec) -> Option<&'a EncodeShape> {
    spec.shapes
        .iter()
        .find(|shape| shape.guard.holds(operands) == Truth::Yes)
}

/// Interprets an [`EncodeSpec`]. `None` when no shape's guard holds or an
/// operand cannot be encoded (e.g. a value `assignment` gives no register);
/// symbol/block operands become fixups with their bits left zero.
pub fn encode_with(
    op: &OpHandle,
    spec: &EncodeSpec,
    assignment: &RegAssignment,
) -> Option<EncodedInst> {
    let mut operands = Operands::new(op, assignment);
    let shape = select(&mut operands, spec)?;
    let mut word = shape.const_word;
    let mut fixups = Vec::new();
    for field in shape.fields {
        // Immediates written in assembly may be spelled signed or unsigned
        // (`-1` vs `0xFFF`), so accept either fit within the declared width.
        let (bits, signed) = match operands.get(field.attr) {
            Operand::Known { bits, signed } => (*bits, *signed),
            Operand::Fixup(target) => {
                let patch = shape.patch.iter().find(|p| p.attr == field.attr)?;
                fixups.push(EncodedFixup {
                    target: target.clone(),
                    patch,
                });
                continue;
            }
            Operand::Missing => return None,
        };
        if let Some((min, max, umax)) = field.int_range
            && !(i128::from(min)..i128::from(max)).contains(&signed)
            && bits >= u128::from(umax)
        {
            return None;
        }
        // A value the operand's declared constraints exclude is not encodable:
        // the scatter would drop the bits `#[align]` promises are zero.
        if bits & field.align_mask != 0 || (field.nonzero && bits == 0) {
            return None;
        }
        scatter(&mut word, bits, field.runs);
    }
    Some(EncodedInst {
        bytes: word.to_le_bytes()[..usize::from(shape.width_bytes)].to_vec(),
        fixups,
    })
}

/// How many bytes `op` encodes to: the width of the shape its operands select,
/// or the widest when none does — the same shape a fixup takes.
pub fn encoded_width(op: &OpHandle, spec: &EncodeSpec, assignment: &RegAssignment) -> u8 {
    match spec.shapes {
        [shape] => shape.width_bytes,
        _ => {
            let mut operands = Operands::new(op, assignment);
            select(&mut operands, spec).map_or(spec.width_bytes.1, |shape| shape.width_bytes)
        }
    }
}

/// Interprets a [`PatchField`] over the bytes of the instruction it was taken
/// from. `None` when the value does not fit the field (out of range, misaligned
/// or wider than the instruction).
pub fn patch_with(bytes: &mut [u8], value: i64, field: &PatchField) -> Option<()> {
    if let Some((min, max)) = field.range
        && !(min..max).contains(&value)
    {
        return None;
    }
    if field.dropped_mask != 0 && (value as u128) & field.dropped_mask != 0 {
        return None;
    }
    let width_bits = bytes.len().min(16) * 8;
    if field
        .runs
        .iter()
        .any(|run| usize::from(run.word_lo + run.width) > width_bits)
    {
        return None;
    }
    let mut word: u128 = 0;
    for (i, b) in bytes.iter().enumerate().take(16) {
        word |= (*b as u128) << (8 * i);
    }
    scatter(&mut word, value as u128, field.runs);
    let out = word.to_le_bytes();
    let len = bytes.len().min(16);
    bytes[..len].copy_from_slice(&out[..len]);
    Some(())
}

/// What a decoded field becomes: a physical register of a fixed class, or a
/// raw integer immediate.
#[derive(Clone, Copy)]
pub enum DecodeFieldKind {
    Register(RegClassId),
    Int,
}

/// One operand's reconstruction from the encoded word.
#[derive(Clone, Copy)]
pub struct DecodeField {
    pub attr: &'static str,
    pub kind: DecodeFieldKind,
    pub runs: &'static [FieldRun],
}

/// One fixed bit map of an instruction, read backwards: the bits that identify
/// it and the operands gathered from the rest. The inverse of an
/// [`EncodeShape`]; a shape narrower than the fetch window fixes no bit above
/// its own width, so the bytes that follow it do not take part in the match.
pub struct DecodeShape {
    pub fixed_mask: u32,
    pub const_word: u32,
    pub fields: &'static [DecodeField],
}

/// The decoding of one instruction: its shapes, most specific first. Only
/// emitted for instructions the generator can invert.
pub struct DecodeSpec {
    /// `(dialect, op)` identity of the operation to build.
    pub op: (&'static str, &'static str),
    pub shapes: &'static [DecodeShape],
    /// Every attribute the op declares; decoding fills all of them.
    pub attrs: &'static [&'static str],
}

/// Interprets a [`DecodeSpec`]: matches a shape's fixed bits, rebuilds each
/// operand, and builds the op in `context`.
///
/// How many bytes the instruction occupied is not returned: it is a function of
/// the operands, so a caller walking a byte stream reads it off the built op
/// ([`crate::backend::MachineInstruction::width_bytes`]) — one source for the
/// width, on the encode side.
pub fn decode_with(context: &Context, word: u32, spec: &DecodeSpec) -> Option<OpId> {
    let shape = spec
        .shapes
        .iter()
        .find(|shape| word & shape.fixed_mask == shape.const_word)?;
    let attributes: Vec<NamedAttribute> = shape
        .fields
        .iter()
        .map(|field| {
            let value = gather(word, field.runs);
            let attr = match field.kind {
                DecodeFieldKind::Register(class) => {
                    AttributeValue::Register(RegisterAttr::Physical {
                        class,
                        index: value as u16,
                    })
                }
                DecodeFieldKind::Int => AttributeValue::Int(value as i64),
            };
            context.named_attribute(field.attr, attr)
        })
        .collect();
    for declared in spec.attrs {
        if !attributes
            .iter()
            .any(|a| Some(a.name) == context.sym(declared))
        {
            panic!("Missing required attribute: {declared}");
        }
    }
    let instance = OpInstance::new_dynamic(
        spec.op,
        context.as_context_ref(),
        vec![],
        vec![],
        vec![],
        attributes,
    );
    Some(context.add_operation(instance).id)
}
