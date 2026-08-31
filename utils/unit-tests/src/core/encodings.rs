//! Table-driven instruction encoding, patching and decoding.

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::backend::binary::{
    decode_with, encode_with, encoded_width, patch_with, CmpOp, DecodeField, DecodeFieldKind,
    DecodeShape, DecodeSpec, EncodeField, EncodeShape, EncodeSpec, FieldRun, FixupTarget, Guard,
    PatchField,
};
use tir::backend::{
    ControlFlow, InstrInfo, MachineInstruction, RegAssignment, RegClassType, RegPort,
};
use tir::{Context, OpInstance, Operation};

use super::fixtures::r;

fn phys(index: u16) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Physical { class: r(), index })
}

// The instruction the encoder is exercised on: a register slot `rd` it writes,
// a register slot `rs` it reads, and an immediate.
tir::helpers::operation! {
    TestInstOp {
        name: "inst",
        dialect: "test",
        results: R { regs: "*tir::backend::RegClassType" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

static PORTS: [RegPort; 2] = [
    RegPort {
        name: "rd",
        class: Some(r()),
        def: true,
        tied_to: None,
    },
    RegPort {
        name: "rs",
        class: Some(r()),
        def: false,
        tied_to: None,
    },
];

impl MachineInstruction for TestInstOp {
    fn info(&self) -> &'static InstrInfo {
        static INFO: InstrInfo = InstrInfo {
            name: "inst",
            mnemonic: "inst",
            control_flow: ControlFlow::None,
            regs: &PORTS,
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
    width_bytes: (4, 4),
    shapes: &[EncodeShape {
        guard: Guard::True,
        const_word: 55,
        width_bytes: 4,
        fields: &[
            EncodeField {
                attr: "rd",
                int_range: None,
                align_mask: 0,
                nonzero: false,
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 7,
                    width: 5,
                }],
            },
            EncodeField {
                attr: "imm",
                int_range: Some((-524288, 1048576, 1048576)),
                align_mask: 0,
                nonzero: false,
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 12,
                    width: 20,
                }],
            },
        ],
        patch: &[PatchField {
            attr: "imm",
            range: Some((-524288, 524288)),
            dropped_mask: 0,
            runs: &[FieldRun {
                op_lo: 0,
                word_lo: 12,
                width: 20,
            }],
        }],
    }],
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

// `c.addi4spn rd', sp, nzuimm`: the field holds `imm[9:2]`, so the operand is
// a nonzero multiple of four.
const ADDI4SPN: EncodeSpec = EncodeSpec {
    width_bytes: (2, 2),
    shapes: &[EncodeShape {
        guard: Guard::True,
        const_word: 0,
        width_bytes: 2,
        fields: &[
            EncodeField {
                attr: "rd",
                int_range: None,
                align_mask: 0,
                nonzero: false,
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 2,
                    width: 3,
                }],
            },
            EncodeField {
                attr: "imm",
                int_range: Some((-512, 1024, 1024)),
                align_mask: 3,
                nonzero: true,
                runs: &[FieldRun {
                    op_lo: 2,
                    word_lo: 5,
                    width: 8,
                }],
            },
        ],
        patch: &[],
    }],
};

#[test]
fn encode_rejects_values_the_operand_constraints_exclude() {
    let (_context, op) = op_with(vec![("rd", phys(1)), ("imm", AttributeValue::Int(6))]);
    assert!(encode_with(&op, &ADDI4SPN, &RegAssignment::default()).is_none());

    let (_context, op) = op_with(vec![("rd", phys(1)), ("imm", AttributeValue::Int(0))]);
    assert!(encode_with(&op, &ADDI4SPN, &RegAssignment::default()).is_none());

    let (_context, op) = op_with(vec![("rd", phys(1)), ("imm", AttributeValue::Int(8))]);
    let encoded = encode_with(&op, &ADDI4SPN, &RegAssignment::default()).expect("encodes");
    assert_eq!(encoded.bytes, ((1u16 << 2) | (2 << 5)).to_le_bytes());
}

