//! Data-driven assembly syntax for text-only (objectless) targets.
//!
//! For pseudo-ISAs like PTX whose textual structure the shared flat assembler
//! cannot represent, the TMDL backend emits an [`InstrSyntax`] table describing
//! each instruction's assembly form as an ordered sequence of literal text and
//! typed operand slots (derived from the same `asm { "..." }` format string that
//! drives printing). A target-specific front-end interprets this table to parse
//! and print instruction bodies without a hand-written entry per mnemonic.

/// One element of an instruction's assembly form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsmSyntaxPart {
    /// Literal text (mnemonic, punctuation, register sigils, whitespace) that is
    /// emitted verbatim and matched when parsing.
    Text(&'static str),
    /// An operand slot. `class` is the register-class name for a register
    /// operand, or `None` for an immediate.
    Operand {
        name: &'static str,
        class: Option<&'static str>,
    },
}

/// The assembly syntax of one instruction: its dialect op name (the key the ASM
/// printer and op registry use), the emitted mnemonic, and its ordered parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstrSyntax {
    pub op_name: &'static str,
    pub mnemonic: &'static str,
    pub parts: &'static [AsmSyntaxPart],
}
