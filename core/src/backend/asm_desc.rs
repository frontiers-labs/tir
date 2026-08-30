//! Descriptor-driven assembly parsing and printing for the flat assembler.
//!
//! An instruction's assembly syntax is regular: parsing and printing are the
//! same ordered walk over literal text and typed operand slots, differing only
//! in the operand names, register classes and immediate ranges. Emitting that
//! walk as Rust code once per instruction made the generated backend sources
//! tens of thousands of lines of near-identical function bodies and dominated
//! the backend crates' compile time, so TMDL rustgen emits one static
//! [`InstrDesc`] per instruction instead — reached through its
//! [`InstrInfo::asm`](crate::backend::InstrInfo::asm) — and these helpers
//! interpret it at run time.
//!
//! An instruction whose syntax this table cannot express keeps a generated
//! parser/printer of its own; nothing here silently accepts a syntax the
//! descriptor does not describe.

use tir::attributes::{AttributeValue, NamedAttribute, RegisterAttr, find_attribute};
use tir::backend::{RegAssignment, reg_slot, slot_register};
use tir::parse::tokens::Parser;
use tir::{Context, OpHandle, Operation};

use crate::backend::Token;
use crate::backend::parser::parse_hex;
use crate::backend::regalloc::RegClassId;

/// Punctuation an instruction's syntax matches literally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsmSymbol {
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Star,
    Plus,
}

/// Parses one register of a fixed class, returning its encoding index.
pub type RegisterTokenParser = for<'src> fn(&mut Parser<'src, Token<'src>>) -> Option<u16>;

/// Renders a register of a fixed class by encoding index (`prefer_abi`).
pub type RegisterNamePrinter = fn(u16, bool) -> Option<String>;