#[test]
fn encode_leaves_symbol_operand_as_fixup() {
    let (_context, op) = op_with(vec![
        ("rd", phys(5)),
        ("imm", AttributeValue::Str("g".into())),
    ]);
    let encoded = encode_with(&op, &LUI, &RegAssignment::default()).unwrap();
    assert_eq!(encoded.bytes, (55u32 | (5 << 7)).to_le_bytes());
    assert_eq!(encoded.fixups.len(), 1);
    assert_eq!(
        encoded.fixups[0].target,
        FixupTarget::Symbol("g".to_string())
    );
    assert_eq!(encoded.fixups[0].patch.attr, "imm");
}

// `mov rd, rs`: an x86-shaped two-shape encoding. Either register index above
// seven needs a REX prefix byte, which the other shape does not spell.
const MOV_RR: EncodeSpec = EncodeSpec {
    width_bytes: (2, 3),
    shapes: &[
        EncodeShape {
            guard: Guard::Or(&[
                Guard::Bit { op: "rd", bit: 3 },
                Guard::Bit { op: "rs", bit: 3 },
            ]),
            const_word: 0x40 | (0x8b << 8) | (0xc0 << 16),
            width_bytes: 3,
            fields: &[
                EncodeField {
                    attr: "rd",
                    int_range: None,
                    align_mask: 0,
                    nonzero: false,
                    runs: &[
                        FieldRun {
                            op_lo: 3,
                            word_lo: 2,
                            width: 1,
                        },
                        FieldRun {
                            op_lo: 0,
                            word_lo: 19,
                            width: 3,
                        },
                    ],
                },
                EncodeField {
                    attr: "rs",
                    int_range: None,
                    align_mask: 0,
                    nonzero: false,
                    runs: &[
                        FieldRun {
                            op_lo: 3,
                            word_lo: 0,
                            width: 1,
                        },
                        FieldRun {
                            op_lo: 0,
                            word_lo: 16,
                            width: 3,
                        },
                    ],
                },
            ],
            patch: &[],
        },
        EncodeShape {
            guard: Guard::True,
            const_word: 0x8b | (0xc0 << 8),
            width_bytes: 2,
            fields: &[
                EncodeField {
                    attr: "rd",
                    int_range: None,
                    align_mask: 0,
                    nonzero: false,
                    runs: &[FieldRun {
                        op_lo: 0,
                        word_lo: 11,
                        width: 3,
                    }],
                },
                EncodeField {
                    attr: "rs",
                    int_range: None,
                    align_mask: 0,
                    nonzero: false,
                    runs: &[FieldRun {
                        op_lo: 0,
                        word_lo: 8,
                        width: 3,
                    }],
                },
            ],
            patch: &[],
        },
    ],
};

#[test]
fn encode_picks_the_shape_a_register_index_selects() {
    let (_context, op) = op_with(vec![("rd", phys(1)), ("rs", phys(2))]);
    let encoded = encode_with(&op, &MOV_RR, &RegAssignment::default()).expect("encodes");
    assert_eq!(encoded.bytes, vec![0x8b, 0xc0 | (1 << 3) | 2]);

    let (_context, op) = op_with(vec![("rd", phys(9)), ("rs", phys(2))]);
    let encoded = encode_with(&op, &MOV_RR, &RegAssignment::default()).expect("encodes");
    assert_eq!(encoded.bytes, vec![0x44, 0x8b, 0xc0 | (1 << 3) | 2]);
}

// `add rd, imm`: the immediate is one byte when it fits signed and is not
// zero, four when it does not — the shape of an x86 group-1 immediate, whose
// guard reads the operand twice.
const SMALL_IMM: Guard = Guard::And(&[
    Guard::SignedFits {
        op: "imm",
        width: 32,
        bits: 8,
    },
    Guard::Cmp {
        op: "imm",
        width: 32,
        cmp_width: 64,
        cmp: CmpOp::Ne,
        value: 0,
    },
]);

