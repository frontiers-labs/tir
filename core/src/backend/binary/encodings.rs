//! Table-driven instruction encoders and decoders.
//!
//! TMDL emits one spec per instruction instead of a generated function; these
//! engines interpret the spec. Bit placement is described as runs: operand bits
//! `[op_lo, op_lo + width)` map to word bits `[word_lo, word_lo + width)`.

use tir::attributes::{AttributeValue, NamedAttribute, RegisterAttr};
use tir::backend::{RegAssignment, reg_slot, slot_register};
use tir::{Context, OpHandle, OpId, OpInstance};

use crate::backend::binary::{EncodedInst, FixupTarget};
use crate::backend::regalloc::RegClassId;

/// One contiguous run of an operand's bits placed into the encoded word.
#[derive(Debug, Clone, Copy)]
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

/// One operand's contribution to an instruction encoding.
pub struct EncodeField {
    pub attr: &'static str,
    /// Immediate fit check for a field narrower than 64 bits: `(min, max)` for
    /// signed spellings, `umax` (exclusive) for unsigned. `None` for full-width
    /// fields and unconstrained operands.
    pub int_range: Option<(i64, i64, u64)>,
    pub runs: &'static [FieldRun],
    pub register: bool,
}

/// The encoding of one instruction: fixed bits plus per-operand field runs.
pub struct EncodeSpec {
    pub const_word: u128,
    pub width_bytes: usize,
    pub fields: &'static [EncodeField],
}

/// Interprets an [`EncodeSpec`]. `None` when an operand cannot be encoded (e.g.
/// a value `assignment` gives no register); symbol/block operands become fixups
/// with their bits left zero.
pub fn encode_with(
    op: &OpHandle,
    spec: &EncodeSpec,
    assignment: &RegAssignment,
) -> Option<EncodedInst> {
    let mut word = spec.const_word;
    let mut fixups = Vec::new();
    for field in spec.fields {
        if field.register {
            let (_, index) = slot_register(reg_slot(op, field.attr)?, assignment)?;
            scatter(&mut word, index as u128, field.runs);
            continue;
        }
        // Immediates written in assembly may be spelled signed or unsigned
        // (`-1` vs `0xFFF`), so accept either fit within the declared width.
        match op.attr(field.attr)? {
            AttributeValue::Int(v) => {
                if let Some((min, max, _)) = field.int_range
                    && !(min..max).contains(&v)
                {
                    return None;
                }
                scatter(&mut word, v as u128, field.runs);
            }
            AttributeValue::UInt(v) => {
                if let Some((_, _, umax)) = field.int_range
                    && v >= umax
                {
                    return None;
                }
                scatter(&mut word, v as u128, field.runs);
            }
            AttributeValue::Str(s) => fixups.push(FixupTarget::Symbol(s.to_string())),
            AttributeValue::Block(b) => fixups.push(FixupTarget::Block(b)),
            _ => return None,
        }
    }
    Some(EncodedInst {
        bytes: word.to_le_bytes()[..spec.width_bytes].to_vec(),
        fixups,
    })
}

/// Re-scatter of a resolved fixup value into an instruction's immediate field.
pub struct PatchSpec {
    /// Signed fit check for the value; `None` for full-width fields.
    pub range: Option<(i64, i64)>,
    /// Operand bits below the lowest encoded bit are silently dropped by the
    /// scatter (e.g. bit 0 of RISC-V branch offsets); a value with any of them
    /// set cannot be represented.
    pub dropped_mask: u128,
    pub width_bytes: usize,
    pub runs: &'static [FieldRun],
}

/// Interprets a [`PatchSpec`]. `None` when the value does not fit the operand's
/// encoding (out of range or misaligned).
pub fn patch_with(bytes: &mut [u8], value: i64, spec: &PatchSpec) -> Option<()> {
    if let Some((min, max)) = spec.range
        && !(min..max).contains(&value)
    {
        return None;
    }
    if spec.dropped_mask != 0 && (value as u128) & spec.dropped_mask != 0 {
        return None;
    }
    if bytes.len() < spec.width_bytes {
        return None;
    }
    let mut word: u128 = 0;
    for (i, b) in bytes.iter().enumerate().take(spec.width_bytes) {
        word |= (*b as u128) << (8 * i);
    }
    scatter(&mut word, value as u128, spec.runs);
    let out = word.to_le_bytes();
    bytes[..spec.width_bytes].copy_from_slice(&out[..spec.width_bytes]);
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

/// The decoding of one instruction: fixed-bit match plus per-operand field
/// runs. Only emitted for instructions the generator can invert.
pub struct DecodeSpec {
    /// `(dialect, op)` identity of the operation to build.
    pub op: (&'static str, &'static str),
    pub fixed_mask: u32,
    pub const_word: u32,
    pub fields: &'static [DecodeField],
    /// Every attribute the op declares; decoding fills all of them.
    pub attrs: &'static [&'static str],
}

/// Interprets a [`DecodeSpec`]: matches the fixed bits, rebuilds each operand,
/// and builds the op in `context`.
pub fn decode_with(context: &Context, word: u32, spec: &DecodeSpec) -> Option<OpId> {
    if word & spec.fixed_mask != spec.const_word {
        return None;
    }
    let attributes: Vec<NamedAttribute> = spec
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
