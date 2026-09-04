use tir_adt::{APFloat, APInt, RawBits};
use tir_graph::{Dag, NodeId};

use crate::lang::{AtomicRmwOp, MemOrdering, SymKind, SymPayload, Value, scalar_op};

/// Memory backend for `LoadMemory`/`StoreMemory` nodes.
pub trait Memory {
    type Error;

    fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error>;
    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), Self::Error>;

    /// Read `size` bytes as raw lanes (for accesses wider than a word, e.g. a
    /// 128-bit vector load). The default composes word-sized reads little-endian.
    fn read_memory_bytes(&mut self, address: u64, size: usize) -> Result<RawBits, Self::Error> {
        let mut bytes = Vec::with_capacity(size);
        let mut offset = 0;
        while offset < size {
            let chunk = (size - offset).min(8);
            let word = self.read_memory(address + offset as u64, chunk)?;
            for i in 0..chunk {
                bytes.push((word >> (i * 8)) as u8);
            }
            offset += chunk;
        }
        Ok(RawBits::from_bytes(bytes))
    }

    /// Write `size` raw byte lanes (e.g. a 128-bit vector store). The default
    /// decomposes into word-sized writes little-endian.
    fn write_memory_bytes(
        &mut self,
        address: u64,
        size: usize,
        value: RawBits,
    ) -> Result<(), Self::Error> {
        let bytes = value.bytes();
        let mut offset = 0;
        while offset < size {
            let chunk = (size - offset).min(8);
            let mut word = 0u64;
            for i in 0..chunk {
                word |= u64::from(bytes.get(offset + i).copied().unwrap_or(0)) << (i * 8);
            }
            self.write_memory(address + offset as u64, chunk, word)?;
            offset += chunk;
        }
        Ok(())
    }

    /// Read `size` bytes and register a reservation covering the access. The
    /// default has no reservation concept and behaves like a plain read.
    fn load_reserved(
        &mut self,
        address: u64,
        size: usize,
        _ord: MemOrdering,
    ) -> Result<u64, Self::Error> {
        self.read_memory(address, size)
    }

    /// Write `value` iff a valid reservation covers the access, returning success.
    /// The default has no reservation concept, so the write always succeeds.
    fn store_conditional(
        &mut self,
        address: u64,
        size: usize,
        value: u64,
        _ord: MemOrdering,
    ) -> Result<bool, Self::Error> {
        self.write_memory(address, size, value)?;
        Ok(true)
    }

    /// Single-copy-atomic read-modify-write; returns the old memory value. The
    /// default reads, applies `op` at `size*8` bits, and writes back.
    fn atomic_rmw(
        &mut self,
        op: AtomicRmwOp,
        address: u64,
        size: usize,
        value: u64,
        _ord: MemOrdering,
    ) -> Result<u64, Self::Error> {
        let old = self.read_memory(address, size)?;
        let width = (size as u32) * 8;
        let result = op.apply(APInt::new(width, old), APInt::new(width, value));
        self.write_memory(address, size, result.to_u64())?;
        Ok(old)
    }

    /// Memory/instruction fence. The default has no ordering state and is a no-op.
    fn fence(&mut self, _pred: u32, _succ: u32, _kind: u32) -> Result<(), Self::Error> {
        Ok(())
    }
}

enum NoMemoryError {}

struct NoMemory;

impl Memory for NoMemory {
    type Error = NoMemoryError;

    fn read_memory(&mut self, _address: u64, _size: usize) -> Result<u64, Self::Error> {
        unimplemented!("memory operations are not supported by this interpreter")
    }

    fn write_memory(
        &mut self,
        _address: u64,
        _size: usize,
        _value: u64,
    ) -> Result<(), Self::Error> {
        unimplemented!("memory operations are not supported by this interpreter")
    }
}

/// Evaluate the expression DAG; `symbols[i]` is the value for `SymbolId(i)`.
pub fn execute<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    symbols: &[Value],
) -> Value {
    match execute_with_memory(graph, symbols, &mut NoMemory) {
        Ok(value) => value,
        Err(err) => match err {},
    }
}

/// Like [`execute`] but routes load/store nodes through `memory`; stores yield a dummy 1-bit value.
pub fn execute_with_memory<V, M: Memory>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    symbols: &[Value],
    memory: &mut M,
) -> Result<Value, M::Error> {
    let root = graph.root().expect("cannot execute empty graph");
    let mut cache = vec![None::<Value>; graph.len()];
    let mut args: Vec<Value> = Vec::new();
    eval_node(graph, root, symbols, &mut cache, &mut args, memory)
}