const ADD_IMM: EncodeSpec = EncodeSpec {
    width_bytes: (3, 6),
    shapes: &[
        EncodeShape {
            guard: SMALL_IMM,
            const_word: 0x83 | (0xc0 << 8),
            width_bytes: 3,
            fields: &[
                EncodeField {
                    attr: "rd",
                    int_range: None,
                    align_mask: 0,
                    nonzero: false,
                    runs: &[FieldRun {
                        op_lo: 0,
                        word_lo: 8,
                        width: 3,
                    }],
                },
                EncodeField {
                    attr: "imm",
                    int_range: Some((-128, 256, 256)),
                    align_mask: 0,
                    nonzero: false,
                    runs: &[FieldRun {
                        op_lo: 0,
                        word_lo: 16,
                        width: 8,
                    }],
                },
            ],
            patch: &[PatchField {
                attr: "imm",
                range: Some((-128, 128)),
                dropped_mask: 0,
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 16,
                    width: 8,
                }],
            }],
        },
        EncodeShape {
            guard: Guard::Not(&SMALL_IMM),
            const_word: 0x81 | (0xc0 << 8),
            width_bytes: 6,
            fields: &[
                EncodeField {
                    attr: "rd",
                    int_range: None,
                    align_mask: 0,
                    nonzero: false,
                    runs: &[FieldRun {
                        op_lo: 0,
                        word_lo: 8,
                        width: 3,
                    }],
                },
                EncodeField {
                    attr: "imm",
                    int_range: Some((-2147483648, 4294967296, 4294967296)),
                    align_mask: 0,
                    nonzero: false,
                    runs: &[FieldRun {
                        op_lo: 0,
                        word_lo: 16,
                        width: 32,
                    }],
                },
            ],
            patch: &[PatchField {
                attr: "imm",
                range: Some((-2147483648, 2147483648)),
                dropped_mask: 0,
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 16,
                    width: 32,
                }],
            }],
        },
    ],
};

#[test]
fn encode_picks_the_shape_an_immediate_fits() {
    let (_context, op) = op_with(vec![("rd", phys(1)), ("imm", AttributeValue::Int(5))]);
    let encoded = encode_with(&op, &ADD_IMM, &RegAssignment::default()).expect("encodes");
    assert_eq!(encoded.bytes, vec![0x83, 0xc1, 5]);

    let (_context, op) = op_with(vec![("rd", phys(1)), ("imm", AttributeValue::Int(1000))]);
    let encoded = encode_with(&op, &ADD_IMM, &RegAssignment::default()).expect("encodes");
    assert_eq!(encoded.bytes, vec![0x81, 0xc1, 0xe8, 0x03, 0, 0]);
}

#[test]
fn encode_gives_a_fixup_the_widest_shape_and_its_patch() {
    // The narrow shape asks two questions about the immediate: whether it fits
    // and whether it is zero. A symbol answers "does not fit" and leaves the
    // rest unknown, which is enough to reject that shape and take the widest.
    let (_context, op) = op_with(vec![
        ("rd", phys(1)),
        ("imm", AttributeValue::Str("g".into())),
    ]);
    let mut encoded = encode_with(&op, &ADD_IMM, &RegAssignment::default()).expect("encodes");
    assert_eq!(encoded.bytes, vec![0x81, 0xc1, 0, 0, 0, 0]);

    // The patch the fixup carries is the chosen shape's, so the resolved value
    // lands in that shape's immediate field.
    let fixup = &encoded.fixups[0];
    assert!(patch_with(&mut encoded.bytes, 1000, fixup.patch).is_some());
    assert_eq!(encoded.bytes, vec![0x81, 0xc1, 0xe8, 0x03, 0, 0]);
}

#[test]
fn encoded_width_is_the_selected_shapes_width() {
    let (_context, op) = op_with(vec![("rd", phys(1)), ("rs", phys(2))]);
    assert_eq!(encoded_width(&op, &MOV_RR, &RegAssignment::default()), 2);

    let (_context, op) = op_with(vec![("rd", phys(9)), ("rs", phys(2))]);
    assert_eq!(encoded_width(&op, &MOV_RR, &RegAssignment::default()), 3);
}

