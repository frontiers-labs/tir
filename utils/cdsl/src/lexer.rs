//! Zero-copy Logos lexer for CDSL v3 with significant and lossless modes.

use std::ops::Range;

use logos::Logos;

#[derive(Clone, Debug)]
pub enum Span {
    Source(Range<usize>),
    Macro {
        start: proc_macro::Span,
        end: proc_macro::Span,
    },
}

#[derive(Clone, Debug)]
pub struct LexError {
    pub span: Span,
    pub message: String,
}

/// Controls whether the shared scanner returns trivia to its caller.
#[derive(Clone, Copy, Debug, Default)]
pub enum LexMode {
    /// Compiler input skips trivia inside the scanner.
    #[default]
    Significant,
    /// Formatting input retains every source token.
    Lossless,
}

/// Zero-copy token referencing slices of the input source directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Logos)]
#[logos(extras = LexMode)]
pub enum Token<'src> {
    #[regex(r"[\x09-\x0D\x20\x85\u200E\u200F\u2028\u2029]+", trivia)]
    Whitespace,
    #[token("//", line_comment)]
    LineComment,
    #[token("/*", block_comment)]
    BlockComment,
    #[regex(r"(\p{XID_Start}|_)(\p{XID_Continue})*", |lex| identifier(lex.slice()))]
    Ident(&'src str),
    #[regex(r"r#(\p{XID_Start}|_)(\p{XID_Continue})*", |lex| raw_identifier(lex.slice()))]
    RawIdent(&'src str),
    #[regex(r"[0-9][0-9_]*", |lex| lex.slice())]
    Natural(&'src str),
    #[regex(r#""([^"\\]|\\(?s:.))*""#, |lex| validate_string(lex.slice()))]
    Str(&'src str),
    #[token("_", priority = 3)]
    Wildcard,
    #[token("pub")]
    Pub,
    #[token("mod")]
    Mod,
    #[token("use")]
    Use,
    #[token("as")]
    As,
    #[token("type")]
    Type,
    #[token("enum")]
    Enum,
    #[token("let")]
    Let,
    #[token("rec")]
    Rec,
    #[token("match")]
    Match,
    #[token("memo")]
    Memo,
    #[token("op")]
    Op,
    #[token("extract")]
    Extract,
    #[token("Prop")]
    Prop,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("::")]
    PathSep,
    #[token("->")]
    Arrow,
    #[token("~>")]
    TildeArrow,
    #[token("=>")]
    FatArrow,
    #[token("..")]
    DotDot,
    #[token("++")]
    PlusPlus,
    #[token("||")]
    OrOr,
    #[token("&&")]
    AndAnd,
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token("<=")]
    LessEq,
    #[token(">=")]
    GreaterEq,
    #[token("|")]
    Pipe,
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
    #[token("!")]
    Bang,
    #[token("=")]
    Assign,
    #[token(":")]
    Colon,
    #[token(";")]
    Semi,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token("?")]
    Question,
    #[token("<")]
    Less,
    #[token(">")]
    Greater,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
}

#[inline]
fn trivia<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> logos::Filter<()> {
    match lex.extras {
        LexMode::Significant => logos::Filter::Skip,
        LexMode::Lossless => logos::Filter::Emit(()),
    }
}

#[inline]
fn line_comment<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> logos::Filter<()> {
    let bytes = lex.remainder().as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
        i += 1;
    }
    lex.bump(i);
    trivia(lex)
}

fn block_comment<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> logos::FilterResult<(), ()> {
    let bytes = lex.remainder().as_bytes();
    let mut depth = 1usize;
    let mut index = 0;
    while index + 1 < bytes.len() {
        match &bytes[index..index + 2] {
            b"/*" => {
                depth += 1;
                index += 2;
            }
            b"*/" => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    lex.bump(index);
                    return match lex.extras {
                        LexMode::Significant => logos::FilterResult::Skip,
                        LexMode::Lossless => logos::FilterResult::Emit(()),
                    };
                }
            }
            _ => index += 1,
        }
    }
    lex.bump(bytes.len());
    logos::FilterResult::Error(())
}

#[inline]
fn identifier(text: &str) -> Option<&str> {
    // Fast path for pure ASCII identifiers.
    if text.is_ascii() || !text.contains(['\u{200c}', '\u{200d}']) {
        Some(text)
    } else {
        None
    }
}

