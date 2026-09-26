//! Tokenizer for LLVM textual IR, built on `logos`. Newlines are significant —
//! LLVM instructions are line-terminated with no explicit separator — so `\n`
//! is its own token. Comments (`;` and `//`) are skipped; any byte `logos` cannot
//! classify is dropped, which keeps unsupported top-level lines (metadata,
//! attribute groups, target triples) from aborting the lex.

use chumsky::span::SimpleSpan;
use logos::Logos;

pub type Span = SimpleSpan;
pub type Spanned<'src> = (Token<'src>, Span);

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r]+")]
// `;`/`//` comments and string literals are dropped. `allow_greedy` silences
// logos' scan-to-end warning; clippy then sees the repeated meta as duplicated,
// which it is not (each decorates a distinct pattern).
#[allow(clippy::duplicated_attributes)]
#[logos(skip(r"(;|//)[^\n]*", allow_greedy = true))]
#[logos(skip(r#""[^"]*""#, allow_greedy = true))]
pub enum Token<'src> {
    #[token("\n")]
    Newline,
    #[token(",")]
    Comma,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("<")]
    LAngle,
    #[token(">")]
    RAngle,
    #[token("=")]
    Eq,
    #[token("*")]
    Star,
    #[token(":")]
    Colon,

    #[regex(r#"c\"([^\"\\]|\\.)*\""#, |l| &l.slice()[2..l.slice().len() - 1], priority = 7)]
    CString(&'src str),

    /// `iN`, carrying the bit width.
    #[regex(r"i[0-9]+", |l| l.slice()[1..].parse().ok(), priority = 5)]
    IntTy(u32),

    #[regex(r"-?[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?", |l| l.slice().parse().ok(), priority = 6)]
    Float(f64),

    #[regex(r"0x[0-9a-fA-F]+", |l| u64::from_str_radix(&l.slice()[2..], 16).ok(), priority = 7)]
    HexFloatBits(u64),

    #[regex(r"-?[0-9]+", |l| l.slice().parse().ok())]
    Int(i64),

    /// `%name` — an SSA value or a block label (the `%` is stripped).
    #[regex(r"%[A-Za-z0-9_.]+", |l| &l.slice()[1..])]
    Local(&'src str),

    /// `@name` — a global/function symbol (the `@` is stripped).
    #[regex(r"@[A-Za-z0-9_.]+", |l| &l.slice()[1..])]
    Global(&'src str),

    #[regex(r"[A-Za-z_.][A-Za-z0-9_.]*", |l| l.slice())]
    Ident(&'src str),
}

pub fn lex(src: &str) -> Vec<Spanned<'_>> {
    Token::lexer(src)
        .spanned()
        .filter_map(|(tok, span)| tok.ok().map(|t| (t, SimpleSpan::from(span))))
        .collect()
}
