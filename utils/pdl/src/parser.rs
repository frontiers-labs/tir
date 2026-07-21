use chumsky::{input::ValueInput, prelude::*};

use crate::ast::*;
use crate::lexer::{Token, parse_integer};
use crate::{Diagnostic, Span, Spanned};

type Error<'src> = extra::Err<Rich<'src, Token, Span>>;

pub fn parse(source_len: usize, tokens: &[Spanned<Token>]) -> (Option<File>, Vec<Diagnostic>) {
    let (file, errors) = file()
        .then_ignore(end())
        .parse(
            tokens.map(Span::from(source_len..source_len), |(token, span)| {
                (token, span)
            }),
        )
        .into_output_errors();
    let diagnostics = errors
        .into_iter()
        .map(|error| Diagnostic::new(error.to_string(), error.reason().to_string(), *error.span()))
        .collect();
    (file, diagnostics)
}

fn file<'src, I>() -> impl Parser<'src, I, File, Error<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    choice((
        group().map(Item::Group),
        rule().map(|rule| Item::Rule(Box::new(rule))),
    ))
    .repeated()
    .collect()
    .map(|items| File { items })
}

fn group<'src, I>() -> impl Parser<'src, I, Group, Error<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    just(Token::Group)
        .ignore_then(identifier())
        .then_ignore(just(Token::Equal))
        .then(
            type_expr()
                .separated_by(just(Token::Pipe))
                .at_least(1)
                .collect(),
        )
        .then_ignore(just(Token::Semicolon))
        .map_with(|(name, alternatives), extra| Group {
            name,
            alternatives,
            span: extra.span(),
        })
        .labelled("type group")
}

fn rule<'src, I>() -> impl Parser<'src, I, Rule, Error<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let name = identifier()
        .then(
            just(Token::Minus)
                .ignore_then(identifier())
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(|(head, tail)| {
            tail.into_iter().fold(head, |mut name, part| {
                name.push('-');
                name.push_str(&part);
                name
            })
        });
    let direction = choice((
        just(Token::Bidirectional).to(Direction::Bidirectional),
        just(Token::Forward).to(Direction::Forward),
    ));
    let guards = just(Token::Where)
        .ignore_then(
            expression()
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect(),
        )
        .or_not()
        .map(Option::unwrap_or_default);

    just(Token::Rule)
        .ignore_then(name)
        .then_ignore(just(Token::Colon))
        .then(term())
        .then(direction)
        .then(term())
        .then(guards)
        .then_ignore(just(Token::Semicolon))
        .map_with(|((((name, lhs), direction), rhs), guards), extra| Rule {
            name,
            lhs,
            direction,
            rhs,
            guards,
            span: extra.span(),
        })
        .labelled("rewrite rule")
}

fn term<'src, I>() -> impl Parser<'src, I, Term, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|term| {
        let operator = choice((
            identifier()
                .then_ignore(just(Token::Dot))
                .then(identifier())
                .map(|(dialect, name)| Operator::Dialect { dialect, name }),
            just(Token::Hash)
                .ignore_then(identifier())
                .map(Operator::Gate),
        ));
        let attributes = attribute()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect()
            .delimited_by(just(Token::Less), just(Token::Greater))
            .or_not()
            .map(Option::unwrap_or_default);
        let operands = term
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect()
            .delimited_by(just(Token::LeftParen), just(Token::RightParen));
        let operation = operator
            .then(attributes)
            .then(operands)
            .then(just(Token::Colon).ignore_then(type_expr()).or_not())
            .map_with(|(((operator, attributes), operands), ty), extra| Term {
                kind: TermKind::Operation {
                    operator,
                    attributes,
                    operands,
                },
                ty,
                span: extra.span(),
            });
        let constant = just(Token::Const)
            .ignore_then(width_expression().delimited_by(just(Token::Less), just(Token::Greater)))
            .then(expression().delimited_by(just(Token::LeftParen), just(Token::RightParen)))
            .map_with(|(width, value), extra| Term {
                kind: TermKind::Constant { width, value },
                ty: None,
                span: extra.span(),
            });
        let integer = integer().map_with(|value, extra| Term {
            kind: TermKind::Integer(value),
            ty: None,
            span: extra.span(),
        });
        let string = select! { Token::String(value) => value }.map_with(|value, extra| Term {
            kind: TermKind::String(value),
            ty: None,
            span: extra.span(),
        });
        let binder = identifier()
            .then(just(Token::Colon).ignore_then(binding_type()).or_not())
            .map_with(|(name, ty), extra| Term {
                kind: TermKind::Binder { name, ty },
                ty: None,
                span: extra.span(),
            });

        choice((operation, constant, integer, string, binder)).labelled("term")
    })
    .boxed()
}

fn attribute<'src, I>() -> impl Parser<'src, I, Attribute, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let value = choice((
        just(Token::Dollar)
            .ignore_then(identifier())
            .map(AttributeValue::Binder),
        integer().map(AttributeValue::Integer),
        select! { Token::String(value) => AttributeValue::String(value) },
    ));
    identifier()
        .then_ignore(just(Token::Equal))
        .then(value)
        .map_with(|(name, value), extra| Attribute {
            name,
            value,
            span: extra.span(),
        })
}

