use std::fmt;

use logos::Logos;

use tir::utils::APInt;

#[derive(Logos, Debug, Clone, PartialEq)]
pub enum Token {
    #[regex(r"[ \t\n\r\f]+", |lex| lex.slice().to_string())]
    Whitespace(String),

    #[token("alignas")]
    KwAlignas,
    #[token("alignof")]
    KwAlignof,
    #[token("auto")]
    KwAuto,
    #[token("bool")]
    KwBool,
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
    #[regex("[0-9][0-9_]*|0[xX][0-9a-fA-F][0-9a-fA-F_]*|0[oO][0-7][0-7_]*|0[bB][01][01_]*", |lex| lex.slice().parse::<APInt>().ok())]
    IntegerLiteral(APInt),

    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token("=")]
    Assign,
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
            Token::KwAlignas => f.write_str("alignas"),
            Token::KwAlignof => f.write_str("alignof"),
            Token::KwAuto => f.write_str("auto"),
            Token::KwBool => f.write_str("bool"),
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
            Token::IntegerLiteral(n) => write!(f, "{n}"),
            Token::LParen => f.write_str("("),
            Token::RParen => f.write_str(")"),
            Token::LBrace => f.write_str("{"),
            Token::RBrace => f.write_str("}"),
            Token::Semicolon => f.write_str(";"),
            Token::Comma => f.write_str(","),
            Token::Assign => f.write_str("="),
            Token::Plus => f.write_str("+"),
            Token::Minus => f.write_str("-"),
            Token::Star => f.write_str("*"),
            Token::Slash => f.write_str("/"),
            Token::Percent => f.write_str("%"),
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
