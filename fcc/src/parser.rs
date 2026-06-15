//! A [`chumsky`]-based parser turning a token stream into the [`crate::ast`], in the
//! same style as the TMDL compiler's parser (combinators over a token slice
//! with `Rich` errors).
//!
//! Types are still limited to `int`/`void`, but the statement and expression
//! grammar covers a useful C89/C99 subset: `if`/`else`, `while`, `do`/`while`,
//! `for`, `break`, `continue`, compound blocks and expression statements;
//! arithmetic (`+ - * / %`), relational and equality operators, logical
//! `&& || !`, unary minus, parentheses and function calls.
//!
//! Nodes are appended straight into the [`Ast`] DAG carried as parser state.
//! Because combinators run bottom-up, every child is added before its parent,
//! which is exactly the post-order layout the DAG requires.

use chumsky::input::{MapExtra, ValueInput};
use chumsky::inspector::SimpleState;
use chumsky::prelude::*;

use tir::graph::{MutDag, NodeId};

use crate::ast::*;
use crate::diagnostics::{Diagnostic, FileId, UnexpectedEof, UnexpectedToken};
use crate::lexer::Token;

/// Index-based span over the token slice (we parse already-lexed tokens, so
/// byte offsets are not available — token indices are the natural span).
type Span = SimpleSpan<usize>;
type Extra<'src> = extra::Full<Rich<'src, Token, Span>, SimpleState<ParseState>, ()>;

/// Parser state: the tree under construction plus the byte span of every input
/// token, so each node can record where its construct starts in the source.
struct ParseState {
    ast: Ast,
    spans: Vec<crate::diagnostics::Span>,
}

impl ParseState {
    /// Append a node, spanning it at the byte position of token index `tok`
    /// (the first token of the construct being reduced).
    fn add(&mut self, kind: AstKind, tok: usize) -> NodeId {
        let span = self
            .spans
            .get(tok)
            .copied()
            .unwrap_or(crate::diagnostics::Span::new(FileId::default(), 0));
        self.ast.add_node(AstNode::new(kind, span))
    }
}

/// Parse a stream of tokens, each paired with its byte [`crate::diagnostics::Span`]
/// in the source. Whitespace tokens are dropped first; on failure each parser
/// error is turned into a [`Diagnostic`] whose label points back at the source.
pub fn parse(tokens: &[(Token, crate::diagnostics::Span)]) -> Result<Ast, Vec<Diagnostic>> {
    let mut filtered = Vec::with_capacity(tokens.len());
    let mut byte_spans = Vec::with_capacity(tokens.len());
    for (tok, span) in tokens {
        if !matches!(tok, Token::Whitespace(_)) {
            filtered.push(tok.clone());
            byte_spans.push(*span);
        }
    }

    let mut state = SimpleState(ParseState {
        ast: Ast::new(),
        spans: byte_spans.clone(),
    });
    let (out, errors) = translation_unit()
        .parse_with_state(filtered.as_slice(), &mut state)
        .into_output_errors();

    match out {
        Some(_) if errors.is_empty() => Ok(state.0.ast),
        _ => Err(errors
            .into_iter()
            .map(|e| rich_to_diagnostic(&e, &byte_spans))
            .collect()),
    }
}

/// Convert a chumsky [`Rich`] error (spanned over token indices) into a
/// [`Diagnostic`] spanned at the offending token's source position. An error
/// past the final token (`found` is `None`) is reported at the last token.
fn rich_to_diagnostic(
    err: &Rich<'_, Token, Span>,
    byte_spans: &[crate::diagnostics::Span],
) -> Diagnostic {
    let index = err.span().into_range().start;
    let span = byte_spans
        .get(index)
        .or_else(|| byte_spans.last())
        .copied()
        .unwrap_or(crate::diagnostics::Span::new(
            crate::diagnostics::FileId::default(),
            0,
        ));
    let reason = err.reason().to_string();

    if err.found().is_none() {
        UnexpectedEof::new(span, reason).into()
    } else {
        UnexpectedToken::new(span, reason).into()
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
        let literal = select! { Token::IntegerLiteral(n) => n.to_i64() }.map_with(
            |n, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Int, tok);
                st.ast.set_leaf_data(id, AstLeaf::Int(n));
                id
            },
        );

        // A call must be tried before a bare identifier so `f(x)` is not read as
        // the variable `f`.
        let call = ident()
            .then(
                expr.clone()
                    .separated_by(just(Token::Comma))
                    .collect::<Vec<NodeId>>()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map_with(|(name, args), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Call, tok);
                st.ast.set_leaf_data(id, AstLeaf::Call(name));
                for arg in args {
                    st.ast.add_edge(id, arg);
                }
                id
            });

        let var = ident().map_with(|name, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Var, tok);
            st.ast.set_leaf_data(id, AstLeaf::Var(name));
            id
        });

        let primary = choice((
            literal,
            call,
            var,
            expr.delimited_by(just(Token::LParen), just(Token::RParen)),
        ));

        // Prefix unary operators (`-`, `!`), applied right-to-left so the
        // innermost operator wraps the operand first.
        let unary = choice((
            just(Token::Minus).to(AstKind::Neg),
            just(Token::Bang).to(AstKind::Not),
        ))
        .repeated()
        .collect::<Vec<AstKind>>()
        .then(primary)
        .map_with(
            |(ops, operand), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                ops.into_iter()
                    .rev()
                    .fold(operand, |child, op| unary(st, op, child, tok))
            },
        );

        // Precedence ladder, tightest first. Every operator is left-associative.
        let product = binop(
            unary,
            choice((
                just(Token::Star).to(AstKind::Mul),
                just(Token::Slash).to(AstKind::Div),
                just(Token::Percent).to(AstKind::Mod),
            )),
        );
        let sum = binop(
            product,
            choice((
                just(Token::Plus).to(AstKind::Add),
                just(Token::Minus).to(AstKind::Sub),
            )),
        );
        let relational = binop(
            sum,
            choice((
                just(Token::Le).to(AstKind::Le),
                just(Token::Ge).to(AstKind::Ge),
                just(Token::Lt).to(AstKind::Lt),
                just(Token::Gt).to(AstKind::Gt),
            )),
        );
        let equality = binop(
            relational,
            choice((
                just(Token::EqEq).to(AstKind::Eq),
                just(Token::BangEq).to(AstKind::Ne),
            )),
        );
        let logical_and = binop(equality, just(Token::AmpAmp).to(AstKind::LogAnd));
        binop(logical_and, just(Token::PipePipe).to(AstKind::LogOr))
    })
}