/// One step of an instruction's assembly parse, in syntax order.
#[derive(Debug, Clone, Copy)]
pub enum ParseStep {
    Symbol(AsmSymbol),
    /// A `+` separating a base from an immediate: the sign it carries (or a
    /// `-`, or a negative literal in its place) applies to that immediate.
    Sign,
    /// A literal number in the syntax (x86 `shl dst, 1`).
    Number(&'static str),
    /// A literal identifier in the syntax (arm64 `lsl`).
    Keyword(&'static str),
    /// A register operand: attribute name, its class, and the class's parser.
    Register(&'static str, RegClassId, RegisterTokenParser),
    /// An immediate operand: attribute name, whether a preceding
    /// [`ParseStep::Sign`] applies to it, and the values it admits. A value
    /// outside them fails the candidate so per-mnemonic dispatch can backtrack
    /// to a wider form.
    Immediate(&'static str, bool, ImmConstraint),
}

/// The values an immediate operand admits: the half-open interval its `bits<N>`
/// width spells, plus the `#[align]`/`#[nonzero]` constraints it declares.
#[derive(Debug, Clone, Copy)]
pub struct ImmConstraint {
    pub range: Option<(i64, i64)>,
    /// The declared alignment; 1 when the operand declares none.
    pub align: u32,
    pub nonzero: bool,
}

impl ImmConstraint {
    /// Whether the operand admits `value`.
    pub fn admits(&self, value: i64) -> bool {
        if let Some((min, max)) = self.range
            && !(min..max).contains(&value)
        {
            return false;
        }
        (value as u64).is_multiple_of(u64::from(self.align)) && !(self.nonzero && value == 0)
    }
}

/// One element of an instruction's printed assembly, in syntax order.
#[derive(Debug, Clone, Copy)]
pub enum PrintPart {
    Text(&'static str),
    /// A register operand: attribute name and its class's name table.
    Register(&'static str, RegisterNamePrinter),
    /// An immediate operand, printed as a number, symbol or block label.
    Immediate(&'static str),
    /// A string-valued operand.
    Str(&'static str),
}

/// The assembly syntax of one instruction, as the flat assembler's parser and
/// printer consume it.
#[derive(Debug)]
pub struct InstrDesc {
    pub parse: &'static [ParseStep],
    pub print: &'static [PrintPart],
}

/// Parse an instruction body (the tokens after its mnemonic) per `desc`, build
/// the op from the parsed attributes and insert it.
#[allow(clippy::result_unit_err)]
pub fn parse_and_insert<'src, T: Operation, F: FnOnce(Vec<NamedAttribute>) -> T>(
    context: &Context,
    desc: &InstrDesc,
    parser: &mut Parser<'src, Token<'src>>,
    builder: &mut super::AsmCursor,
    build: F,
) -> Result<(), ()> {
    let attributes = parse_operands(context, desc.parse, parser)?;
    // A trailing comma means the input has more operands than this form — another
    // candidate with the same mnemonic (e.g. a masked ", v0.t" twin) must get its
    // turn.
    if matches!(parser.peek(), Some(Token::Comma)) {
        return Err(());
    }
    builder.insert(build(attributes));
    Ok(())
}

fn parse_operands<'src>(
    context: &Context,
    steps: &[ParseStep],
    parser: &mut Parser<'src, Token<'src>>,
) -> Result<Vec<NamedAttribute>, ()> {
    let mut attributes = Vec::new();
    let mut sign = 1i64;
    for step in steps {
        match step {
            ParseStep::Symbol(symbol) => {
                if !matches_symbol(parser.bump(), *symbol) {
                    return Err(());
                }
            }
            ParseStep::Sign => {
                sign = match parser.peek() {
                    Some(Token::Plus) => {
                        let _ = parser.bump();
                        1
                    }
                    Some(Token::Minus) => {
                        let _ = parser.bump();
                        -1
                    }
                    // A negative literal carries its own sign.
                    Some(Token::DecNumber(value) | Token::HexNumber(value))
                        if value.starts_with('-') =>
                    {
                        1
                    }
                    _ => return Err(()),
                };
            }
            ParseStep::Number(text) => match parser.bump() {
                Some(Token::DecNumber(value)) if value == text => {}
                _ => return Err(()),
            },
            ParseStep::Keyword(keyword) => match parser.bump() {
                Some(Token::Ident(name)) if name == keyword => {}
                _ => return Err(()),
            },
            ParseStep::Register(name, class, parse) => {
                let index = parse(parser).ok_or(())?;
                attributes.push(NamedAttribute::new(
                    context.intern(name),
                    AttributeValue::Register(RegisterAttr::Physical {
                        class: *class,
                        index,
                    }),
                ));
            }
            ParseStep::Immediate(name, signed, constraint) => {
                let value = parse_immediate(parser, *signed, sign, *constraint)?;
                attributes.push(NamedAttribute::new(context.intern(name), value));
            }
        }
    }
    Ok(attributes)
}

fn matches_symbol(token: Option<&Token<'_>>, symbol: AsmSymbol) -> bool {
    matches!(
        (token, symbol),
        (Some(Token::Comma), AsmSymbol::Comma)
            | (Some(Token::LParen), AsmSymbol::LParen)
            | (Some(Token::RParen), AsmSymbol::RParen)
            | (Some(Token::LBracket), AsmSymbol::LBracket)
            | (Some(Token::RBracket), AsmSymbol::RBracket)
            | (Some(Token::Star), AsmSymbol::Star)
            | (Some(Token::Plus), AsmSymbol::Plus)
    )
}

fn parse_immediate<'src>(
    parser: &mut Parser<'src, Token<'src>>,
    signed: bool,
    sign: i64,
    constraint: ImmConstraint,
) -> Result<AttributeValue, ()> {
    let value = match parser.peek() {
        Some(Token::DecNumber(number)) => number.parse::<i64>().map_err(|_| ())?,
        Some(Token::HexNumber(number)) => parse_hex(number)?,
        // A bare identifier in an immediate position is a symbol reference,
        // resolved at object emission. It carries no sign to negate.
        Some(Token::Ident(name)) => {
            if signed && sign < 0 {
                return Err(());
            }
            let symbol = (*name).to_string();
            let _ = parser.bump();
            return Ok(AttributeValue::Str(symbol.into()));
        }
        _ => return Err(()),
    };
    let value = if signed {
        value.checked_mul(sign).ok_or(())?
    } else {
        value
    };
    if !constraint.admits(value) {
        return Err(());
    }
    let _ = parser.bump();
    Ok(AttributeValue::Int(value))
}

/// Render an instruction's assembly per `desc`, or `None` if an operand
/// attribute is missing or holds a value the syntax cannot print.
///
/// A register slot holding a value prints the register `assignment` placed it
/// in; a slot naming a physical register prints that one.
pub fn print(
    desc: &InstrDesc,
    context: &Context,
    op: &OpHandle,
    assignment: &RegAssignment,
) -> Option<String> {
    let attributes = &op.attributes();
    let mut out = String::new();
    for part in desc.print {
        match part {
            PrintPart::Text(text) => out.push_str(text),
            PrintPart::Register(name, print) => {
                let slot = reg_slot(op, name)?;
                let (_, index) = slot_register(slot, assignment)?;
                out.push_str(&print(index, false)?)
            }
            PrintPart::Immediate(name) => match &find_attribute(context, attributes, name)?.value {
                AttributeValue::Int(value) => out.push_str(&value.to_string()),
                AttributeValue::UInt(value) => out.push_str(&value.to_string()),
                AttributeValue::Str(symbol) => out.push_str(symbol),
                // A local branch target: print the block's label, falling back
                // to `.L<n>` for unnamed blocks.
                AttributeValue::Block(block) => match context.get_block(*block).attr("name") {
                    Some(AttributeValue::Str(label)) => out.push_str(&label),
                    _ => {
                        out.push_str(".L");
                        out.push_str(&block.number().to_string());
                    }
                },
                _ => return None,
            },
            PrintPart::Str(name) => match &find_attribute(context, attributes, name)?.value {
                AttributeValue::Str(value) => out.push_str(value),
                _ => return None,
            },
        }
    }
    Some(out)
}
