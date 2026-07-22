use std::fmt;

use logos::Logos;

use crate::{Diagnostic, Span, Spanned};

#[derive(Logos, Clone, Debug, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n\f]+")]
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
pub enum Token {
    #[token("group")]
    Group,
    #[token("rule")]
    Rule,
    #[token("where")]
    Where,
    #[token("const")]
    Const,
    #[token("int")]
    Int,
    #[token("<=>")]
    Bidirectional,
    #[token("=>")]
    Forward,
    #[token("==")]
    EqualEqual,
    #[token("!=")]
    NotEqual,
    #[token("<=")]
    LessEqual,
    #[token(">=")]
    GreaterEqual,
    #[token("<<")]
    ShiftLeft,
    #[token(">>")]
    ShiftRight,
    #[token("&&")]
    LogicalAnd,
    #[token("||")]
    LogicalOr,
    #[token("=")]
    Equal,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("&")]
    Ampersand,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("!")]
    Bang,
    #[token("<")]
    Less,
    #[token(">")]
    Greater,
    #[token("#")]
    Hash,
    #[token("$")]
    Dollar,
    #[token(".")]
    Dot,
    #[token(",")]
    Comma,
    #[token(":")]
    Colon,
    #[token(";")]
    Semicolon,
    #[token("(")]
    LeftParen,
    #[token(")")]
    RightParen,
    #[regex(r#"\"([^\"\\]|\\.)*\""#, string_literal)]
    String(String),
    #[regex(r"0[xX][0-9a-fA-F]+|0[bB][01]+|[0-9]+", |lex| lex.slice().to_string())]
    Integer(String),
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*", |lex| lex.slice().to_string())]
    Identifier(String),
}

fn string_literal(lexer: &mut logos::Lexer<'_, Token>) -> String {
    let text = lexer.slice();
    text.strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
}

pub fn lex(source: &str) -> (Vec<Spanned<Token>>, Vec<Diagnostic>) {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();
    for (token, range) in Token::lexer(source).spanned() {
        let span = Span::from(range);
        match token {
            Ok(Token::Integer(value)) if parse_integer(&value).is_none() => {
                diagnostics.push(Diagnostic::new(
                    "integer literal is out of range",
                    "PDL integers must fit in a signed 64-bit value",
                    span,
                ));
            }
            Ok(token) => tokens.push((token, span)),
            Err(()) => diagnostics.push(Diagnostic::new(
                "invalid token",
                "this character does not start a PDL token",
                span,
            )),
        }
    }
    (tokens, diagnostics)
}

pub(crate) fn parse_integer(value: &str) -> Option<i64> {
    if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        i64::from_str_radix(value, 16).ok()
    } else if let Some(value) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        i64::from_str_radix(value, 2).ok()
    } else {
        value.parse().ok()
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identifier(value) | Self::Integer(value) => f.write_str(value),
            Self::String(value) => write!(f, "\"{value}\""),
            token => write!(f, "{token:?}"),
        }
    }
}
