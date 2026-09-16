#[path = "exec_pure.rs"]
mod pure;
pub use pure::execute_pure;

use tir_adt::{APFloat, APInt, RawBits};
use tir_graph::{Dag, NodeId};

use crate::lang::{AtomicRmwOp, MemOrdering, SymKind, SymPayload, Value, scalar_op};

/// Memory backend for symbolic memory effects.
///
/// An error rejects the operation before it takes effect. A [`Continuation`]
/// retries that same operation on its next resume. Effectful backends should
/// override the byte methods so each wide access is accepted as one operation.
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
    Continuation::new(graph).resume(graph, symbols, memory)
}

enum EvalFrame {
    Node {
        node: NodeId,
        context: usize,
        next_child: usize,
    },
    Map {
        node: NodeId,
        context: usize,
        body: NodeId,
        elements: Option<Vec<Value>>,
        lane: usize,
        lane_context: Option<usize>,
        results: Vec<Value>,
    },
    Reduce {
        node: NodeId,
        context: usize,
        body: NodeId,
        elements: Option<Vec<Value>>,
        lane: usize,
        lane_context: Option<usize>,
        accumulator: Option<Value>,
    },
}

struct EvalContext {
    cache: Vec<Option<Value>>,
    args: Vec<Value>,
}

/// A suspended symbolic evaluation.
///
/// Successful nodes remain cached across [`resume`](Self::resume) calls. If a
/// memory backend returns an error, the exact effectful node remains pending and
/// is retried without evaluating its completed predecessors again. Keep the graph
/// and symbol bindings unchanged while an evaluation is suspended.
pub struct Continuation {
    root: NodeId,
    contexts: Vec<EvalContext>,
    stack: Vec<EvalFrame>,
}

impl Continuation {
    pub fn new<V>(graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>) -> Self {
        let root = graph.root().expect("cannot execute empty graph");
        Self {
            root,
            contexts: vec![EvalContext {
                cache: vec![None; graph.len()],
                args: vec![],
            }],
            stack: vec![EvalFrame::Node {
                node: root,
                context: 0,
                next_child: 0,
            }],
        }
    }

    pub fn resume<V, M: Memory>(
        &mut self,
        graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
        symbols: &[Value],
        memory: &mut M,
    ) -> Result<Value, M::Error> {
        if self.stack.is_empty() {
            return Ok(self.contexts[0].cache[self.root.index()]
                .as_ref()
                .expect("completed continuation must cache its root")
                .clone());
        }

        loop {
            match self.stack.last().expect("evaluation stack is not empty") {
                EvalFrame::Node {
                    node,
                    context,
                    next_child,
                } => {
                    let (node, context, next_child) = (*node, *context, *next_child);
                    self.step_node(graph, symbols, memory, node, context, next_child)?;
                }
                EvalFrame::Map { .. } => {
                    let frame = self.stack.pop().unwrap();
                    self.step_map(graph, frame);
                }
                EvalFrame::Reduce { .. } => {
                    let frame = self.stack.pop().unwrap();
                    self.step_reduce(graph, frame);
                }
            }

            if self.stack.is_empty() {
                return Ok(self.contexts[0].cache[self.root.index()]
                    .as_ref()
                    .expect("completed continuation must cache its root")
                    .clone());
            }
        }
    }

