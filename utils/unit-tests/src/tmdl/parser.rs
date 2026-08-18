use tir_adt::{APInt, RawBits};
use tir_symbolic::lang::{execute, Value};
use tir_symbolic::sem::{decode_sem_ops, ExtendSemBytes, SemGraph, SemOp, SemPayloadDesc};

use super::support::generate_source;

const HSUM: &str = r#"
isa Test { param XLEN: Integer = 32; }

register_class GPR for [Test] {
    param ENCODING_LEN: Integer = 5;
    param WIDTH: Integer = self.XLEN;
    registers { r0..r31 => {}, }
}

instruction HSum for [Test] {
    param MNEMONIC: String = "hsum";
    operands { rd: GPR, rs1: GPR, }
    asm { "{self.MNEMONIC} {rd}, {rs1}" }
    behavior {
        rd = reduce(split(rs1, 4), |acc, x| acc + x);
    }
}
"#;

/// split a 32-bit raw value into four bytes and horizontally sum them:
/// bytes [1, 2, 3, 4] -> 10.
#[test]
fn functional_pipeline_lowers_and_executes() {
    let generated = generate_source("hsum.tmdl", HSUM, "test");
    let offset = generated.offset_after("impl tir::sem::AsSemExpr for HSumOp");
    let ops = decode_sem_ops(&generated.blob, offset, &generated.kinds);

    let symbol_ids: Vec<u32> = ops
        .iter()
        .filter_map(|op| match op {
            SemOp::Payload(SemPayloadDesc::SymbolId(id)) => Some(*id),
            _ => None,
        })
        .collect();
    let rs1 = symbol_ids[0];
    assert!(
        symbol_ids.iter().all(|id| *id == rs1),
        "only rs1 is read: {symbol_ids:?}"
    );

    let mut graph = SemGraph::<()>::new();
    graph.extend_sem_bytes_with(&generated.kinds, &generated.blob, offset, |_, _, _| {});

    let mut symbols = vec![Value::Int(APInt::new(1, 0)); rs1 as usize + 1];
    symbols[rs1 as usize] = Value::RawBits(RawBits::from_bytes(vec![1, 2, 3, 4]));

    match execute(&graph, &symbols) {
        Value::Int(v) => assert_eq!(v.to_i64(), 10),
        other => panic!("expected scalar result, got {other:?}"),
    }
}
