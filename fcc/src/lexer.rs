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
    ImaginaryFloat,
    ImaginaryDouble,
    ImaginaryLongDouble,
}

fn parse_floating_literal(spelling: &str) -> Option<FloatingLiteral> {
    let suffix_start = spelling
        .trim_end_matches(['f', 'F', 'l', 'L', 'i', 'I'])
        .len();
    let kind = match spelling[suffix_start..].to_ascii_lowercase().as_str() {
        "f" => FloatingLiteralKind::Float,
        "" => FloatingLiteralKind::Double,
        "l" => FloatingLiteralKind::LongDouble,
        "if" | "fi" => FloatingLiteralKind::ImaginaryFloat,
        "i" => FloatingLiteralKind::ImaginaryDouble,
        "il" | "li" => FloatingLiteralKind::ImaginaryLongDouble,
        _ => return None,
    };
    let digits = spelling[..suffix_start].replace('\'', "");
    let value = if digits.starts_with("0x") || digits.starts_with("0X") {
        parse_hexadecimal_floating_literal(&digits, kind)?
    } else {
        match kind {
            FloatingLiteralKind::Float | FloatingLiteralKind::ImaginaryFloat => {
                APFloat::from_bits(8, 23, false, digits.parse::<f32>().ok()?.to_bits() as u128)
            }
            FloatingLiteralKind::Double
            | FloatingLiteralKind::LongDouble
            | FloatingLiteralKind::ImaginaryDouble
            | FloatingLiteralKind::ImaginaryLongDouble => {
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
        FloatingLiteralKind::Float | FloatingLiteralKind::ImaginaryFloat => (8, 23),
        FloatingLiteralKind::Double
        | FloatingLiteralKind::LongDouble
        | FloatingLiteralKind::ImaginaryDouble
        | FloatingLiteralKind::ImaginaryLongDouble => (11, 52),
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

fn parse_integer_literal(spelling: &str) -> Option<IntegerLiteral> {
    let suffix_start = spelling.trim_end_matches(['u', 'U', 'l', 'L']).len();
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
    normalized.parse().ok().map(|value| IntegerLiteral {
        value,
        spelling: spelling.to_string(),
    })
}

#[derive(Logos, Debug, Clone, PartialEq)]
pub enum Token {
    #[regex(r"[ \t\n\r\f]+", |lex| lex.slice().to_string())]
    Whitespace(String),
    #[regex(r"//[^\n]*", |lex| lex.slice().to_string(), allow_greedy = true)]
    #[regex(r"/\*([^*]|\*+[^*/])*\*+/", |lex| lex.slice().to_string())]
    Comment(String),

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
    #[token("_Complex")]
    KwComplex,
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
    #[regex("[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Identifier(String),
    #[regex(r"0[xX](([0-9a-fA-F][0-9a-fA-F']*(\.[0-9a-fA-F']*)?)|(\.[0-9a-fA-F][0-9a-fA-F']*))[pP][+-]?[0-9][0-9']*[fFlLiI]{0,2}", |lex| parse_floating_literal(lex.slice()))]
    #[regex(r"(([0-9][0-9']*\.[0-9']*|\.[0-9][0-9']*)([eE][+-]?[0-9][0-9']*)?|[0-9][0-9']*[eE][+-]?[0-9][0-9']*)[fFlLiI]{0,2}", |lex| parse_floating_literal(lex.slice()))]
    FloatingLiteral(FloatingLiteral),
    #[regex("0[xX][0-9a-fA-F'][0-9a-fA-F']*[uUlL]*|0[bB][01'][01']*[uUlL]*|[0-9][0-9']*[uUlL]*", |lex| parse_integer_literal(lex.slice()))]
    IntegerLiteral(IntegerLiteral),
    #[regex(r#"(u8|u|U|L)?'([^'\\]|\\.)+'"#, |lex| lex.slice().to_string())]
    CharacterLiteral(String),
    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let s = lex.slice();
        s[1..s.len() - 1].to_string()
    })]
    StringLiteral(String),

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

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Whitespace(s) => f.write_str(s),
            Token::Comment(s) => f.write_str(s),
            Token::KwAlignas => f.write_str("alignas"),
            Token::KwAlignof => f.write_str("alignof"),
            Token::KwAuto => f.write_str("auto"),
            Token::KwBool => f.write_str("bool"),
            Token::KwUnderscoreBool => f.write_str("_Bool"),
            Token::KwComplex => f.write_str("_Complex"),
            Token::KwBreak => f.write_str("break"),
            Token::KwCase => f.write_str("case"),
            Token::KwChar => f.write_str("char"),
            Token::KwConst => f.write_str("const"),
            Token::KwConstexpr => f.write_str("constexpr"),
            Token::KwContinue => f.write_str("continue"),
            Token::KwDefault => f.write_str("default"),
            Token::KwDo => f.write_str("do"),
            Token::KwDouble => f.write_str("double"),
            Token::KwElse => f.write_str("else"),
            Token::KwEnum => f.write_str("enum"),
            Token::KwExtern => f.write_str("extern"),
            Token::KwFalse => f.write_str("false"),
            Token::KwFloat => f.write_str("float"),
            Token::KwFor => f.write_str("for"),
            Token::KwGoto => f.write_str("goto"),
            Token::KwIf => f.write_str("if"),
            Token::KwInline => f.write_str("inline"),
            Token::KwInt => f.write_str("int"),
            Token::KwLong => f.write_str("long"),
            Token::KwNullptr => f.write_str("nullptr"),
            Token::KwRegister => f.write_str("register"),
            Token::KwRestrict => f.write_str("restrict"),
            Token::KwReturn => f.write_str("return"),
            Token::KwShort => f.write_str("short"),
            Token::KwSigned => f.write_str("signed"),
            Token::KwSizeof => f.write_str("sizeof"),
            Token::KwStatic => f.write_str("static"),
            Token::KwStaticAssert => f.write_str("static_assert"),
            Token::KwStruct => f.write_str("struct"),
            Token::KwSwitch => f.write_str("switch"),
            Token::KwThreadLocal => f.write_str("thread_local"),
            Token::KwTrue => f.write_str("true"),
            Token::KwTypedef => f.write_str("typedef"),
            Token::KwTypeof => f.write_str("typeof"),
            Token::KwTypeofUnqual => f.write_str("typeof_unqual"),
            Token::KwUnion => f.write_str("union"),
            Token::KwUnsigned => f.write_str("unsigned"),
            Token::KwVoid => f.write_str("void"),
            Token::KwVolatile => f.write_str("volatile"),
            Token::KwWhile => f.write_str("while"),
            Token::HashHash => f.write_str("##"),
            Token::Hash => f.write_str("#"),
            Token::Identifier(s) => f.write_str(s),
            Token::FloatingLiteral(n) => f.write_str(&n.spelling),
            Token::IntegerLiteral(n) => f.write_str(&n.spelling),
            Token::CharacterLiteral(s) => f.write_str(s),
            Token::StringLiteral(s) => write!(f, "\"{s}\""),
            Token::Ellipsis => f.write_str("..."),
            Token::LParen => f.write_str("("),
            Token::RParen => f.write_str(")"),
            Token::LBrace => f.write_str("{"),
            Token::RBrace => f.write_str("}"),
            Token::LBracket => f.write_str("["),
            Token::RBracket => f.write_str("]"),
            Token::Semicolon => f.write_str(";"),
            Token::Comma => f.write_str(","),
            Token::Dot => f.write_str("."),
            Token::Arrow => f.write_str("->"),
            Token::Assign => f.write_str("="),
            Token::PlusAssign => f.write_str("+="),
            Token::MinusAssign => f.write_str("-="),
            Token::StarAssign => f.write_str("*="),
            Token::SlashAssign => f.write_str("/="),
            Token::PercentAssign => f.write_str("%="),
            Token::AmpAssign => f.write_str("&="),
            Token::PipeAssign => f.write_str("|="),
            Token::CaretAssign => f.write_str("^="),
            Token::ShlAssign => f.write_str("<<="),
            Token::ShrAssign => f.write_str(">>="),
            Token::PlusPlus => f.write_str("++"),
            Token::MinusMinus => f.write_str("--"),
            Token::Plus => f.write_str("+"),
            Token::Minus => f.write_str("-"),
            Token::Star => f.write_str("*"),
            Token::Slash => f.write_str("/"),
            Token::Percent => f.write_str("%"),
            Token::Amp => f.write_str("&"),
            Token::Pipe => f.write_str("|"),
            Token::Caret => f.write_str("^"),
            Token::Tilde => f.write_str("~"),
            Token::Shl => f.write_str("<<"),
            Token::Shr => f.write_str(">>"),
            Token::Question => f.write_str("?"),
            Token::Colon => f.write_str(":"),
            Token::EqEq => f.write_str("=="),
            Token::BangEq => f.write_str("!="),
            Token::Lt => f.write_str("<"),
            Token::Gt => f.write_str(">"),
            Token::Le => f.write_str("<="),
            Token::Ge => f.write_str(">="),
            Token::AmpAmp => f.write_str("&&"),
            Token::PipePipe => f.write_str("||"),
            Token::Bang => f.write_str("!"),
        }
    }
}
