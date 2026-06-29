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

#[cfg(test)]
mod tests {
    use crate::lexer::{Token, lex};

    #[test]
    fn asm_rejects_unknown_punctuation_without_panicking() {
        assert_eq!(lex(".0"), Err(()));
    }

    #[test]
    fn asm_accepts_single_character_identifiers_and_labels() {
        assert_eq!(lex("a b:"), Ok(vec![Token::Ident("a"), Token::Label("b")]));
    }

    #[test]
    fn asm_lexes_directives_and_string_literals() {
        assert_eq!(
            lex(".dword 0x100000000"),
            Ok(vec![
                Token::Directive(".dword"),
                Token::HexNumber("0x100000000")
            ])
        );
        assert_eq!(
            lex(".string \"Hello, RISC-V!\""),
            Ok(vec![
                Token::Directive(".string"),
                Token::StringLit("Hello, RISC-V!")
            ])
        );
        // Dedicated tokens still win over the directive catch-all.
        assert_eq!(lex(".text .data"), Ok(vec![Token::Text, Token::Data]));
    }

    #[test]
    fn asm_lexes_memory_operand_punctuation() {
        assert_eq!(
            lex("mov rax, [rbx]"),
            Ok(vec![
                Token::Ident("mov"),
                Token::Ident("rax"),
                Token::Comma,
                Token::LBracket,
                Token::Ident("rbx"),
                Token::RBracket,
            ])
        );
        assert_eq!(
            lex("jmp *rax"),
            Ok(vec![Token::Ident("jmp"), Token::Star, Token::Ident("rax")])
        );
    }

    #[test]
    fn asm_smoke() {
        let program = "
.text
.global _start
    _start:
    inst1 r1, r2, r3
    ret
";

        assert_eq!(
            lex(program),
            Ok(vec![
                Token::Text,
                Token::Global,
                Token::Ident("_start"),
                Token::Label("_start"),
                Token::Ident("inst1"),
                Token::Ident("r1"),
                Token::Comma,
                Token::Ident("r2"),
                Token::Comma,
                Token::Ident("r3"),
                Token::Ident("ret"),
            ])
        );
    }
}
