use crate::{
    graph::{Dag, NodeId},
    sem_expr::{ExprKind, ExprPayload, Value},
    utils::APInt,
};

/// Memory backend used by semantic expressions containing `LoadMemory` or
/// `StoreMemory`.
pub trait Memory {
    type Error;

    fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error>;
    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), Self::Error>;
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

/// Evaluate the expression DAG given concrete values for each symbol.
///
/// `symbols[i]` is the value for the operand with `SymbolId(i)`.
/// Returns the value of the root node.
pub fn execute(graph: &impl Dag<Node = ExprKind, Leaf = ExprPayload>, symbols: &[Value]) -> Value {
    match execute_with_memory(graph, symbols, &mut NoMemory) {
        Ok(value) => value,
        Err(err) => match err {},
    }
}

/// Evaluate the expression DAG with a memory backend for load/store nodes.
///
/// Loads read little-endian byte sequences and produce an integer whose width is
/// `size * 8`. Stores write the low bytes of their value and return a dummy
/// 1-bit integer; callers normally ignore the result for store statements.
pub fn execute_with_memory<M: Memory>(
    graph: &impl Dag<Node = ExprKind, Leaf = ExprPayload>,
    symbols: &[Value],
    memory: &mut M,
) -> Result<Value, M::Error> {
    let root = graph.root().expect("cannot execute empty graph");
    let mut cache = vec![None::<Value>; graph.len()];
    let mut frames: Vec<(Value, Value)> = Vec::new();
    eval_node(graph, root, symbols, &mut cache, &mut frames, memory)
}

fn child_val(
    graph: &impl Dag<Node = ExprKind, Leaf = ExprPayload>,
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
            Value::Vector(_) => panic!("{} requires scalar operands", $op),
        }
    };
}

macro_rules! as_float {
    ($v:expr, $op:literal) => {
        match $v {
            Value::Float(f) => f,
            Value::Int(_) => panic!("{} requires float operands", $op),
            Value::Vector(_) => panic!("{} requires scalar operands", $op),
        }
    };
}

/// Widen `v` to `width`, sign-extending signed values and zero-extending unsigned
/// ones; a no-op when it is already at least that wide.
fn widen(v: APInt, width: u32) -> APInt {
    if v.width() >= width {
        v
    } else if v.is_signed() {
        v.sign_extend(width)
    } else {
        v.zero_extend(width)
    }
}

/// Bring two integers to a common width before a binary operation. Behavior
/// expressions freely mix a wide value (a register, `XLEN`) with a bare narrow
/// literal (`- 1`, `<< 2`, a `zext`-ed constant), so the interpreter extends the
/// narrower operand rather than requiring exactly matching widths. Equal-width
/// operands — the common case — pass through unchanged.
fn coerce_ints(a: APInt, b: APInt) -> (APInt, APInt) {
    let width = a.width().max(b.width());
    (widen(a, width), widen(b, width))
}

/// Equality of two integers independent of width and signedness: operands are
/// widened to a common width and compared by value, so e.g. a 64-bit register
/// equals a narrow literal of the same magnitude.
fn ints_equal(a: APInt, b: APInt) -> bool {
    let (a, b) = coerce_ints(a, b);
    a.with_signed(false) == b.with_signed(false)
}

/// Evaluate a `Loop` node: fold `step` over `[start, end)`, threading the
/// accumulator and exposing the induction value through the frame stack.
fn eval_loop<M: Memory>(
    graph: &impl Dag<Node = ExprKind, Leaf = ExprPayload>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
    frames: &mut Vec<(Value, Value)>,
    memory: &mut M,
) -> Result<Value, M::Error> {
    let children: Vec<NodeId> = graph.children(node).collect();
    let (start_n, end_n, init_n, step_n) = (children[0], children[1], children[2], children[3]);

    let start = eval_node(graph, start_n, symbols, cache, frames, memory)?;
    let end = eval_node(graph, end_n, symbols, cache, frames, memory)?;
    let mut acc = eval_node(graph, init_n, symbols, cache, frames, memory)?;

    let start = as_int!(start, "loop bound").to_i64();
    let end = as_int!(end, "loop bound").to_i64();

    for i in start..end {
        frames.push((Value::Int(APInt::new_signed(64, i)), acc.clone()));
        // `step` depends on the induction/accumulator values, which change each
        // iteration, so it cannot share the surrounding cache: evaluate it fresh.
        let mut step_cache = vec![None::<Value>; graph.len()];
        let next = eval_node(graph, step_n, symbols, &mut step_cache, frames, memory);
        frames.pop();
        acc = next?;
    }
    Ok(acc)
}

