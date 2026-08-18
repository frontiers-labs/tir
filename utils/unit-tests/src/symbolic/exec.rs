use tir_adt::{APFloat, APInt, RawBits};
use tir_graph::{GenericDag, MutDag, NodeId};
use tir_symbolic::lang::{
    execute, execute_with_memory, AtomicRmwOp, MemOrdering, Memory, SymKind, SymPayload, Value,
};

type Graph = GenericDag<SymKind, SymPayload<()>>;

fn sym(g: &mut Graph, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}
fn int_con(g: &mut Graph, v: i64) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new_signed(64, v)));
    node
}
fn inner(g: &mut Graph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}
fn arg(g: &mut Graph, k: u64) -> NodeId {
    let node = g.add_node(SymKind::Arg);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(32, k)));
    node
}

fn iv(v: i64) -> Value {
    Value::Int(APInt::new_signed(32, v))
}
fn fv(v: f64) -> Value {
    Value::Float(APFloat::from_f64(v))
}
fn uv(v: u64) -> Value {
    Value::Int(APInt::new(32, v))
}
fn rb(bytes: &[u8]) -> Value {
    Value::RawBits(RawBits::from_bytes(bytes.to_vec()))
}

fn as_i64(v: Value) -> i64 {
    match v {
        Value::Int(i) => i.to_i64(),
        _ => panic!(),
    }
}
fn as_u64(v: Value) -> u64 {
    match v {
        Value::Int(i) => i.to_u64(),
        _ => panic!(),
    }
}
fn as_f64(v: Value) -> f64 {
    match v {
        Value::Float(f) => f.to_f64(),
        _ => panic!(),
    }
}
fn raw_bytes(v: Value) -> Vec<u8> {
    match v {
        Value::RawBits(b) => b.bytes().to_vec(),
        other => panic!("expected raw bits, got {other:?}"),
    }
}
fn int_lanes(v: Value) -> Vec<i64> {
    match v {
        Value::Iterator(xs) => xs.into_iter().map(as_i64).collect(),
        other => panic!("expected iterator, got {other:?}"),
    }
}

/// Build a graph applying `kind` to one symbol per input, then execute it.
fn exec_op(kind: SymKind, inputs: &[Value]) -> Value {
    let mut g = Graph::new();
    let args: Vec<NodeId> = (0..inputs.len() as u32).map(|i| sym(&mut g, i)).collect();
    inner(&mut g, kind, &args);
    execute(&g, inputs)
}

#[derive(Default)]
struct TestMemory {
    bytes: Vec<u8>,
}

impl Memory for TestMemory {
    type Error = ();

    fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error> {
        let start = address as usize;
        let mut value = 0;
        for (offset, byte) in self.bytes[start..start + size].iter().enumerate() {
            value |= u64::from(*byte) << (offset * 8);
        }
        Ok(value)
    }

    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), Self::Error> {
        let start = address as usize;
        for offset in 0..size {
            self.bytes[start + offset] = ((value >> (offset * 8)) & 0xff) as u8;
        }
        Ok(())
    }
}

#[test]
fn memory_load_and_store_execute_little_endian() {
    let mut g = Graph::new();
    let address = int_con(&mut g, 4);
    let bytes = int_con(&mut g, 4);
    let metadata = int_con(&mut g, 0);
    inner(&mut g, SymKind::LoadMemory, &[address, bytes, metadata]);

    let mut memory = TestMemory { bytes: vec![0; 16] };
    memory.bytes[4..8].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
    let loaded = execute_with_memory(&g, &[], &mut memory).unwrap();
    assert_eq!(as_i64(loaded), 0x1234_5678);

    let mut g = Graph::new();
    let address = int_con(&mut g, 8);
    let bytes = int_con(&mut g, 2);
    let value = int_con(&mut g, 0xbeef);
    let address_space = int_con(&mut g, 0);
    inner(
        &mut g,
        SymKind::StoreMemory,
        &[address, bytes, value, address_space],
    );
    execute_with_memory(&g, &[], &mut memory).unwrap();
    assert_eq!(&memory.bytes[8..10], &[0xef, 0xbe]);
}

