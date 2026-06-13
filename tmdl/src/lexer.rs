use core::fmt;
use std::fmt::Write;

use chumsky::prelude::*;

use crate::Spanned;

// Token definition
#[derive(Debug, Clone, PartialEq)]
pub enum Token<'a> {
    Comment(&'a str),
    Identifier(&'a str),
    Number(&'a str),
    StringLit(&'a str),

    /// `=>`
    FatArrow,
    /// `..`
    Range,

    /// `=`
    Equals,

    /// `+`
    Plus,
    /// `-`
    Dash,
    /// `/`
    ForwardSlash,
    /// `*`
    Asterisk,
    /// `&`
    Ampersand,
    /// `^`
    Hat,
    /// `!`
    Bang,
    /// `~`
    Tilde,

    /// `.`
    Dot,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `;`
    Semicolon,
    /// `\`
    BackSlash,

    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `<`
    LAngle,
    /// `>`
    RAngle,

    /// `|`
    Pipe,

    /// `isa`
    KwIsa,
    /// `requires`
    KwRequires,
    /// `register_class`
    KwRegClass,
    /// `for`
    KwFor,
    /// `in`
    KwIn,
    /// `registers`
    KwRegisters,
    /// `parameters`
    KwParameters,
    /// `template`
    KwTemplate,
    /// `instruction`
    KwInstruction,
    /// `param`
    KwParam,
    /// `operands`
    KwOperands,
    /// `encoding`
    KwEncoding,
    /// `if`
    KwIf,
    /// `else`
    KwElse,
    /// `asm`
    KwAsm,
    /// `behavior`
    KwBehavior,
    /// `unit`
    KwUnit,
    /// `machine`
    KwMachine,
    /// `buffers`
    KwBuffers,
    /// `bind`
    KwBind,
    /// `schedule`
    KwSchedule,
    /// `pipeline`
    KwPipeline,
    /// `override`
    KwOverride,
    /// `forward`
    KwForward,
    /// `sched_class`
    KwSchedClass,
    /// `reg_file`
    KwRegFile,
    /// `try`
    KwTry,
    /// `except`
    KwExcept,
}

impl<'a> Token<'a> {
    pub fn as_ident(&self) -> &'a str {
        if let Token::Identifier(ident) = self {
            ident
        } else {
            unreachable!()
        }
    }
}

pub fn lex<'src>(source: &'src str) -> (Vec<Spanned<Token<'src>>>, Vec<Cheap>) {
    let (tokens, errors) = lexer().parse(source).into_output_errors();

    (tokens.unwrap_or_default(), errors)
}

pub(crate) fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<Spanned<Token<'src>>>, extra::Err<Cheap>> {
    let num = just("0b")
        .then(text::int(2).repeated().at_least(1))
        .to_slice()
        .or(just("0x")
            .then(text::int(16).repeated().at_least(1))
            .to_slice())
        .or(text::int(10).repeated().at_least(1).to_slice())
        .map(|n: &str| Token::Number(n));

    let str_ = just('"')
        .ignore_then(none_of('"').repeated().to_slice())
        .then_ignore(just('"'))
        .map(|s: &str| Token::StringLit(s));

    let control = choice((
        just('{').to(Token::LBrace),
        just('}').to(Token::RBrace),
        just('[').to(Token::LBracket),
        just(']').to(Token::RBracket),
        just('(').to(Token::LParen),
        just(')').to(Token::RParen),
        just('<').to(Token::LAngle),
        just('>').to(Token::RAngle),
        just(',').to(Token::Comma),
        just(':').to(Token::Colon),
        just(';').to(Token::Semicolon),
    ));

    let op = choice((
        just("=>").to(Token::FatArrow),
        just("..").to(Token::Range),
        just('=').to(Token::Equals),
        just('!').to(Token::Bang),
        just('~').to(Token::Tilde),
        just('+').to(Token::Plus),
        just('-').to(Token::Dash),
        just('*').to(Token::Asterisk),
        just('/').to(Token::ForwardSlash),
        just('|').to(Token::Pipe),
        just('.').to(Token::Dot),
        just('&').to(Token::Ampersand),
        just('^').to(Token::Hat),
    ));

    let ident = text::ascii::ident().map(|ident: &str| match ident {
        "isa" => Token::KwIsa,
        "requires" => Token::KwRequires,
        "for" => Token::KwFor,
        "in" => Token::KwIn,
        "registers" => Token::KwRegisters,
        "register_class" => Token::KwRegClass,
        "parameters" => Token::KwParameters,
        "template" => Token::KwTemplate,
        "instruction" => Token::KwInstruction,
        "param" => Token::KwParam,
        "operands" => Token::KwOperands,
        "encoding" => Token::KwEncoding,
        "if" => Token::KwIf,
        "else" => Token::KwElse,
        "asm" => Token::KwAsm,
        "behavior" => Token::KwBehavior,
        "unit" => Token::KwUnit,
        "machine" => Token::KwMachine,
        "buffers" => Token::KwBuffers,
        "bind" => Token::KwBind,
        "schedule" => Token::KwSchedule,
        "pipeline" => Token::KwPipeline,
        "override" => Token::KwOverride,
        "forward" => Token::KwForward,
        "sched_class" => Token::KwSchedClass,
        "reg_file" => Token::KwRegFile,
        "try" => Token::KwTry,
        "except" => Token::KwExcept,
        _ => Token::Identifier(ident),
    });

    let token = str_.or(num).or(control).or(op).or(ident);

    let comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .padded();

    token
        .padded_by(comment.repeated())
        .padded()
        .map_with(|tok, e| (tok, e.span()))
        .recover_with(skip_then_retry_until(any().ignored(), end()))
        .repeated()
        .collect()
}