fn child_val<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    idx: usize,
    cache: &[Option<Value>],
) -> Value {
    let child = graph
        .children(node)
        .nth(idx)
        .expect("child index must be in bounds");
    cache[child.index()]
        .as_ref()
        .expect("child must be evaluated before parent in post-order")
        .clone()
}

macro_rules! as_int {
    ($v:expr, $op:literal) => {
        match $v {
            Value::Int(i) => i,
            Value::Float(_) => panic!("{} requires integer operands", $op),
            Value::Iterator(_) => panic!("{} requires scalar operands", $op),
            Value::RawBits(_) => panic!("{} requires integer operands", $op),
        }
    };
}

macro_rules! as_float {
    ($v:expr, $op:literal) => {
        match $v {
            Value::Float(f) => f,
            Value::Int(_) => panic!("{} requires float operands", $op),
            Value::Iterator(_) => panic!("{} requires scalar operands", $op),
            Value::RawBits(_) => panic!("{} requires float operands", $op),
        }
    };
}

/// Binary arithmetic over int (width-coerced) or float; `$c(0)` selects the type.
macro_rules! arith_op {
    ($c:ident, $int_m:ident, $float_m:ident, $op:literal) => {
        match $c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!($c(1), $op));
                Value::Int(a.$int_m(&b))
            }
            Value::Float(a) => Value::Float(a.$float_m(&as_float!($c(1), $op))),
            Value::Iterator(_) | Value::RawBits(_) => {
                panic!(concat!($op, " requires scalar operands"))
            }
        }
    };
}

/// Signed/float comparison yielding a 1-bit `Int`.
macro_rules! cmp_op {
    ($c:ident, $int_m:ident, $float_m:ident, $op:literal) => {
        Value::Int(APInt::new(
            1,
            match $c(0) {
                Value::Int(a) => {
                    let (a, b) = coerce_ints(a, as_int!($c(1), $op));
                    bool_result(a.$int_m(&b))
                }
                Value::Float(a) => bool_result(a.$float_m(&as_float!($c(1), $op))),
                Value::Iterator(_) | Value::RawBits(_) => {
                    panic!(concat!($op, " requires scalar operands"))
                }
            },
        ))
    };
}

/// Widen `v` to `width` (sign- or zero-extend per its signedness); no-op if already wide enough.
fn widen(v: APInt, width: u32) -> APInt {
    if v.width() >= width {
        v
    } else if v.is_signed() {
        v.sign_extend(width)
    } else {
        v.zero_extend(width)
    }
}

/// Widen the narrower of two operands to a common width; behavior expressions mix
/// wide values with bare narrow literals rather than matching widths exactly.
fn coerce_ints(a: APInt, b: APInt) -> (APInt, APInt) {
    let width = a.width().max(b.width());
    (widen(a, width), widen(b, width))
}

fn scalar_equal(lhs: Value, rhs: Value) -> bool {
    match (lhs, rhs) {
        (Value::Int(lhs), Value::Int(rhs)) => {
            let (lhs, rhs) = coerce_ints(lhs, rhs);
            lhs == rhs
        }
        (Value::Float(lhs), Value::Float(rhs)) => {
            matches!(lhs.compare(&rhs), Some(std::cmp::Ordering::Equal))
        }
        _ => panic!("eq requires matching scalar operands"),
    }
}

/// Evaluate an integer division/remainder kind with SMT-LIB div-by-zero rules,
/// matching the bitblaster: `bvudiv x 0 = ~0`, `bvsdiv x 0` is `-1` for a
/// non-negative dividend and `1` otherwise, and both remainders return the
/// dividend. A nonzero divisor defers to APInt, whose signed ops already wrap
/// `MIN / -1`. `None` for any other kind, or for `Div` over floats (handled by
/// the float path).
fn eval_divrem(kind: SymKind, c: &impl Fn(usize) -> Value) -> Option<Value> {
    let (signed, quotient) = match kind {
        SymKind::Div => (true, true),
        SymKind::UDiv => (false, true),
        SymKind::SRem => (true, false),
        SymKind::URem => (false, false),
        _ => return None,
    };
    let (Value::Int(a), Value::Int(b)) = (c(0), c(1)) else {
        return None;
    };
    let (a, b) = coerce_ints(a, b);
    let width = a.width();
    let result = if b.is_zero() {
        match (signed, quotient) {
            (false, true) => APInt::max_value(width, false),
            (true, true) if a.with_signed(true).is_negative() => APInt::new_signed(width, 1),
            (true, true) => APInt::new_signed(width, -1),
            (_, false) => a,
        }
    } else {
        match (signed, quotient) {
            (true, true) => a.sdiv(&b),
            (false, true) => a.udiv(&b),
            (true, false) => a.srem(&b),
            (false, false) => a.urem(&b),
        }
    };
    Some(Value::Int(result))
}