#[inline]
fn raw_identifier(text: &str) -> Option<&str> {
    let name = &text[2..];
    if matches!(name, "_" | "crate" | "self" | "super" | "Self") {
        None
    } else {
        identifier(name)
    }
}

#[inline]
fn validate_string(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    // Fast path: no escapes and no bare CR.
    let bytes = inner.as_bytes();
    if !bytes.contains(&b'\\') && !bytes.contains(&b'\r') {
        return Some(inner);
    }
    validate_string_slow(inner).then_some(inner)
}

fn validate_string_slow(inner: &str) -> bool {
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if ch == '\r' && chars.next() != Some('\n') {
                return false;
            }
            continue;
        }
        let Some(esc) = chars.next() else {
            return false;
        };
        match esc {
            'n' | 'r' | 't' | '0' | '\\' | '\'' | '"' => {}
            'x' => {
                let (Some(hi), Some(lo)) = (chars.next(), chars.next()) else {
                    return false;
                };
                let (Some(h), Some(l)) = (hi.to_digit(16), lo.to_digit(16)) else {
                    return false;
                };
                if h * 16 + l > 0x7f {
                    return false;
                }
            }
            'u' => {
                if chars.next() != Some('{') {
                    return false;
                }
                let mut digits = 0;
                let mut value = 0u32;
                loop {
                    let Some(c) = chars.next() else {
                        return false;
                    };
                    match c {
                        '}' if digits > 0 => break,
                        '_' if digits > 0 => {}
                        digit if digits < 6 => {
                            let Some(d) = digit.to_digit(16) else {
                                return false;
                            };
                            value = value * 16 + d;
                            digits += 1;
                        }
                        _ => return false,
                    }
                }
                if char::from_u32(value).is_none() {
                    return false;
                }
            }
            '\r' => {
                if chars.next() != Some('\n') {
                    return false;
                }
                while chars
                    .peek()
                    .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            '\n' => {
                while chars
                    .peek()
                    .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            _ => return false,
        }
    }
    true
}

/// Decodes a validated string literal's inner slice (without surrounding quotes).
pub fn decode_string_inner(inner: &str) -> std::borrow::Cow<'_, str> {
    if !inner.as_bytes().contains(&b'\\') && !inner.as_bytes().contains(&b'\r') {
        return std::borrow::Cow::Borrowed(inner);
    }
    let mut chars = inner.chars().peekable();
    let mut result = String::with_capacity(inner.len());
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if ch == '\r' {
                let _ = chars.next();
                result.push('\n');
            } else {
                result.push(ch);
            }
            continue;
        }
        match chars.next().unwrap_or('\\') {
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            '0' => result.push('\0'),
            '\\' => result.push('\\'),
            '\'' => result.push('\''),
            '"' => result.push('"'),
            'x' => {
                let h = chars.next().and_then(|c| c.to_digit(16)).unwrap_or(0);
                let l = chars.next().and_then(|c| c.to_digit(16)).unwrap_or(0);
                if let Some(c) = char::from_u32(h * 16 + l) {
                    result.push(c);
                }
            }
            'u' => {
                let _ = chars.next(); // '{'
                let mut value = 0u32;
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    if c != '_'
                        && let Some(d) = c.to_digit(16)
                    {
                        value = value * 16 + d;
                    }
                }
                if let Some(c) = char::from_u32(value) {
                    result.push(c);
                }
            }
            '\r' => {
                let _ = chars.next();
                while chars
                    .peek()
                    .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            '\n' => {
                while chars
                    .peek()
                    .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            other => result.push(other),
        }
    }
    std::borrow::Cow::Owned(result)
}

/// Decodes a natural literal slice (stripping `_` separators).
pub fn parse_natural(raw: &str) -> Result<i64, String> {
    if !raw.as_bytes().contains(&b'_') {
        raw.parse::<i64>()
            .map_err(|_| format!("integer literal out of range: {raw}"))
    } else {
        let mut acc: i64 = 0;
        for b in raw.bytes() {
            if b == b'_' {
                continue;
            }
            acc = acc
                .checked_mul(10)
                .and_then(|v| v.checked_add(i64::from(b - b'0')))
                .ok_or_else(|| format!("integer literal out of range: {raw}"))?;
        }
        Ok(acc)
    }
}