/// Evaluate a `VectorMap` node: build a vector by evaluating `elem` over each
/// lane index `[0, count)`, exposing the index through the frame stack the same
/// way `Loop` exposes its induction value.
fn eval_vector_map<M: Memory>(
    graph: &impl Dag<Node = ExprKind, Leaf = ExprPayload>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
    frames: &mut Vec<(Value, Value)>,
    memory: &mut M,
) -> Result<Value, M::Error> {
    let children: Vec<NodeId> = graph.children(node).collect();
    let (count_n, elem_n) = (children[0], children[1]);

    let count = eval_node(graph, count_n, symbols, cache, frames, memory)?;
    let count = as_int!(count, "vector length").to_i64();

    let mut lanes = Vec::with_capacity(count.max(0) as usize);
    for i in 0..count {
        // The accumulator slot is unused by a map; the induction value is read by
        // `IndVar` and by `Lane` indices, and changes each lane, so `elem` cannot
        // share the surrounding cache.
        frames.push((
            Value::Int(APInt::new_signed(64, i)),
            Value::Int(APInt::new(1, 0)),
        ));
        let mut lane_cache = vec![None::<Value>; graph.len()];
        let lane = eval_node(graph, elem_n, symbols, &mut lane_cache, frames, memory);
        frames.pop();
        lanes.push(lane?);
    }
    Ok(Value::Vector(lanes))
}