/// The IEEE binary format of a `width`-bit register value, for the float kinds'
/// bit-reinterpreting integer path. Only binary32/binary64 registers exist.
fn float_format(width: u32, op: &str) -> (u32, u32) {
    match width {
        16 => (5, 10),
        32 => (8, 23),
        64 => (11, 52),
        other => panic!("{op} requires a 16-, 32- or 64-bit operand, got {other} bits"),
    }
}

/// Binary IEEE arithmetic: over `Float` operands directly (constant folding);
/// over `Int` operands the register bits are reinterpreted in the binary format
/// of the operand width and the result is returned as bits of the same width.
fn float_binop(lhs: Value, rhs: Value, f: fn(&APFloat, &APFloat) -> APFloat, op: &str) -> Value {
    match (lhs, rhs) {
        (Value::Float(a), Value::Float(b)) => Value::Float(f(&a, &b)),
        (Value::Int(a), Value::Int(b)) => {
            let width = a.width().max(b.width());
            let (exp, mant) = float_format(width, op);
            let a = APFloat::from_bits(exp, mant, false, a.to_u64() as u128);
            let b = APFloat::from_bits(exp, mant, false, b.to_u64() as u128);
            Value::Int(APInt::new(width, f(&a, &b).to_bits() as u64))
        }
        _ => panic!("{op} requires two float or two integer operands"),
    }
}

/// Evaluate `body` with `binding` pushed as the innermost lambda argument, under a
/// fresh cache so each lane's `Arg` reads its own value rather than a stale cached one.
fn eval_lambda_body<V, M: Memory>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    body: NodeId,
    symbols: &[Value],
    args: &mut Vec<Value>,
    memory: &mut M,
    binding: Value,
) -> Result<Value, M::Error> {
    args.push(binding);
    let mut body_cache = vec![None::<Value>; graph.len()];
    let result = eval_node(graph, body, symbols, &mut body_cache, args, memory);
    args.pop();
    result
}

/// Evaluate a `Map` node: apply `body` to each lane of `iter` via the lambda-argument stack.
fn eval_map<V, M: Memory>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
    args: &mut Vec<Value>,
    memory: &mut M,
) -> Result<Value, M::Error> {
    let children: Vec<NodeId> = graph.children(node).collect();
    let (iter_n, body_n) = (children[0], children[1]);

    let iter = eval_node(graph, iter_n, symbols, cache, args, memory)?;
    let Value::Iterator(elems) = iter else {
        panic!("map requires an iterator operand");
    };

    let mut out = Vec::with_capacity(elems.len());
    for elem in elems {
        out.push(eval_lambda_body(
            graph, body_n, symbols, args, memory, elem,
        )?);
    }
    Ok(Value::Iterator(out))
}

/// Evaluate a `Reduce` node: left-fold `body` over `iter`, `Arg(0)`=acc, `Arg(1)`=lane.
fn eval_reduce<V, M: Memory>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
    args: &mut Vec<Value>,
    memory: &mut M,
) -> Result<Value, M::Error> {
    let children: Vec<NodeId> = graph.children(node).collect();
    let (iter_n, body_n) = (children[0], children[1]);

    let iter = eval_node(graph, iter_n, symbols, cache, args, memory)?;
    let Value::Iterator(elems) = iter else {
        panic!("reduce requires an iterator operand");
    };
    let mut elems = elems.into_iter();
    let mut acc = elems.next().expect("reduce requires a non-empty iterator");
    for elem in elems {
        // Pack acc/lane as a two-element binding read via `Arg(0)`/`Arg(1)`.
        let binding = Value::Iterator(vec![acc, elem]);
        acc = eval_lambda_body(graph, body_n, symbols, args, memory, binding)?;
    }
    Ok(acc)
}

/// Evaluate a `Split` node: cut raw bits into `n` integer lanes, lane 0 from the low bits.
/// Reinterpret a value as raw bits: integers (e.g. a register file entry) are
/// their two's-complement bit pattern.
fn as_raw_bits(value: Value) -> RawBits {
    match value {
        Value::RawBits(bits) => bits,
        Value::Int(i) => RawBits::from_apint(&i),
        Value::Float(f) => RawBits::from_apfloat(&f),
        Value::Iterator(_) => panic!("split requires a raw-bits operand"),
    }
}

