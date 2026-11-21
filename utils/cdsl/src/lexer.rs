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

#[derive(Clone, Debug, PartialEq, Logos)]
#[logos(extras = LexMode)]
pub enum Token {
    #[regex(r"[\x09-\x0D\x20\x85\u200E\u200F\u2028\u2029]+", trivia)]
    Whitespace,
    #[token("//", line_comment)]
    LineComment,
    #[token("/*", block_comment)]
    BlockComment,
    #[regex(r"(\p{XID_Start}|_)(\p{XID_Continue})*", |lex| identifier(lex.slice()))]
    #[regex(r"r#(\p{XID_Start}|_)(\p{XID_Continue})*", |lex| raw_identifier(lex.slice()))]
    Ident(String),
    #[regex(r"[0-9][0-9_]*", |lex| lex.slice().to_owned())]
    Natural(String),
    #[regex(r#""([^"\\]|\\(?s:.))*""#, |lex| decode_string(lex.slice()))]
    String(String),
    #[token("_", priority = 3)]
    Wildcard,
    #[token("pub")]
    Pub,
    #[token("mod")]
    Mod,
    #[token("let")]
    Let,
    #[token("enum")]
    Enum,
    #[token("use")]
    Use,
    #[token("as")]
    As,
    #[token("extern")]
    Extern,
    #[token("fn")]
    Fn,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("match")]
    Match,
    #[token("op")]
    Op,
    #[token("signature")]
    Signature,
    #[token("attributes")]
    Attributes,
    #[token("exec")]
    Exec,
    #[token("ir")]
    Ir,
    #[token("yield")]
    Yield,
    #[token("limit")]
    Limit,
    #[token("decreases")]
    Decreases,
    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("::")]
    PathSep,
    #[token("->")]
    Arrow,
    #[token("=>")]
    FatArrow,
    #[token("||")]
    Or,
    #[token("&&")]
    And,
    #[token("==")]
    Equal,
    #[token("!=")]
    NotEqual,
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

fn trivia(lex: &mut logos::Lexer<'_, Token>) -> logos::Filter<()> {
    match lex.extras {
        LexMode::Significant => logos::Filter::Skip,
        LexMode::Lossless => logos::Filter::Emit(()),
    }
}

fn line_comment(lex: &mut logos::Lexer<'_, Token>) -> logos::Filter<()> {
    let length = lex
        .remainder()
        .find(['\r', '\n'])
        .unwrap_or(lex.remainder().len());
    lex.bump(length);
    trivia(lex)
}

fn block_comment(lex: &mut logos::Lexer<'_, Token>) -> logos::FilterResult<(), ()> {
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

impl Token {
    /// Whether this token carries source trivia rather than grammar input.
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }
}

/// One source token, with UTF-8 byte coordinates rather than parser indices.
#[derive(Clone, Debug)]
pub struct SourceToken {
    /// Decoded token classification. Source spelling remains in the input.
    pub token: Token,
    /// Exact source byte range, including all comment delimiters.
    pub range: Range<usize>,
}

/// Lexes all source tokens, retaining trivia and reporting malformed literals/comments.
pub fn lex_source(source: &str) -> Result<Vec<SourceToken>, Vec<LexError>> {
    let mut tokens = Vec::new();
    scan_source(source, LexMode::Lossless, |token, range| {
        tokens.push(SourceToken { token, range })
    })?;
    Ok(tokens)
}

