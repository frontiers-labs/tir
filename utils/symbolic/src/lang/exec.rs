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
        32 => (8, 23),
        64 => (11, 52),
        other => panic!("{op} requires a 32- or 64-bit operand, got {other} bits"),
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
    let raw: Vec<RawBits> = lanes
        .into_iter()
        .map(|lane| match lane {
            Value::Int(i) => RawBits::from_apint(&i),
            Value::Float(f) => RawBits::from_apfloat(&f),
            Value::RawBits(b) => b,
            Value::Iterator(_) => panic!("concat lanes must be scalar"),
        })
        .collect();
    Value::RawBits(RawBits::concat(&raw))
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

    let result = match graph.get_kind(node) {
        SymKind::Map | SymKind::Reduce => {
            unreachable!("map/reduce handled before child pre-evaluation")
        }
        SymKind::Arg => {
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
        SymKind::Zip => {
            let (Value::Iterator(lhs), Value::Iterator(rhs)) = (c(0), c(1)) else {
                panic!("zip requires iterator operands");
            };
            assert!(
                lhs.len() == rhs.len(),
                "zip requires equal-length iterators"
            );
            Value::Iterator(
                lhs.into_iter()
                    .zip(rhs)
                    .map(|(a, b)| Value::Iterator(vec![a, b]))
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
        SymKind::IterConcat => concat_lanes(c(0)),
        SymKind::Symbol => {
            let SymPayload::SymbolId(id) = graph.get_leaf_data(node).unwrap() else {
                panic!("Symbol node must have SymbolId payload");
            };
            symbols[*id as usize].clone()
        }
        SymKind::Constant => match graph.get_leaf_data(node).unwrap() {
            SymPayload::Int(v) => Value::Int(v.clone()),
            SymPayload::Float(v) => Value::Float(v.clone()),
            _ => panic!("Constant node must have Int or Float payload"),
        },

        // ── Arithmetic (int or float) ──────────────────────────────────────
        SymKind::Add => arith_op!(c, add, add, "add"),
        SymKind::Sub => arith_op!(c, sub, sub, "sub"),
        SymKind::Mul => arith_op!(c, mul, mul, "mul"),
        SymKind::Div => arith_op!(c, sdiv, div, "div"),

        // ── Floating point ─────────────────────────────────────────────────
        SymKind::FAdd => float_binop(c(0), c(1), APFloat::add, "fadd"),
        SymKind::FSub => float_binop(c(0), c(1), APFloat::sub, "fsub"),
        SymKind::FMul => float_binop(c(0), c(1), APFloat::mul, "fmul"),
        SymKind::FDiv => float_binop(c(0), c(1), APFloat::div, "fdiv"),
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
        SymKind::FPToUI => {
            let value = as_float!(c(0), "fptoui").to_f64() as u64;
            let width = as_int!(c(1), "fptoui").to_u64() as u32;
            Value::Int(APInt::new(width, value))
        }

        // ── Bitwise (int only) ─────────────────────────────────────────────
        SymKind::Eq => Value::Int(APInt::new(1, bool_result(c(0) == c(1)))),
        SymKind::Ne => Value::Int(APInt::new(1, bool_result(c(0) != c(1)))),
        SymKind::Lt => cmp_op!(c, slt, lt, "lt"),
        SymKind::Le => cmp_op!(c, sle, le, "le"),
        SymKind::Gt => cmp_op!(c, sgt, gt, "gt"),
        SymKind::Ge => cmp_op!(c, sge, ge, "ge"),

        // ── Control ────────────────────────────────────────────────────────
        SymKind::If => {
            let cond_zero = match c(0) {
                Value::Int(i) => i.is_zero(),
                Value::Float(f) => f.is_zero(),
                Value::Iterator(_) | Value::RawBits(_) => panic!("if condition must be scalar"),
            };
            if cond_zero { c(2) } else { c(1) }
        }
        SymKind::Clamp => {
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

        // ── Math (int or float) ────────────────────────────────────────────
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

        SymKind::Extract => {
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
        SymKind::ZExt => {
            let value = as_int!(c(0), "zext");
            let width = as_int!(c(1), "zext").to_u64() as u32;
            Value::Int(value.zero_extend(width))
        }
        SymKind::SExt => {
            let value = as_int!(c(0), "sext");
            let width = as_int!(c(1), "sext").to_u64() as u32;
            // Force signed: `extract` yields unsigned, but sext must use the current MSB.
            Value::Int(value.with_signed(true).sign_extend(width))
        }

        // ── Memory ─────────────────────────────────────────────────────────
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
        SymKind::StoreMemory => {
            let address = as_int!(c(0), "store").to_u64();
            let size = as_int!(c(1), "store").to_u64() as usize;
            if size > 8 {
                memory.write_memory_bytes(address, size, as_raw_bits(c(2)))?;
            } else {
                memory.write_memory(address, size, as_int!(c(2), "store").to_u64())?;
            }
            Value::Int(APInt::new(1, 0))
        }

        // ── Atomics ────────────────────────────────────────────────────────
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
        SymKind::Fence => {
            let pred = as_int!(c(0), "fence").to_u64() as u32;
            let succ = as_int!(c(1), "fence").to_u64() as u32;
            let kind = as_int!(c(2), "fence").to_u64() as u32;
            memory.fence(pred, succ, kind)?;
            Value::Int(APInt::new(1, 0))
        }
        _ => unreachable!("operator has no concrete evaluator"),
    };

    cache[node.index()] = Some(result.clone());
    Ok(result)
}

fn bool_result(b: bool) -> u64 {
    b as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tir_adt::APFloat;
    use tir_graph::{GenericDag, MutDag};

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

        fn write_memory(
            &mut self,
            address: u64,
            size: usize,
            value: u64,
        ) -> Result<(), Self::Error> {
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

    // ── Integer arithmetic ─────────────────────────────────────────────────

    #[test]
    fn int_add() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Add, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(3), iv(4)])), 7);
    }

    #[test]
    fn int_sub() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Sub, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(10), iv(3)])), 7);
    }

    #[test]
    fn int_mul() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Mul, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(6), iv(7)])), 42);
    }

    #[test]
    fn int_neg() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        inner(&mut g, SymKind::Neg, &[a]);
        assert_eq!(as_i64(execute(&g, &[iv(5)])), -5);
        assert_eq!(as_i64(execute(&g, &[iv(-3)])), 3);
    }

    #[test]
    fn int_srem_and_urem() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::SRem, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(-7), iv(3)])), -1);

        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::URem, &[a, b]);
        assert_eq!(as_u64(execute(&g, &[uv(7), uv(3)])), 1);
    }

    fn exec_bin(kind: SymKind, a: Value, b: Value) -> Value {
        let mut g = Graph::new();
        let x = sym(&mut g, 0);
        let y = sym(&mut g, 1);
        inner(&mut g, kind, &[x, y]);
        execute(&g, &[a, b])
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
    fn int_and() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::And, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[uv(0b1100), uv(0b1010)])), 0b1000);
    }

    #[test]
    fn int_not() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        inner(&mut g, SymKind::Not, &[a]);
        assert_eq!(as_u64(execute(&g, &[uv(0b1010)])), 0xFFFF_FFF5);
    }

    #[test]
    fn int_shl() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::ShiftLeft, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[uv(1), uv(3)])), 8);
    }

    #[test]
    fn int_lshr() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::ShiftRightLogic, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[uv(16), uv(2)])), 4);
    }

    #[test]
    fn int_ashr_negative() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::ShiftRightArithmetic, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(-8), iv(1)])), -4);
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
    fn int_fma() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        inner(&mut g, SymKind::Fma, &[a, b, c]);
        assert_eq!(as_i64(execute(&g, &[iv(3), iv(4), iv(5)])), 17);
    }

    // ── Comparisons ────────────────────────────────────────────────────────

    #[test]
    fn int_eq() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Eq, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(5), iv(5)])), 1);
        assert_eq!(as_i64(execute(&g, &[iv(5), iv(6)])), 0);
    }

    #[test]
    fn int_if() {
        let mut g = Graph::new();
        let cond = sym(&mut g, 0);
        let t = sym(&mut g, 1);
        let e = sym(&mut g, 2);
        inner(&mut g, SymKind::If, &[cond, t, e]);
        assert_eq!(as_i64(execute(&g, &[iv(1), iv(42), iv(0)])), 42);
        assert_eq!(as_i64(execute(&g, &[iv(0), iv(42), iv(99)])), 99);
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

    // ── Float arithmetic ───────────────────────────────────────────────────

    #[test]
    fn float_add() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Add, &[a, b]);
        assert!((as_f64(execute(&g, &[fv(1.5), fv(2.5)])) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn float_div() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Div, &[a, b]);
        assert!((as_f64(execute(&g, &[fv(7.0), fv(2.0)])) - 3.5).abs() < 1e-9);
    }

    #[test]
    fn float_sqrt() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        inner(&mut g, SymKind::Sqrt, &[a]);
        assert!((as_f64(execute(&g, &[fv(9.0)])) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn float_fma() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        inner(&mut g, SymKind::Fma, &[a, b, c]);
        assert!((as_f64(execute(&g, &[fv(2.0), fv(3.0), fv(1.0)])) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn float_lt() {
        let mut g = Graph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, SymKind::Lt, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[fv(1.0), fv(2.0)])), 1);
        assert_eq!(as_i64(execute(&g, &[fv(3.0), fv(2.0)])), 0);
    }

    // ── Iterator nodes ─────────────────────────────────────────────────────

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

    // ── Atomics ─────────────────────────────────────────────────────────────

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

        fn write_memory(
            &mut self,
            address: u64,
            size: usize,
            value: u64,
        ) -> Result<(), Self::Error> {
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
}
