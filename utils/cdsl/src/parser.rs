use chumsky::prelude::*;

use crate::{
    ast::Ast,
    lexer::{Span, Token, TokenStream},
};

type Extra<'a> = extra::Err<Rich<'a, Token>>;
type P<'a, T> = Boxed<'a, 'a, &'a [Token], T, Extra<'a>>;

#[derive(Clone, Debug)]
pub struct ParseError {
    pub found: Option<Token>,
    pub spans: Vec<Span>,
    pub expected: Vec<String>,
}

pub fn parse(stream: TokenStream) -> Result<Ast, Vec<ParseError>> {
    let declarations = compile::module(&())
        .parse(&stream.tokens)
        .into_result()
        .map_err(|errors| diagnostics(&stream, errors))?;
    Ok(Ast {
        declarations,
        token_spans: stream.spans,
        end: stream.end,
    })
}

pub(crate) fn parse_lossless<'a>(
    stream: &'a TokenStream,
    ctx: &'a crate::parser_syntax::Context<'a>,
) -> Result<Vec<crate::parser_syntax::DeclNode>, Vec<ParseError>> {
    lossless::module(ctx)
        .parse(&stream.tokens)
        .into_result()
        .map_err(|errors| diagnostics(stream, errors))
}

fn diagnostics(stream: &TokenStream, errors: Vec<Rich<'_, Token>>) -> Vec<ParseError> {
    errors
        .into_iter()
        .map(|error| {
            let range = error.span().into_range();
            let mut spans = stream.spans[range].to_vec();
            if spans.is_empty() {
                spans.push(stream.end.clone());
            }
            ParseError {
                spans,
                found: error.found().cloned(),
                expected: error
                    .expected()
                    .map(|expected| format!("{expected:?}"))
                    .collect(),
            }
        })
        .collect()
}

macro_rules! grammar { ($suffix_build:item) => {fn node<'a, T: Build + 'a>(ctx: &'a Context<'a>, parser: impl Parser<'a, &'a [Token], T, Extra<'a>> + Clone + 'a) -> P<'a, Node<T::Built>> where T::Built: 'a {
    parser.map_with(move |value, extra| finish(ctx, value, extra.span().into_range())).boxed()
}
fn identifier<'a>(_ctx: &'a Context<'a>) -> P<'a, String> {
    choice((
        select! { Token::Ident(name) => name },
        just(Token::Attributes).to("attributes".to_owned()),
        just(Token::Exec).to("exec".to_owned()),
        just(Token::Signature).to("signature".to_owned()),
        just(Token::Ir).to("ir".to_owned()),
    ))
    .boxed()
}

fn path<'a>(ctx: &'a Context<'a>) -> P<'a, Path> {
    identifier(ctx)
        .separated_by(just(Token::PathSep))
        .at_least(1)
        .collect()
        .boxed()
}

fn literal<'a>(_ctx: &'a Context<'a>) -> P<'a, Literal> {
    select! {
        Token::Natural(value) => Literal::Natural(value),
        Token::String(value) => Literal::String(value),
        Token::True => Literal::Bool(true),
        Token::False => Literal::Bool(false),
    }
    .boxed()
}

fn list<'a, T: 'a>(_ctx: &'a Context<'a>, item: P<'a, T>) -> P<'a, Vec<T>> {
    item.separated_by(just(Token::Comma))
        .allow_trailing()
        .collect()
        .boxed()
}

fn binders<'a>(ctx: &'a Context<'a>, expr: P<'a, Expr>) -> P<'a, Vec<Binder>> {
    list(ctx,
        identifier(ctx)
            .then_ignore(just(Token::Colon))
            .then(expr)
            .map(move |(name, ty)| Binder { name, ty })
            .boxed(),
    )
}

fn generics<'a>(ctx: &'a Context<'a>, expr: P<'a, Expr>) -> P<'a, Vec<Binder>> {
    identifier(ctx)
        .then_ignore(just(Token::Colon))
        .then(expr)
        .map(move |(name, ty)| Binder { name, ty })
        .separated_by(just(Token::Comma))
        .at_least(1)
        .allow_trailing()
        .collect()
        .delimited_by(just(Token::Less), just(Token::Greater))
        .or_not()
        .map(Option::unwrap_or_default)
        .boxed()
}