// ── Integer and float scalar ops, table-driven ─────────────────────────────

#[test]
fn signed_int_ops_evaluate() {
    let cases: &[(SymKind, &[i64], i64)] = &[
        (SymKind::Add, &[3, 4], 7),
        (SymKind::Sub, &[10, 3], 7),
        (SymKind::Mul, &[6, 7], 42),
        (SymKind::Neg, &[5], -5),
        (SymKind::Neg, &[-3], 3),
        (SymKind::SRem, &[-7, 3], -1),
        (SymKind::And, &[0b1100, 0b1010], 0b1000),
        (SymKind::ShiftLeft, &[1, 3], 8),
        (SymKind::ShiftRightLogic, &[16, 2], 4),
        (SymKind::ShiftRightArithmetic, &[-8, 1], -4),
        (SymKind::Fma, &[3, 4, 5], 17),
        (SymKind::Eq, &[5, 5], 1),
        (SymKind::Eq, &[5, 6], 0),
        (SymKind::If, &[1, 42, 0], 42),
        (SymKind::If, &[0, 42, 99], 99),
    ];
    for &(kind, inputs, expected) in cases {
        let inputs: Vec<Value> = inputs.iter().map(|&v| iv(v)).collect();
        assert_eq!(as_i64(exec_op(kind, &inputs)), expected, "{kind:?}");
    }
}

#[test]
fn unsigned_int_ops_evaluate() {
    let cases: &[(SymKind, &[u64], u64)] = &[
        (SymKind::URem, &[7, 3], 1),
        (SymKind::Not, &[0b1010], 0xFFFF_FFF5),
    ];
    for &(kind, inputs, expected) in cases {
        let inputs: Vec<Value> = inputs.iter().map(|&v| uv(v)).collect();
        assert_eq!(as_u64(exec_op(kind, &inputs)), expected, "{kind:?}");
    }
}

#[test]
fn float_ops_evaluate() {
    let cases: &[(SymKind, &[f64], f64)] = &[
        (SymKind::Add, &[1.5, 2.5], 4.0),
        (SymKind::Div, &[7.0, 2.0], 3.5),
        (SymKind::Sqrt, &[9.0], 3.0),
        (SymKind::Fma, &[2.0, 3.0, 1.0], 7.0),
    ];
    for &(kind, inputs, expected) in cases {
        let inputs: Vec<Value> = inputs.iter().map(|&v| fv(v)).collect();
        assert!(
            (as_f64(exec_op(kind, &inputs)) - expected).abs() < 1e-9,
            "{kind:?}"
        );
    }
}

#[test]
fn float_lt() {
    assert_eq!(as_i64(exec_op(SymKind::Lt, &[fv(1.0), fv(2.0)])), 1);
    assert_eq!(as_i64(exec_op(SymKind::Lt, &[fv(3.0), fv(2.0)])), 0);
}

fn exec_bin(kind: SymKind, a: Value, b: Value) -> Value {
    exec_op(kind, &[a, b])
}

#[test]
fn division_by_zero_follows_smtlib_conventions() {
    assert_eq!(
        as_u64(exec_bin(SymKind::UDiv, uv(7), uv(0))),
        u32::MAX as u64
    );
    assert_eq!(as_u64(exec_bin(SymKind::URem, uv(7), uv(0))), 7);
    assert_eq!(as_i64(exec_bin(SymKind::Div, iv(7), iv(0))), -1);
    assert_eq!(as_i64(exec_bin(SymKind::Div, iv(-7), iv(0))), 1);
    assert_eq!(as_i64(exec_bin(SymKind::SRem, iv(-7), iv(0))), -7);
    assert_eq!(as_i64(exec_bin(SymKind::SRem, iv(7), iv(0))), 7);
}

#[test]
fn signed_division_overflow_wraps() {
    let min = i32::MIN as i64;
    assert_eq!(as_i64(exec_bin(SymKind::Div, iv(min), iv(-1))), min);
    assert_eq!(as_i64(exec_bin(SymKind::SRem, iv(min), iv(-1))), 0);
}

