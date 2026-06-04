//! A [`chumsky`]-based parser turning a token stream into the [`crate::ast`], in the
//! same style as the TMDL compiler's parser (combinators over a token slice
//! with `Rich` errors).
//!
//! It accepts just enough C to express integer functions: `int`/`void` return
//! types and parameters, local `int` declarations, assignments, simple
//! arithmetic (`+`, `-`, `*`, parentheses) and `return`.

use chumsky::input::ValueInput;
use chumsky::prelude::*;

use crate::ast::*;
use crate::lexer::Token;

/// Index-based span over the token slice (we parse already-lexed tokens, so
/// byte offsets are not available — token indices are the natural span).
type Span = SimpleSpan<usize>;
type Extra<'src> = extra::Err<Rich<'src, Token, Span>>;

/// Parse a token stream into a translation unit. Whitespace tokens are dropped
/// first; on failure the collected diagnostics are returned as strings.
pub fn parse(tokens: &[Token]) -> Result<TranslationUnit, Vec<String>> {
    let filtered: Vec<Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .cloned()
        .collect();

    let (out, errors) = translation_unit()
        .parse(filtered.as_slice())
        .into_output_errors();

    match out {
        Some(unit) if errors.is_empty() => Ok(unit),
        _ => Err(errors.into_iter().map(|e| e.to_string()).collect()),
    }
}

fn ctype<'src, I>() -> impl Parser<'src, I, CType, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    select! {
        Token::KwInt => CType::Int,
        Token::KwVoid => CType::Void,
    }
}

fn ident<'src, I>() -> impl Parser<'src, I, String, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    select! { Token::Identifier(name) => name }
}

fn expr<'src, I>() -> impl Parser<'src, I, Expr, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|expr| {
        let primary = choice((
            select! { Token::IntegerLiteral(n) => Expr::Int(n.to_i64()) },
            ident().map(Expr::Var),
            expr.delimited_by(just(Token::LParen), just(Token::RParen)),
        ));

        // `*` binds tighter than `+`/`-`; both are left-associative.
        let product = primary.clone().foldl(
            just(Token::Star).ignore_then(primary).repeated(),
            |lhs, rhs| Expr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        );

        let add_op = choice((
            just(Token::Plus).to(BinOp::Add),
            just(Token::Minus).to(BinOp::Sub),
        ));

        product
            .clone()
            .foldl(add_op.then(product).repeated(), |lhs, (op, rhs)| {
                Expr::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            })
    })
}

fn stmt<'src, I>() -> impl Parser<'src, I, Stmt, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let ret = just(Token::KwReturn)
        .ignore_then(expr().or_not())
        .then_ignore(just(Token::Semicolon))
        .map(Stmt::Return);

    let decl = ctype()
        .then(ident())
        .then(just(Token::Assign).ignore_then(expr()).or_not())
        .then_ignore(just(Token::Semicolon))
        .map(|((ty, name), init)| Stmt::Decl { name, ty, init });

    let assign = ident()
        .then_ignore(just(Token::Assign))
        .then(expr())
        .then_ignore(just(Token::Semicolon))
        .map(|(name, value)| Stmt::Assign { name, value });

    choice((ret, decl, assign))
}

fn function<'src, I>() -> impl Parser<'src, I, Function, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let param = ctype().then(ident()).map(|(ty, name)| Param { name, ty });

    // `(void)` is an explicit empty parameter list; `()` is also accepted.
    let params = choice((
        just(Token::KwVoid).to(Vec::new()),
        param.separated_by(just(Token::Comma)).collect::<Vec<_>>(),
    ))
    .delimited_by(just(Token::LParen), just(Token::RParen));

    let body = stmt()
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace));

    ctype()
        .then(ident())
        .then(params)
        .then(body)
        .map(|(((ret, name), params), body)| Function {
            name,
            ret,
            params,
            body,
        })
}

fn translation_unit<'src, I>() -> impl Parser<'src, I, TranslationUnit, Extra<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    function()
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(end())
        .map(|functions| TranslationUnit { functions })
}