    fn step_node<V, M: Memory>(
        &mut self,
        graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
        symbols: &[Value],
        memory: &mut M,
        node: NodeId,
        context: usize,
        next_child: usize,
    ) -> Result<(), M::Error> {
        if self.contexts[context].cache[node.index()].is_some() {
            self.stack.pop();
            return Ok(());
        }

        let children: Vec<_> = graph.children(node).collect();
        match *graph.get_kind(node) {
            SymKind::Map | SymKind::Reduce if next_child == 0 => {
                let replacement = if *graph.get_kind(node) == SymKind::Map {
                    EvalFrame::Map {
                        node,
                        context,
                        body: children[1],
                        elements: None,
                        lane: 0,
                        lane_context: None,
                        results: vec![],
                    }
                } else {
                    EvalFrame::Reduce {
                        node,
                        context,
                        body: children[1],
                        elements: None,
                        lane: 0,
                        lane_context: None,
                        accumulator: None,
                    }
                };
                *self.stack.last_mut().unwrap() = replacement;
                return Ok(());
            }
            _ => {}
        }

        let child_index = match *graph.get_kind(node) {
            SymKind::If if next_child == 0 => Some(0),
            SymKind::If if next_child == 1 => {
                let condition = child_val(graph, node, 0, &self.contexts[context].cache);
                Some(if scalar_is_zero(condition) { 2 } else { 1 })
            }
            SymKind::Switch if next_child == 0 => Some(0),
            SymKind::Switch if next_child == 1 => {
                let Value::Int(index) = child_val(graph, node, 0, &self.contexts[context].cache)
                else {
                    panic!("switch requires an integer predicate");
                };
                let index = index.to_u64() as usize;
                Some((index + 1).min(children.len() - 1))
            }
            SymKind::If | SymKind::Switch => None,
            _ => (next_child < children.len()).then_some(next_child),
        };

        if let Some(index) = child_index {
            let child = children[index];
            if self.contexts[context].cache[child.index()].is_none() {
                self.stack.push(EvalFrame::Node {
                    node: child,
                    context,
                    next_child: 0,
                });
            } else if let EvalFrame::Node { next_child, .. } = self.stack.last_mut().unwrap() {
                *next_child += 1;
            }
            return Ok(());
        }

        let result = eval_ready(
            graph,
            node,
            symbols,
            &self.contexts[context].cache,
            &self.contexts[context].args,
            memory,
        )?;
        self.contexts[context].cache[node.index()] = Some(result);
        self.stack.pop();
        Ok(())
    }

    fn new_lambda_context(&mut self, outer: usize, binding: Value, len: usize) -> usize {
        let mut args = self.contexts[outer].args.clone();
        args.push(binding);
        self.contexts.push(EvalContext {
            cache: vec![None; len],
            args,
        });
        self.contexts.len() - 1
    }

    fn step_map<V>(
        &mut self,
        graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
        frame: EvalFrame,
    ) {
        let EvalFrame::Map {
            node,
            context,
            body,
            mut elements,
            lane,
            lane_context,
            mut results,
        } = frame
        else {
            unreachable!()
        };
        if elements.is_none() {
            let iter = graph.children(node).next().unwrap();
            match self.contexts[context].cache[iter.index()].clone() {
                Some(Value::Iterator(values)) => elements = Some(values),
                Some(_) => panic!("map requires an iterator operand"),
                None => {
                    self.stack.push(EvalFrame::Map {
                        node,
                        context,
                        body,
                        elements,
                        lane,
                        lane_context,
                        results,
                    });
                    self.stack.push(EvalFrame::Node {
                        node: iter,
                        context,
                        next_child: 0,
                    });
                    return;
                }
            }
        }
        let values = elements.as_ref().unwrap();
        if lane == values.len() {
            self.contexts[context].cache[node.index()] = Some(Value::Iterator(results));
            return;
        }
        if let Some(lane_context) = lane_context {
            if let Some(value) = self.contexts[lane_context].cache[body.index()].clone() {
                results.push(value);
                self.contexts.truncate(lane_context);
                self.stack.push(EvalFrame::Map {
                    node,
                    context,
                    body,
                    elements,
                    lane: lane + 1,
                    lane_context: None,
                    results,
                });
            }
            return;
        }
        let lane_context = self.new_lambda_context(context, values[lane].clone(), graph.len());
        self.stack.push(EvalFrame::Map {
            node,
            context,
            body,
            elements,
            lane,
            lane_context: Some(lane_context),
            results,
        });
        self.stack.push(EvalFrame::Node {
            node: body,
            context: lane_context,
            next_child: 0,
        });
    }

