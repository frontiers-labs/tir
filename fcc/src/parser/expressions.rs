//! Expression precedence, operators and initializer expressions.

use super::declarations::ctype;
use super::{Extra, ParseState, Span, ident};
use crate::ast::*;
use crate::lexer::Token;
use chumsky::input::{MapExtra, ValueInput};
use chumsky::prelude::*;
use tir::graph::{MutDag, NodeId};

#[derive(Clone)]
enum PostfixOp {
    Unary(AstKind),
    Member { indirect: bool, name: String },
    Subscript(NodeId),
    Call(Vec<NodeId>),
}

pub(super) fn expr<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    expression_parsers().0
}

pub(super) fn assignment_expr<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    expression_parsers().1
}

pub(super) fn constant_expr<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    expression_parsers().2
}

fn expression_parsers<'src, I>() -> (
    impl Parser<'src, I, NodeId, Extra<'src>> + Clone,
    impl Parser<'src, I, NodeId, Extra<'src>> + Clone,
    impl Parser<'src, I, NodeId, Extra<'src>> + Clone,
)
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let mut expr = Recursive::declare();
    let mut assignment = Recursive::declare();
    let conditional = {
        let literal = select! { Token::IntegerLiteral(n) => n }.map_with(
            |n, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Int, tok);
                st.ast.set_leaf_data(id, AstLeaf::Int(n));
                id
            },
        );
        let floating = select! { Token::FloatingLiteral(n) => n }.map_with(
            |n, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::FloatLiteral, tok);
                st.ast.set_leaf_data(id, AstLeaf::Float(n));
                id
            },
        );
        let character = select! { Token::CharacterLiteral(value) => value }.map_with(
            |value, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::Character, tok);
                st.ast.set_leaf_data(id, AstLeaf::Character(value));
                id
            },
        );
        let string = select! { Token::StringLiteral(s) => s }.map_with(
            |s, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::String, tok);
                st.ast.set_leaf_data(id, AstLeaf::String(s));
                id
            },
        );
        let va_start = just(Token::Identifier("__builtin_va_start".to_string()))
            .ignore_then(
                assignment
                    .clone()
                    .then_ignore(just(Token::Comma))
                    .then(assignment.clone())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map_with(|(list, last), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::VaStart, tok);
                st.ast.add_edge(id, list);
                st.ast.add_edge(id, last);
                id
            });
        let va_arg = just(Token::Identifier("__builtin_va_arg".to_string()))
            .ignore_then(
                assignment
                    .clone()
                    .then_ignore(just(Token::Comma))
                    .then(ctype())
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map_with(|(list, ty), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::VaArg, tok);
                st.ast.set_leaf_data(id, AstLeaf::Type(ty));
                st.ast.add_edge(id, list);
                id
            });
        let va_end = just(Token::Identifier("__builtin_va_end".to_string()))
            .ignore_then(
                assignment
                    .clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map_with(|list, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::VaEnd, tok);
                st.ast.add_edge(id, list);
                id
            });
        let call = ident()
            .then(
                assignment
                    .clone()
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
            floating,
            character,
            string,
            va_start,
            va_arg,
            va_end,
            call,
            var,
            expr.clone()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        ));
        let postfix = primary
            .then(
                choice((
                    just(Token::PlusPlus).to(PostfixOp::Unary(AstKind::PostInc)),
                    just(Token::MinusMinus).to(PostfixOp::Unary(AstKind::PostDec)),
                    just(Token::Dot)
                        .ignore_then(ident())
                        .map(|name| PostfixOp::Member {
                            indirect: false,
                            name,
                        }),
                    just(Token::Arrow)
                        .ignore_then(ident())
                        .map(|name| PostfixOp::Member {
                            indirect: true,
                            name,
                        }),
                    expr.clone()
                        .delimited_by(just(Token::LBracket), just(Token::RBracket))
                        .map(PostfixOp::Subscript),
                    assignment
                        .clone()
                        .separated_by(just(Token::Comma))
                        .collect::<Vec<NodeId>>()
                        .delimited_by(just(Token::LParen), just(Token::RParen))
                        .map(PostfixOp::Call),
                ))
                .repeated()
                .collect::<Vec<_>>(),
            )
            .map_with(
                |(operand, ops), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    ops.into_iter().fold(operand, |child, op| match op {
                        PostfixOp::Unary(kind) => unary(st, kind, child, tok),
                        PostfixOp::Member { indirect, name } => {
                            let id = st.add(AstKind::Member, tok);
                            st.ast.set_leaf_data(id, AstLeaf::Member { name, indirect });
                            st.ast.add_edge(id, child);
                            id
                        }
                        PostfixOp::Subscript(index) => {
                            let add = st.add(AstKind::Add, tok);
                            st.ast.add_edge(add, child);
                            st.ast.add_edge(add, index);
                            unary(st, AstKind::Deref, add, tok)
                        }
                        PostfixOp::Call(arguments) => {
                            let call = st.add(AstKind::CallExpr, tok);
                            st.ast.add_edge(call, child);
                            for argument in arguments {
                                st.ast.add_edge(call, argument);
                            }
                            call
                        }
                    })
                },
            );
        let unary_expr = recursive(|unary_expr| {
            let type_name = ctype().delimited_by(just(Token::LParen), just(Token::RParen));
            let cast = type_name.clone().then(unary_expr.clone()).map_with(
                |(ty, operand), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::Cast, tok);
                    st.ast.set_leaf_data(id, AstLeaf::Type(ty));
                    st.ast.add_edge(id, operand);
                    id
                },
            );
            let sizeof_type = just(Token::KwSizeof).ignore_then(type_name).map_with(
                |ty, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::SizeofType, tok);
                    st.ast.set_leaf_data(id, AstLeaf::Type(ty));
                    id
                },
            );
            let sizeof_expr = just(Token::KwSizeof)
                .ignore_then(unary_expr.clone())
                .map_with(|operand, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    unary(st, AstKind::SizeofExpr, operand, tok)
                });
            let prefix = choice((
                just(Token::Minus).to(AstKind::Neg),
                just(Token::Plus).to(AstKind::Pos),
                just(Token::Bang).to(AstKind::Not),
                just(Token::Tilde).to(AstKind::BitNot),
                just(Token::Amp).to(AstKind::AddressOf),
                just(Token::Star).to(AstKind::Deref),
                just(Token::PlusPlus).to(AstKind::PreInc),
                just(Token::MinusMinus).to(AstKind::PreDec),
            ))
            .then(unary_expr)
            .map_with(
                |(op, operand), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    unary(&mut e.state().0, op, operand, tok)
                },
            );
            choice((sizeof_type, cast, sizeof_expr, prefix, postfix.clone())).boxed()
        });
        let product = binop(
            unary_expr,
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
        let shift = binop(
            sum,
            choice((
                just(Token::Shl).to(AstKind::Shl),
                just(Token::Shr).to(AstKind::Shr),
            )),
        );
        let relational = binop(
            shift,
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
        let bit_and = binop(equality, just(Token::Amp).to(AstKind::BitAnd));
        let bit_xor = binop(bit_and, just(Token::Caret).to(AstKind::BitXor));
        let bit_or = binop(bit_xor, just(Token::Pipe).to(AstKind::BitOr));
        let logical_and = binop(bit_or, just(Token::AmpAmp).to(AstKind::LogAnd));
        let logical_or = binop(logical_and, just(Token::PipePipe).to(AstKind::LogOr));
        logical_or
            .then(
                just(Token::Question)
                    .ignore_then(expr.clone())
                    .then_ignore(just(Token::Colon))
                    .then(assignment.clone())
                    .or_not(),
            )
            .map_with(
                |(condition, tail), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let Some((then_value, else_value)) = tail else {
                        return condition;
                    };
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    let id = st.add(AstKind::Conditional, tok);
                    st.ast.add_edge(id, condition);
                    st.ast.add_edge(id, then_value);
                    st.ast.add_edge(id, else_value);
                    id
                },
            )
            .boxed()
    };
    assignment.define(
        conditional
            .clone()
            .then(
                choice((
                    just(Token::Assign).to(AstKind::AssignExpr),
                    just(Token::PlusAssign).to(AstKind::AddAssign),
                    just(Token::MinusAssign).to(AstKind::SubAssign),
                    just(Token::StarAssign).to(AstKind::MulAssign),
                    just(Token::SlashAssign).to(AstKind::DivAssign),
                    just(Token::PercentAssign).to(AstKind::ModAssign),
                    just(Token::ShlAssign).to(AstKind::ShlAssign),
                    just(Token::ShrAssign).to(AstKind::ShrAssign),
                    just(Token::AmpAssign).to(AstKind::AndAssign),
                    just(Token::CaretAssign).to(AstKind::XorAssign),
                    just(Token::PipeAssign).to(AstKind::OrAssign),
                ))
                .then(assignment.clone())
                .or_not(),
            )
            .map_with(|(lhs, tail), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                match tail {
                    Some((op, rhs)) => binary(&mut e.state().0, op, lhs, rhs, tok),
                    None => lhs,
                }
            })
            .boxed(),
    );
    expr.define(binop(
        assignment.clone(),
        just(Token::Comma).to(AstKind::Comma),
    ));
    (expr, assignment, conditional)
}

