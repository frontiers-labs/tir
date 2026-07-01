use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to parse LLVM IR: {0}")]
    Parse(String),
    #[error("unsupported instruction: {0}")]
    Unsupported(String),
    #[error("reference to undefined value '{0}'")]
    UndefinedValue(String),
    #[error("branch to undefined block '{0}'")]
    UndefinedBlock(String),
}
