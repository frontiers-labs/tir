//! Statement grammar and lexical block scopes.

use super::declarations::{ctype, local_decl};
use super::expressions::{expr, initializer};
use super::{Extra, Span, ident};
use crate::ast::*;
use crate::lexer::Token;
use chumsky::input::{MapExtra, ValueInput};
use chumsky::prelude::*;
use tir::graph::{MutDag, NodeId};

/// Build an `int x = init` declaration node (without the trailing `;`, so the
/// same body serves both a declaration statement and a `for` init clause).
fn decl_body<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let array_length = select! { Token::IntegerLiteral(value) => value }
        .or_not()
        .delimited_by(just(Token::LBracket), just(Token::RBracket));
    ctype()
        .then(ident().then(array_length.repeated().collect::<Vec<_>>()))
        .then(just(Token::Assign).ignore_then(initializer()).or_not())
        .map_with(
            |((ty, (name, dimensions)), init), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                st.declare_ordinary(name.clone());
                let ty = dimensions.into_iter().rev().fold(ty, |element, length| {
                    let length = length.map(|literal| {
                        let spelling = literal.spelling.clone();
                        let expression = st.add(AstKind::Int, tok);
                        st.ast.set_leaf_data(expression, AstLeaf::Int(literal));
                        ArrayLength::new(spelling, Some(expression))
                    });
                    CType::Array(Box::new(element), length)
                });
                let id = st.add(AstKind::Decl, tok);
                st.ast.set_leaf_data(id, AstLeaf::Decl { name, ty });
                if let Some(init) = init {
                    st.ast.add_edge(id, init);
                }
                id
            },
        )
}

/// Build an `x = value` assignment node (without the trailing `;`).
fn assign_body<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    ident()
        .then_ignore(just(Token::Assign))
        .then(expr())
        .map_with(
            |(name, value), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Assign, tok);
                st.ast.set_leaf_data(id, AstLeaf::Assign(name));
                st.ast.add_edge(id, value);
                id
            },
        )
}

fn empty_node<'src, I>(e: &mut MapExtra<'src, '_, I, Extra<'src>>) -> NodeId
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let tok = e.span().start;
    e.state().0.add(AstKind::Empty, tok)
}

fn open_scope<'src, I>() -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    just(Token::LBrace).map_with(|_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        e.state().0.push_scope();
    })
}

pub(super) fn close_scope<'src, I>() -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    just(Token::RBrace).map_with(|_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        e.state().0.pop_scope();
    })
}