impl<'a> fmt::Display for Token<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Dot => f.write_str("."),
            Token::Asterisk => f.write_str("*"),
            Token::Identifier(i) => f.write_str(i),
            Token::LBrace => f.write_str("{"),
            Token::RBrace => f.write_str("}"),
            Token::KwParameters => f.write_str("parameters"),
            Token::Comment(s) => write!(f, "#{}", s),
            Token::Number(n) => write!(f, "{}", n),
            Token::StringLit(s) => write!(f, "\"{}\"", s),
            Token::FatArrow => f.write_str("=>"),
            Token::Range => f.write_str(".."),
            Token::Equals => f.write_str("="),
            Token::Plus => f.write_str("+"),
            Token::Dash => f.write_str("-"),
            Token::Colon => f.write_char(':'),
            Token::Semicolon => f.write_char(';'),
            Token::ForwardSlash => f.write_str("/"),
            Token::BackSlash => f.write_str("\\"),
            Token::Comma => f.write_str(","),
            Token::Ampersand => f.write_char('&'),
            Token::Hat => f.write_char('^'),
            Token::Bang => f.write_char('!'),
            Token::Tilde => f.write_char('~'),
            Token::LBracket => f.write_str("["),
            Token::RBracket => f.write_str("]"),
            Token::LParen => f.write_str("("),
            Token::RParen => f.write_str(")"),
            Token::LAngle => f.write_str("<"),
            Token::RAngle => f.write_str(">"),
            Token::Pipe => f.write_str("|"),
            Token::KwIsa => f.write_str("isa"),
            Token::KwRequires => f.write_str("requires"),
            Token::KwRegClass => f.write_str("register_class"),
            Token::KwFor => f.write_str("for"),
            Token::KwIn => f.write_str("in"),
            Token::KwRegisters => f.write_str("registers"),
            Token::KwTemplate => f.write_str("template"),
            Token::KwInstruction => f.write_str("instruction"),
            Token::KwParam => f.write_str("param"),
            Token::KwOperands => f.write_str("operands"),
            Token::KwEncoding => f.write_str("encoding"),
            Token::KwIf => f.write_str("if"),
            Token::KwElse => f.write_str("else"),
            Token::KwAsm => f.write_str("asm"),
            Token::KwBehavior => f.write_str("behavior"),
            Token::KwUnit => f.write_str("unit"),
            Token::KwMachine => f.write_str("machine"),
            Token::KwBuffers => f.write_str("buffers"),
            Token::KwBind => f.write_str("bind"),
            Token::KwSchedule => f.write_str("schedule"),
            Token::KwPipeline => f.write_str("pipeline"),
            Token::KwOverride => f.write_str("override"),
            Token::KwForward => f.write_str("forward"),
            Token::KwSchedClass => f.write_str("sched_class"),
            Token::KwRegFile => f.write_str("reg_file"),
            Token::KwTry => f.write_str("try"),
            Token::KwExcept => f.write_str("except"),
        }
    }
}

#[cfg(test)]
mod test {
    use chumsky::Parser;

    use super::lexer;

    #[test]
    fn smoke_lexer() {
        let input = "
          isa RV32I {
              XLEN = 32
          }
       ";

        let parser = lexer();
        let result = parser.parse(input);

        println!("{:#?}", result);
    }
}
