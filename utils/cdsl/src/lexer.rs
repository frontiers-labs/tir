//! Rust-compatible source tokenization for CDSL.
use logos::{FilterResult, Logos};

#[derive(Clone, Debug, Logos, PartialEq, Eq)]
#[logos(error = String)]
#[logos(skip r"[ \t\n\r\x0B\x0C\u0085\u200E\u200F\u2028\u2029]+")]
#[logos(skip(r"//[^\n\r]*", allow_greedy = true))]
pub(crate) enum Token<'src> {
    #[regex(r"(?:r#)?[_\p{XID_Start}][_\p{XID_Continue}]*", |lex| lex.slice())]
    Word(&'src str),
    #[regex(r"[0-9][0-9a-zA-Z_]*", integer)]
    Integer(i128),
    #[regex(
        r"[0-9][0-9_]*(?:\.[0-9_]+(?:[eE][+-]?[0-9_]+)?|[eE][+-]?[0-9_]+)",
        reserved_float
    )]
    Float,
    #[token("\"", string)]
    String(String),
    #[regex("r#*\"", raw_string)]
    RawString(&'src str),
    #[token("'", character)]
    Character,
    #[token("/*", comment)]
    Comment,
    #[token("::")]
    ColonColon,
    #[token(":")]
    Colon,
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
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
    #[token("->")]
    Arrow,
    #[token("=>")]
    FatArrow,
    #[token("=")]
    Equal,
    #[token("==")]
    EqualEqual,
    #[token("!=")]
    NotEqual,
    #[token("<")]
    Less,
    #[token(">")]
    Greater,
    #[token("<=")]
    LessEqual,
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
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("&")]
    And,
    #[token("|")]
    Or,
    #[token("^")]
    Xor,
}

pub(crate) fn is_keyword(word: &str) -> bool {
    matches!(
        word,
        "pub"
            | "use"
            | "as"
            | "mod"
            | "extern"
            | "type"
            | "enum"
            | "let"
            | "rec"
            | "fn"
            | "if"
            | "else"
            | "match"
            | "true"
            | "false"
    )
}

fn integer<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> Result<i128, String> {
    if !lex.slice().starts_with("0x")
        && !lex.slice().starts_with("0o")
        && !lex.slice().starts_with("0b")
        && lex.remainder().strip_prefix('.').is_some_and(|after| {
            !after.starts_with('.')
                && after
                    .chars()
                    .next()
                    .is_none_or(|ch| !ch.is_alphabetic() && ch != '_')
        })
    {
        lex.bump(1);
        return Err("floating-point literals are reserved in CDSL 0.1".into());
    }
    let digits = lex.slice().replace('_', "");
    let (radix, digits) = if let Some(digits) = digits.strip_prefix("0x") {
        (16, digits)
    } else if let Some(digits) = digits.strip_prefix("0o") {
        (8, digits)
    } else if let Some(digits) = digits.strip_prefix("0b") {
        (2, digits)
    } else {
        (10, digits.as_str())
    };
    i128::from_str_radix(digits, radix)
        .map_err(|_| "invalid or out-of-range integer literal".into())
}

fn reserved_float<'src>(_: &mut logos::Lexer<'src, Token<'src>>) -> Result<(), String> {
    Err("floating-point literals are reserved in CDSL 0.1".into())
}

fn character<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> Result<(), String> {
    let rest = lex.remainder();
    let mut chars = rest.char_indices();
    let mut end = 0;
    if let Some((_, first)) = chars.next() {
        end = first.len_utf8();
        if first == '\\' {
            for (offset, ch) in chars {
                end = offset + ch.len_utf8();
                if ch == '\'' || ch == '\n' {
                    break;
                }
            }
        } else if let Some((offset, '\'')) = chars.next() {
            end = offset + 1;
        } else {
            let mut following = Token::lexer(rest);
            if matches!(following.next(), Some(Ok(Token::Word(_)))) {
                end = following.span().end;
            }
        }
    }
    lex.bump(end);
    Err("character literals and lifetimes are not CDSL constructs".into())
}

fn comment<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> FilterResult<(), String> {
    let rest = lex.remainder().as_bytes();
    let mut depth = 1;
    let mut offset = 0;
    while offset + 1 < rest.len() {
        match &rest[offset..offset + 2] {
            b"/*" => {
                depth += 1;
                offset += 2;
            }
            b"*/" => {
                depth -= 1;
                offset += 2;
                if depth == 0 {
                    lex.bump(offset);
                    return FilterResult::Skip;
                }
            }
            _ => offset += 1,
        }
    }
    lex.bump(rest.len());
    FilterResult::Error("unterminated block comment".into())
}

