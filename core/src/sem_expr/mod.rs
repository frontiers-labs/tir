use crate::{
    Operation, ValueId,
    graph::{MutDag, NodeId, PostOrderDag},
    helpers::SimpleNode,
    utils::{APFloat, APInt},
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
    Int(APInt),
    Float(APFloat),
}
