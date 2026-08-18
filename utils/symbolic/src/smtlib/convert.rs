//! Conversion between the SMT-LIB AST and the evaluatable [`crate::lang`] graph,
//! over Core (Bool, as 1-bit) + FixedSizeBitVectors only.

mod lift;
mod lower;

pub use lift::{lift_script, lift_term};
pub use lower::{Lowered, lower_script};

use std::fmt::{self, Display, Formatter};

/// A free variable in the lowered graph, indexed by its `SymbolId`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolInfo {
    pub name: String,
    pub width: Option<u32>,
    /// Sort was `Bool`, not a 1-bit bit-vector; re-emitted as `Bool` when lifting.
    pub is_bool: bool,
}

/// Why a term or graph could not be converted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConvertError {
    /// `forall`/`exists` cannot be reduced to a constant by substitution.
    Quantifier,
    UnknownSymbol(String),
    /// A construct outside the Core + BitVec subset.
    Unsupported(String),
    BadArity {
        op: String,
        expected: String,
        got: usize,
    },
    BadLiteral(String),
    UnknownWidth(String),
}

impl Display for ConvertError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::Quantifier => {
                f.write_str("quantifiers are not evaluatable and cannot be lowered")
            }
            ConvertError::UnknownSymbol(s) => write!(f, "unknown symbol `{s}`"),
            ConvertError::Unsupported(s) => write!(f, "unsupported construct: {s}"),
            ConvertError::BadArity { op, expected, got } => {
                write!(f, "`{op}` expects {expected} arguments, got {got}")
            }
            ConvertError::BadLiteral(s) => write!(f, "invalid literal: {s}"),
            ConvertError::UnknownWidth(s) => write!(f, "could not determine width for {s}"),
        }
    }
}

impl std::error::Error for ConvertError {}