fn binding<'a>(ctx: &'a Context<'a>, name: P<'a, Path>, expr: P<'a, Expr>) -> P<'a, Binding> {
    just(Token::Let)
        .ignore_then(name)
        .then(generics(ctx, expr.clone()))
        .then(just(Token::Colon).ignore_then(expr.clone()).or_not())
        .then_ignore(just(Token::Assign))
        .then(expr)
        .then_ignore(just(Token::Semi))
        .map(move |(((path, generics), ty), value)| Binding {
            path,
            generics,
            ty,
            value,
        })
        .boxed()
}

fn rule_binding<'a>(
    ctx: &'a Context<'a>,
    expr: P<'a, Expr>,
) -> P<'a, (String, Vec<RuleBinder>, Expr)> {
    just(Token::Let)
        .ignore_then(identifier(ctx))
        .then(identifier(ctx)
            .then(just(Token::Colon).ignore_then(expr.clone()).or_not())
            .map(|(name, ty)| RuleBinder { name, ty })
            .separated_by(just(Token::Comma)).at_least(1).allow_trailing().collect()
            .delimited_by(just(Token::Less), just(Token::Greater))
            .or_not().map(Option::unwrap_or_default))
        .then_ignore(just(Token::Colon))
        .then(expr)
        .then_ignore(just(Token::Semi))
        .map(|((name, binders), clause)| (name, binders, clause))
        .boxed()
}

fn block<'a>(ctx: &'a Context<'a>, expr: P<'a, Expr>) -> P<'a, Block> {
    let rule = rule_binding(ctx, expr.clone())
        .map_with(move |(name, binders, clause), extra| Binding {
            path: vec![name],
            generics: Vec::new(),
            ty: None,
            value: {
                let value = if is_local_table(&clause) && binders.is_empty() {
                    Expression::LocalTable(Box::new(clause))
                } else {
                    Expression::Rule { binders, clause: Box::new(clause) }
                };
                finish(ctx, value, extra.span().into_range())
            },
        });
    let bindings = binding(ctx, identifier(ctx).map(move |name| vec![name]).boxed(), expr.clone())
        .or(rule)
        .repeated()
        .collect::<Vec<_>>();
    recursive(move |tail| {
        bindings.clone()
            .then(expr.clone())
            .then(
                just(Token::Question)
                    .ignore_then(just(Token::Semi))
                    .ignore_then(tail)
                    .or_not(),
            )
            .map(move |((mut bindings, expression), tail): ((Vec<Binding>, Expr), Option<Block>)| {
                if let Some(Block { bindings: next, value }) = tail {
                    let range = expression.range.clone();
                    bindings.push(Binding {
                        path: vec![format!("\0run{}", range.start)],
                        generics: Vec::new(),
                        ty: None,
                        value: finish(ctx, Expression::Run(Box::new(expression)), range),
                    });
                    bindings.extend(next);
                    Block { bindings, value }
                } else {
                    Block { bindings, value: Box::new(expression) }
                }
            })
    })
    .delimited_by(just(Token::LBrace), just(Token::RBrace))
    .boxed()
}

fn record<'a>(ctx: &'a Context<'a>, expr: P<'a, Expr>) -> P<'a, Record> {
    list(ctx,
        identifier(ctx)
            .then_ignore(just(Token::Colon))
            .then(expr)
            .boxed(),
    )
    .delimited_by(just(Token::LBrace), just(Token::RBrace))
    .boxed()
}

fn function<'a>(ctx: &'a Context<'a>, expr: P<'a, Expr>) -> P<'a, Function> {
    just(Token::Memo).or_not().then(
        just(Token::Or)
            .to(Vec::new())
            .or(binders(ctx, expr.clone()).delimited_by(just(Token::Pipe), just(Token::Pipe)))
    )
        .then(just(Token::Arrow).ignore_then(expr.clone()).or_not())
        .then(just(Token::Decreases).ignore_then(expr.clone()).or_not())
        .then(block(ctx, expr))
        .map(|((((memo, parameters), result), decreases), body)| Function {
            memo: memo.is_some(),
            generics: Vec::new(),
            parameters,
            result,
            decreases,
            body,
        })
        .boxed()
}

fn pattern<'a>(ctx: &'a Context<'a>) -> P<'a, Pattern> {
    recursive(move |pattern| {
        let pattern = pattern.boxed();
        let tuple = just(Token::LParen).ignore_then(
            just(Token::RParen).to(PatternKind::Unit).or(pattern
                .clone()
                .then_ignore(just(Token::Comma))
                .then(list(ctx, pattern.clone()))
                .then_ignore(just(Token::RParen))
                .map(move |(first, rest)| {
                    PatternKind::Tuple(std::iter::once(first).chain(rest).collect())
                })),
        );
        node(ctx, choice((
            just(Token::Wildcard).to(PatternKind::Wildcard),
            literal(ctx).map(PatternKind::Literal),
            path(ctx)
                .then(
                    list(ctx, pattern)
                        .delimited_by(just(Token::LParen), just(Token::RParen))
                        .or_not(),
                )
                .map(move |(path, args)| match args {
                    Some(arguments) => PatternKind::Constructor { path, arguments },
                    None => PatternKind::Path(path),
                }),
            tuple,
        )))
    })
    .boxed()
}