fn split_bits(value: Value, n: usize) -> Value {
    let bits = as_raw_bits(value);
    let lanes = bits
        .split(n)
        .into_iter()
        .map(|lane| Value::Int(lane.to_apint()))
        .collect();
    Value::Iterator(lanes)
}

fn split_bits_lanes(value: Value, n: usize, width: usize) -> Value {
    let bits = as_raw_bits(value);
    if !width.is_multiple_of(8) {
        // Sub-byte lanes (e.g. an RVV mask register's 1-bit elements): extract
        // bit ranges directly. Lane values are APInts, so the existing
        // 64-bit lane ceiling applies here too.
        assert!(
            width <= 64,
            "sub-byte lanes wider than 64 bits are unsupported, got {width}"
        );
        let lanes = (0..n)
            .map(|lane| {
                let mut lane_value = 0u64;
                for bit in 0..width {
                    let at = lane * width + bit;
                    let byte = bits.bytes().get(at / 8).copied().unwrap_or(0);
                    lane_value |= (u64::from(byte >> (at % 8)) & 1) << bit;
                }
                Value::Int(APInt::new(width as u32, lane_value))
            })
            .collect();
        return Value::Iterator(lanes);
    }
    let lanes = bits
        .split_lanes(n, width)
        .into_iter()
        .map(|lane| Value::Int(lane.to_apint()))
        .collect();
    Value::Iterator(lanes)
}

/// Evaluate an `IterConcat` node: join lanes into one raw-bits value, lane 0 low. Inverse of `Split`.
fn concat_lanes(value: Value) -> Value {
    let Value::Iterator(lanes) = value else {
        panic!("concat requires an iterator operand");
    };
    // Each lane as (width in bits, little-endian bytes holding at least those bits).
    let lanes: Vec<(usize, Vec<u8>)> = lanes
        .into_iter()
        .map(|lane| match lane {
            Value::Int(i) => (i.width() as usize, i.to_u64().to_le_bytes().to_vec()),
            Value::Float(f) => (
                f.bit_width() as usize,
                RawBits::from_apfloat(&f).bytes().to_vec(),
            ),
            Value::RawBits(b) => (b.width(), b.bytes().to_vec()),
            Value::Iterator(_) => panic!("concat lanes must be scalar"),
        })
        .collect();
    if lanes.iter().all(|(width, _)| width.is_multiple_of(8)) {
        let raw: Vec<RawBits> = lanes
            .into_iter()
            .map(|(width, bytes)| RawBits::from_bytes(bytes[..width / 8].to_vec()))
            .collect();
        return Value::RawBits(RawBits::concat(&raw));
    }
    // Sub-byte lanes (packed mask bits): assemble the value bit by bit.
    let total: usize = lanes.iter().map(|(width, _)| width).sum();
    let mut storage = vec![0u8; total.div_ceil(8)];
    let mut at = 0;
    for (width, bytes) in &lanes {
        for bit in 0..*width {
            if (bytes[bit / 8] >> (bit % 8)) & 1 == 1 {
                storage[at / 8] |= 1 << (at % 8);
            }
            at += 1;
        }
    }
    Value::RawBits(RawBits::from_bytes(storage))
}

