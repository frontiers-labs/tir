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
    /// IEEE 754 binary floating-point arithmetic. Distinct from the integer
    /// kinds so integer rewrites and untyped integer instruction patterns can
    /// never apply to float values. Over `Int` operands the bits are
    /// reinterpreted in the binary format of the operand width.
    FAdd,
    FSub,
    FMul,
    FDiv,
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
    /// Load-reserved: `[address, bytes, ordering]`. Reads memory and registers a
    /// reservation; value is the loaded word at `bytes*8` width.
    // #[arity = 3]
    LoadReserved,
    /// Store-conditional: `[address, bytes, value, ordering]`. Writes iff a valid
    /// reservation covers the access; value is `bits<1>`, 1 = success.
    // #[arity = 4]
    StoreConditional,
    /// Atomic read-modify-write: `[op, address, bytes, value, ordering]` where `op`
    /// is a constant [`AtomicRmwOp`] code 0..8; value is the OLD memory value at
    /// `bytes*8` width.
    // #[arity = 5]
    AtomicRmw,
    /// Memory/instruction fence: `[pred, succ, kind]` where `kind` 0 = data fence,
    /// 1 = instruction fence. `pred`/`succ` are target-defined ordering bit sets.
    // #[arity = 3]
    Fence,
}

impl SymKind {
    /// Whether the operator is commutative in its two operands.
    pub fn is_commutative(&self) -> bool {
        matches!(
            self,
            SymKind::Add
                | SymKind::Mul
                | SymKind::And
                | SymKind::Or
                | SymKind::Xor
                | SymKind::FAdd
                | SymKind::FMul
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
            | SymKind::LoadReserved
            | SymKind::Fence
            | SymKind::Fma => 3,
            SymKind::StoreMemory | SymKind::StoreConditional => 4,
            SymKind::AtomicRmw => 5,
            _ => 2,
        }
    }

    /// Whether a node of this kind may take `n` children. `Split` is the one
    /// variadic form: `split(x, n)` cuts into equal lanes, `split(x, n, w)`
    /// takes `n` lanes of `w` bits from the low end.
    pub fn accepts_arity(&self, n: usize) -> bool {
        match self {
            SymKind::Split => n == 2 || n == 3,
            _ => n == self.arity(),
        }
    }
}

/// The closed set of [`SymKind::AtomicRmw`] operations, coded 0..8 in the op child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum AtomicRmwOp {
    Add = 0,
    Swap = 1,
    Xor = 2,
    And = 3,
    Or = 4,
    Min = 5,
    Max = 6,
    MinU = 7,
    MaxU = 8,
}

impl AtomicRmwOp {
    /// The op for code 0..8, or `None` for any other value.
    pub fn from_code(code: u64) -> Option<Self> {
        Some(match code {
            0 => AtomicRmwOp::Add,
            1 => AtomicRmwOp::Swap,
            2 => AtomicRmwOp::Xor,
            3 => AtomicRmwOp::And,
            4 => AtomicRmwOp::Or,
            5 => AtomicRmwOp::Min,
            6 => AtomicRmwOp::Max,
            7 => AtomicRmwOp::MinU,
            8 => AtomicRmwOp::MaxU,
            _ => return None,
        })
    }

    /// Apply the operation at the operands' width; `old`/`val` must share a width.
    /// `Add` wraps; `Min`/`Max` compare signed, `MinU`/`MaxU` unsigned.
    pub fn apply(&self, old: APInt, val: APInt) -> APInt {
        match self {
            AtomicRmwOp::Add => old.add(&val),
            AtomicRmwOp::Swap => val,
            AtomicRmwOp::Xor => old.xor(&val),
            AtomicRmwOp::And => old.and(&val),
            AtomicRmwOp::Or => old.or(&val),
            AtomicRmwOp::Min => {
                if old.with_signed(true).slt(&val.with_signed(true)) {
                    old
                } else {
                    val
                }
            }
            AtomicRmwOp::Max => {
                if old.with_signed(true).sgt(&val.with_signed(true)) {
                    old
                } else {
                    val
                }
            }
            AtomicRmwOp::MinU => {
                if old.with_signed(false).ult(&val.with_signed(false)) {
                    old
                } else {
                    val
                }
            }
            AtomicRmwOp::MaxU => {
                if old.with_signed(false).ugt(&val.with_signed(false)) {
                    old
                } else {
                    val
                }
            }
        }
    }
}

/// Memory-ordering annotation carried by the atomic kinds, coded 0..4. Semantically
/// inert while there is one hart; recorded for a future interleaving model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum MemOrdering {
    Relaxed = 0,
    Acquire = 1,
    Release = 2,
    AcqRel = 3,
    SeqCst = 4,
}

impl MemOrdering {
    /// The ordering for code 0..4; any out-of-range value maps to `Relaxed`.
    pub fn from_code(code: u64) -> Self {
        match code {
            1 => MemOrdering::Acquire,
            2 => MemOrdering::Release,
            3 => MemOrdering::AcqRel,
            4 => MemOrdering::SeqCst,
            _ => MemOrdering::Relaxed,
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