/// A left-associative binary-operator level: a `child` operand followed by any
/// number of `op child` tails, folded into nested operator nodes. Operands are
/// already in the DAG by the time the fold runs, so each operator node is
/// appended after them.
fn binop<'src, I, C, O>(child: C, op: O) -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
    C: Parser<'src, I, NodeId, Extra<'src>> + Clone + 'src,
    O: Parser<'src, I, AstKind, Extra<'src>> + Clone + 'src,
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
        // Type-erase each precedence level. Without this the levels nest into a
        // single concrete combinator type whose drop-glue symbol grows to
        // megabytes and overflows the macOS linker's symbol-name limit.
        .boxed()
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

pub(super) fn initializer<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|initializer| {
        let field_designator = just(Token::Dot)
            .ignore_then(ident())
            .map(|name| (InitializerDesignator::Field(name), None));
        let index_designator = assignment_expr()
            .delimited_by(just(Token::LBracket), just(Token::RBracket))
            .map(|index| (InitializerDesignator::Index, Some(index)));
        let designated = field_designator
            .or(index_designator)
            .repeated()
            .at_least(1)
            .collect::<Vec<_>>()
            .then_ignore(just(Token::Assign))
            .then(initializer.clone())
            .map_with(
                |(designators, value), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                    let tok = e.span().start;
                    let st = &mut e.state().0;
                    designators
                        .into_iter()
                        .rev()
                        .fold(value, |selected, (designator, index)| {
                            let id = st.add(AstKind::DesignatedInitializer, tok);
                            st.ast
                                .set_leaf_data(id, AstLeaf::DesignatedInitializer(designator));
                            if let Some(index) = index {
                                st.ast.add_edge(id, index);
                            }
                            st.ast.add_edge(id, selected);
                            id
                        })
                },
            );
        designated
            .or(initializer.clone())
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map_with(|values, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                let id = st.add(AstKind::InitializerList, tok);
                for value in values {
                    st.ast.add_edge(id, value);
                }
                id
            })
            .or(assignment_expr())
    })
}