    fn step_reduce<V>(
        &mut self,
        graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
        frame: EvalFrame,
    ) {
        let EvalFrame::Reduce {
            node,
            context,
            body,
            mut elements,
            mut lane,
            lane_context,
            mut accumulator,
        } = frame
        else {
            unreachable!()
        };
        if elements.is_none() {
            let iter = graph.children(node).next().unwrap();
            match self.contexts[context].cache[iter.index()].clone() {
                Some(Value::Iterator(values)) => {
                    accumulator = Some(
                        values
                            .first()
                            .expect("reduce requires a non-empty iterator")
                            .clone(),
                    );
                    lane = 1;
                    elements = Some(values);
                }
                Some(_) => panic!("reduce requires an iterator operand"),
                None => {
                    self.stack.push(EvalFrame::Reduce {
                        node,
                        context,
                        body,
                        elements,
                        lane,
                        lane_context,
                        accumulator,
                    });
                    self.stack.push(EvalFrame::Node {
                        node: iter,
                        context,
                        next_child: 0,
                    });
                    return;
                }
            }
        }
        let values = elements.as_ref().unwrap();
        if lane == values.len() {
            self.contexts[context].cache[node.index()] = accumulator;
            return;
        }
        if let Some(lane_context) = lane_context {
            if let Some(value) = self.contexts[lane_context].cache[body.index()].clone() {
                self.contexts.truncate(lane_context);
                self.stack.push(EvalFrame::Reduce {
                    node,
                    context,
                    body,
                    elements,
                    lane: lane + 1,
                    lane_context: None,
                    accumulator: Some(value),
                });
            }
            return;
        }
        let binding = Value::Iterator(vec![
            accumulator.as_ref().unwrap().clone(),
            values[lane].clone(),
        ]);
        let lane_context = self.new_lambda_context(context, binding, graph.len());
        self.stack.push(EvalFrame::Reduce {
            node,
            context,
            body,
            elements,
            lane,
            lane_context: Some(lane_context),
            accumulator,
        });
        self.stack.push(EvalFrame::Node {
            node: body,
            context: lane_context,
            next_child: 0,
        });
    }
}

