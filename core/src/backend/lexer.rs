use logos::Logos;

#[derive(Logos, Debug, PartialEq)]
#[logos(skip(r"\s+"))]
// Line comments: `#` (GNU as / RISC-V), `//` (ARM). Skipping them lets a `.S`
// test file carry lit `RUN:`/`CHECK:` directives without confusing the lexer.
#[logos(skip(r"(#|//)[^\n]*", allow_greedy = true))]
pub enum Token<'src> {
    // Punctuation
    #[token(",")]
    Comma,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("*")]
    Star,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,

    #[token(".section")]
    Section,
    #[token(".text")]
    Text,
    #[token(".data")]
    Data,
    #[token(".global")]
    Global,

    // Catch-all for directives without a dedicated token (`.dword`, `.string`,
    // `.rodata`, ...). Specific tokens above win on priority.
    #[regex("\\.[a-zA-Z_][a-zA-Z0-9_\\.]*", |d| d.slice())]
    Directive(&'src str),

    #[regex("\"[^\"]*\"", |s| { let s = s.slice(); &s[1..s.len() - 1] })]
    StringLit(&'src str),

    #[regex("[a-zA-Z_][a-zA-Z0-9_\\.]*:", |n| { let n = n.slice(); &n[0..n.len() - 1] })]
    Label(&'src str),

    #[regex("[a-zA-Z_][a-zA-Z0-9_\\.]*", |name| name.slice())]
    Ident(&'src str),

    #[regex("-?[0-9]+", |num| num.slice())]
    DecNumber(&'src str),

    #[regex("-?0[xX][0-9a-fA-F]+", |num| num.slice())]
    HexNumber(&'src str),
}

#[allow(clippy::result_unit_err)]
pub fn lex<'src>(source: &'src str) -> Result<Vec<Token<'src>>, ()> {
    let lexer = Token::lexer(source);

    let mut tokens = vec![];

    for token in lexer {
        match token {
            Ok(token) => tokens.push(token),
            Err(_) => return Err(()),
        }
    }

    Ok(tokens)
}

impl<'src> tir::parse::tokens::TokenLike<'src> for Token<'src> {
    fn as_ident(&self) -> Option<&'src str> {
        match self {
            Token::Ident(s) | Token::Label(s) => Some(s),
            _ => None,
        }
    }

    fn is_symbol(&self, sym: tir::parse::tokens::Symbol) -> bool {
        matches!(
            (self, sym),
            (Token::Comma, tir::parse::tokens::Symbol::Comma)
        )
    }
}