#[derive(Clone)]
enum Suffix {
    Generic(Vec<Expr>),
    Call(Vec<Expr>),
    Member(String),
    RecordCall(Record, Vec<Expr>),
}

$suffix_build
fn binary<'a>(ctx: &'a Context<'a>, left: Expr, op: BinaryOp, right: Expr) -> Expr {
    let range = left.range.start..right.range.end;
    finish(ctx, Expression::Binary { op, left: Box::new(left), right: Box::new(right) }, range)
}

fn expression<'a>(ctx: &'a Context<'a>) -> P<'a, Expr> {
    recursive(move |expr| {
        let expr: P<'a, Expr> = expr.boxed();
        let arguments = list(ctx, expr.clone())
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .boxed();
        let tuple = just(Token::LParen).ignore_then(
            just(Token::RParen).to(Expression::Unit).or(expr
                .clone()
                .then(just(Token::Comma).ignore_then(list(ctx, expr.clone())).or_not())
                .then_ignore(just(Token::RParen))
                .map(move |(first, rest)| match rest {
                    None => ungroup(first),
                    Some(rest) => Expression::Tuple(std::iter::once(first).chain(rest).collect()),
                })),
        );
        let conditional_expr = expr.clone();
        let conditional = recursive(move |conditional| {
            let expr = conditional_expr;
            just(Token::If)
                .ignore_then(expr.clone())
                .then(block(ctx, expr.clone()))
                .then_ignore(just(Token::Else))
                .then(node(ctx, block(ctx, expr.clone()).map(Expression::Block)).or(conditional))
                .map(move |((condition, then), otherwise)| Expression::If {
                    condition: Box::new(condition),
                    then,
                    otherwise: Box::new(otherwise),
                })
                .map_with(move |value, extra| finish(ctx, value, extra.span().into_range()))
        })
        .boxed();
        let matching = just(Token::Match)
            .ignore_then(expr.clone())
            .then(
                pattern(ctx)
                    .then_ignore(just(Token::FatArrow))
                    .then(expr.clone())
                    .separated_by(just(Token::Comma))
                    .at_least(1)
                    .allow_trailing()
                    .collect()
                    .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            )
            .map(move |(value, arms)| Expression::Match {
                value: Box::new(value),
                arms,
            });
        let named_results = node(ctx,
            binders(ctx, expr.clone()).delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(move |binders| {
            let names = binders.value.iter().map(move |b| b.name.clone()).collect();
            let value = if binders.value.is_empty() {
                Expression::Unit
            } else {
                Expression::Tuple(binders.value.into_iter().map(move |b| b.ty).collect())
            };
            (
                finish(ctx, value, binders.range),
                names,
            )
        });
        let signature = just(Token::Signature)
            .ignore_then(just(Token::Colon))
            .ignore_then(
                binders(ctx, expr.clone()).delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .then_ignore(just(Token::Arrow))
            .then(named_results.or(expr.clone().map(move |expr| (expr, Vec::new()))))
            .map(move |(parameters, (result, results))| OpField::Signature {
                parameters,
                result,
                results,
            });
        let op_field = node(ctx, choice((
            signature,
            choice((
                identifier(ctx),
                just(Token::Attributes).to("attributes".to_owned()),
                just(Token::Exec).to("exec".to_owned()),
            ))
            .then_ignore(just(Token::Colon))
            .then(expr.clone())
            .map(move |(name, value)| OpField::Field { name, value }),
        )));
        let operation = just(Token::Op)
            .ignore_then(
                op_field
                    .separated_by(just(Token::Comma))
                    .at_least(1)
                    .allow_trailing()
                    .collect()
                    .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            )
            .map(Expression::Op);
        let local = binding(ctx, identifier(ctx).map(move |name| vec![name]).boxed(), expr.clone());
        let ir = just(Token::Ir)
            .ignore_then(just(Token::LBrace))
            .ignore_then(local.repeated().collect())
            .then_ignore(just(Token::Yield))
            .then(expr.clone())
            .then_ignore(just(Token::Semi).or_not())
            .then_ignore(just(Token::RBrace))
            .map(move |(bindings, value)| Expression::Ir {
                bindings,
                value: Box::new(value),
            });
        let primary = node(ctx, choice((
            literal(ctx).map(Expression::Literal),
            ir,
            path(ctx).map(Expression::Path),
            tuple,
            list(ctx, expr.clone())
                .delimited_by(just(Token::LBracket), just(Token::RBracket))
                .map(Expression::Array),
            record(ctx, expr.clone()).map(Expression::Record),
            block(ctx, expr.clone()).map(Expression::Block),
            function(ctx, expr.clone()).map(move |function| Expression::Function(Box::new(function))),
            conditional.map(ungroup),
            matching,
            operation,
            just(Token::Limit)
                .ignore_then(record(ctx, expr.clone()))
                .map(Expression::Limit),
        )));
        let generic_args = expr
            .clone()
            .separated_by(just(Token::Comma))
            .at_least(1)
            .allow_trailing()
            .collect()
            .delimited_by(just(Token::Less), just(Token::Greater))
            .map(Suffix::Generic);
        let suffix = node(ctx, choice((
            generic_args,
            arguments.clone().map(Suffix::Call),
            just(Token::Dot)
                .ignore_then(identifier(ctx).or(select! {Token::Natural(index)=>index}))
                .map(Suffix::Member),
            record(ctx, expr)
                .then(arguments)
                .map(move |(fields, args)| Suffix::RecordCall(fields, args)),
        )));
        let postfix = primary
            .foldl(suffix.repeated(), move |value, suffix| {
                let range = value.range.start..suffix.range.end;
                let value = Box::new(value);
                let value = match suffix.value {
                    Suffix::Generic(arguments) => Expression::Generic { value, arguments },
                    Suffix::Call(arguments) => Expression::Call { value, arguments },
                    Suffix::Member(name) => Expression::Member { value, name },
                    Suffix::RecordCall(fields, arguments) => Expression::RecordCall {
                        value,
                        fields,
                        arguments,
                    },
                };
                finish(ctx, value, range)
            })
            .boxed();
        let unary = node(ctx, choice((
            just(Token::Bang).to(UnaryOp::Not),
            just(Token::Minus).to(UnaryOp::Negate),
        )))
        .repeated()
        .foldr(postfix, move |op, value| { let range = op.range.start..value.range.end; finish(ctx, Expression::Unary { op: op.value, value: Box::new(value) }, range) })
        .boxed();
        let multiply = unary
            .clone()
            .foldl(
                choice((
                    just(Token::Star).to(BinaryOp::Multiply),
                    just(Token::Slash).to(BinaryOp::Divide),
                    just(Token::Percent).to(BinaryOp::Remainder),
                ))
                .then(unary)
                .repeated(),
                |left, (op, right)| binary(ctx, left, op, right),
            )
            .boxed();
        let add = multiply
            .clone()
            .foldl(
                choice((
                    just(Token::Plus).to(BinaryOp::Add),
                    just(Token::Minus).to(BinaryOp::Subtract),
                ))
                .then(multiply)
                .repeated(),
                |left, (op, right)| binary(ctx, left, op, right),
            )
            .boxed();
        let equality = add
            .clone()
            .then(
                choice((
                    just(Token::Equal).to(BinaryOp::Equal),
                    just(Token::NotEqual).to(BinaryOp::NotEqual),
                ))
                .then(add)
                .or_not(),
            )
            .map(move |(left, tail)| match tail {
                Some((op, right)) => binary(ctx, left, op, right),
                None => left,
            })
            .boxed();
        let and = equality
            .clone()
            .foldl(
                just(Token::And).ignore_then(equality).repeated(),
                |left, right| binary(ctx, left, BinaryOp::And, right),
            )
            .boxed();
        let or = and
            .clone()
            .foldl(
                just(Token::Or).ignore_then(and).repeated(),
                |left, right| binary(ctx, left, BinaryOp::Or, right),
            )
            .boxed();
        recursive(move |arrow| {
            or.clone()
                .then(just(Token::Arrow).ignore_then(arrow).or_not())
                .map(move |(left, right)| match right {
                    Some(right) => binary(ctx, left, BinaryOp::Arrow, right),
                    None => left,
                })
        })
        .boxed()
    })
    .boxed()
}

pub(super) fn module<'a>(ctx: &'a Context<'a>) -> P<'a, Vec<DeclNode>> {
    let expr = expression(ctx);
    recursive(move |declaration| {
    let use_decl = just(Token::Use)
        .ignore_then(path(ctx))
        .then(just(Token::PathSep).ignore_then(just(Token::Star)).or_not())
        .then(just(Token::As).ignore_then(identifier(ctx)).or_not())
        .then_ignore(just(Token::Semi))
        .try_map(move |((path, star), alias), span| {
            if star.is_some() && alias.is_some() {
                return Err(Rich::custom(span, "a wildcard import cannot have an alias"));
            }
            Ok(Declaration::Use {
                public: false,
                path,
                glob: star.is_some(),
                alias,
            })
        });
    let mod_decl = just(Token::Mod)
        .ignore_then(identifier(ctx))
        .then(just(Token::Semi).to(None).or(declaration.repeated().collect().delimited_by(just(Token::LBrace), just(Token::RBrace)).map(Some)))
        .map(move |(name, declarations)| Declaration::Mod {
            public: false,
            name,
            declarations,
        });
    let let_decl = binding(ctx, path(ctx), expr.clone()).map(move |binding| Declaration::Let {
        public: false,
        binding,
    });
    let interface = select! { Token::Ident(name) if name == "interface" => () }
        .ignore_then(identifier(ctx))
        .then(generics(ctx, expr.clone()))
        .then(node(ctx, record(ctx, expr.clone()).map(Expression::Record)))
        .map(|((name, generics), value)| Declaration::Interface {
            public: false,
            binding: Binding { path: vec![name], generics, ty: None, value },
        });
    let variant = identifier(ctx)
        .then(generics(ctx, expr.clone()))
        .then(
            list(ctx, expr.clone())
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .then(just(Token::Colon).ignore_then(expr.clone()).or_not())
        .map(move |(((name, generics), arguments), ty)| Variant {
            name,
            generics,
            arguments,
            ty,
        })
        .boxed();
    let enumeration = just(Token::Enum)
        .ignore_then(identifier(ctx))
        .then(generics(ctx, expr.clone()))
        .then(list(ctx, variant).delimited_by(just(Token::LBrace), just(Token::RBrace)))
        .map(move |((name, generics), variants)| Declaration::Enum {
            public: false,
            name,
            generics,
            variants,
        });
    let axiom = rule_binding(ctx, expr)
        .map(|(name, generics, ty)| Declaration::Axiom { public: false, name, generics, ty });
    let declaration = just(Token::Pub).or_not()
        .then(choice((use_decl, mod_decl, let_decl, interface, enumeration, axiom)))
        .map(move |(visibility, mut declaration)| {
            match &mut declaration {
                Declaration::Mod { public, .. }
                | Declaration::Let { public, .. }
                | Declaration::Interface { public, .. }
                | Declaration::Enum { public, .. }
                | Declaration::Axiom { public, .. }
                | Declaration::Use { public, .. } => *public = visibility.is_some(),
            }
            declaration
        });
    node(ctx, declaration)
    })
        .repeated()
        .collect()
        .then_ignore(end())
        .boxed()
}
}; }
mod compile {
    use super::*;
    use crate::ast::*;
    type Context<'a> = ();
    type DeclNode = Node<Declaration>;
    trait Build {
        type Built;
        fn build(self) -> Self::Built;
    }
    impl<T> Build for T {
        type Built = T;
        fn build(self) -> T {
            self
        }
    }
    fn finish<T: Build>(_: &(), value: T, range: std::ops::Range<usize>) -> Node<T::Built> {
        Node {
            value: value.build(),
            range,
        }
    }
    fn ungroup(expr: Expr) -> Expression {
        expr.value
    }
    fn is_local_table(clause: &Expr) -> bool {
        match &clause.value {
            Expression::Binary {
                op: BinaryOp::Arrow,
                right,
                ..
            } => {
                matches!(&right.value, Expression::Path(path) if path == &["Prop"])
            }
            Expression::Generic { value, .. } => {
                matches!(&value.value, Expression::Path(path) if path.last().is_some_and(|s| s == "Table"))
            }
            _ => false,
        }
    }
    grammar! { const _: () = (); }
}
mod lossless {
    use super::*;
    use crate::ast::{BinaryOp, Literal, Node, UnaryOp};
    use crate::parser_syntax::*;
    fn is_local_table(_: &Expr) -> bool {
        false
    }
    grammar! { impl Build for Suffix { type Built = Self; fn build(self, _: &Context<'_>, _: std::ops::Range<usize>) -> Self { self } } }
}
