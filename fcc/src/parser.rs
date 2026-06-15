//! A [`chumsky`]-based parser turning a token stream into the [`crate::ast`], in the
//! same style as the TMDL compiler's parser (combinators over a token slice
//! with `Rich` errors).
//!
//! It accepts just enough C to express integer functions: `int`/`void` return
//! types and parameters, local `int` declarations, assignments, simple
//! arithmetic (`+`, `-`, `*`, parentheses) and `return`.
//!
//! Nodes are appended straight into the [`Ast`] DAG carried as parser state.
//! Because combinators run bottom-up, every child is added before its parent,
//! which is exactly the post-order layout the DAG requires.

use chumsky::input::{MapExtra, ValueInput};
use chumsky::inspector::SimpleState;
use chumsky::prelude::*;

use tir::graph::{MutDag, NodeId};

use crate::ast::*;
use crate::lexer::Token;

/// Index-based span over the token slice (we parse already-lexed tokens, so
/// byte offsets are not available — token indices are the natural span).
type Span = SimpleSpan<usize>;
type Extra<'src> = extra::Full<Rich<'src, Token, Span>, SimpleState<Ast>, ()>;

/// Parse a token stream into a translation unit. Whitespace tokens are dropped
/// first; on failure the collected diagnostics are returned as strings.
pub fn parse(tokens: &[Token]) -> Result<Ast, Vec<String>> {
    let filtered: Vec<Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .cloned()
        .collect();

    let mut state = SimpleState(Ast::new());
    let (out, errors) = translation_unit()
        .parse_with_state(filtered.as_slice(), &mut state)
        .into_output_errors();

    match out {
        Some(_) if errors.is_empty() => Ok(state.0),
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

fn expr<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|expr| {
        let primary = choice((
            select! { Token::IntegerLiteral(n) => n.to_i64() }.map_with(
                |n, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let ast: &mut Ast = &mut e.state().0;
                    let id = ast.add_node(AstKind::Int);
                    ast.set_leaf_data(id, AstLeaf::Int(n));
                    id
                },
            ),
            ident().map_with(|name, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let ast: &mut Ast = &mut e.state().0;
                let id = ast.add_node(AstKind::Var);
                ast.set_leaf_data(id, AstLeaf::Var(name));
                id
            }),
            expr.delimited_by(just(Token::LParen), just(Token::RParen)),
        ));

        // `*` binds tighter than `+`/`-`; both are left-associative. The operands
        // are already in the DAG by the time the fold runs, so each operator node
        // is appended after them.
        let product = primary
            .clone()
            .then(
                just(Token::Star)
                    .ignore_then(primary)
                    .repeated()
                    .collect::<Vec<NodeId>>(),
            )
            .map_with(
                |(first, rest), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let ast: &mut Ast = &mut e.state().0;
                    rest.into_iter()
                        .fold(first, |lhs, rhs| binary(ast, AstKind::Mul, lhs, rhs))
                },
            );

        let add_op = choice((
            just(Token::Plus).to(AstKind::Add),
            just(Token::Minus).to(AstKind::Sub),
        ));

        product
            .clone()
            .then(add_op.then(product).repeated().collect::<Vec<_>>())
            .map_with(
                |(first, rest), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let ast: &mut Ast = &mut e.state().0;
                    rest.into_iter()
                        .fold(first, |lhs, (op, rhs)| binary(ast, op, lhs, rhs))
                },
            )
    })
}

fn binary(ast: &mut Ast, op: AstKind, lhs: NodeId, rhs: NodeId) -> NodeId {
    let id = ast.add_node(op);
    ast.add_edge(id, lhs);
    ast.add_edge(id, rhs);
    id
}

fn stmt<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let ret = just(Token::KwReturn)
        .ignore_then(expr().or_not())
        .then_ignore(just(Token::Semicolon))
        .map_with(|value, e| {
            let ast: &mut Ast = &mut e.state().0;
            let id = ast.add_node(AstKind::Return);
            if let Some(value) = value {
                ast.add_edge(id, value);
            }
            id
        });

    let decl = ctype()
        .then(ident())
        .then(just(Token::Assign).ignore_then(expr()).or_not())
        .then_ignore(just(Token::Semicolon))
        .map_with(|((ty, name), init), e| {
            let ast: &mut Ast = &mut e.state().0;
            let id = ast.add_node(AstKind::Decl);
            ast.set_leaf_data(id, AstLeaf::Decl { name, ty });
            if let Some(init) = init {
                ast.add_edge(id, init);
            }
            id
        });

    let assign = ident()
        .then_ignore(just(Token::Assign))
        .then(expr())
        .then_ignore(just(Token::Semicolon))
        .map_with(|(name, value), e| {
            let ast: &mut Ast = &mut e.state().0;
            let id = ast.add_node(AstKind::Assign);
            ast.set_leaf_data(id, AstLeaf::Assign(name));
            ast.add_edge(id, value);
            id
        });

    choice((ret, decl, assign))
}

fn function<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let param = ctype().then(ident()).map_with(|(ty, name), e| {
        let ast: &mut Ast = &mut e.state().0;
        let id = ast.add_node(AstKind::Param);
        ast.set_leaf_data(id, AstLeaf::Param { name, ty });
        id
    });

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
        .map_with(|(((ret, name), params), body), e| {
            let ast: &mut Ast = &mut e.state().0;
            let id = ast.add_node(AstKind::Function);
            ast.set_leaf_data(id, AstLeaf::Function { name, ret });
            for child in params.into_iter().chain(body) {
                ast.add_edge(id, child);
            }
            id
        })
}

fn translation_unit<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    function()
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(end())
        .map_with(|functions, e| {
            let ast: &mut Ast = &mut e.state().0;
            let id = ast.add_node(AstKind::TranslationUnit);
            for func in functions {
                ast.add_edge(id, func);
            }
            id
        })
}