fn scan_source(
    source: &str,
    mode: LexMode,
    mut emit: impl FnMut(Token, Range<usize>),
) -> Result<(), Vec<LexError>> {
    let mut errors = Vec::new();
    for (token, range) in Token::lexer_with_extras(source, mode).spanned() {
        match token {
            Ok(token) => emit(token, range),
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
        Ok(())
    } else {
        Err(errors)
    }
}

fn identifier(text: &str) -> Option<String> {
    (!text.contains(['\u{200c}', '\u{200d}'])).then(|| text.to_owned())
}

fn raw_identifier(text: &str) -> Option<String> {
    let name = &text[2..];
    if matches!(name, "_" | "crate" | "self" | "super" | "Self") {
        None
    } else {
        identifier(name)
    }
}

fn decode_string(text: &str) -> Option<String> {
    let mut chars = text
        .strip_prefix('"')?
        .strip_suffix('"')?
        .chars()
        .peekable();
    let mut result = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if ch == '\r' {
                if chars.next()? != '\n' {
                    return None;
                }
                result.push('\n');
                continue;
            }
            result.push(ch);
            continue;
        }
        match chars.next()? {
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            '0' => result.push('\0'),
            '\\' => result.push('\\'),
            '\'' => result.push('\''),
            '"' => result.push('"'),
            'x' => {
                let high = chars.next()?.to_digit(16)?;
                let low = chars.next()?.to_digit(16)?;
                let value = high * 16 + low;
                if value > 0x7f {
                    return None;
                }
                result.push(char::from_u32(value)?);
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut digits = 0;
                let mut value = 0;
                loop {
                    match chars.next()? {
                        '}' if digits > 0 => break,
                        '_' if digits > 0 => {}
                        digit if digits < 6 => {
                            value = value * 16 + digit.to_digit(16)?;
                            digits += 1;
                        }
                        _ => return None,
                    }
                }
                result.push(char::from_u32(value)?);
            }
            '\r' => {
                if chars.next()? != '\n' {
                    return None;
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
            _ => return None,
        }
    }
    Some(result)
}

#[derive(Clone, Debug)]
pub struct TokenStream {
    pub tokens: Vec<Token>,
    pub spans: Vec<Span>,
    pub end: Span,
}

impl TokenStream {
    pub fn from_source(source: &str) -> Result<Self, Vec<LexError>> {
        let mut stream = Self {
            tokens: Vec::new(),
            spans: Vec::new(),
            end: Span::Source(source.len()..source.len()),
        };
        scan_source(source, LexMode::Significant, |token, range| {
            stream.tokens.push(token);
            stream.spans.push(Span::Source(range));
        })?;
        Ok(stream)
    }

    pub fn from_proc_macro(input: proc_macro::TokenStream) -> Result<Self, Vec<LexError>> {
        let mut stream = Self {
            tokens: Vec::new(),
            spans: Vec::new(),
            end: Span::Macro {
                start: proc_macro::Span::call_site(),
                end: proc_macro::Span::call_site(),
            },
        };
        let mut errors = Vec::new();
        stream.extend_macro(input, &mut errors);
        if errors.is_empty() {
            Ok(stream)
        } else {
            Err(errors)
        }
    }

    fn push_macro(&mut self, token: Token, start: proc_macro::Span, end: proc_macro::Span) {
        self.tokens.push(token);
        self.spans.push(Span::Macro { start, end });
        self.end = Span::Macro { start: end, end };
    }

    fn extend_macro(&mut self, input: proc_macro::TokenStream, errors: &mut Vec<LexError>) {
        use proc_macro::{Delimiter, Spacing, TokenTree};
        let mut trees = input.into_iter().peekable();
        while let Some(tree) = trees.next() {
            match tree {
                TokenTree::Group(group) => {
                    let delimiters = match group.delimiter() {
                        Delimiter::Parenthesis => Some((Token::LParen, Token::RParen)),
                        Delimiter::Bracket => Some((Token::LBracket, Token::RBracket)),
                        Delimiter::Brace => Some((Token::LBrace, Token::RBrace)),
                        Delimiter::None => None,
                    };
                    if let Some((open, _)) = &delimiters {
                        self.push_macro(open.clone(), group.span_open(), group.span_open());
                    }
                    self.extend_macro(group.stream(), errors);
                    if let Some((_, close)) = delimiters {
                        self.push_macro(close, group.span_close(), group.span_close());
                    }
                }
                TokenTree::Punct(punct) => {
                    let mut spelling = punct.as_char().to_string();
                    let mut end = punct.span();
                    if punct.spacing() == Spacing::Joint
                        && let Some(TokenTree::Punct(next)) = trees.peek()
                    {
                        let pair = format!("{}{}", punct.as_char(), next.as_char());
                        if matches!(
                            pair.as_str(),
                            "::" | "->" | "=>" | "||" | "&&" | "==" | "!="
                        ) {
                            spelling = pair;
                            end = next.span();
                            trees.next();
                        }
                    }
                    self.macro_token(&spelling, punct.span(), end, errors);
                }
                TokenTree::Ident(ident) => {
                    self.macro_token(&ident.to_string(), ident.span(), ident.span(), errors)
                }
                TokenTree::Literal(literal) => {
                    self.macro_token(&literal.to_string(), literal.span(), literal.span(), errors)
                }
            }
        }
    }

    fn macro_token(
        &mut self,
        spelling: &str,
        start: proc_macro::Span,
        end: proc_macro::Span,
        errors: &mut Vec<LexError>,
    ) {
        match Self::from_source(spelling) {
            Ok(mut stream) if stream.tokens.len() == 1 => {
                if let Some(token) = stream.tokens.pop() {
                    self.push_macro(token, start, end);
                }
            }
            _ => errors.push(LexError {
                span: Span::Macro { start, end },
                message: format!("invalid CDSL token: {spelling}"),
            }),
        }
    }
}