// `test rd, imm`: the shape is chosen by a test over the immediate's bit
// pattern, which is what an assembler may spell either signed or unsigned. The
// widths are the ones the generator emits for `imm >= 128` over a `bits<8>`
// operand: the operand is eight bits, and a decimal literal makes the
// comparison itself 64-bit.
const TEST_IMM: EncodeSpec = EncodeSpec {
    width_bytes: (2, 2),
    shapes: &[
        EncodeShape {
            guard: Guard::Cmp {
                op: "imm",
                width: 8,
                cmp_width: 64,
                cmp: CmpOp::Ge,
                value: 128,
            },
            const_word: 0x01,
            width_bytes: 2,
            fields: &[EncodeField {
                attr: "imm",
                int_range: Some((-128, 256, 256)),
                align_mask: 0,
                nonzero: false,
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 8,
                    width: 8,
                }],
            }],
            patch: &[],
        },
        EncodeShape {
            guard: Guard::Not(&Guard::Cmp {
                op: "imm",
                width: 8,
                cmp_width: 64,
                cmp: CmpOp::Ge,
                value: 128,
            }),
            const_word: 0x02,
            width_bytes: 2,
            fields: &[],
            patch: &[],
        },
    ],
};

#[test]
fn a_guard_reads_the_operands_bit_pattern_not_its_spelling() {
    // -1 and 255 are one `bits<8>` pattern, so they choose the same shape —
    // the one the pattern, read as the encoding holds it, satisfies.
    let (_context, op) = op_with(vec![("imm", AttributeValue::Int(-1))]);
    let signed = encode_with(&op, &TEST_IMM, &RegAssignment::default()).expect("encodes");
    let (_context, op) = op_with(vec![("imm", AttributeValue::UInt(255))]);
    let unsigned = encode_with(&op, &TEST_IMM, &RegAssignment::default()).expect("encodes");
    assert_eq!(signed.bytes, vec![0x01, 0xff]);
    assert_eq!(signed.bytes, unsigned.bytes);

    let (_context, op) = op_with(vec![("imm", AttributeValue::Int(7))]);
    let small = encode_with(&op, &TEST_IMM, &RegAssignment::default()).expect("encodes");
    assert_eq!(small.bytes, vec![0x02, 0x00]);
}

#[test]
fn a_guard_over_an_unresolved_operand_selects_no_shape() {
    // The symbol's value decides the shape, and it is not known yet: refusing
    // to encode is the only honest answer.
    let (_context, op) = op_with(vec![("imm", AttributeValue::Str("g".into()))]);
    assert!(encode_with(&op, &TEST_IMM, &RegAssignment::default()).is_none());
}

// `beq rs1, rs2, imm`: 99 | 0 << 12 | rs1 << 15 | rs2 << 20, imm scattered.
const BEQ_PATCH: PatchField = PatchField {
    attr: "imm",
    range: Some((-4096, 4096)),
    dropped_mask: 1,
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
    // A buffer too short for the runs would drop the high bits of the value.
    assert!(patch_with(&mut bytes[..2], 16, &BEQ_PATCH).is_none());
}

const BEQ_DECODE: DecodeSpec = DecodeSpec {
    op: ("test", "beq"),
    attrs: &["rs1", "imm"],
    shapes: &[DecodeShape {
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
    }],
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

// `c.mv rd, rs`: a two-byte shape, matched out of the same 32-bit fetch window
// as any wider instruction.
const CMV_DECODE: DecodeSpec = DecodeSpec {
    op: ("test", "cmv"),
    attrs: &["rd", "rs"],
    shapes: &[DecodeShape {
        fixed_mask: 0xf003,
        const_word: 0x8002,
        fields: &[
            DecodeField {
                attr: "rd",
                kind: DecodeFieldKind::Register(r()),
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 7,
                    width: 5,
                }],
            },
            DecodeField {
                attr: "rs",
                kind: DecodeFieldKind::Register(r()),
                runs: &[FieldRun {
                    op_lo: 0,
                    word_lo: 2,
                    width: 5,
                }],
            },
        ],
    }],
};

#[test]
fn decode_matches_a_two_byte_shape_in_the_fetch_window() {
    let context = Context::with_default_dialects();
    // The high half of the word belongs to the next instruction and must not
    // take part in the match.
    let word = 0x8002u32 | (5 << 7) | (2 << 2) | (0xdead << 16);
    let id = decode_with(&context, word, &CMV_DECODE).expect("decodes");
    let op = context.get_op(id);
    assert_eq!(op.name().as_str(), "cmv");
    assert_eq!(op.attr("rd"), Some(phys(5)));
    assert_eq!(op.attr("rs"), Some(phys(2)));
}
