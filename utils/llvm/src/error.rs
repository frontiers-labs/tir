use std::fmt;

#[derive(Debug)]
pub enum Error {
    Parse(String),
    Unsupported(String),
    UndefinedValue(String),
    UndefinedBlock(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "failed to parse LLVM IR: {e}"),
            Self::Unsupported(i) => write!(f, "unsupported instruction: {i}"),
            Self::UndefinedValue(v) => write!(f, "reference to undefined value '{v}'"),
            Self::UndefinedBlock(b) => write!(f, "branch to undefined block '{b}'"),
        }
    }
}

impl std::error::Error for Error {}
