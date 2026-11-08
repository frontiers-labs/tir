//! Source diagnostics from lexing, parsing, checking, and evaluation.
use crate::ast::Span;
use std::fmt;

/// Compiler phase responsible for a rejected source construct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Lex,
    Parse,
    Resolve,
    Type,
    Termination,
    Evaluate,
}
/// A source-located compiler error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Logical source module name.
    pub source: String,
    /// Original byte range.
    pub span: Span,
    /// Rejecting phase.
    pub phase: Phase,
    /// Explanation of the unmet obligation.
    pub message: String,
}
impl Diagnostic {
    pub(crate) fn new(source: &str, span: Span, phase: Phase, message: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            span,
            phase,
            message: message.into(),
        }
    }
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}..{}: {:?}: {}",
            self.source, self.span.start, self.span.end, self.phase, self.message
        )
    }
}
impl std::error::Error for Diagnostic {}