fn binding_type<'src, I>() -> impl Parser<'src, I, BindingType, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    just(Token::Const)
        .ignore_then(
            width_expression()
                .delimited_by(just(Token::Less), just(Token::Greater))
                .or_not(),
        )
        .map(BindingType::Constant)
        .or(type_expr().map(BindingType::Type))
}

fn type_expr<'src, I>() -> impl Parser<'src, I, Type, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let generic = just(Token::Int)
        .ignore_then(
            choice((
                just(Token::Identifier("_".to_string())).to(Width::Any),
                identifier().map(Width::Named),
                integer().try_map(|width, span| {
                    u32::try_from(width)
                        .map(Width::Concrete)
                        .map_err(|_| Rich::custom(span, "integer type width is out of range"))
                }),
            ))
            .delimited_by(just(Token::Less), just(Token::Greater)),
        )
        .map(Type::Integer);
    generic.or(identifier().map(|name| {
        name.strip_prefix('i')
            .and_then(|width| width.parse().ok())
            .map_or(Type::Named(name), |width| {
                Type::Integer(Width::Concrete(width))
            })
    }))
}

fn expression<'src, I>() -> impl Parser<'src, I, Expr, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    recursive(|expression| {
        let atom = choice((
            integer().map_with(|value, extra| Expr {
                kind: ExprKind::Integer(value),
                span: extra.span(),
            }),
            identifier()
                .then(
                    expression
                        .clone()
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect()
                        .delimited_by(just(Token::LeftParen), just(Token::RightParen)),
                )
                .map_with(|(name, args), extra| Expr {
                    kind: ExprKind::Call { name, args },
                    span: extra.span(),
                }),
            identifier().map_with(|name, extra| Expr {
                kind: ExprKind::Name(name),
                span: extra.span(),
            }),
            expression
                .clone()
                .delimited_by(just(Token::LeftParen), just(Token::RightParen)),
        ));
        let unary = choice((
            just(Token::Minus).to(UnaryOp::Negate),
            just(Token::Bang).to(UnaryOp::Not),
        ))
        .repeated()
        .collect::<Vec<_>>()
        .then(atom)
        .map(|(operators, value)| {
            operators.into_iter().rev().fold(value, |value, op| Expr {
                span: value.span,
                kind: ExprKind::Unary {
                    op,
                    value: Box::new(value),
                },
            })
        });

        binary_level(
            binary_level(
                binary_level(
                    binary_level(
                        binary_level(
                            binary_level(
                                binary_level(
                                    binary_level(
                                        unary,
                                        [
                                            (Token::Star, BinaryOp::Multiply),
                                            (Token::Slash, BinaryOp::Divide),
                                            (Token::Percent, BinaryOp::Remainder),
                                        ],
                                    ),
                                    [
                                        (Token::Plus, BinaryOp::Add),
                                        (Token::Minus, BinaryOp::Subtract),
                                    ],
                                ),
                                [
                                    (Token::ShiftLeft, BinaryOp::ShiftLeft),
                                    (Token::ShiftRight, BinaryOp::ShiftRight),
                                ],
                            ),
                            [
                                (Token::Less, BinaryOp::Less),
                                (Token::LessEqual, BinaryOp::LessEqual),
                                (Token::Greater, BinaryOp::Greater),
                                (Token::GreaterEqual, BinaryOp::GreaterEqual),
                            ],
                        ),
                        [
                            (Token::EqualEqual, BinaryOp::Equal),
                            (Token::NotEqual, BinaryOp::NotEqual),
                        ],
                    ),
                    [(Token::Ampersand, BinaryOp::BitAnd)],
                ),
                [
                    (Token::Caret, BinaryOp::BitXor),
                    (Token::Pipe, BinaryOp::BitOr),
                ],
            ),
            [
                (Token::LogicalAnd, BinaryOp::LogicalAnd),
                (Token::LogicalOr, BinaryOp::LogicalOr),
            ],
        )
    })
    .boxed()
}

fn width_expression<'src, I>() -> impl Parser<'src, I, Expr, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    choice((
        integer().map_with(|value, extra| Expr {
            kind: ExprKind::Integer(value),
            span: extra.span(),
        }),
        identifier().map_with(|name, extra| Expr {
            kind: ExprKind::Name(name),
            span: extra.span(),
        }),
    ))
}

fn binary_level<'src, I, P, const N: usize>(
    operand: P,
    operators: [(Token, BinaryOp); N],
) -> impl Parser<'src, I, Expr, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
    P: Parser<'src, I, Expr, Error<'src>> + Clone + 'src,
{
    let operator = choice(operators.map(|(token, op)| just(token).to(op)));
    operand
        .clone()
        .foldl(operator.then(operand).repeated(), |lhs, (op, rhs)| Expr {
            span: Span::from(lhs.span.start..rhs.span.end),
            kind: ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        })
        .boxed()
}

fn identifier<'src, I>() -> impl Parser<'src, I, String, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    select! { Token::Identifier(value) => value }
}

fn integer<'src, I>() -> impl Parser<'src, I, i64, Error<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    select! { Token::Integer(value) => value }.try_map(|value, span| {
        parse_integer(&value).ok_or_else(|| Rich::custom(span, "integer literal is out of range"))
    })
}
