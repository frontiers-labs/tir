use std::fmt;

#[derive(Debug)]
pub enum Error {
    UnknownDialect(String),
    UnknownOperation(String, String),
    UnknownType(String, String),
    ExpectedToken(&'static str),
    ExpectedOpName,
    ExpectedOperation(&'static str, &'static str),
    ExpectedType,
    ExpectedValueRef,
    ExpectedSymbolName,
    UnknownValueRef(String),
    UnknownAttributeAlias(String),
    InvalidPredicate(String, String),
    VerificationError(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDialect(d) => write!(f, "Unknown dialect '{d}'"),
            Self::UnknownOperation(d, op) => write!(f, "Unknown operation '{op}' in dialect '{d}'"),
            Self::UnknownType(d, ty) => write!(f, "Unknown type '{ty}' in dialect '{d}'"),
            Self::ExpectedToken(t) => write!(f, "Expected '{t}'"),
            Self::ExpectedOpName => {
                f.write_str("Expected operation name in format 'op_name' or 'dialect_name.op_name'")
            }
            Self::ExpectedOperation(d, op) => write!(f, "Expected '{d}.{op}'"),
            Self::ExpectedType => f.write_str("Expected type"),
            Self::ExpectedValueRef => f.write_str("Expected value reference"),
            Self::ExpectedSymbolName => f.write_str("Expected symbol name"),
            Self::UnknownValueRef(v) => write!(f, "Unknown value reference '%{v}'"),
            Self::UnknownAttributeAlias(a) => write!(f, "Unknown attribute alias '#{a}'"),
            Self::InvalidPredicate(d, p) => {
                write!(f, "'{p}' is not a comparison predicate for '{d}'")
            }
            Self::VerificationError(e) => write!(f, "Operation verification failed: {e}"),
        }
    }
}

impl std::error::Error for Error {}