fn eval_node<V, M: Memory>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
    args: &mut Vec<Value>,
    memory: &mut M,
) -> Result<Value, M::Error> {
    if let Some(ref v) = cache[node.index()] {
        return Ok(v.clone());
    }

    // Intercept before generic child pre-evaluation: Map/Reduce re-evaluate their
    // body per lane with a fresh `Arg`, so it must not be pre-evaluated here.
    match *graph.get_kind(node) {
        SymKind::Map => {
            let result = eval_map(graph, node, symbols, cache, args, memory)?;
            cache[node.index()] = Some(result.clone());
            return Ok(result);
        }
        SymKind::Reduce => {
            let result = eval_reduce(graph, node, symbols, cache, args, memory)?;
            cache[node.index()] = Some(result.clone());
            return Ok(result);
        }
        _ => {}
    }

    for child_id in graph.children(node) {
        if cache[child_id.index()].is_none() {
            let v = eval_node(graph, child_id, symbols, cache, args, memory)?;
            cache[child_id.index()] = Some(v);
        }
    }

    let c = |idx: usize| child_val(graph, node, idx, cache);

    // Integer division and remainder are total under SMT-LIB div-by-zero rules,
    // matching the bitblaster. An if-guarded behavior (e.g. riscv `div`) evaluates
    // its dead arm eagerly, so a zero divisor must fold rather than trap in the
    // asserting APInt path `scalar_op` would take.
    if let Some(result) = eval_divrem(*graph.get_kind(node), &c) {
        cache[node.index()] = Some(result.clone());
        return Ok(result);
    }

    if let Some(op) = scalar_op(*graph.get_kind(node)) {
        let operands = (0..op.arity)
            .map(|index| match c(index) {
                Value::Int(value) => Some(value),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        if let Some(operands) = operands {
            let result = Value::Int(op.eval_int(&operands));
            cache[node.index()] = Some(result.clone());
            return Ok(result);
        }
    }

    let result = match *graph.get_kind(node) {
        SymKind::Map | SymKind::Reduce => {
            unreachable!("map/reduce handled before child pre-evaluation")
        }
        SymKind::Arg => eval_arg(graph, node, args),
        kind @ (SymKind::Symbol | SymKind::Constant) => eval_leaf(graph, node, kind, symbols),
        kind @ (SymKind::Zip | SymKind::Split | SymKind::IterConcat | SymKind::Iota) => {
            eval_iterator(graph, node, kind, &c)
        }
        kind @ (SymKind::Add | SymKind::Sub | SymKind::Mul | SymKind::Div) => eval_arith(kind, &c),
        kind @ (SymKind::Eq
        | SymKind::Ne
        | SymKind::Lt
        | SymKind::Le
        | SymKind::Gt
        | SymKind::Ge) => eval_compare(kind, &c),
        kind @ (SymKind::FAdd
        | SymKind::FSub
        | SymKind::FMul
        | SymKind::FDiv
        | SymKind::FMin
        | SymKind::FMax
        | SymKind::AsFloat
        | SymKind::FCvt
        | SymKind::SIToFP
        | SymKind::UIToFP
        | SymKind::FPToSI
        | SymKind::FPToUI) => eval_float(kind, &c),
        kind @ (SymKind::If | SymKind::Theta | SymKind::Clamp) => eval_control(kind, &c),
        kind @ (SymKind::Fma
        | SymKind::Sqrt
        | SymKind::Log2Ceil
        | SymKind::Bitcast
        | SymKind::Extract
        | SymKind::ZExt
        | SymKind::SExt) => eval_math(graph, node, cache, kind, &c),
        kind @ (SymKind::LoadMemory | SymKind::StoreMemory) => eval_memory(kind, &c, memory)?,
        kind @ (SymKind::LoadReserved
        | SymKind::StoreConditional
        | SymKind::AtomicRmw
        | SymKind::Fence) => eval_atomic(kind, &c, memory)?,
        _ => unreachable!("operator has no concrete evaluator"),
    };

    cache[node.index()] = Some(result.clone());
    Ok(result)
}

fn eval_arg<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    args: &[Value],
) -> Value {
    let SymPayload::Int(idx) = graph.get_leaf_data(node).unwrap() else {
        panic!("Arg node must have Int payload");
    };
    let idx = idx.to_u64() as usize;
    let binding = args.last().expect("Arg evaluated outside a lambda");
    match binding {
        // Pair binding (Zip lanes or Reduce acc/lane pack): index positionally.
        Value::Iterator(parts) => parts[idx].clone(),
        // Scalar binding: the single argument of a unary lambda.
        scalar => {
            assert!(idx == 0, "scalar lambda argument has only index 0");
            scalar.clone()
        }
    }
}

fn eval_leaf<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    kind: SymKind,
    symbols: &[Value],
) -> Value {
    match (kind, graph.get_leaf_data(node).unwrap()) {
        (SymKind::Symbol, SymPayload::SymbolId(id)) => symbols[*id as usize].clone(),
        (SymKind::Symbol, _) => panic!("Symbol node must have SymbolId payload"),
        (_, SymPayload::Int(v)) => Value::Int(v.clone()),
        (_, SymPayload::Float(v)) => Value::Float(v.clone()),
        _ => panic!("Constant node must have Int or Float payload"),
    }
}