#[test]
fn int_concat_places_first_operand_high() {
    // concat(0xAB @ 8, 0xCD @ 8) -> 0xABCD @ 16.
    let mut g = Graph::new();
    let hi = {
        let n = g.add_node(SymKind::Constant);
        g.set_leaf_data(n, SymPayload::Int(APInt::new(8, 0xAB)));
        n
    };
    let lo = {
        let n = g.add_node(SymKind::Constant);
        g.set_leaf_data(n, SymPayload::Int(APInt::new(8, 0xCD)));
        n
    };
    inner(&mut g, SymKind::Concat, &[hi, lo]);
    assert_eq!(as_u64(execute(&g, &[])), 0xABCD);
}

#[test]
fn extract_above_mul_yields_signed_high_product() {
    // The RISC-V `mulh` semantics expressed the TMDL way:
    // extract(rs1 * rs2, 127, 64) on 64-bit operands.
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let mul = inner(&mut g, SymKind::Mul, &[a, b]);
    let hi = int_con(&mut g, 127);
    let lo = int_con(&mut g, 64);
    inner(&mut g, SymKind::Extract, &[mul, hi, lo]);

    // -3 * 7 = -21: the high half of the signed 128-bit product is -1.
    let inputs = [
        Value::Int(APInt::new(64, (-3i64) as u64)),
        Value::Int(APInt::new(64, 7)),
    ];
    assert_eq!(as_i64(execute(&g, &inputs)), -1);

    // 2^62 * 4 = 2^64: high half is 1.
    let inputs = [
        Value::Int(APInt::new(64, 1u64 << 62)),
        Value::Int(APInt::new(64, 4)),
    ];
    assert_eq!(as_i64(execute(&g, &inputs)), 1);
}

#[test]
fn addw_tree_sign_extends_low_word() {
    // The RV64 `addw` semantics expressed directly in the graph, no extra
    // primitives: sext(extract(rs1 + rs2, 31, 0), 64).
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let add = inner(&mut g, SymKind::Add, &[a, b]);
    let hi = int_con(&mut g, 31);
    let lo = int_con(&mut g, 0);
    let ext = inner(&mut g, SymKind::Extract, &[add, hi, lo]);
    let width = int_con(&mut g, 64);
    inner(&mut g, SymKind::SExt, &[ext, width]);

    // 0x7FFF_FFFF + 1 = 0x8000_0000, whose low word is negative as i32 and
    // sign-extends to -2147483648 in 64 bits.
    let inputs = [
        Value::Int(APInt::new(64, 0x7FFF_FFFF)),
        Value::Int(APInt::new(64, 1)),
    ];
    assert_eq!(as_i64(execute(&g, &inputs)), -2_147_483_648);
}

#[test]
fn int_constant() {
    let mut g = Graph::new();
    int_con(&mut g, 42);
    assert_eq!(as_i64(execute(&g, &[])), 42);
}

#[test]
fn int_shared_node() {
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    inner(&mut g, SymKind::Add, &[a, a]);
    assert_eq!(as_i64(execute(&g, &[iv(5)])), 10);
}

#[test]
fn int_clamp() {
    let mut g = Graph::new();
    let input = sym(&mut g, 0);
    let min = {
        let node = g.add_node(SymKind::Constant);
        g.set_leaf_data(node, SymPayload::Int(APInt::new_signed(32, 3)));
        node
    };
    let max = {
        let node = g.add_node(SymKind::Constant);
        g.set_leaf_data(node, SymPayload::Int(APInt::new_signed(32, 10)));
        node
    };
    inner(&mut g, SymKind::Clamp, &[input, min, max]);
    assert_eq!(as_i64(execute(&g, &[iv(20)])), 10);
}

