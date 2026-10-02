use std::fmt;

#[derive(Debug, Clone)]
pub enum TMDLError {
    Unknown,
    IO(String),
    Serialization(String),
    UnexpectedExpression,
    Codegen(String),
}

impl fmt::Display for TMDLError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("Unknown error"),
            Self::IO(e) => write!(f, "File error: {e}"),
            Self::Serialization(e) => write!(f, "Serialization error: {e}"),
            Self::UnexpectedExpression => f.write_str("Unexpected expression"),
            Self::Codegen(e) => write!(f, "Code generation error: {e}"),
        }
    }
}

impl std::error::Error for TMDLError {}

impl From<std::io::Error> for TMDLError {
    fn from(value: std::io::Error) -> Self {
        Self::IO(format!("{:?}", value))
    }
}

impl From<serde_json::Error> for TMDLError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}