fn eval_iterator<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    kind: SymKind,
    c: &impl Fn(usize) -> Value,
) -> Value {
    match kind {
        SymKind::Zip => {
            let arity = graph.children(node).count();
            let iters: Vec<Vec<Value>> = (0..arity)
                .map(|slot| match c(slot) {
                    Value::Iterator(elems) => elems,
                    _ => panic!("zip requires iterator operands"),
                })
                .collect();
            let len = iters[0].len();
            assert!(
                iters.iter().all(|iter| iter.len() == len),
                "zip requires equal-length iterators"
            );
            Value::Iterator(
                (0..len)
                    .map(|lane| {
                        Value::Iterator(iters.iter().map(|iter| iter[lane].clone()).collect())
                    })
                    .collect(),
            )
        }
        SymKind::Split => {
            let count = as_int!(c(1), "split").to_u64() as usize;
            // A third child fixes the lane width (`split(x, n, w)`), so only the
            // low `n * w` bits participate — the RVV shape, where the active
            // element count and element width come from `vl`/`vtype`, not from
            // the register's total width. Without it, lanes are `total / n`.
            if graph.children(node).count() > 2 {
                let width = as_int!(c(2), "split").to_u64() as usize;
                split_bits_lanes(c(0), count, width)
            } else {
                split_bits(c(0), count)
            }
        }
        SymKind::IterConcat => {
            // Each operand contributes its lanes in order, the earliest operand
            // into the low bits.
            let mut lanes = vec![];
            for index in 0..graph.children(node).count() {
                let Value::Iterator(part) = c(index) else {
                    panic!("concat requires iterator operands");
                };
                lanes.extend(part);
            }
            concat_lanes(Value::Iterator(lanes))
        }
        _ => {
            let count = as_int!(c(0), "iota").to_u64();
            let width = as_int!(c(1), "iota").to_u64() as u32;
            Value::Iterator(
                (0..count)
                    .map(|index| Value::Int(APInt::new(width, index)))
                    .collect(),
            )
        }
    }
}

fn eval_arith(kind: SymKind, c: &impl Fn(usize) -> Value) -> Value {
    match kind {
        SymKind::Add => arith_op!(c, add, add, "add"),
        SymKind::Sub => arith_op!(c, sub, sub, "sub"),
        SymKind::Mul => arith_op!(c, mul, mul, "mul"),
        _ => arith_op!(c, sdiv, div, "div"),
    }
}

fn eval_compare(kind: SymKind, c: &impl Fn(usize) -> Value) -> Value {
    match kind {
        SymKind::Eq => Value::Int(APInt::new(1, bool_result(scalar_equal(c(0), c(1))))),
        SymKind::Ne => Value::Int(APInt::new(1, bool_result(!scalar_equal(c(0), c(1))))),
        SymKind::Lt => cmp_op!(c, slt, lt, "lt"),
        SymKind::Le => cmp_op!(c, sle, le, "le"),
        SymKind::Gt => cmp_op!(c, sgt, gt, "gt"),
        _ => cmp_op!(c, sge, ge, "ge"),
    }
}

fn eval_float(kind: SymKind, c: &impl Fn(usize) -> Value) -> Value {
    match kind {
        SymKind::FAdd => float_binop(c(0), c(1), APFloat::add, "fadd"),
        SymKind::FSub => float_binop(c(0), c(1), APFloat::sub, "fsub"),
        SymKind::FMul => float_binop(c(0), c(1), APFloat::mul, "fmul"),
        SymKind::FDiv => float_binop(c(0), c(1), APFloat::div, "fdiv"),
        SymKind::FMin => float_binop(c(0), c(1), APFloat::minnum, "fmin"),
        SymKind::FMax => float_binop(c(0), c(1), APFloat::maxnum, "fmax"),
        SymKind::AsFloat => match c(0) {
            Value::Int(v) => {
                let (exp, mant) = float_format(v.width(), "asfloat");
                Value::Float(APFloat::from_bits(exp, mant, false, v.to_u64() as u128))
            }
            Value::Float(f) => Value::Float(f),
            _ => panic!("asfloat requires a scalar operand"),
        },
        SymKind::FCvt => {
            let (value, width) = match c(0) {
                Value::Int(v) => (v.to_u64() as u128, v.width()),
                Value::Float(f) => (f.to_bits(), f.bit_width()),
                _ => panic!("fcvt requires a scalar operand"),
            };
            let (exp, mant) = float_format(width, "fcvt");
            let exponent = as_int!(c(1), "fcvt").to_u64() as u32;
            let mantissa = as_int!(c(2), "fcvt").to_u64() as u32;
            let converted =
                APFloat::from_bits(exp, mant, false, value).convert(exponent, mantissa, false);
            Value::Int(APInt::new(
                converted.bit_width(),
                converted.to_bits() as u64,
            ))
        }
        SymKind::SIToFP => {
            let value = as_int!(c(0), "sitofp").to_i64();
            let exponent = as_int!(c(1), "sitofp").to_u64() as u32;
            let mantissa = as_int!(c(2), "sitofp").to_u64() as u32;
            Value::Float(APFloat::from_f64(value as f64).convert(exponent, mantissa, false))
        }
        SymKind::UIToFP => {
            let value = as_int!(c(0), "uitofp").to_u64();
            let exponent = as_int!(c(1), "uitofp").to_u64() as u32;
            let mantissa = as_int!(c(2), "uitofp").to_u64() as u32;
            Value::Float(APFloat::from_f64(value as f64).convert(exponent, mantissa, false))
        }
        SymKind::FPToSI => {
            let value = as_float!(c(0), "fptosi").to_f64() as i64;
            let width = as_int!(c(1), "fptosi").to_u64() as u32;
            Value::Int(APInt::new_signed(width, value))
        }
        _ => {
            let value = as_float!(c(0), "fptoui").to_f64() as u64;
            let width = as_int!(c(1), "fptoui").to_u64() as u32;
            Value::Int(APInt::new(width, value))
        }
    }
}

