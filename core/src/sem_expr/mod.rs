use crate::{
    Operation, ValueId,
    graph::{MutDag, NodeId, PostOrderDag},
    helpers::SimpleNode,
    utils::{APFloat, APInt, RawBits},
};

mod discover;
mod exec;
mod infer;
mod unroll;

pub use discover::{EquivalenceOracle, FuzzOracle, confirm_extension_via_shifts};
pub use exec::{Memory, execute, execute_with_memory};
pub use infer::{canonicalize_for_selection, infer_widths};
pub use unroll::unroll_loops;

pub type ExprPostGraph = PostOrderDag<ExprKind, ExprPayload>;

/// Fold an operation over constant operand `values` by evaluating its declared
/// semantic expression. `values[i]` is the value of operand `i` (i.e. `SymbolId(i)`
/// in the op's `sem`). Returns `None` for ops without a semantic expression. This
/// backs the `ConstantFold` impl the `operation!` macro derives from `sem`.
pub fn fold_with_sem(op: &dyn Operation, values: &[Value]) -> Option<Value> {
    let mut graph = ExprPostGraph::new();
    op.semantic_expr(&mut graph)?;
    Some(execute(&graph, values))
}

pub trait AsSemExpr: Operation {
    fn convert(&self, g: &mut impl MutDag<Node = ExprKind, Leaf = ExprPayload>) -> NodeId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, SimpleNode)]
#[repr(u16)]
#[simple_node(default_arity = 2)]
pub enum ExprKind {
    #[leaf]
    Symbol,
    #[leaf]
    Constant,
    Add,
    Sub,
    Mul,
    Div,
    UDiv,
    Eq,
    Ne,
    Lt,
    Gt,
    Ge,
    ULt,
    ULe,
    UGt,
    UGe,
    ShiftLeft,
    ShiftRightArithmetic,
    ShiftRightLogic,
    Or,
    And,
    Xor,
    Not,
    /// Arguments are condition, then branch, else branch
    #[arity = 3]
    If,
    #[arity = 3]
    Clamp,
    /// Arguments are address, bytes read, signedness/address-space metadata.
    /// The third operand is nonsemantic for raw memory execution; explicit
    /// `SExt`/`ZExt` nodes model signedness.
    #[arity = 3]
    LoadMemory,
    /// Arguments are address, bytes written, value, address-space metadata.
    #[arity = 4]
    StoreMemory,
    ZExt,
    SExt,
    /// Bit-field extract: arguments are value, high bit, low bit (both inclusive).
    /// The result is the `high - low + 1` low bits. This is the single canonical
    /// representation of truncation/bit-slicing — there is deliberately no separate
    /// `Trunc` (`Trunc(x, n) == Extract(x, n-1, 0)`).
    #[arity = 3]
    Extract,
    #[arity = 1]
    Log2Ceil,
    #[arity = 1]
    Sqrt,
    #[arity = 3]
    Fma,
    /// Apply a function to each lane of an iterator. Arguments are `[iter, body]`:
    /// `body` is evaluated once per element with the element bound as the lambda's
    /// argument, read via `Arg(0)` (or `Arg(0)`/`Arg(1)` when the element is a pair
    /// produced by `Zip`). The node's value is the iterator of results.
    #[arity = 2]
    Map,
    /// Pair two iterators lane-wise. Arguments are `[lhs, rhs]`; the value is an
    /// iterator whose element `i` is the two-element iterator `[lhs[i], rhs[i]]`.
    /// Feeding a `Zip` into a `Map` lets a binary lambda read both sides via
    /// `Arg(0)`/`Arg(1)`.
    #[arity = 2]
    Zip,
    /// Concatenate the lanes of an iterator into a single bit value, lane 0 in the
    /// low bits. The inverse of `Split`. One argument: the iterator.
    #[arity = 1]
    IterConcat,
    /// Split a bit value into `n` equal-width lanes. Arguments are `[bits, n]`;
    /// the value is an iterator of `n` elements, lane 0 taken from the low bits.
    /// The inverse of `IterConcat`.
    #[arity = 2]
    Split,
    /// Left-fold a function over an iterator's lanes. Arguments are `[iter, body]`:
    /// the accumulator starts at lane 0 and, for each later lane, is replaced by
    /// `body` evaluated with `Arg(0)` bound to the accumulator and `Arg(1)` to the
    /// lane. The node's value is the final accumulator (e.g. a horizontal add).
    #[arity = 2]
    Reduce,
    /// The k-th parameter of the innermost enclosing `Map`/`Reduce` lambda. A leaf
    /// carrying its index as an `Int` payload; only meaningful inside that lambda's
    /// `body` subexpression.
    #[leaf]
    Arg,
    /// Bounded fold/reduce, the IR's first-class loop. Arguments are
    /// `[start, end, init, step]`: the accumulator begins at `init` and, for each
    /// integer `i` in the half-open range `[start, end)`, is replaced by `step`.
    /// Inside `step`, the current induction value is read with `IndVar` and the
    /// running accumulator with `Acc`. The node's value is the final accumulator.
    /// Bounds may be arbitrary subexpressions (the interpreter evaluates them at
    /// run time); backends without native iteration unroll it when the bounds are
    /// constant.
    #[arity = 4]
    Loop,
    /// The induction variable of the innermost enclosing `Loop`. Only meaningful
    /// inside that loop's `step` subexpression.
    #[leaf]
    IndVar,
    /// The running accumulator of the innermost enclosing `Loop`. Only meaningful
    /// inside that loop's `step` subexpression.
    #[leaf]
    Acc,
    /// Build a vector value by mapping over lanes. Arguments are `[count, elem]`:
    /// `elem` is evaluated once per lane index `i` in `[0, count)` with `IndVar`
    /// bound to `i`, and the node's value is the vector of those `count` elements.
    /// This is the vector counterpart of `Loop` — a map rather than a fold — and
    /// lets an elementwise vector operation and the target instruction that
    /// implements it lower to the same DAG. `elem` reads the induction value via
    /// `IndVar` and operand lanes via `Lane`.
    #[arity = 2]
    VectorMap,
    /// Read one lane of a vector value. Arguments are `[vector, index]`.
    #[arity = 2]
    Lane,
    /// A conditional control transfer: branch to the op's target when the single
    /// child (the condition) is nonzero, else fall through. The target is not a
    /// semantic value — it is a block reference carried as an op attribute and
    /// resolved at emit time — so it never appears in the expression. This is an
    /// effect node, never a pure value: it roots a match and is never internalized.
    #[arity = 1]
    CondBranch,
}

impl ExprKind {
    /// Whether the operator is commutative in its two operands, so a builder may
    /// canonicalize operand order and the matcher may match either order. This is
    /// the single source of truth shared by the program-graph builder and the
    /// e-graph node label.
    pub fn is_commutative(&self) -> bool {
        matches!(
            self,
            ExprKind::Add | ExprKind::Mul | ExprKind::And | ExprKind::Or | ExprKind::Xor
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprPayload {
    SymbolId(u32),
    Value(ValueId),
    Int(APInt),
    Float(APFloat),
}

/// A runtime value produced by the expression interpreter.
#[derive(Clone, Debug)]
pub enum Value {
    /// Arbitrary-precision integers
    Int(APInt),
    /// Arbitrary-precision floats
    Float(APFloat),
    /// A fixed-size array of other types, like a vector
    Iterator(Vec<Value>),
    /// An untyped bag of bits
    RawBits(RawBits),
}