#[test]
fn float_min_returns_the_smaller_or_non_nan_operand() {
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    inner(&mut g, SymKind::FMin, &[a, b]);
    assert_eq!(as_f64(execute(&g, &[fv(1.5), fv(-2.5)])), -2.5);
    assert_eq!(as_f64(execute(&g, &[fv(f64::NAN), fv(-2.5)])), -2.5);
    assert_eq!(as_f64(execute(&g, &[fv(1.5), fv(f64::NAN)])), 1.5);
}

#[test]
fn float_max_returns_the_larger_or_non_nan_operand() {
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    inner(&mut g, SymKind::FMax, &[a, b]);
    assert_eq!(as_f64(execute(&g, &[fv(1.5), fv(-2.5)])), 1.5);
    assert_eq!(as_f64(execute(&g, &[fv(f64::NAN), fv(-2.5)])), -2.5);
    assert_eq!(as_f64(execute(&g, &[fv(1.5), fv(f64::NAN)])), 1.5);
}

#[test]
fn asfloat_reinterprets_register_bits_as_float() {
    // asfloat(0x3f800000) == 1.0f32; comparisons on the reinterpreted
    // values are IEEE-correct: NaN != NaN, -0.0 == +0.0.
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let fa = inner(&mut g, SymKind::AsFloat, &[a]);
    let fb = inner(&mut g, SymKind::AsFloat, &[b]);
    inner(&mut g, SymKind::Lt, &[fa, fb]);
    let one = APInt::new(32, 0x3f80_0000);
    let two = APInt::new(32, 0x4000_0000);
    assert_eq!(
        as_i64(execute(&g, &[Value::Int(one.clone()), Value::Int(two)])),
        1
    );

    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let fa = inner(&mut g, SymKind::AsFloat, &[a]);
    inner(&mut g, SymKind::Eq, &[fa, fa]);
    let nan = APInt::new(32, 0x7fc0_0000);
    assert_eq!(as_i64(execute(&g, &[Value::Int(nan)])), 0);

    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let fa = inner(&mut g, SymKind::AsFloat, &[a]);
    let fb = inner(&mut g, SymKind::AsFloat, &[b]);
    inner(&mut g, SymKind::Eq, &[fa, fb]);
    let pos_zero = APInt::new(32, 0);
    let neg_zero = APInt::new(32, 0x8000_0000);
    assert_eq!(
        as_i64(execute(&g, &[Value::Int(pos_zero), Value::Int(neg_zero)])),
        1
    );
}

#[test]
fn fcvt_converts_between_float_formats() {
    // fcvt(1.5f32 bits, 11, 52) -> 1.5f64 bits.
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let e = int_con(&mut g, 11);
    let m = int_con(&mut g, 52);
    inner(&mut g, SymKind::FCvt, &[a, e, m]);
    let one_half_f32 = Value::Int(APInt::new(32, 0x3fc0_0000));
    let out = execute(&g, &[one_half_f32]);
    assert_eq!(as_u64(out), 0x3ff8_0000_0000_0000);
}

// ── Iterator nodes ─────────────────────────────────────────────────────────

#[test]
fn split_then_concat_roundtrips_raw_bits() {
    // split a 16-bit raw value 0xBA21 into two bytes, then concat them back.
    let mut g = Graph::new();
    let bits = sym(&mut g, 0);
    let n = int_con(&mut g, 2);
    let split = inner(&mut g, SymKind::Split, &[bits, n]);

    assert_eq!(
        int_lanes(execute(&g, &[rb(&[0x21, 0xBA])])),
        vec![0x21, 0xBA]
    );

    inner(&mut g, SymKind::IterConcat, &[split]);
    assert_eq!(
        raw_bytes(execute(&g, &[rb(&[0x21, 0xBA])])),
        vec![0x21, 0xBA]
    );
}

