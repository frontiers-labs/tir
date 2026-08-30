//! Table-driven instruction encoding, patching and decoding.

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::backend::binary::{
    decode_with, encode_with, patch_with, DecodeField, DecodeFieldKind, DecodeSpec, EncodeField,
    EncodeSpec, FieldRun, FixupTarget, PatchSpec,
};
use tir::backend::{
    ControlFlow, InstrInfo, MachineInstruction, RegAssignment, RegClassType, RegPort,
};
use tir::{Context, OpInstance, Operation};

use super::fixtures::r;

fn phys(index: u16) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Physical { class: r(), index })
}

// The instruction the encoder is exercised on: one register slot `rd` it
// writes, and an immediate.
tir::helpers::operation! {
    TestInstOp {
        name: "inst",
        dialect: "test",
        results: R { regs: "*tir::backend::RegClassType" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

static RD_PORT: [RegPort; 1] = [RegPort {
    name: "rd",
    class: Some(r()),
    def: true,
    tied_to: None,
}];

impl MachineInstruction for TestInstOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = InstrInfo {
            name: "inst",
            mnemonic: "inst",
            control_flow: ControlFlow::None,
            regs: &RD_PORT,
            ..InstrInfo::BASE
        };
        &INFO
    }

    fn instance(&self) -> &tir::OpHandle {
        &self.0
    }
}

/// The op with its `rd` slot naming a physical register directly.
fn op_with(attrs: Vec<(&str, AttributeValue)>) -> (Context, tir::OpHandle) {
    let context = Context::with_default_dialects();
    let handle = build(&context, attrs, Vec::new());
    (context, handle)
}

fn build(
    context: &Context,
    attrs: Vec<(&str, AttributeValue)>,
    results: Vec<tir::ValueId>,
) -> tir::OpHandle {
    TestInstOp::register_interfaces(context);
    let attributes = attrs
        .into_iter()
        .map(|(name, value)| context.named_attribute(name, value))
        .collect();
    let instance = OpInstance::new_dynamic(
        ("test", "inst"),
        context.as_context_ref(),
        vec![],
        results,
        vec![],
        attributes,
    );
    context.add_operation(instance)
}

// `lui rd, imm`: 55 | rd << 7 | (imm & 0xFFFFF) << 12
const LUI: EncodeSpec = EncodeSpec {
    const_word: 55,
    width_bytes: 4,
    fields: &[
        EncodeField {
            attr: "rd",
            int_range: None,
            runs: &[FieldRun {
                op_lo: 0,
                word_lo: 7,
                width: 5,
            }],
            register: true,
        },
        EncodeField {
            attr: "imm",
            int_range: Some((-524288, 1048576, 1048576)),
            runs: &[FieldRun {
                op_lo: 0,
                word_lo: 12,
                width: 20,
            }],
            register: false,
        },
    ],
};

#[test]
fn encode_scatters_register_and_immediate() {
    let (_context, op) = op_with(vec![("rd", phys(5)), ("imm", AttributeValue::Int(0x12345))]);
    let encoded = encode_with(&op, &LUI, &RegAssignment::default()).unwrap();
    assert_eq!(
        encoded.bytes,
        (55u32 | (5 << 7) | (0x12345 << 12)).to_le_bytes()
    );
    assert!(encoded.fixups.is_empty());
}

#[test]
fn encode_rejects_out_of_range_and_unallocated() {
    let (_context, op) = op_with(vec![("rd", phys(5)), ("imm", AttributeValue::Int(1048576))]);
    assert!(encode_with(&op, &LUI, &RegAssignment::default()).is_none());

    // A slot holding a value the assignment does not place has no register to
    // encode.
    let context = Context::with_default_dialects();
    let ty = RegClassType::new(&context, r());
    let unplaced = context.create_value(ty, None).id();
    let op = build(
        &context,
        vec![("imm", AttributeValue::Int(1))],
        vec![unplaced],
    );
    assert!(encode_with(&op, &LUI, &RegAssignment::default()).is_none());

    // Placed, it encodes as that register.
    let mut assignment = RegAssignment::default();
    assignment.insert(unplaced, (r(), 5));
    let encoded = encode_with(&op, &LUI, &assignment).expect("encodes once placed");
    assert_eq!(encoded.bytes, (55u32 | (5 << 7) | (1 << 12)).to_le_bytes());
}

#[test]
fn encode_leaves_symbol_operand_as_fixup() {
    let (_context, op) = op_with(vec![
        ("rd", phys(5)),
        ("imm", AttributeValue::Str("g".into())),
    ]);
    let encoded = encode_with(&op, &LUI, &RegAssignment::default()).unwrap();
    assert_eq!(encoded.bytes, (55u32 | (5 << 7)).to_le_bytes());
    assert_eq!(encoded.fixups, vec![FixupTarget::Symbol("g".to_string())]);
}

// `beq rs1, rs2, imm`: 99 | 0 << 12 | rs1 << 15 | rs2 << 20, imm scattered.
const BEQ_PATCH: PatchSpec = PatchSpec {
    range: Some((-4096, 4096)),
    dropped_mask: 1,
    width_bytes: 4,
    runs: &[
        FieldRun {
            op_lo: 11,
            word_lo: 7,
            width: 1,
        },
        FieldRun {
            op_lo: 1,
            word_lo: 8,
            width: 4,
        },
        FieldRun {
            op_lo: 5,
            word_lo: 25,
            width: 6,
        },
        FieldRun {
            op_lo: 12,
            word_lo: 31,
            width: 1,
        },
    ],
};

/// Reassembles the operand value scattered by `runs` across `word`.
fn gather(word: u32, runs: &[FieldRun]) -> u64 {
    let mut value = 0u64;
    for run in runs {
        let bits = u64::from((word >> run.word_lo) & ((1 << run.width) - 1));
        value |= bits << run.op_lo;
    }
    value
}

#[test]
fn patch_scatters_resolved_value() {
    let mut bytes = 99u32.to_le_bytes();
    assert!(patch_with(&mut bytes, 16, &BEQ_PATCH).is_some());
    let word = u32::from_le_bytes(bytes);
    assert_eq!(word & 28799, 99, "fixed bits untouched");
    assert_eq!(gather(word & !28799, BEQ_PATCH.runs), 16);
}

#[test]
fn patch_rejects_unrepresentable_values() {
    let mut bytes = 99u32.to_le_bytes();
    assert!(patch_with(&mut bytes, 4096, &BEQ_PATCH).is_none());
    assert!(patch_with(&mut bytes, 15, &BEQ_PATCH).is_none());
    assert!(patch_with(&mut bytes[..2], 16, &BEQ_PATCH).is_none());
}

const BEQ_DECODE: DecodeSpec = DecodeSpec {
    op: ("test", "beq"),
    fixed_mask: 28799,
    const_word: 99,
    fields: &[
        DecodeField {
            attr: "rs1",
            kind: DecodeFieldKind::Register(r()),
            runs: &[FieldRun {
                op_lo: 0,
                word_lo: 15,
                width: 5,
            }],
        },
        DecodeField {
            attr: "imm",
            kind: DecodeFieldKind::Int,
            runs: BEQ_PATCH.runs,
        },
    ],
    attrs: &["rs1", "imm"],
};

#[test]
fn decode_matches_and_gathers() {
    let context = Context::with_default_dialects();
    let word = 99u32 | (5 << 15) | (1 << 8); // rs1 = 5, imm bit1 set
    let id = decode_with(&context, word, &BEQ_DECODE).expect("decodes");
    let op = context.get_op(id);
    assert_eq!(op.dialect().as_str(), "test");
    assert_eq!(op.name().as_str(), "beq");
    assert_eq!(op.attr("rs1"), Some(phys(5)));
    assert_eq!(op.attr("imm"), Some(AttributeValue::Int(2)));

    assert!(decode_with(&context, word | 1 << 14, &BEQ_DECODE).is_none());
}