fn scalar_is_zero(value: Value) -> bool {
    match value {
        Value::Int(value) => value.is_zero(),
        Value::Float(value) => value.is_zero(),
        Value::Iterator(_) | Value::RawBits(_) => panic!("condition must be scalar"),
    }
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

/// Binary float arithmetic; all-integer operands never reach here, the `SCALAR_OPS` table takes them.
macro_rules! arith_op {
    ($c:ident, $float_m:ident, $op:literal) => {
        Value::Float(as_float!($c(0), $op).$float_m(&as_float!($c(1), $op)))
    };
}

/// Float comparison yielding a 1-bit `Int`; integer comparisons come from the `SCALAR_OPS` table.
macro_rules! cmp_op {
    ($c:ident, $float_m:ident, $op:literal) => {
        Value::Int(APInt::new(
            1,
            bool_result(as_float!($c(0), $op).$float_m(&as_float!($c(1), $op))),
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

fn integer_view(value: Value) -> Option<APInt> {
    match value {
        Value::Int(value) => Some(value),
        Value::RawBits(bits) => Some(bits.to_apint()),
        Value::Float(_) | Value::Iterator(_) => None,
    }
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
    let (Some(a), Some(b)) = (integer_view(c(0)), integer_view(c(1))) else {
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

fn eval_ready<V, M: Memory>(
    graph: &impl Dag<Node = SymKind, Leaf = SymPayload<V>>,
    node: NodeId,
    symbols: &[Value],
    cache: &[Option<Value>],
    args: &[Value],
    memory: &mut M,
) -> Result<Value, M::Error> {
    let c = |idx: usize| child_val(graph, node, idx, cache);

    // Integer division and remainder are total under SMT-LIB div-by-zero rules,
    // matching the bitblaster. An if-guarded behavior (e.g. riscv `div`) evaluates
    // its dead arm eagerly, so a zero divisor must fold rather than trap in the
    // asserting APInt path `scalar_op` would take.
    if let Some(result) = eval_divrem(*graph.get_kind(node), &c) {
        return Ok(result);
    }

    if let Some(op) = scalar_op(*graph.get_kind(node)) {
        let operands = (0..op.arity)
            .map(|index| integer_view(c(index)))
            .collect::<Option<Vec<_>>>();
        if let Some(operands) = operands {
            return Ok(Value::Int(op.eval_int(&operands)));
        }
    }

    let result = match *graph.get_kind(node) {
        kind if super::rounded::operation(kind).is_some() => super::rounded::evaluate(kind, &c).0,
        SymKind::FPFlags => {
            let operation = graph.children(node).next().unwrap();
            let kind = *graph.get_kind(operation);
            let (_, flags) =
                super::rounded::evaluate(kind, &|index| child_val(graph, operation, index, cache));
            Value::Int(APInt::new(5, u64::from(flags)))
        }
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
        kind @ (SymKind::If | SymKind::Theta | SymKind::Loop | SymKind::Port | SymKind::Clamp) => {
            eval_control(kind, &c)
        }
        SymKind::Switch => {
            let index = as_int!(c(0), "switch").to_u64() as usize;
            c((index + 1).min(graph.children(node).count() - 1))
        }
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
        SymKind::Add => arith_op!(c, add, "add"),
        SymKind::Sub => arith_op!(c, sub, "sub"),
        SymKind::Mul => arith_op!(c, mul, "mul"),
        _ => arith_op!(c, div, "div"),
    }
}

fn eval_compare(kind: SymKind, c: &impl Fn(usize) -> Value) -> Value {
    match kind {
        SymKind::Eq => Value::Int(APInt::new(1, bool_result(scalar_equal(c(0), c(1))))),
        SymKind::Ne => Value::Int(APInt::new(1, bool_result(!scalar_equal(c(0), c(1))))),
        SymKind::Lt => cmp_op!(c, lt, "lt"),
        SymKind::Le => cmp_op!(c, le, "le"),
        SymKind::Gt => cmp_op!(c, gt, "gt"),
        _ => cmp_op!(c, ge, "ge"),
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
            let exponent = as_int!(c(1), "fcvt").to_u64() as u32;
            let mantissa = as_int!(c(2), "fcvt").to_u64() as u32;
            match c(0) {
                Value::Int(value) => {
                    let (exp, mant) = float_format(value.width(), "fcvt");
                    let converted = APFloat::from_bits(exp, mant, false, value.to_u64() as u128)
                        .convert(exponent, mantissa, false);
                    Value::Int(APInt::new(
                        converted.bit_width(),
                        converted.to_bits() as u64,
                    ))
                }
                Value::Float(value) => Value::Float(value.convert(exponent, mantissa, false)),
                _ => panic!("fcvt requires a scalar operand"),
            }
        }
        SymKind::SIToFP | SymKind::UIToFP => {
            let signed = kind == SymKind::SIToFP;
            let value = as_int!(c(0), "integer to float").with_signed(signed);
            let exponent = as_int!(c(1), "integer to float").to_u64() as u32;
            let mantissa = as_int!(c(2), "integer to float").to_u64() as u32;
            let destination = match (exponent, mantissa) {
                (8, 23) => Some(tir_adt::FloatWidth::W32),
                (11, 52) => Some(tir_adt::FloatWidth::W64),
                _ => None,
            };
            if let Some(destination) = destination {
                let op = if signed {
                    tir_adt::FloatOp::SignedToFloat
                } else {
                    tir_adt::FloatOp::UnsignedToFloat
                };
                let bits = if signed {
                    value.to_i64() as u64
                } else {
                    value.to_u64()
                };
                let result = tir_adt::eval_float(
                    op,
                    tir_adt::FloatWidth::W64,
                    destination,
                    [bits, 0, 0],
                    tir_adt::RoundingMode::TiesToEven,
                );
                Value::Float(APFloat::from_bits(
                    exponent,
                    mantissa,
                    false,
                    result.bits as u128,
                ))
            } else {
                let value = if signed {
                    value.to_i64() as f64
                } else {
                    value.to_u64() as f64
                };
                Value::Float(APFloat::from_f64(value).convert(exponent, mantissa, false))
            }
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
                Value::Iterator(_) | Value::RawBits(_) => {
                    panic!("if condition must be scalar")
                }
            };
            if cond_zero { c(2) } else { c(1) }
        }
        SymKind::Theta | SymKind::Loop | SymKind::Port => {
            panic!("a loop requires loop-sequence semantics")
        }
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
            Value::Iterator(_) | Value::RawBits(_) => {
                panic!("fma requires scalar operands")
            }
        },
        SymKind::Sqrt => match c(0) {
            Value::Int(a) => {
                let v = a.to_u64();
                Value::Int(APInt::new(a.width(), (v as f64).sqrt() as u64))
            }
            Value::Float(a) => Value::Float(a.sqrt()),
            Value::Iterator(_) | Value::RawBits(_) => {
                panic!("sqrt requires a scalar operand")
            }
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
    let value = as_raw_bits(c(0)).to_apint();
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
                memory.write_memory(address, size, as_raw_bits(c(2)).to_apint().to_u64())?;
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