#[test]
fn split_with_lane_width_takes_low_lanes_and_zero_pads() {
    // split(x, 2, 16): two 16-bit lanes from the low bits. A 3-byte value
    // supplies lane 0 fully and lane 1 zero-padded — a stored value is the
    // low bits of a conceptually wider register. An integer operand (a
    // register-file read) is reinterpreted as its bit pattern.
    let mut g = Graph::new();
    let bits = sym(&mut g, 0);
    let n = int_con(&mut g, 2);
    let w = int_con(&mut g, 16);
    inner(&mut g, SymKind::Split, &[bits, n, w]);

    assert_eq!(
        int_lanes(execute(&g, &[rb(&[0x21, 0xBA, 0x07])])),
        vec![0xBA21, 0x07]
    );
    assert_eq!(
        int_lanes(execute(
            &g,
            &[Value::Int(APInt::new(64, 0x0004_0003_0002_0001))]
        )),
        vec![1, 2]
    );
}

#[test]
fn map_applies_unary_lambda_per_lane() {
    // map(split(0x0201, 2), |x| x + 1) -> [1+1, 2+1] = [2, 3].
    let mut g = Graph::new();
    let bits = sym(&mut g, 0);
    let n = int_con(&mut g, 2);
    let iter = inner(&mut g, SymKind::Split, &[bits, n]);
    let x = arg(&mut g, 0);
    let one = int_con(&mut g, 1);
    let body = inner(&mut g, SymKind::Add, &[x, one]);
    inner(&mut g, SymKind::Map, &[iter, body]);

    assert_eq!(int_lanes(execute(&g, &[rb(&[0x01, 0x02])])), vec![2, 3]);
}

#[test]
fn zip_then_map_lane_wise_add_concats() {
    // concat(map(zip(split(a, 2), split(b, 2)), |x, y| x + y)) for
    // a=[1,2], b=[3,4] -> lanes [4, 6] -> raw bytes [0x04, 0x06].
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let n = int_con(&mut g, 2);
    let split_a = inner(&mut g, SymKind::Split, &[a, n]);
    let split_b = inner(&mut g, SymKind::Split, &[b, n]);
    let zip = inner(&mut g, SymKind::Zip, &[split_a, split_b]);
    let x = arg(&mut g, 0);
    let y = arg(&mut g, 1);
    let body = inner(&mut g, SymKind::Add, &[x, y]);
    let map = inner(&mut g, SymKind::Map, &[zip, body]);
    inner(&mut g, SymKind::IterConcat, &[map]);

    let out = execute(&g, &[rb(&[0x01, 0x02]), rb(&[0x03, 0x04])]);
    assert_eq!(raw_bytes(out), vec![0x04, 0x06]);
}

#[test]
fn iota_produces_lane_indices() {
    // iota(4, 8) -> lanes [0, 1, 2, 3] of 8 bits each; concatenated they
    // form 0x03020100 (lane 0 low).
    let mut g = Graph::new();
    let n = int_con(&mut g, 4);
    let w = int_con(&mut g, 8);
    let iota = inner(&mut g, SymKind::Iota, &[n, w]);
    inner(&mut g, SymKind::IterConcat, &[iota]);

    assert_eq!(raw_bytes(execute(&g, &[])), vec![0x00, 0x01, 0x02, 0x03]);
}

#[test]
fn iota_zipped_with_split_exposes_index_and_lane() {
    // map(zip(iota(2, 8), split(0x0201, 2)), |i, x| i + x)
    //   -> [0 + 1, 1 + 2] = [1, 3].
    let mut g = Graph::new();
    let bits = sym(&mut g, 0);
    let n = int_con(&mut g, 2);
    let w = int_con(&mut g, 8);
    let iota = inner(&mut g, SymKind::Iota, &[n, w]);
    let split = inner(&mut g, SymKind::Split, &[bits, n]);
    let zip = inner(&mut g, SymKind::Zip, &[iota, split]);
    let i = arg(&mut g, 0);
    let x = arg(&mut g, 1);
    let body = inner(&mut g, SymKind::Add, &[i, x]);
    let map = inner(&mut g, SymKind::Map, &[zip, body]);
    inner(&mut g, SymKind::IterConcat, &[map]);

    assert_eq!(raw_bytes(execute(&g, &[rb(&[0x01, 0x02])])), vec![1, 3]);
}

