use tir_adt::{APFloat, APInt, RawBits};
use tir_graph::Matchable;

mod exec;
mod infer;
mod sexpr;

pub use exec::{Memory, execute, execute_with_memory};
pub use infer::{canonicalize_for_selection, infer_widths};
pub use sexpr::{BuildError, SemBuilderHooks, SemExpr, build, op_kind, op_name, parse};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SymKind {
    Symbol,
    Constant,
    Add,
    Sub,
    Mul,
    Div,
    UDiv,
    /// Signed and unsigned remainder (truncated division).
    SRem,
    URem,
    /// Unary two's-complement negation.
    Neg,
    Eq,
    Ne,
    Lt,
    Le,
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
    /// Bit concatenation; first operand occupies the high bits, width is the sum.
    Concat,
    /// Arguments are condition, then branch, else branch
    // #[arity = 3]
    If,
    // #[arity = 3]
    Clamp,
    /// Args: address, bytes read, metadata. Metadata is nonsemantic; `SExt`/`ZExt` model signedness.
    // #[arity = 3]
    LoadMemory,
    /// Arguments are address, bytes written, value, address-space metadata.
    // #[arity = 4]
    StoreMemory,
    ZExt,
    SExt,
    /// Bit-field extract: args value, high, low (inclusive) -> `high-low+1` bits. The
    /// canonical truncation/slice; no separate `Trunc` (`Trunc(x,n) == Extract(x,n-1,0)`).
    // #[arity = 3]
    Extract,
    // #[arity = 1]
    Log2Ceil,
    // #[arity = 1]
    Sqrt,
    // #[arity = 3]
    Fma,
    /// `[iter, body]`: map `body` over each lane, element bound via `Arg(0)` (or
    /// `Arg(0)`/`Arg(1)` for a `Zip` pair); value is the iterator of results.
    // #[arity = 2]
    Map,
    /// `[lhs, rhs]`: pair two iterators lane-wise into element `i` = `[lhs[i], rhs[i]]`.
    // #[arity = 2]
    Zip,
    /// Concatenate an iterator's lanes into one bit value, lane 0 low. Inverse of `Split`.
    // #[arity = 1]
    IterConcat,
    /// `[bits, n]`: split into `n` equal lanes, lane 0 low. Inverse of `IterConcat`.
    // #[arity = 2]
    Split,
    /// `[iter, body]`: left-fold from lane 0, `Arg(0)`=acc, `Arg(1)`=lane; value is final acc.
    // #[arity = 2]
    Reduce,
    /// The k-th parameter of the innermost enclosing `Map`/`Reduce` lambda (`Int` payload).
    Arg,
}

impl SymKind {
    /// Whether the operator is commutative in its two operands.
    pub fn is_commutative(&self) -> bool {
        matches!(
            self,
            SymKind::Add | SymKind::Mul | SymKind::And | SymKind::Or | SymKind::Xor
        )
    }

    /// Structural arity: number of operand children.
    pub fn arity(&self) -> usize {
        match self {
            SymKind::Symbol | SymKind::Constant | SymKind::Arg => 0,
            SymKind::Not
            | SymKind::Neg
            | SymKind::Log2Ceil
            | SymKind::Sqrt
            | SymKind::IterConcat => 1,
            SymKind::If
            | SymKind::Clamp
            | SymKind::Extract
            | SymKind::LoadMemory
            | SymKind::Fma => 3,
            SymKind::StoreMemory => 4,
            _ => 2,
        }
    }
}

/// Structural matcher facts; context `C` is ignored so a label matches in any context.
impl<C> Matchable<C> for SymKind {
    fn is_leaf(&self, _: &C) -> bool {
        self.arity() == 0
    }

    fn num_children(&self, _: &C) -> usize {
        self.arity()
    }

    fn is_commutative(&self) -> bool {
        SymKind::is_commutative(self)
    }

    fn is_constant(&self) -> bool {
        matches!(self, SymKind::Constant)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SymPayload<V> {
    SymbolId(u32),
    Value(V),
    Int(APInt),
    Float(APFloat),
}

/// A runtime value produced by the expression interpreter.
#[derive(Clone, Debug)]
pub enum Value {
    Int(APInt),
    Float(APFloat),
    /// A fixed-size array of values, like a vector.
    Iterator(Vec<Value>),
    /// An untyped bag of bits.
    RawBits(RawBits),
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Iterator(a), Value::Iterator(b)) => a == b,
            _ => false,
        }
    }
}
