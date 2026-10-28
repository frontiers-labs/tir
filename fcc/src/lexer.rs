use std::fmt;

use logos::Logos;

use tir::utils::{APFloat, APInt};

pub(crate) fn decode_c_escapes(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

pub(crate) fn decode_character_constant(source: &str) -> Option<i64> {
    let first_quote = source.find('\'')?;
    let body = source.get(first_quote + 1..source.len().checked_sub(1)?)?;
    let decoded = decode_c_escapes(body);
    let mut characters = decoded.chars();
    let value = characters.next()?;
    characters
        .next()
        .is_none()
        .then_some(i64::from(value as u32))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerLiteral {
    pub value: APInt,
    pub spelling: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FloatingLiteral {
    pub value: APFloat,
    pub spelling: String,
    pub kind: FloatingLiteralKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingLiteralKind {
    Float,
    Double,
    LongDouble,
}

pub(crate) fn parse_floating_literal(spelling: &str) -> Option<FloatingLiteral> {
    let suffix_start = spelling.trim_end_matches(['f', 'F', 'l', 'L']).len();
    let kind = match &spelling[suffix_start..] {
        "f" | "F" => FloatingLiteralKind::Float,
        "" => FloatingLiteralKind::Double,
        "l" | "L" => FloatingLiteralKind::LongDouble,
        _ => return None,
    };
    let digits = spelling[..suffix_start].replace('\'', "");
    let value = if digits.starts_with("0x") || digits.starts_with("0X") {
        parse_hexadecimal_floating_literal(&digits, kind)?
    } else {
        match kind {
            FloatingLiteralKind::Float => {
                APFloat::from_bits(8, 23, false, digits.parse::<f32>().ok()?.to_bits() as u128)
            }
            FloatingLiteralKind::Double | FloatingLiteralKind::LongDouble => {
                APFloat::from_f64(digits.parse::<f64>().ok()?)
            }
        }
    };
    Some(FloatingLiteral {
        value,
        spelling: spelling.to_string(),
        kind,
    })
}

fn parse_hexadecimal_floating_literal(digits: &str, kind: FloatingLiteralKind) -> Option<APFloat> {
    let digits = digits.get(2..)?;
    let exponent_separator = digits.find(['p', 'P'])?;
    let (fraction, exponent) = digits.split_at(exponent_separator);
    let exponent = exponent.get(1..)?.parse::<i32>().ok()?;
    let (integer, fraction) = fraction.split_once('.').unwrap_or((fraction, ""));
    let significand = u128::from_str_radix(&format!("{integer}{fraction}"), 16).ok()?;
    let exp2 = exponent.checked_sub(i32::try_from(fraction.len()).ok()?.checked_mul(4)?)?;
    let (exp_width, mant_width) = match kind {
        FloatingLiteralKind::Float => (8, 23),
        FloatingLiteralKind::Double | FloatingLiteralKind::LongDouble => (11, 52),
    };
    Some(APFloat::from_significand(
        exp_width,
        mant_width,
        false,
        false,
        significand,
        exp2,
    ))
}

pub(crate) fn bit_precise_suffix(spelling: &str) -> Option<(usize, bool)> {
    let lower = spelling.to_ascii_lowercase();
    ["uwb", "wbu", "wb"]
        .into_iter()
        .find(|suffix| lower.ends_with(suffix))
        .map(|suffix| (suffix.len(), suffix.contains('u')))
}

pub(crate) fn parse_integer_literal(spelling: &str) -> Option<IntegerLiteral> {
    let bit_precise = bit_precise_suffix(spelling);
    let suffix_start = match bit_precise {
        Some((length, _)) => spelling.len() - length,
        None => spelling.trim_end_matches(['u', 'U', 'l', 'L']).len(),
    };
    let digits = spelling[..suffix_start].replace('\'', "");
    let normalized = if digits.len() > 1
        && digits.starts_with('0')
        && !digits.starts_with("0x")
        && !digits.starts_with("0X")
        && !digits.starts_with("0b")
        && !digits.starts_with("0B")
    {
        format!("0o{}", &digits[1..])
    } else {
        digits
    };
    let value: APInt = normalized.parse().ok()?;
    // Only a bit-precise literal is wider than the widest standard type.
    if bit_precise.is_none() && value.width() > 64 {
        return None;
    }
    Some(IntegerLiteral {
        value,
        spelling: spelling.to_string(),
    })
}

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token<'src> {
    #[regex(r"[ \t\n\r\f]+", |lex| lex.slice())]
    Whitespace(&'src str),
    #[regex(r"//[^\n]*", |lex| lex.slice(), allow_greedy = true)]
    #[regex(r"/\*([^*]|\*+[^*/])*\*+/", |lex| lex.slice())]
    Comment(&'src str),

    #[token("alignas")]
    KwAlignas,
    #[token("alignof")]
    KwAlignof,
    #[token("auto")]
    KwAuto,
    #[token("bool")]
    KwBool,
    #[token("_Bool")]
    KwUnderscoreBool,
    #[token("_BitInt")]
    KwBitInt,
    #[token("break")]
    KwBreak,
    #[token("case")]
    KwCase,
    #[token("char")]
    KwChar,
    #[token("const")]
    KwConst,
    #[token("constexpr")]
    KwConstexpr,
    #[token("continue")]
    KwContinue,
    #[token("default")]
    KwDefault,
    #[token("do")]
    KwDo,
    #[token("double")]
    KwDouble,
    #[token("else")]
    KwElse,
    #[token("enum")]
    KwEnum,
    #[token("extern")]
    KwExtern,
    #[token("false")]
    KwFalse,
    #[token("float")]
    KwFloat,
    #[token("for")]
    KwFor,
    #[token("goto")]
    KwGoto,
    #[token("if")]
    KwIf,
    #[token("inline")]
    KwInline,
    #[token("int")]
    KwInt,
    #[token("long")]
    KwLong,
    #[token("nullptr")]
    KwNullptr,
    #[token("register")]
    KwRegister,
    #[token("restrict")]
    KwRestrict,
    #[token("return")]
    KwReturn,
    #[token("short")]
    KwShort,
    #[token("signed")]
    KwSigned,
    #[token("sizeof")]
    KwSizeof,
    #[token("static")]
    KwStatic,
    #[token("static_assert")]
    KwStaticAssert,
    #[token("struct")]
    KwStruct,
    #[token("switch")]
    KwSwitch,
    #[token("thread_local")]
    KwThreadLocal,
    #[token("true")]
    KwTrue,
    #[token("typedef")]
    KwTypedef,
    #[token("typeof")]
    KwTypeof,
    #[token("typeof_unqual")]
    KwTypeofUnqual,
    #[token("union")]
    KwUnion,
    #[token("unsigned")]
    KwUnsigned,
    #[token("void")]
    KwVoid,
    #[token("volatile")]
    KwVolatile,
    #[token("while")]
    KwWhile,

    // TODO C11 underscore keywords?

    // Preprocessor punctuation (must come before Hash so ## wins on longest-match).
    #[token("##")]
    HashHash,
    #[token("#")]
    Hash,

    // Or regular expressions.
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice())]
    Identifier(&'src str),
    #[regex(r"0[xX](([0-9a-fA-F][0-9a-fA-F']*(\.[0-9a-fA-F']*)?)|(\.[0-9a-fA-F][0-9a-fA-F']*))[pP][+-]?[0-9][0-9']*[fFlL]?", |lex| lex.slice())]
    #[regex(r"(([0-9][0-9']*\.[0-9']*|\.[0-9][0-9']*)([eE][+-]?[0-9][0-9']*)?|[0-9][0-9']*[eE][+-]?[0-9][0-9']*)[fFlL]?", |lex| lex.slice())]
    FloatingLiteral(&'src str),
    #[regex("(0[xX][0-9a-fA-F'][0-9a-fA-F']*|0[bB][01'][01']*|[0-9][0-9']*)([uUlL]*|[uU]?(wb|WB)[uU]?)", |lex| lex.slice())]
    IntegerLiteral(&'src str),
    #[regex(r#"(u8|u|U|L)?'([^'\\]|\\.)+'"#, |lex| lex.slice())]
    CharacterLiteral(&'src str),
    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let s = lex.slice();
        &s[1..s.len() - 1]
    })]
    StringLiteral(&'src str),

    #[token("...")]
    Ellipsis,
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
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token("->")]
    Arrow,
    #[token("=")]
    Assign,
    #[token("+=")]
    PlusAssign,
    #[token("-=")]
    MinusAssign,
    #[token("*=")]
    StarAssign,
    #[token("/=")]
    SlashAssign,
    #[token("%=")]
    PercentAssign,
    #[token("&=")]
    AmpAssign,
    #[token("|=")]
    PipeAssign,
    #[token("^=")]
    CaretAssign,
    #[token("<<=")]
    ShlAssign,
    #[token(">>=")]
    ShrAssign,
    #[token("++")]
    PlusPlus,
    #[token("--")]
    MinusMinus,
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
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("~")]
    Tilde,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token("?")]
    Question,
    #[token(":")]
    Colon,
    #[token("==")]
    EqEq,
    #[token("!=")]
    BangEq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("<=")]
    Le,
    #[token(">=")]
    Ge,
    #[token("&&")]
    AmpAmp,
    #[token("||")]
    PipePipe,
    #[token("!")]
    Bang,
}

impl<'src> Token<'src> {
    /// The token's source text, except that a string literal's has no quotes.
    pub fn spelling(&self) -> &'src str {
        match *self {
            Token::Whitespace(s) => s,
            Token::Comment(s) => s,
            Token::KwAlignas => "alignas",
            Token::KwAlignof => "alignof",
            Token::KwAuto => "auto",
            Token::KwBool => "bool",
            Token::KwUnderscoreBool => "_Bool",
            Token::KwBitInt => "_BitInt",
            Token::KwBreak => "break",
            Token::KwCase => "case",
            Token::KwChar => "char",
            Token::KwConst => "const",
            Token::KwConstexpr => "constexpr",
            Token::KwContinue => "continue",
            Token::KwDefault => "default",
            Token::KwDo => "do",
            Token::KwDouble => "double",
            Token::KwElse => "else",
            Token::KwEnum => "enum",
            Token::KwExtern => "extern",
            Token::KwFalse => "false",
            Token::KwFloat => "float",
            Token::KwFor => "for",
            Token::KwGoto => "goto",
            Token::KwIf => "if",
            Token::KwInline => "inline",
            Token::KwInt => "int",
            Token::KwLong => "long",
            Token::KwNullptr => "nullptr",
            Token::KwRegister => "register",
            Token::KwRestrict => "restrict",
            Token::KwReturn => "return",
            Token::KwShort => "short",
            Token::KwSigned => "signed",
            Token::KwSizeof => "sizeof",
            Token::KwStatic => "static",
            Token::KwStaticAssert => "static_assert",
            Token::KwStruct => "struct",
            Token::KwSwitch => "switch",
            Token::KwThreadLocal => "thread_local",
            Token::KwTrue => "true",
            Token::KwTypedef => "typedef",
            Token::KwTypeof => "typeof",
            Token::KwTypeofUnqual => "typeof_unqual",
            Token::KwUnion => "union",
            Token::KwUnsigned => "unsigned",
            Token::KwVoid => "void",
            Token::KwVolatile => "volatile",
            Token::KwWhile => "while",
            Token::HashHash => "##",
            Token::Hash => "#",
            Token::Identifier(s) => s,
            Token::FloatingLiteral(s) => s,
            Token::IntegerLiteral(s) => s,
            Token::CharacterLiteral(s) => s,
            Token::StringLiteral(s) => s,
            Token::Ellipsis => "...",
            Token::LParen => "(",
            Token::RParen => ")",
            Token::LBrace => "{",
            Token::RBrace => "}",
            Token::LBracket => "[",
            Token::RBracket => "]",
            Token::Semicolon => ";",
            Token::Comma => ",",
            Token::Dot => ".",
            Token::Arrow => "->",
            Token::Assign => "=",
            Token::PlusAssign => "+=",
            Token::MinusAssign => "-=",
            Token::StarAssign => "*=",
            Token::SlashAssign => "/=",
            Token::PercentAssign => "%=",
            Token::AmpAssign => "&=",
            Token::PipeAssign => "|=",
            Token::CaretAssign => "^=",
            Token::ShlAssign => "<<=",
            Token::ShrAssign => ">>=",
            Token::PlusPlus => "++",
            Token::MinusMinus => "--",
            Token::Plus => "+",
            Token::Minus => "-",
            Token::Star => "*",
            Token::Slash => "/",
            Token::Percent => "%",
            Token::Amp => "&",
            Token::Pipe => "|",
            Token::Caret => "^",
            Token::Tilde => "~",
            Token::Shl => "<<",
            Token::Shr => ">>",
            Token::Question => "?",
            Token::Colon => ":",
            Token::EqEq => "==",
            Token::BangEq => "!=",
            Token::Lt => "<",
            Token::Gt => ">",
            Token::Le => "<=",
            Token::Ge => ">=",
            Token::AmpAmp => "&&",
            Token::PipePipe => "||",
            Token::Bang => "!",
        }
    }
}

impl fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::StringLiteral(s) => write!(f, "\"{s}\""),
            token => f.write_str(token.spelling()),
        }
    }
}