/// A left-associative binary-operator level: a `child` operand followed by any
/// number of `op child` tails, folded into nested operator nodes. Operands are
/// already in the DAG by the time the fold runs, so each operator node is
/// appended after them.
fn binop<'src, I, C, O>(child: C, op: O) -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
    C: Parser<'src, I, NodeId, Extra<'src>> + Clone,
    O: Parser<'src, I, AstKind, Extra<'src>> + Clone,
{
    child
        .clone()
        .then(
            op.then(child)
                .repeated()
                .collect::<Vec<(AstKind, NodeId)>>(),
        )
        .map_with(
            |(first, rest), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                rest.into_iter()
                    .fold(first, |lhs, (op, rhs)| binary(st, op, lhs, rhs, tok))
            },
        )
}

fn binary(st: &mut ParseState, op: AstKind, lhs: NodeId, rhs: NodeId, tok: usize) -> NodeId {
    let id = st.add(op, tok);
    st.ast.add_edge(id, lhs);
    st.ast.add_edge(id, rhs);
    id
}

fn unary(st: &mut ParseState, op: AstKind, operand: NodeId, tok: usize) -> NodeId {
    let id = st.add(op, tok);
    st.ast.add_edge(id, operand);
    id
}

/// Build an `int x = init` declaration node (without the trailing `;`, so the
/// same body serves both a declaration statement and a `for` init clause).
fn decl_body<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    ctype()
        .then(ident())
        .then(just(Token::Assign).ignore_then(expr()).or_not())
        .map_with(
            |((ty, name), init), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
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

fn stmt<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|stmt| {
        let semi = just(Token::Semicolon);

        let block = stmt
            .clone()
            .repeated()
            .collect::<Vec<NodeId>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
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

        let decl = decl_body().then_ignore(semi.clone());
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
            decl,
            ret,
            if_stmt,
            while_stmt,
            do_while,
            for_stmt,
            break_stmt,
            continue_stmt,
            null_stmt,
            assign,
            expr_stmt,
        ))
    })
}

fn function<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let param =
        ctype()
            .then(ident())
            .map_with(|(ty, name), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Param, tok);
                st.ast.set_leaf_data(id, AstLeaf::Param { name, ty });
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

    ctype().then(ident()).then(params).then(body).map_with(
        |(((ret, name), params), body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Function, tok);
            st.ast.set_leaf_data(id, AstLeaf::Function { name, ret });
            for child in params.into_iter().chain(body) {
                st.ast.add_edge(id, child);
            }
            id
        },
    )
}

fn translation_unit<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    function()
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(end())
        .map_with(|functions, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::TranslationUnit, tok);
            for func in functions {
                st.ast.add_edge(id, func);
            }
            id
        })
}

#[cfg(test)]
mod tests {
    use logos::Logos;

    use super::parse;
    use crate::diagnostics::{Code, Span as ByteSpan, intern_file};
    use crate::lexer::Token;

    fn lex(src: &str) -> Vec<(Token, ByteSpan)> {
        let file = intern_file("<parser-test>", src);
        Token::lexer(src)
            .spanned()
            .map(|(r, span)| (r.unwrap(), ByteSpan::new(file, span.start)))
            .collect()
    }

    #[test]
    fn accepts_a_well_formed_function() {
        assert!(parse(&lex("int main(void) { return 0; }")).is_ok());
    }

    fn errors(src: &str) -> Vec<Code> {
        match parse(&lex(src)) {
            Ok(_) => panic!("expected parse to fail for {src:?}"),
            Err(diags) => diags.iter().map(|d| d.code()).collect(),
        }
    }

    #[test]
    fn missing_semicolon_is_unexpected_token() {
        assert_eq!(
            errors("int main(void) { return 0 }"),
            vec![Code::UnexpectedToken]
        );
    }

    #[test]
    fn missing_closing_brace_is_unexpected_eof() {
        assert!(errors("int main(void) { return 0;").contains(&Code::UnexpectedEof));
    }
}