impl Token<'_> {
    /// Whether this token carries source trivia rather than grammar input.
    #[inline]
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }
}

/// One source token, with UTF-8 byte coordinates rather than parser indices.
#[derive(Clone, Copy, Debug)]
pub struct SourceToken<'src> {
    /// Zero-copy token classification.
    pub token: Token<'src>,
    /// Exact source byte start.
    pub start: usize,
    /// Exact source byte end.
    pub end: usize,
}

impl SourceToken<'_> {
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

/// Lexes all source tokens, retaining trivia and reporting malformed literals/comments.
pub fn lex_source(source: &str) -> Result<Vec<SourceToken<'_>>, Vec<LexError>> {
    let mut tokens = Vec::with_capacity(source.len() / 4 + 8);
    let mut errors = Vec::new();
    let mut lexer = Token::lexer_with_extras(source, LexMode::Lossless);
    while let Some(res) = lexer.next() {
        let range = lexer.span();
        match res {
            Ok(token) => tokens.push(SourceToken {
                token,
                start: range.start,
                end: range.end,
            }),
            Err(()) => errors.push(LexError {
                message: if source[range.clone()].starts_with("/*") {
                    "unterminated block comment".into()
                } else {
                    "invalid CDSL token or literal".into()
                },
                span: Span::Source(range),
            }),
        }
    }
    if errors.is_empty() {
        Ok(tokens)
    } else {
        Err(errors)
    }
}

/// Compact token stream referencing the original source string without copying.
#[derive(Clone, Debug)]
pub struct TokenStream<'src> {
    pub tokens: Vec<Token<'src>>,
    pub spans: Vec<(u32, u32)>,
    pub end: u32,
}

impl<'src> TokenStream<'src> {
    /// Fast zero-copy scan skipping trivia.
    pub fn from_source(source: &'src str) -> Result<Self, Vec<LexError>> {
        let cap = source.len() / 5 + 8;
        let mut tokens = Vec::with_capacity(cap);
        let mut spans = Vec::with_capacity(cap);
        let mut errors = Vec::new();
        let mut lexer = Token::lexer_with_extras(source, LexMode::Significant);
        while let Some(res) = lexer.next() {
            let range = lexer.span();
            match res {
                Ok(token) => {
                    tokens.push(token);
                    spans.push((range.start as u32, range.end as u32));
                }
                Err(()) => errors.push(LexError {
                    message: if source[range.clone()].starts_with("/*") {
                        "unterminated block comment".into()
                    } else {
                        "invalid CDSL token or literal".into()
                    },
                    span: Span::Source(range),
                }),
            }
        }
        if errors.is_empty() {
            Ok(Self {
                tokens,
                spans,
                end: source.len() as u32,
            })
        } else {
            Err(errors)
        }
    }

    #[inline]
    pub fn span_at(&self, index: usize) -> Range<usize> {
        self.spans
            .get(index)
            .map_or(self.end as usize..self.end as usize, |&(s, e)| {
                s as usize..e as usize
            })
    }
}

/// Converts a `proc_macro::TokenStream` into normalized CDSL source text so the
/// proc-macro entry point shares the exact same zero-copy lexer and parser.
pub fn source_from_proc_macro(input: proc_macro::TokenStream) -> String {
    let mut out = String::new();
    append_macro_tokens(input, &mut out);
    out
}

fn append_macro_tokens(input: proc_macro::TokenStream, out: &mut String) {
    use proc_macro::{Delimiter, Spacing, TokenTree};
    let mut trees = input.into_iter().peekable();
    for tree in trees.by_ref() {
        match tree {
            TokenTree::Group(group) => {
                let (open, close) = match group.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::None => ("", ""),
                };
                out.push_str(open);
                append_macro_tokens(group.stream(), out);
                out.push_str(close);
                out.push(' ');
            }
            TokenTree::Punct(punct) => {
                out.push(punct.as_char());
                if punct.spacing() == Spacing::Alone {
                    out.push(' ');
                }
            }
            TokenTree::Ident(ident) => {
                out.push_str(&ident.to_string());
                out.push(' ');
            }
            TokenTree::Literal(literal) => {
                out.push_str(&literal.to_string());
                out.push(' ');
            }
        }
    }
}