#[test]
fn three_way_zip_binds_ternary_lambda_positionally() {
    // concat(map(zip(a, b, c), |x, y, z| x + y + z)) for
    // a=[1,2], b=[3,4], c=[5,6] -> lanes [9, 12].
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let c_sym = sym(&mut g, 2);
    let n = int_con(&mut g, 2);
    let split_a = inner(&mut g, SymKind::Split, &[a, n]);
    let split_b = inner(&mut g, SymKind::Split, &[b, n]);
    let split_c = inner(&mut g, SymKind::Split, &[c_sym, n]);
    let zip = inner(&mut g, SymKind::Zip, &[split_a, split_b, split_c]);
    let x = arg(&mut g, 0);
    let y = arg(&mut g, 1);
    let z = arg(&mut g, 2);
    let xy = inner(&mut g, SymKind::Add, &[x, y]);
    let body = inner(&mut g, SymKind::Add, &[xy, z]);
    let map = inner(&mut g, SymKind::Map, &[zip, body]);
    inner(&mut g, SymKind::IterConcat, &[map]);

    let out = execute(
        &g,
        &[rb(&[0x01, 0x02]), rb(&[0x03, 0x04]), rb(&[0x05, 0x06])],
    );
    assert_eq!(raw_bytes(out), vec![9, 12]);
}

#[test]
fn masked_select_via_zip_and_if() {
    // The RVV masked-op shape: lanes of new value, old destination, and a
    // 1-bit mask combined as |m, new, old| if m { new } else { old }.
    // new=[10,20], old=[1,2], mask=[1,0] -> [10, 2].
    let mut g = Graph::new();
    let new = sym(&mut g, 0);
    let old = sym(&mut g, 1);
    let mask = sym(&mut g, 2);
    let n = int_con(&mut g, 2);
    let one = int_con(&mut g, 1);
    let new_lanes = inner(&mut g, SymKind::Split, &[new, n]);
    let old_lanes = inner(&mut g, SymKind::Split, &[old, n]);
    let mask_lanes = inner(&mut g, SymKind::Split, &[mask, n, one]);
    let zip = inner(&mut g, SymKind::Zip, &[mask_lanes, new_lanes, old_lanes]);
    let m = arg(&mut g, 0);
    let new_lane = arg(&mut g, 1);
    let old_lane = arg(&mut g, 2);
    let zero = int_con(&mut g, 0);
    let cond = inner(&mut g, SymKind::Ne, &[m, zero]);
    let body = inner(&mut g, SymKind::If, &[cond, new_lane, old_lane]);
    let map = inner(&mut g, SymKind::Map, &[zip, body]);
    inner(&mut g, SymKind::IterConcat, &[map]);

    let out = execute(&g, &[rb(&[10, 20]), rb(&[1, 2]), rb(&[0b01])]);
    assert_eq!(raw_bytes(out), vec![10, 2]);
}

#[test]
fn compare_lanes_concat_into_packed_mask_bits() {
    // The RVV compare shape: concat(map(zip(a, b), |x, y| x == y)) packs
    // 1-bit result lanes. a=[1,2], b=[1,3] -> mask bits [1, 0] -> 0b01.
    let mut g = Graph::new();
    let a = sym(&mut g, 0);
    let b = sym(&mut g, 1);
    let n = int_con(&mut g, 2);
    let split_a = inner(&mut g, SymKind::Split, &[a, n]);
    let split_b = inner(&mut g, SymKind::Split, &[b, n]);
    let zip = inner(&mut g, SymKind::Zip, &[split_a, split_b]);
    let x = arg(&mut g, 0);
    let y = arg(&mut g, 1);
    let body = inner(&mut g, SymKind::Eq, &[x, y]);
    let map = inner(&mut g, SymKind::Map, &[zip, body]);
    inner(&mut g, SymKind::IterConcat, &[map]);

    let out = execute(&g, &[rb(&[1, 2]), rb(&[1, 3])]);
    assert_eq!(raw_bytes(out), vec![0b01]);
}

// ── Atomics ────────────────────────────────────────────────────────────────