fn eval_node<M: Memory>(
    graph: &impl Dag<Node = ExprKind, Leaf = ExprPayload>,
    node: NodeId,
    symbols: &[Value],
    cache: &mut Vec<Option<Value>>,
    frames: &mut Vec<(Value, Value)>,
    memory: &mut M,
) -> Result<Value, M::Error> {
    if let Some(ref v) = cache[node.index()] {
        return Ok(v.clone());
    }

    // A `Loop` must not have its `step` child pre-evaluated: `step` depends on the
    // per-iteration induction/accumulator values, so it is evaluated repeatedly,
    // by hand, below. Intercept before the generic child pre-evaluation.
    if *graph.get_kind(node) == ExprKind::Loop {
        let result = eval_loop(graph, node, symbols, cache, frames, memory)?;
        cache[node.index()] = Some(result.clone());
        return Ok(result);
    }

    // A `VectorMap`, like `Loop`, evaluates its `elem` child fresh per lane with
    // the induction value bound, so intercept it before generic child pre-eval.
    if *graph.get_kind(node) == ExprKind::VectorMap {
        let result = eval_vector_map(graph, node, symbols, cache, frames, memory)?;
        cache[node.index()] = Some(result.clone());
        return Ok(result);
    }

    for child_id in graph.children(node) {
        if cache[child_id.index()].is_none() {
            let v = eval_node(graph, child_id, symbols, cache, frames, memory)?;
            cache[child_id.index()] = Some(v);
        }
    }

    let c = |idx: usize| child_val(graph, node, idx, cache);

    let result = match graph.get_kind(node) {
        ExprKind::IndVar => frames
            .last()
            .expect("IndVar evaluated outside a loop")
            .0
            .clone(),
        ExprKind::Acc => frames
            .last()
            .expect("Acc evaluated outside a loop")
            .1
            .clone(),
        ExprKind::Loop => unreachable!("Loop handled before child pre-evaluation"),
        ExprKind::VectorMap => {
            unreachable!("VectorMap handled before child pre-evaluation")
        }
        ExprKind::Lane => {
            let Value::Vector(lanes) = c(0) else {
                panic!("Lane requires a vector operand");
            };
            let index = as_int!(c(1), "lane").to_u64() as usize;
            lanes[index].clone()
        }
        ExprKind::Symbol => {
            let ExprPayload::SymbolId(id) = graph.get_leaf_data(node).unwrap() else {
                panic!("Symbol node must have SymbolId payload");
            };
            symbols[*id as usize].clone()
        }
        ExprKind::Constant => match graph.get_leaf_data(node).unwrap() {
            ExprPayload::Int(v) => Value::Int(v.clone()),
            ExprPayload::Float(v) => Value::Float(v.clone()),
            _ => panic!("Constant node must have Int or Float payload"),
        },

        // ── Arithmetic (int or float) ──────────────────────────────────────
        ExprKind::Add => match c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!(c(1), "add"));
                Value::Int(a.add(&b))
            }
            Value::Float(a) => Value::Float(a.add(&as_float!(c(1), "add"))),
            Value::Vector(_) => panic!("add requires scalar operands"),
        },
        ExprKind::Sub => match c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!(c(1), "sub"));
                Value::Int(a.sub(&b))
            }
            Value::Float(a) => Value::Float(a.sub(&as_float!(c(1), "sub"))),
            Value::Vector(_) => panic!("sub requires scalar operands"),
        },
        ExprKind::Mul => match c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!(c(1), "mul"));
                Value::Int(a.mul(&b))
            }
            Value::Float(a) => Value::Float(a.mul(&as_float!(c(1), "mul"))),
            Value::Vector(_) => panic!("mul requires scalar operands"),
        },
        ExprKind::Div => match c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!(c(1), "div"));
                Value::Int(a.sdiv(&b))
            }
            Value::Float(a) => Value::Float(a.div(&as_float!(c(1), "div"))),
            Value::Vector(_) => panic!("div requires scalar operands"),
        },
        ExprKind::UDiv => {
            let (a, b) = coerce_ints(as_int!(c(0), "udiv"), as_int!(c(1), "udiv"));
            Value::Int(a.udiv(&b))
        }

        // ── Bitwise (int only) ─────────────────────────────────────────────
        ExprKind::And => {
            let (a, b) = coerce_ints(as_int!(c(0), "and"), as_int!(c(1), "and"));
            Value::Int(a.and(&b))
        }
        ExprKind::Or => {
            let (a, b) = coerce_ints(as_int!(c(0), "or"), as_int!(c(1), "or"));
            Value::Int(a.or(&b))
        }
        ExprKind::Xor => {
            let (a, b) = coerce_ints(as_int!(c(0), "xor"), as_int!(c(1), "xor"));
            Value::Int(a.xor(&b))
        }
        ExprKind::ShiftLeft => {
            Value::Int(as_int!(c(0), "shl").shl(as_int!(c(1), "shl").to_u64() as u32))
        }
        ExprKind::ShiftRightLogic => {
            Value::Int(as_int!(c(0), "lshr").lshr(as_int!(c(1), "lshr").to_u64() as u32))
        }
        ExprKind::ShiftRightArithmetic => {
            // An arithmetic shift always treats its operand as signed (sign bit =
            // MSB of the operand width), regardless of the value's stored
            // signedness flag. Register values are stored unsigned, so without
            // forcing this `>>>` would silently degrade to a logical shift.
            let mut value = as_int!(c(0), "ashr");
            value.set_signed(true);
            Value::Int(value.ashr(as_int!(c(1), "ashr").to_u64() as u32))
        }
        ExprKind::Not => Value::Int(as_int!(c(0), "not").not()),

        // ── Comparisons ────────────────────────────────────────────────────
        ExprKind::Eq => {
            let eq = match (c(0), c(1)) {
                (Value::Int(a), Value::Int(b)) => ints_equal(a, b),
                (l, r) => l == r,
            };
            Value::Int(APInt::new(1, bool_result(eq)))
        }
        ExprKind::Ne => {
            let ne = match (c(0), c(1)) {
                (Value::Int(a), Value::Int(b)) => !ints_equal(a, b),
                (l, r) => l != r,
            };
            Value::Int(APInt::new(1, bool_result(ne)))
        }
        ExprKind::Lt => Value::Int(APInt::new(
            1,
            match c(0) {
                Value::Int(a) => {
                    let (a, b) = coerce_ints(a, as_int!(c(1), "lt"));
                    bool_result(a.slt(&b))
                }
                Value::Float(a) => bool_result(a.lt(&as_float!(c(1), "lt"))),
                Value::Vector(_) => panic!("lt requires scalar operands"),
            },
        )),
        ExprKind::Gt => Value::Int(APInt::new(
            1,
            match c(0) {
                Value::Int(a) => {
                    let (a, b) = coerce_ints(a, as_int!(c(1), "gt"));
                    bool_result(a.sgt(&b))
                }
                Value::Float(a) => bool_result(a.gt(&as_float!(c(1), "gt"))),
                Value::Vector(_) => panic!("gt requires scalar operands"),
            },
        )),
        ExprKind::Ge => Value::Int(APInt::new(
            1,
            match c(0) {
                Value::Int(a) => {
                    let (a, b) = coerce_ints(a, as_int!(c(1), "ge"));
                    bool_result(a.sge(&b))
                }
                Value::Float(a) => bool_result(a.ge(&as_float!(c(1), "ge"))),
                Value::Vector(_) => panic!("ge requires scalar operands"),
            },
        )),
        ExprKind::ULt => {
            let (a, b) = coerce_ints(as_int!(c(0), "ult"), as_int!(c(1), "ult"));
            Value::Int(APInt::new(1, bool_result(a.ult(&b))))
        }
        ExprKind::ULe => {
            let (a, b) = coerce_ints(as_int!(c(0), "ule"), as_int!(c(1), "ule"));
            Value::Int(APInt::new(1, bool_result(a.ule(&b))))
        }
        ExprKind::UGt => {
            let (a, b) = coerce_ints(as_int!(c(0), "ugt"), as_int!(c(1), "ugt"));
            Value::Int(APInt::new(1, bool_result(a.ugt(&b))))
        }
        ExprKind::UGe => {
            let (a, b) = coerce_ints(as_int!(c(0), "uge"), as_int!(c(1), "uge"));
            Value::Int(APInt::new(1, bool_result(a.uge(&b))))
        }

        // ── Control ────────────────────────────────────────────────────────
        ExprKind::If => {
            let cond_zero = match c(0) {
                Value::Int(i) => i.is_zero(),
                Value::Float(f) => f.is_zero(),
                Value::Vector(_) => panic!("if condition must be scalar"),
            };
            if cond_zero { c(2) } else { c(1) }
        }
        ExprKind::Clamp => {
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
        ExprKind::Fma => match c(0) {
            Value::Int(a) => {
                let (a, b) = coerce_ints(a, as_int!(c(1), "fma"));
                let (prod, addend) = coerce_ints(a.mul(&b), as_int!(c(2), "fma"));
                Value::Int(prod.add(&addend))
            }
            Value::Float(a) => {
                Value::Float(a.fma(&as_float!(c(1), "fma"), &as_float!(c(2), "fma")))
            }
            Value::Vector(_) => panic!("fma requires scalar operands"),
        },
        ExprKind::Sqrt => match c(0) {
            Value::Int(a) => {
                let v = a.to_u64();
                Value::Int(APInt::new(a.width(), (v as f64).sqrt() as u64))
            }
            Value::Float(a) => Value::Float(a.sqrt()),
            Value::Vector(_) => panic!("sqrt requires a scalar operand"),
        },
        ExprKind::Log2Ceil => {
            let a = as_int!(c(0), "log2ceil");
            let v = a.to_u64();
            let result = if v <= 1 {
                0u64
            } else {
                64 - (v - 1).leading_zeros() as u64
            };
            Value::Int(APInt::new(a.width(), result))
        }

        ExprKind::Extract => {
            let value = as_int!(c(0), "extract");
            let high = as_int!(c(1), "extract").to_u64() as u32;
            let low = as_int!(c(2), "extract").to_u64() as u32;
            // `extract(a * b, 2N-1, N)` is the TMDL idiom for the high half of a
            // full multiply (e.g. RISC-V `mulh`). The `Mul` node itself only
            // keeps the low N bits, so when the slice lies entirely past the
            // product's width, recompute it from the multiply's operands as a
            // signed full-width product.
            let mul = graph.children(node).next().expect("extract has children");
            if low >= value.width() && matches!(graph.get_kind(mul), ExprKind::Mul) {
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
        ExprKind::ZExt => {
            let value = as_int!(c(0), "zext");
            let width = as_int!(c(1), "zext").to_u64() as u32;
            Value::Int(value.zero_extend(width))
        }
        ExprKind::SExt => {
            let value = as_int!(c(0), "sext");
            let width = as_int!(c(1), "sext").to_u64() as u32;
            // Sign-extend from the value's current MSB regardless of how its
            // signedness flag happens to be set (e.g. `extract` yields unsigned).
            Value::Int(value.with_signed(true).sign_extend(width))
        }

        // ── Memory ─────────────────────────────────────────────────────────
        ExprKind::LoadMemory => {
            let address = as_int!(c(0), "load").to_u64();
            let size = as_int!(c(1), "load").to_u64() as usize;
            let value = memory.read_memory(address, size)?;
            Value::Int(APInt::new((size as u32) * 8, value))
        }
        ExprKind::StoreMemory => {
            let address = as_int!(c(0), "store").to_u64();
            let size = as_int!(c(1), "store").to_u64() as usize;
            let value = as_int!(c(2), "store").to_u64();
            memory.write_memory(address, size, value)?;
            Value::Int(APInt::new(1, 0))
        }
    };

    cache[node.index()] = Some(result.clone());
    Ok(result)
}

fn bool_result(b: bool) -> u64 {
    b as u64
}

// PartialEq for Value so comparisons work
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Vector(a), Value::Vector(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        graph::MutDag,
        sem_expr::{ExprKind, ExprPayload, ExprPostGraph},
        utils::APFloat,
    };

    fn sym(g: &mut ExprPostGraph, id: u32) -> NodeId {
        let node = g.add_node(ExprKind::Symbol);
        g.set_leaf_data(node, ExprPayload::SymbolId(id));
        node
    }
    fn int_con(g: &mut ExprPostGraph, v: i64) -> NodeId {
        let node = g.add_node(ExprKind::Constant);
        g.set_leaf_data(node, ExprPayload::Int(APInt::new_signed(64, v)));
        node
    }
    fn flt_con(g: &mut ExprPostGraph, v: f64) -> NodeId {
        let node = g.add_node(ExprKind::Constant);
        g.set_leaf_data(node, ExprPayload::Float(APFloat::from_f64(v)));
        node
    }

    fn inner(g: &mut ExprPostGraph, kind: ExprKind, children: &[NodeId]) -> NodeId {
        let node = g.add_node(kind);
        for &child in children {
            g.add_edge(node, child);
        }
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

    fn as_i64(v: Value) -> i64 {
        match v {
            Value::Int(i) => i.to_i64(),
            _ => panic!(),
        }
    }
    fn as_f64(v: Value) -> f64 {
        match v {
            Value::Float(f) => f.to_f64(),
            _ => panic!(),
        }
    }

    #[test]
    fn memory_load_and_store_execute_little_endian() {
        let mut g = ExprPostGraph::new();
        let address = int_con(&mut g, 4);
        let bytes = int_con(&mut g, 4);
        let metadata = int_con(&mut g, 0);
        inner(&mut g, ExprKind::LoadMemory, &[address, bytes, metadata]);

        let mut memory = TestMemory { bytes: vec![0; 16] };
        memory.bytes[4..8].copy_from_slice(&[0x78, 0x56, 0x34, 0x12]);
        let loaded = execute_with_memory(&g, &[], &mut memory).unwrap();
        assert_eq!(as_i64(loaded), 0x1234_5678);

        let mut g = ExprPostGraph::new();
        let address = int_con(&mut g, 8);
        let bytes = int_con(&mut g, 2);
        let value = int_con(&mut g, 0xbeef);
        let address_space = int_con(&mut g, 0);
        inner(
            &mut g,
            ExprKind::StoreMemory,
            &[address, bytes, value, address_space],
        );
        execute_with_memory(&g, &[], &mut memory).unwrap();
        assert_eq!(&memory.bytes[8..10], &[0xef, 0xbe]);
    }

    // ── Integer arithmetic ─────────────────────────────────────────────────

    #[test]
    fn int_add() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Add, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(3), iv(4)])), 7);
    }

    #[test]
    fn int_sub() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Sub, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(10), iv(3)])), 7);
    }

    #[test]
    fn int_mul() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Mul, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(6), iv(7)])), 42);
    }

    #[test]
    fn extract_above_mul_yields_signed_high_product() {
        // The RISC-V `mulh` semantics expressed the TMDL way:
        // extract(rs1 * rs2, 127, 64) on 64-bit operands.
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let mul = inner(&mut g, ExprKind::Mul, &[a, b]);
        let hi = int_con(&mut g, 127);
        let lo = int_con(&mut g, 64);
        inner(&mut g, ExprKind::Extract, &[mul, hi, lo]);

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
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let add = inner(&mut g, ExprKind::Add, &[a, b]);
        let hi = int_con(&mut g, 31);
        let lo = int_con(&mut g, 0);
        let ext = inner(&mut g, ExprKind::Extract, &[add, hi, lo]);
        let width = int_con(&mut g, 64);
        inner(&mut g, ExprKind::SExt, &[ext, width]);

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
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::And, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[uv(0b1100), uv(0b1010)])), 0b1000);
    }

    #[test]
    fn int_not() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        inner(&mut g, ExprKind::Not, &[a]);
        assert_eq!(as_i64(execute(&g, &[uv(0b1010)])), 0xFFFF_FFF5);
    }

    #[test]
    fn int_and_folded_not_literal() {
        // The shape TMDL lowers `x & ~1` to: a narrow signed -2 constant that
        // sign-extends to 0b11..10 at the other operand's width.
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let c = g.add_node(ExprKind::Constant);
        g.set_leaf_data(c, ExprPayload::Int(APInt::new_signed(3, -2)));
        inner(&mut g, ExprKind::And, &[a, c]);
        assert_eq!(as_i64(execute(&g, &[uv(0b1011)])), 0b1010);
    }

    #[test]
    fn int_shl() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::ShiftLeft, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[uv(1), uv(3)])), 8);
    }

    #[test]
    fn int_lshr() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::ShiftRightLogic, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[uv(16), uv(2)])), 4);
    }

    #[test]
    fn int_ashr_negative() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::ShiftRightArithmetic, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(-8), iv(1)])), -4);
    }

    #[test]
    fn int_constant() {
        let mut g = ExprPostGraph::new();
        int_con(&mut g, 42);
        assert_eq!(as_i64(execute(&g, &[])), 42);
    }

    #[test]
    fn int_shared_node() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        inner(&mut g, ExprKind::Add, &[a, a]);
        assert_eq!(as_i64(execute(&g, &[iv(5)])), 10);
    }

    #[test]
    fn int_fma() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        inner(&mut g, ExprKind::Fma, &[a, b, c]);
        assert_eq!(as_i64(execute(&g, &[iv(3), iv(4), iv(5)])), 17);
    }

    // ── Comparisons ────────────────────────────────────────────────────────

    #[test]
    fn int_eq_true() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Eq, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(5), iv(5)])), 1);
    }

    #[test]
    fn int_eq_false() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Eq, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[iv(5), iv(6)])), 0);
    }

    #[test]
    fn int_if_taken() {
        let mut g = ExprPostGraph::new();
        let cond = sym(&mut g, 0);
        let t = sym(&mut g, 1);
        let e = sym(&mut g, 2);
        inner(&mut g, ExprKind::If, &[cond, t, e]);
        assert_eq!(as_i64(execute(&g, &[iv(1), iv(42), iv(0)])), 42);
    }

    #[test]
    fn int_if_not_taken() {
        let mut g = ExprPostGraph::new();
        let cond = sym(&mut g, 0);
        let t = sym(&mut g, 1);
        let e = sym(&mut g, 2);
        inner(&mut g, ExprKind::If, &[cond, t, e]);
        assert_eq!(as_i64(execute(&g, &[iv(0), iv(42), iv(99)])), 99);
    }

    // ── Float arithmetic ───────────────────────────────────────────────────

    #[test]
    fn float_add() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Add, &[a, b]);
        assert!((as_f64(execute(&g, &[fv(1.5), fv(2.5)])) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn float_sub() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Sub, &[a, b]);
        assert!((as_f64(execute(&g, &[fv(5.0), fv(3.0)])) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn float_mul() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Mul, &[a, b]);
        assert!((as_f64(execute(&g, &[fv(2.0), fv(3.5)])) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn float_div() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Div, &[a, b]);
        assert!((as_f64(execute(&g, &[fv(7.0), fv(2.0)])) - 3.5).abs() < 1e-9);
    }

    #[test]
    fn float_sqrt() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        inner(&mut g, ExprKind::Sqrt, &[a]);
        assert!((as_f64(execute(&g, &[fv(9.0)])) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn float_fma() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        let c = sym(&mut g, 2);
        inner(&mut g, ExprKind::Fma, &[a, b, c]);
        // 2.0 * 3.0 + 1.0 = 7.0
        assert!((as_f64(execute(&g, &[fv(2.0), fv(3.0), fv(1.0)])) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn int_clamp() {
        let mut g = ExprPostGraph::new();
        let input = sym(&mut g, 0);
        let min = {
            let node = g.add_node(ExprKind::Constant);
            g.set_leaf_data(node, ExprPayload::Int(APInt::new_signed(32, 3)));
            node
        };
        let max = {
            let node = g.add_node(ExprKind::Constant);
            g.set_leaf_data(node, ExprPayload::Int(APInt::new_signed(32, 10)));
            node
        };
        inner(&mut g, ExprKind::Clamp, &[input, min, max]);
        assert_eq!(as_i64(execute(&g, &[iv(20)])), 10);
    }

    #[test]
    fn float_constant() {
        let mut g = ExprPostGraph::new();
        flt_con(&mut g, 3.125);
        assert!((as_f64(execute(&g, &[])) - 3.125).abs() < 1e-9);
    }

    #[test]
    fn float_lt_true() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Lt, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[fv(1.0), fv(2.0)])), 1);
    }

    #[test]
    fn loop_sums_induction_variable_with_symbolic_bound() {
        // acc = 0; for i in 0..n { acc = acc + i }  ==>  0+1+...+(n-1).
        let mut g = ExprPostGraph::new();
        let start = int_con(&mut g, 0);
        let end = sym(&mut g, 0); // n, a symbolic (runtime) bound
        let init = int_con(&mut g, 0);
        let ind = g.add_node(ExprKind::IndVar);
        let acc = g.add_node(ExprKind::Acc);
        let step = inner(&mut g, ExprKind::Add, &[acc, ind]);
        inner(&mut g, ExprKind::Loop, &[start, end, init, step]);

        assert_eq!(as_i64(execute(&g, &[iv(5)])), 1 + 2 + 3 + 4);
        assert_eq!(as_i64(execute(&g, &[iv(1)])), 0);
        assert_eq!(as_i64(execute(&g, &[iv(0)])), 0);
    }

    #[test]
    fn loop_accumulates_from_nonzero_init() {
        // acc = base; for i in 0..3 { acc = acc + step }  with step a symbol.
        let mut g = ExprPostGraph::new();
        let start = int_con(&mut g, 0);
        let end = int_con(&mut g, 3);
        let base = sym(&mut g, 0);
        let acc = g.add_node(ExprKind::Acc);
        let addend = sym(&mut g, 1);
        let step = inner(&mut g, ExprKind::Add, &[acc, addend]);
        inner(&mut g, ExprKind::Loop, &[start, end, base, step]);

        // 10 + 4 + 4 + 4 = 22.
        assert_eq!(as_i64(execute(&g, &[iv(10), iv(4)])), 22);
    }

    #[test]
    fn nested_loop_multiplies_via_repeated_addition() {
        // acc = 0; for i in 0..a { acc = acc + b }  ==> a*b, with b symbolic.
        let mut g = ExprPostGraph::new();
        let start = int_con(&mut g, 0);
        let a = sym(&mut g, 0);
        let init = int_con(&mut g, 0);
        let acc = g.add_node(ExprKind::Acc);
        let b = sym(&mut g, 1);
        let step = inner(&mut g, ExprKind::Add, &[acc, b]);
        inner(&mut g, ExprKind::Loop, &[start, a, init, step]);

        assert_eq!(as_i64(execute(&g, &[iv(6), iv(7)])), 42);
    }

    #[test]
    fn float_lt_false() {
        let mut g = ExprPostGraph::new();
        let a = sym(&mut g, 0);
        let b = sym(&mut g, 1);
        inner(&mut g, ExprKind::Lt, &[a, b]);
        assert_eq!(as_i64(execute(&g, &[fv(3.0), fv(2.0)])), 0);
    }
}