fn eval_control(kind: SymKind, c: &impl Fn(usize) -> Value) -> Value {
    match kind {
        SymKind::If => {
            let cond_zero = match c(0) {
                Value::Int(i) => i.is_zero(),
                Value::Float(f) => f.is_zero(),
                Value::Iterator(_) | Value::RawBits(_) => panic!("if condition must be scalar"),
            };
            if cond_zero { c(2) } else { c(1) }
        }
        SymKind::Theta => panic!("theta requires loop-sequence semantics"),
        _ => {
            let input = as_int!(c(0), "clamp");
            let min = as_int!(c(1), "clamp");
            let max = as_int!(c(2), "clamp");

            let result = if input.is_signed() {
                if input.slt(&min) {
                    min
                } else if input.sgt(&max) {
                    max
                } else {
                    input
                }
            } else if input.ult(&min) {
                min
            } else if input.ugt(&max) {
                max
            } else {
                input
            };

            Value::Int(result)
        }
    }
}

fn eval_math<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    cache: &[Option<Value>],
    kind: SymKind,
    c: &impl Fn(usize) -> Value,
) -> Value {
    match kind {
        SymKind::Fma => match c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!(c(1), "fma"));
                let (prod, addend) = coerce_ints(a.mul(&b), as_int!(c(2), "fma"));
                Value::Int(prod.add(&addend))
            }
            Value::Float(a) => {
                Value::Float(a.fma(&as_float!(c(1), "fma"), &as_float!(c(2), "fma")))
            }
            Value::Iterator(_) | Value::RawBits(_) => panic!("fma requires scalar operands"),
        },
        SymKind::Sqrt => match c(0) {
            Value::Int(a) => {
                let v = a.to_u64();
                Value::Int(APInt::new(a.width(), (v as f64).sqrt() as u64))
            }
            Value::Float(a) => Value::Float(a.sqrt()),
            Value::Iterator(_) | Value::RawBits(_) => panic!("sqrt requires a scalar operand"),
        },
        SymKind::Log2Ceil => {
            let a = as_int!(c(0), "log2ceil");
            let v = a.to_u64();
            let result = if v <= 1 {
                0u64
            } else {
                64 - (v - 1).leading_zeros() as u64
            };
            Value::Int(APInt::new(a.width(), result))
        }
        SymKind::Bitcast => Value::RawBits(as_raw_bits(c(0))),
        SymKind::Extract => eval_extract(graph, node, cache, c),
        SymKind::ZExt => {
            let value = as_int!(c(0), "zext");
            let width = as_int!(c(1), "zext").to_u64() as u32;
            Value::Int(value.zero_extend(width))
        }
        _ => {
            let value = as_int!(c(0), "sext");
            let width = as_int!(c(1), "sext").to_u64() as u32;
            // Force signed: `extract` yields unsigned, but sext must use the current MSB.
            Value::Int(value.with_signed(true).sign_extend(width))
        }
    }
}