/// Memory with single-hart reservation tracking, mirroring the executor's policy.
#[derive(Default)]
struct ResvMemory {
    bytes: Vec<u8>,
    reservation: Option<(u64, usize)>,
    fences: usize,
}

impl Memory for ResvMemory {
    type Error = ();

    fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error> {
        let start = address as usize;
        let mut value = 0;
        for (offset, byte) in self.bytes[start..start + size].iter().enumerate() {
            value |= u64::from(*byte) << (offset * 8);
        }
        Ok(value)
    }

    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), Self::Error> {
        let start = address as usize;
        for offset in 0..size {
            self.bytes[start + offset] = ((value >> (offset * 8)) & 0xff) as u8;
        }
        Ok(())
    }

    fn load_reserved(
        &mut self,
        address: u64,
        size: usize,
        _ord: MemOrdering,
    ) -> Result<u64, Self::Error> {
        self.reservation = Some((address, size));
        self.read_memory(address, size)
    }

    fn store_conditional(
        &mut self,
        address: u64,
        size: usize,
        value: u64,
        _ord: MemOrdering,
    ) -> Result<bool, Self::Error> {
        let ok = self.reservation == Some((address, size));
        self.reservation = None;
        if ok {
            self.write_memory(address, size, value)?;
        }
        Ok(ok)
    }

    fn fence(&mut self, _pred: u32, _succ: u32, _kind: u32) -> Result<(), Self::Error> {
        self.fences += 1;
        Ok(())
    }
}

fn lr(g: &mut Graph, address: i64, bytes: i64) -> NodeId {
    let a = int_con(g, address);
    let b = int_con(g, bytes);
    let ord = int_con(g, 0);
    inner(g, SymKind::LoadReserved, &[a, b, ord])
}

fn sc(g: &mut Graph, address: i64, bytes: i64, value: i64) -> NodeId {
    let a = int_con(g, address);
    let b = int_con(g, bytes);
    let v = int_con(g, value);
    let ord = int_con(g, 0);
    inner(g, SymKind::StoreConditional, &[a, b, v, ord])
}

#[test]
fn lr_then_sc_succeeds_and_writes() {
    let mut mem = ResvMemory {
        bytes: vec![0; 16],
        ..Default::default()
    };

    let mut g = Graph::new();
    lr(&mut g, 4, 4);
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 0);

    let mut g = Graph::new();
    sc(&mut g, 4, 4, 0xdead_beef);
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 1);
    assert_eq!(&mem.bytes[4..8], &[0xef, 0xbe, 0xad, 0xde]);
}

#[test]
fn sc_without_lr_fails_and_leaves_memory() {
    let mut mem = ResvMemory {
        bytes: vec![0; 16],
        ..Default::default()
    };
    let mut g = Graph::new();
    sc(&mut g, 4, 4, 0x1234);
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 0);
    assert_eq!(&mem.bytes[4..8], &[0, 0, 0, 0]);
}

#[test]
fn sc_after_mismatched_lr_fails() {
    let mut mem = ResvMemory {
        bytes: vec![0; 16],
        ..Default::default()
    };
    let mut g = Graph::new();
    lr(&mut g, 4, 4);
    execute_with_memory(&g, &[], &mut mem).unwrap();

    // SC to a different address does not match the reservation.
    let mut g = Graph::new();
    sc(&mut g, 8, 4, 0x1234);
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 0);
}

#[test]
fn default_store_conditional_always_succeeds() {
    // TestMemory has no reservation concept, so the default SC unconditionally writes.
    let mut mem = TestMemory { bytes: vec![0; 16] };
    let mut g = Graph::new();
    sc(&mut g, 4, 4, 0xabcd);
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 1);
    assert_eq!(&mem.bytes[4..8], &[0xcd, 0xab, 0, 0]);
}