fn raw_string<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> Result<&'src str, String> {
    let hashes = lex.slice().len() - 2;
    let rest = lex.remainder();
    if hashes > 255 {
        lex.bump(rest.len());
        return Err("raw strings allow at most 255 hash delimiters".into());
    }
    for (offset, _) in rest.match_indices('"') {
        let end = offset + 1 + hashes;
        if rest
            .as_bytes()
            .get(offset + 1..end)
            .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            lex.bump(end);
            let content = &rest[..offset];
            if content.as_bytes().iter().enumerate().any(|(index, byte)| {
                *byte == b'\r' && content.as_bytes().get(index + 1) != Some(&b'\n')
            }) {
                return Err("bare carriage return in raw string".into());
            }
            return Ok(content);
        }
    }
    lex.bump(rest.len());
    Err("unterminated raw string".into())
}

fn string<'src>(lex: &mut logos::Lexer<'src, Token<'src>>) -> Result<String, String> {
    let rest = lex.remainder();
    let mut chars = rest.char_indices().peekable();
    let mut value = String::new();
    let mut error = None;
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '"' => {
                lex.bump(offset + 1);
                return error.map_or(Ok(value), Err);
            }
            '\r' => {
                if chars.next_if(|(_, next)| *next == '\n').is_some() {
                    value.push('\n');
                } else {
                    error.get_or_insert_with(|| "bare carriage return in string".to_owned());
                }
            }
            '\\' => {
                let escaped = match chars.next() {
                    Some((_, 'n')) => Some('\n'),
                    Some((_, 'r')) => Some('\r'),
                    Some((_, 't')) => Some('\t'),
                    Some((_, '0')) => Some('\0'),
                    Some((_, '\\')) => Some('\\'),
                    Some((_, '\'')) => Some('\''),
                    Some((_, '"')) => Some('"'),
                    Some((_, '\n')) => {
                        while chars
                            .next_if(|(_, next)| matches!(next, ' ' | '\t' | '\n' | '\r'))
                            .is_some()
                        {}
                        None
                    }
                    Some((_, '\r')) if chars.next_if(|(_, next)| *next == '\n').is_some() => {
                        while chars
                            .next_if(|(_, next)| matches!(next, ' ' | '\t' | '\n' | '\r'))
                            .is_some()
                        {}
                        None
                    }
                    Some((_, 'x')) => {
                        let first = chars.next().and_then(|(_, ch)| ch.to_digit(16));
                        let second = chars.next().and_then(|(_, ch)| ch.to_digit(16));
                        match first
                            .zip(second)
                            .map(|(a, b)| a * 16 + b)
                            .filter(|code| *code <= 0x7f)
                            .and_then(char::from_u32)
                        {
                            Some(ch) => Some(ch),
                            None => {
                                error.get_or_insert_with(|| {
                                    "invalid ASCII string escape".to_owned()
                                });
                                None
                            }
                        }
                    }
                    Some((_, 'u')) => {
                        let mut digits = String::new();
                        let mut closed = false;
                        if chars.next_if(|(_, next)| *next == '{').is_some() {
                            for (_, ch) in chars.by_ref() {
                                if ch == '}' {
                                    closed = true;
                                    break;
                                }
                                if ch != '_' {
                                    digits.push(ch);
                                }
                                if ch == '"' || ch == '\n' {
                                    break;
                                }
                            }
                        }
                        let decoded = if closed && !digits.is_empty() && digits.len() <= 6 {
                            u32::from_str_radix(&digits, 16)
                                .ok()
                                .and_then(char::from_u32)
                        } else {
                            None
                        };
                        match decoded {
                            Some(ch) => Some(ch),
                            None => {
                                error.get_or_insert_with(|| {
                                    "invalid Unicode string escape".to_owned()
                                });
                                None
                            }
                        }
                    }
                    _ => {
                        error.get_or_insert_with(|| "invalid string escape".to_owned());
                        None
                    }
                };
                if let Some(ch) = escaped {
                    value.push(ch);
                }
            }
            _ => value.push(ch),
        }
    }
    lex.bump(rest.len());
    Err("unterminated string".into())
}
