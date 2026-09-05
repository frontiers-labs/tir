use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Unknown dialect '{0}'")]
    UnknownDialect(String),
    #[error("Unknown operation '{1}' in dialect '{0}'")]
    UnknownOperation(String, String),
    #[error("Unknown type '{1}' in dialect '{0}'")]
    UnknownType(String, String),
    #[error("Expected '{0}'")]
    ExpectedToken(&'static str),
    #[error("Expected operation name in format 'op_name' or 'dialect_name.op_name'")]
    ExpectedOpName,
    #[error("Expected '{0}.{1}'")]
    ExpectedOperation(&'static str, &'static str),
    #[error("Expected type")]
    ExpectedType,
    #[error("Expected value reference")]
    ExpectedValueRef,
    #[error("Expected symbol name")]
    ExpectedSymbolName,
    #[error("Unknown value reference '%{0}'")]
    UnknownValueRef(String),
    #[error("Unknown attribute alias '#{0}'")]
    UnknownAttributeAlias(String),
    #[error("'{1}' is not a comparison predicate for '{0}'")]
    InvalidPredicate(String, String),
    #[error("Operation verification failed: {0}")]
    VerificationError(String),
}