pub(super) fn stmt<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|stmt| {
        let semi = just(Token::Semicolon);

        let block = stmt
            .clone()
            .repeated()
            .collect::<Vec<NodeId>>()
            .delimited_by(open_scope(), close_scope())
            .map_with(|stmts, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Block, tok);
                for s in stmts {
                    st.ast.add_edge(id, s);
                }
                id
            });

        let ret = just(Token::KwReturn)
            .ignore_then(expr().or_not())
            .then_ignore(semi.clone())
            .map_with(|value, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Return, tok);
                if let Some(value) = value {
                    st.ast.add_edge(id, value);
                }
                id
            });

        let typedef_decl = just(Token::KwTypedef)
            .ignore_then(ctype().then(ident()))
            .then_ignore(semi.clone())
            .map_with(|(ty, name), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                st.declare_typedef(name.clone());
                let id = st.add(AstKind::Typedef, tok);
                st.ast.set_leaf_data(id, AstLeaf::Typedef { name, ty });
                id
            });

        let assign = assign_body().then_ignore(semi.clone());

        let cond = expr().delimited_by(just(Token::LParen), just(Token::RParen));

        let if_stmt = just(Token::KwIf)
            .ignore_then(cond.clone())
            .then(stmt.clone())
            .then(just(Token::KwElse).ignore_then(stmt.clone()).or_not())
            .map_with(
                |((c, then), els), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::If, tok);
                    st.ast.add_edge(id, c);
                    st.ast.add_edge(id, then);
                    if let Some(els) = els {
                        st.ast.add_edge(id, els);
                    }
                    id
                },
            );

        let while_stmt = just(Token::KwWhile)
            .ignore_then(cond.clone())
            .then(stmt.clone())
            .map_with(|(c, body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::While, tok);
                st.ast.add_edge(id, c);
                st.ast.add_edge(id, body);
                id
            });

        let do_while = just(Token::KwDo)
            .ignore_then(stmt.clone())
            .then_ignore(just(Token::KwWhile))
            .then(cond.clone())
            .then_ignore(semi.clone())
            .map_with(|(body, c), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::DoWhile, tok);
                st.ast.add_edge(id, body);
                st.ast.add_edge(id, c);
                id
            });

        // Each `for` clause may be omitted; an omitted clause becomes an
        // `Empty` node so the node always has exactly four children.
        let for_init = choice((decl_body(), assign_body()))
            .or_not()
            .map_with(|c, e| c.unwrap_or_else(|| empty_node(e)));
        let for_cond = expr()
            .or_not()
            .map_with(|c, e| c.unwrap_or_else(|| empty_node(e)));
        let for_step = choice((assign_body(), expr()))
            .or_not()
            .map_with(|c, e| c.unwrap_or_else(|| empty_node(e)));

        let for_stmt = just(Token::KwFor)
            .ignore_then(
                for_init
                    .then_ignore(semi.clone())
                    .then(for_cond)
                    .then_ignore(semi.clone())
                    .then(for_step)
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .then(stmt.clone())
            .map_with(
                |(((init, c), step), body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::For, tok);
                    st.ast.add_edge(id, init);
                    st.ast.add_edge(id, c);
                    st.ast.add_edge(id, step);
                    st.ast.add_edge(id, body);
                    id
                },
            );

        let switch_stmt = just(Token::KwSwitch)
            .ignore_then(cond.clone())
            .then(stmt.clone())
            .map_with(
                |(value, body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::Switch, tok);
                    st.ast.add_edge(id, value);
                    st.ast.add_edge(id, body);
                    id
                },
            );
        let case_stmt = just(Token::KwCase)
            .ignore_then(expr())
            .then_ignore(just(Token::Colon))
            .then(stmt.clone())
            .map_with(
                |(value, body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::Case, tok);
                    st.ast.add_edge(id, value);
                    st.ast.add_edge(id, body);
                    id
                },
            );
        let default_stmt = just(Token::KwDefault)
            .ignore_then(just(Token::Colon))
            .ignore_then(stmt.clone())
            .map_with(|body, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Default, tok);
                st.ast.add_edge(id, body);
                id
            });
        let goto_stmt = just(Token::KwGoto)
            .ignore_then(ident())
            .then_ignore(semi.clone())
            .map_with(|name, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Goto, tok);
                st.ast.set_leaf_data(id, AstLeaf::Label(name));
                id
            });
        let label_stmt = ident()
            .then_ignore(just(Token::Colon))
            .then(stmt.clone())
            .map_with(|(name, body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Label, tok);
                st.ast.set_leaf_data(id, AstLeaf::Label(name));
                st.ast.add_edge(id, body);
                id
            });

        let break_stmt = just(Token::KwBreak).then_ignore(semi.clone()).map_with(
            |_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                e.state().0.add(AstKind::Break, tok)
            },
        );
        let continue_stmt = just(Token::KwContinue).then_ignore(semi.clone()).map_with(
            |_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                e.state().0.add(AstKind::Continue, tok)
            },
        );

        let null_stmt = semi
            .clone()
            .map_with(|_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                e.state().0.add(AstKind::Empty, tok)
            });

        let expr_stmt = expr().then_ignore(semi).map_with(
            |value, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::ExprStmt, tok);
                st.ast.add_edge(id, value);
                id
            },
        );

        // Declarations start with a type keyword and control flow with its own
        // keyword, so they are unambiguous. An assignment is tried before an
        // expression statement because the latter would also accept the left
        // operand of an assignment on its own.
        choice((
            block,
            typedef_decl,
            local_decl(),
            ret,
            if_stmt,
            while_stmt,
            do_while,
            for_stmt,
            switch_stmt,
            case_stmt,
            default_stmt,
            goto_stmt,
            label_stmt,
            break_stmt,
            continue_stmt,
            null_stmt,
            assign,
            expr_stmt,
        ))
    })
}