fn eval_extract<V>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    cache: &[Option<Value>],
    c: &impl Fn(usize) -> Value,
) -> Value {
    let value = as_int!(c(0), "extract");
    let high = as_int!(c(1), "extract").to_u64() as u32;
    let low = as_int!(c(2), "extract").to_u64() as u32;
    // `extract(a*b, 2N-1, N)` is the TMDL idiom for a full-multiply high half
    // (e.g. `mulh`); `Mul` keeps only the low N bits, so when the slice lies
    // wholly past the product width, recompute it as a signed full-width product.
    let mul = graph.children(node).next().expect("extract has children");
    if low >= value.width() && matches!(graph.get_kind(mul), SymKind::Mul) {
        let (a, b) = coerce_ints(
            as_int!(child_val(graph, mul, 0, cache), "extract"),
            as_int!(child_val(graph, mul, 1, cache), "extract"),
        );
        let product_high = a.with_signed(true).mulh(&b.with_signed(true));
        Value::Int(product_high.extract_bits(high - a.width(), low - a.width()))
    } else {
        Value::Int(value.extract_bits(high, low))
    }
}

fn eval_memory<M: Memory>(
    kind: SymKind,
    c: &impl Fn(usize) -> Value,
    memory: &mut M,
) -> Result<Value, M::Error> {
    let result = match kind {
        SymKind::LoadMemory => {
            let address = as_int!(c(0), "load").to_u64();
            let size = as_int!(c(1), "load").to_u64() as usize;
            // Accesses wider than a word (a vector load) read as raw byte lanes.
            if size > 8 {
                Value::RawBits(memory.read_memory_bytes(address, size)?)
            } else {
                let value = memory.read_memory(address, size)?;
                Value::Int(APInt::new((size as u32) * 8, value))
            }
        }
        _ => {
            let address = as_int!(c(0), "store").to_u64();
            let size = as_int!(c(1), "store").to_u64() as usize;
            if size > 8 {
                memory.write_memory_bytes(address, size, as_raw_bits(c(2)))?;
            } else {
                memory.write_memory(address, size, as_int!(c(2), "store").to_u64())?;
            }
            Value::Int(APInt::new(1, 0))
        }
    };
    Ok(result)
}

fn eval_atomic<M: Memory>(
    kind: SymKind,
    c: &impl Fn(usize) -> Value,
    memory: &mut M,
) -> Result<Value, M::Error> {
    let result = match kind {
        SymKind::LoadReserved => {
            let address = as_int!(c(0), "load_reserved").to_u64();
            let size = as_int!(c(1), "load_reserved").to_u64() as usize;
            assert!(
                size <= 8,
                "load_reserved does not support accesses wider than 8 bytes"
            );
            let ord = MemOrdering::from_code(as_int!(c(2), "load_reserved").to_u64());
            let value = memory.load_reserved(address, size, ord)?;
            Value::Int(APInt::new((size as u32) * 8, value))
        }
        SymKind::StoreConditional => {
            let address = as_int!(c(0), "store_conditional").to_u64();
            let size = as_int!(c(1), "store_conditional").to_u64() as usize;
            assert!(
                size <= 8,
                "store_conditional does not support accesses wider than 8 bytes"
            );
            let value = as_int!(c(2), "store_conditional").to_u64();
            let ord = MemOrdering::from_code(as_int!(c(3), "store_conditional").to_u64());
            let ok = memory.store_conditional(address, size, value, ord)?;
            Value::Int(APInt::new(1, ok as u64))
        }
        SymKind::AtomicRmw => {
            let op = AtomicRmwOp::from_code(as_int!(c(0), "atomic_rmw").to_u64())
                .expect("atomic_rmw op child must be a constant op code 0..8");
            let address = as_int!(c(1), "atomic_rmw").to_u64();
            let size = as_int!(c(2), "atomic_rmw").to_u64() as usize;
            assert!(
                size <= 8,
                "atomic_rmw does not support accesses wider than 8 bytes"
            );
            let value = as_int!(c(3), "atomic_rmw").to_u64();
            let ord = MemOrdering::from_code(as_int!(c(4), "atomic_rmw").to_u64());
            let old = memory.atomic_rmw(op, address, size, value, ord)?;
            Value::Int(APInt::new((size as u32) * 8, old))
        }
        _ => {
            let pred = as_int!(c(0), "fence").to_u64() as u32;
            let succ = as_int!(c(1), "fence").to_u64() as u32;
            let kind = as_int!(c(2), "fence").to_u64() as u32;
            memory.fence(pred, succ, kind)?;
            Value::Int(APInt::new(1, 0))
        }
    };
    Ok(result)
}

fn bool_result(b: bool) -> u64 {
    b as u64
}