#[test]
fn atomic_rmw_returns_old_and_applies_op() {
    let mut mem = TestMemory { bytes: vec![0; 16] };
    mem.bytes[4..8].copy_from_slice(&5i32.to_le_bytes());

    let mut g = Graph::new();
    let op = int_con(&mut g, AtomicRmwOp::Add as i64);
    let a = int_con(&mut g, 4);
    let b = int_con(&mut g, 4);
    let v = int_con(&mut g, 7);
    let ord = int_con(&mut g, 0);
    inner(&mut g, SymKind::AtomicRmw, &[op, a, b, v, ord]);

    // Old value is returned; memory holds old + val.
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 5);
    assert_eq!(i32::from_le_bytes(mem.bytes[4..8].try_into().unwrap()), 12);
}

#[test]
fn fence_is_a_noop_that_records() {
    let mut mem = ResvMemory {
        bytes: vec![0; 16],
        ..Default::default()
    };
    let mut g = Graph::new();
    let pred = int_con(&mut g, 3);
    let succ = int_con(&mut g, 3);
    let kind = int_con(&mut g, 0);
    inner(&mut g, SymKind::Fence, &[pred, succ, kind]);
    assert_eq!(as_u64(execute_with_memory(&g, &[], &mut mem).unwrap()), 0);
    assert_eq!(mem.fences, 1);
}

#[test]
fn atomic_rmw_op_apply_edge_cases() {
    let w = 32u32;
    let neg = |v: i32| APInt::new(w, v as u32 as u64);
    let pos = |v: u32| APInt::new(w, v as u64);

    // Wrap-around add at 32-bit width.
    assert_eq!(AtomicRmwOp::Add.apply(pos(0xffff_ffff), pos(1)).to_u64(), 0);

    // Swap yields the new value; Xor/And/Or are bitwise.
    assert_eq!(AtomicRmwOp::Swap.apply(pos(5), pos(9)).to_u64(), 9);
    assert_eq!(
        AtomicRmwOp::Xor.apply(pos(0b1100), pos(0b1010)).to_u64(),
        0b0110
    );
    assert_eq!(
        AtomicRmwOp::And.apply(pos(0b1100), pos(0b1010)).to_u64(),
        0b1000
    );
    assert_eq!(
        AtomicRmwOp::Or.apply(pos(0b1100), pos(0b1010)).to_u64(),
        0b1110
    );

    // Signed min/max treat a high-bit-set operand as negative. `apply` keeps the
    // chosen operand's bits verbatim, so read the result back as signed.
    let signed = |v: APInt| v.with_signed(true).to_i64();
    assert_eq!(signed(AtomicRmwOp::Min.apply(neg(-1), pos(1))), -1);
    assert_eq!(signed(AtomicRmwOp::Max.apply(neg(-1), pos(1))), 1);
    assert_eq!(signed(AtomicRmwOp::Min.apply(neg(-5), neg(-3))), -5);

    // Unsigned min/max treat the same bits as a large positive number.
    assert_eq!(AtomicRmwOp::MinU.apply(neg(-1), pos(1)).to_u64(), 1);
    assert_eq!(
        AtomicRmwOp::MaxU.apply(neg(-1), pos(1)).to_u64(),
        0xffff_ffff
    );
}

#[test]
fn atomic_rmw_op_from_code_roundtrips() {
    for code in 0..=8u64 {
        let op = AtomicRmwOp::from_code(code).unwrap();
        assert_eq!(op as u64, code);
    }
    assert_eq!(AtomicRmwOp::from_code(9), None);
}

#[test]
fn reduce_folds_to_horizontal_sum() {
    // reduce(split(0x04030201, 4), |acc, x| acc + x) -> 1+2+3+4 = 10.
    let mut g = Graph::new();
    let bits = sym(&mut g, 0);
    let n = int_con(&mut g, 4);
    let iter = inner(&mut g, SymKind::Split, &[bits, n]);
    let acc = arg(&mut g, 0);
    let x = arg(&mut g, 1);
    let body = inner(&mut g, SymKind::Add, &[acc, x]);
    inner(&mut g, SymKind::Reduce, &[iter, body]);

    assert_eq!(as_i64(execute(&g, &[rb(&[0x01, 0x02, 0x03, 0x04])])), 10);
}
