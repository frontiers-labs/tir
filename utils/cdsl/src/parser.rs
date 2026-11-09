//! The domain-independent CDSL grammar over source tokens.
use crate::ast::*;
use crate::diagnostic::{Diagnostic, Phase};
use crate::lexer::{Token, is_keyword};
use chumsky::{
    input::{Stream, ValueInput},
    prelude::*,
};
use logos::Logos;

type Error<'tok, 'src> = extra::Err<Rich<'tok, Token<'src>>>;
type P<'tok, 'src, I, O> = Boxed<'tok, 'tok, I, O, Error<'tok, 'src>>;

fn identifier<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, String, Error<'tok, 'src>> + Clone
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    select! { Token::Word(word) if !is_keyword(word) && word != "_" => word.strip_prefix("r#").unwrap_or(word).to_owned() }
}

fn keyword<'tok, 'src: 'tok, I>(
    name: &'static str,
) -> impl Parser<'tok, I, (), Error<'tok, 'src>> + Clone
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    just(Token::Word(name)).ignored()
}

fn path<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Path, Error<'tok, 'src>> + Clone
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    just(Token::ColonColon)
        .or_not()
        .then(
            identifier().then(
                just(Token::ColonColon)
                    .ignore_then(identifier())
                    .repeated()
                    .collect::<Vec<_>>(),
            ),
        )
        .map(|(root, (first, tail))| Path {
            absolute: root.is_some(),
            segments: std::iter::once(first).chain(tail).collect(),
        })
}

fn literal<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Literal, Error<'tok, 'src>> + Clone
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    select! {
        Token::Integer(value) => Literal::Integer(value),
        Token::String(value) => Literal::String(value),
        Token::RawString(value) => Literal::String(value.replace("\r\n", "\n")),
        Token::Word("true") => Literal::Bool(true),
        Token::Word("false") => Literal::Bool(false),
    }
}

fn binary(operator: &str, left: Expr, right: Expr) -> Expr {
    let span = left.span.start..right.span.end;
    Located {
        node: Expression::Binary(operator.into(), Box::new(left), Box::new(right)),
        span,
    }
}

#[derive(Clone)]
enum Postfix {
    Call(Vec<(Option<String>, Expr)>),
    Project(String),
    Index(Expr),
}

fn pattern_grammar<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Pattern, Error<'tok, 'src>> + Clone
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    let name = identifier::<I>().boxed();
    let source_path = path::<I>().boxed();
    let literal = literal::<I>().boxed();
    let comma = just(Token::Comma);
    recursive(|pattern| {
        let constructor = source_path
            .clone()
            .then(
                pattern
                    .clone()
                    .separated_by(comma.clone())
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(|(path, fields)| PatternSyntax::Constructor(path, fields));
        let tuple = pattern
            .clone()
            .separated_by(comma.clone())
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen))
            .map(PatternSyntax::Tuple);
        let record = name
            .clone()
            .map_with(|name, e| (name, e.span()))
            .then(just(Token::Colon).ignore_then(pattern).or_not())
            .map(|((name, span), pattern)| {
                let pattern = pattern.unwrap_or_else(|| Located {
                    node: PatternSyntax::Bind(name.clone()),
                    span: span.into_range(),
                });
                (name, pattern)
            })
            .separated_by(comma.clone())
            .allow_trailing()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(PatternSyntax::Record);
        choice((
            keyword("_").to(PatternSyntax::Wildcard),
            literal.clone().map(PatternSyntax::Literal),
            constructor,
            name.clone().map(PatternSyntax::Bind),
            tuple,
            record,
        ))
        .map_with(|node, e| Located {
            node,
            span: e.span().into_range(),
        })
        .boxed()
    })
    .boxed()
}

fn precedence<'tok, 'src: 'tok, I>(primary: P<'tok, 'src, I, Expr>) -> P<'tok, 'src, I, Expr>
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    let unary = choice((just(Token::Bang).to("!"), just(Token::Minus).to("-")))
        .map_with(|operator, e| {
            let span: SimpleSpan = e.span();
            (operator, span.start)
        })
        .repeated()
        .foldr(primary, |(operator, start), value| Located {
            span: start..value.span.end,
            node: Expression::Unary(operator.into(), Box::new(value)),
        })
        .boxed();
    let multiplicative = unary
        .clone()
        .foldl(
            choice((
                just(Token::Star).to("*"),
                just(Token::Slash).to("/"),
                just(Token::Percent).to("%"),
            ))
            .then(unary)
            .repeated(),
            |left, (op, right)| binary(op, left, right),
        )
        .boxed();
    let additive = multiplicative
        .clone()
        .foldl(
            choice((just(Token::Plus).to("+"), just(Token::Minus).to("-")))
                .then(multiplicative)
                .repeated(),
            |left, (op, right)| binary(op, left, right),
        )
        .boxed();
    // Joint spans distinguish `<<` from `< <`, including intervening comments.
    let shift_operator = just(Token::Less)
        .then(just(Token::Less))
        .to("<<")
        .or(just(Token::Greater).then(just(Token::Greater)).to(">>"))
        .try_map(|operator, span: SimpleSpan| {
            if span.end - span.start == 2 {
                Ok(operator)
            } else {
                Err(Rich::custom(span, "shift punctuation must be adjacent"))
            }
        });
    let shift = additive
        .clone()
        .foldl(
            shift_operator.then(additive).repeated(),
            |left, (op, right)| binary(op, left, right),
        )
        .boxed();
    let greater_equal =
        just(Token::Greater)
            .then(just(Token::Equal))
            .try_map(|_, span: SimpleSpan| {
                if span.end - span.start == 2 {
                    Ok(">=")
                } else {
                    Err(Rich::custom(
                        span,
                        "comparison punctuation must be adjacent",
                    ))
                }
            });
    let comparison = shift
        .clone()
        .then(
            choice((
                just(Token::EqualEqual).to("=="),
                just(Token::NotEqual).to("!="),
                just(Token::LessEqual).to("<="),
                greater_equal,
                just(Token::Less).to("<"),
                just(Token::Greater).to(">"),
            ))
            .then(shift)
            .or_not(),
        )
        .map(|(left, tail)| match tail {
            Some((op, right)) => binary(op, left, right),
            None => left,
        })
        .boxed();
    let bit_and = comparison
        .clone()
        .foldl(
            just(Token::And).ignore_then(comparison).repeated(),
            |left, right| binary("&", left, right),
        )
        .boxed();
    let bit_xor = bit_and
        .clone()
        .foldl(
            just(Token::Xor).ignore_then(bit_and).repeated(),
            |left, right| binary("^", left, right),
        )
        .boxed();
    let bit_or = bit_xor
        .clone()
        .foldl(
            just(Token::Or).ignore_then(bit_xor).repeated(),
            |left, right| binary("|", left, right),
        )
        .boxed();
    let logical_and = bit_or
        .clone()
        .foldl(
            just(Token::AndAnd).ignore_then(bit_or).repeated(),
            |left, right| binary("&&", left, right),
        )
        .boxed();
    logical_and
        .clone()
        .foldl(
            just(Token::OrOr).ignore_then(logical_and).repeated(),
            |left, right| binary("||", left, right),
        )
        .boxed()
}

fn item_grammar<'tok, 'src: 'tok, I>(
    item: P<'tok, 'src, I, Item>,
    type_expression: P<'tok, 'src, I, TypeExpr>,
    expression: P<'tok, 'src, I, Expr>,
    generics: P<'tok, 'src, I, Vec<Generic>>,
) -> P<'tok, 'src, I, Item>
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    let name = identifier::<I>().boxed();
    let source_path = path::<I>().boxed();
    let comma = just(Token::Comma);
    let import_entry = name
        .clone()
        .then(keyword("as").ignore_then(name.clone()).or_not());
    let selection = just(Token::ColonColon).ignore_then(
        just(Token::Star).to(Import::Glob).or(import_entry
            .separated_by(comma.clone())
            .allow_trailing()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(Import::Group)),
    );
    let use_item = keyword("use")
        .ignore_then(source_path.clone())
        .then(selection.or_not())
        .then(keyword("as").ignore_then(name.clone()).or_not())
        .try_map(
            |((path, selection), alias), span| match (selection, alias) {
                (None, alias) => Ok(ItemKind::Use(path, Import::Name(alias))),
                (Some(selection), None) => Ok(ItemKind::Use(path, selection)),
                (Some(_), Some(_)) => Err(Rich::custom(
                    span,
                    "an import group or glob cannot have a single alias",
                )),
            },
        )
        .boxed();
    let module_item = keyword("mod")
        .ignore_then(name.clone())
        .then(
            item.clone()
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .or_not(),
        )
        .map(|(name, items)| ItemKind::Module(name, items));
    let external_function = keyword("fn")
        .ignore_then(source_path.clone())
        .then(generics.clone().or_not())
        .then(type_expression.clone())
        .try_map(|((path, generics), ty), span| {
            if !matches!(ty.node, TypeSyntax::Function(..)) {
                return Err(Rich::custom(
                    span,
                    "external function requires a function signature",
                ));
            }
            Ok(ItemKind::ExternalValue(
                path,
                generics.unwrap_or_default(),
                ty,
            ))
        });
    let external_item = keyword("extern").ignore_then(
        keyword("type")
            .ignore_then(name.clone())
            .then(generics.clone().or_not())
            .then(
                just(Token::Equal)
                    .ignore_then(type_expression.clone())
                    .or_not(),
            )
            .map_with(|((name, generics), ty), e| {
                ItemKind::ExternalType(
                    name,
                    generics.unwrap_or_default(),
                    ty.unwrap_or_else(|| Located {
                        node: TypeSyntax::Path(
                            Path {
                                absolute: false,
                                segments: vec!["Type".into()],
                            },
                            Vec::new(),
                        ),
                        span: e.span().into_range(),
                    }),
                )
            })
            .or(keyword("let")
                .ignore_then(source_path.clone())
                .then(generics.clone().or_not())
                .then_ignore(just(Token::Colon))
                .then(type_expression.clone())
                .map(|((path, generics), ty)| {
                    ItemKind::ExternalValue(path, generics.unwrap_or_default(), ty)
                }))
            .or(external_function),
    );
    let type_item = keyword("type")
        .ignore_then(name.clone())
        .then(generics.clone().or_not())
        .then_ignore(just(Token::Equal))
        .then(type_expression.clone())
        .map(|((name, generics), ty)| ItemKind::Type(name, generics.unwrap_or_default(), ty));
    let variant = name
        .clone()
        .then(generics.clone().or_not())
        .then(
            type_expression
                .clone()
                .separated_by(comma.clone())
                .allow_trailing()
                .at_least(1)
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or_not(),
        )
        .then(
            just(Token::Arrow)
                .ignore_then(type_expression.clone())
                .or_not(),
        )
        .map(|(((name, generics), fields), result)| Variant {
            name,
            generics: generics.unwrap_or_default(),
            fields: fields.unwrap_or_default(),
            result,
        });
    let enum_item = keyword("enum")
        .ignore_then(name)
        .then(generics.clone().or_not())
        .then(
            variant
                .separated_by(comma)
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|((name, generics), variants)| {
            ItemKind::Enum(name, generics.unwrap_or_default(), variants)
        });
    let binding = keyword("let")
        .ignore_then(keyword("rec").or_not())
        .then(source_path)
        .then(generics.or_not())
        .then(just(Token::Colon).ignore_then(type_expression).or_not())
        .then_ignore(just(Token::Equal))
        .then(expression)
        .map(
            |((((recursive, path), generics), annotation), value)| ItemKind::Let {
                path,
                generics: generics.unwrap_or_default(),
                recursive: recursive.is_some(),
                annotation,
                value,
            },
        );
    keyword("pub")
        .or_not()
        .then(choice((
            use_item,
            module_item.boxed(),
            external_item.boxed(),
            type_item.boxed(),
            enum_item.boxed(),
            binding.boxed(),
        )))
        .then_ignore(just(Token::Semicolon))
        .map_with(|(public, kind), e| Located {
            node: Declaration {
                public: public.is_some(),
                kind,
            },
            span: e.span().into_range(),
        })
        .boxed()
}

fn grammar<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Vec<Item>, Error<'tok, 'src>>
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    // `recursive` gives each grammar body weak back-references. Keep the owning
    // type parser beside the expression parser, outside both recursive bodies.
    let mut type_parsers = Vec::with_capacity(1);
    let mut generic_parsers = Vec::with_capacity(1);
    let expression = recursive(|expression| {
        let mut expression_parsers = Vec::with_capacity(1);
        let type_expression = recursive(|type_expression| {
            let name = identifier::<I>().boxed();
            let source_path = path::<I>().boxed();
            let literal = literal::<I>().boxed();
            let comma = just(Token::Comma);

            let parameter = name
                .clone()
                .then(
                    just(Token::Colon)
                        .ignore_then(type_expression.clone())
                        .or_not(),
                )
                .map(|(name, annotation)| Parameter { name, annotation })
                .boxed();
            let parameters = parameter
                .clone()
                .separated_by(comma.clone())
                .allow_trailing()
                .collect::<Vec<_>>()
                .boxed();
            let generics = name
                .clone()
                .then(
                    just(Token::Colon)
                        .ignore_then(type_expression.clone())
                        .or_not(),
                )
                .map(|(name, kind)| Generic { name, kind })
                .separated_by(comma.clone())
                .allow_trailing()
                .at_least(1)
                .collect::<Vec<_>>()
                .delimited_by(just(Token::Less), just(Token::Greater))
                .boxed();
            generic_parsers.push(generics.clone());

            let pattern = pattern_grammar::<I>().boxed();

            let local = keyword("let")
                .ignore_then(keyword("rec").or_not())
                .then(pattern.clone())
                .then(generics.clone().or_not())
                .then(
                    just(Token::Colon)
                        .ignore_then(type_expression.clone())
                        .or_not(),
                )
                .then_ignore(just(Token::Equal))
                .then(expression.clone())
                .then_ignore(just(Token::Semicolon))
                .map(
                    |((((recursive, pattern), generics), annotation), value)| Local {
                        recursive: recursive.is_some(),
                        pattern,
                        generics: generics.unwrap_or_default(),
                        annotation,
                        value,
                    },
                )
                .boxed();
            let block = local
                .repeated()
                .collect::<Vec<_>>()
                .then(expression.clone())
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .map_with(|(locals, value), e| Located {
                    node: Expression::Block(locals, Box::new(value)),
                    span: e.span().into_range(),
                })
                .boxed();
            let record = name
                .clone()
                .then_ignore(just(Token::Colon))
                .then(expression.clone())
                .separated_by(comma.clone())
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .map_with(|fields, e| Located {
                    node: Expression::Record(fields),
                    span: e.span().into_range(),
                })
                .boxed();

            let generic_argument = type_expression
                .clone()
                .map(GenericArg::Type)
                .or(literal.clone().map_with(|literal, e| {
                    GenericArg::Value(Located {
                        node: Expression::Literal(literal),
                        span: e.span().into_range(),
                    })
                }))
                .or(record.clone().map(GenericArg::Value))
                .or(block.clone().map(GenericArg::Value))
                .boxed();
            let generic_arguments = generic_argument
                .separated_by(comma.clone())
                .allow_trailing()
                .at_least(1)
                .collect::<Vec<_>>()
                .delimited_by(just(Token::Less), just(Token::Greater))
                .boxed();
            let type_path = source_path
                .clone()
                .then(generic_arguments.clone().or_not())
                .map(|(path, arguments)| TypeSyntax::Path(path, arguments.unwrap_or_default()));
            let function_type = parameters
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .then_ignore(just(Token::Arrow))
                .then(type_expression.clone())
                .map_with(|(parameters, result), e| Located {
                    node: TypeSyntax::Function(parameters, Box::new(result)),
                    span: e.span().into_range(),
                })
                .boxed();
            let component = name
                .clone()
                .then_ignore(just(Token::Colon))
                .or_not()
                .then(type_expression.clone())
                .boxed();
            let type_product = component
                .clone()
                .then(
                    comma
                        .clone()
                        .ignore_then(component)
                        .repeated()
                        .collect::<Vec<_>>(),
                )
                .then(comma.clone().or_not())
                .or_not()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map_with(|contents, e| {
                    let span = e.span().into_range();
                    match contents {
                        Some((((None, mut value), tail), None)) if tail.is_empty() => {
                            value.span = span;
                            value
                        }
                        Some(((first, tail), _)) => Located {
                            node: TypeSyntax::Tuple(std::iter::once(first).chain(tail).collect()),
                            span,
                        },
                        None => Located {
                            node: TypeSyntax::Tuple(Vec::new()),
                            span,
                        },
                    }
                });
            let type_parser = choice((
                function_type.clone(),
                type_product,
                type_path.map_with(|node, e| Located {
                    node,
                    span: e.span().into_range(),
                }),
            ))
            .boxed();

            // A generic suffix wins only when the following token can follow a complete value.
            let suffix_end = any()
                .filter(|token| {
                    !matches!(
                        token,
                        Token::Word(_)
                            | Token::Integer(_)
                            | Token::String(_)
                            | Token::RawString(_)
                            | Token::Arrow
                            | Token::FatArrow
                            | Token::ColonColon
                    )
                })
                .ignored()
                .rewind()
                .or(end());
            let value_path = source_path
                .clone()
                .then(generic_arguments.clone().then_ignore(suffix_end).or_not())
                .map_with(|(path, arguments), e| Located {
                    node: Expression::Path(path, arguments.unwrap_or_default()),
                    span: e.span().into_range(),
                })
                .boxed();
            let function = keyword("fn")
                .ignore_then(generics.clone().or_not())
                .then(
                    parameters
                        .clone()
                        .delimited_by(just(Token::LParen), just(Token::RParen)),
                )
                .then(
                    just(Token::Arrow)
                        .ignore_then(type_expression.clone())
                        .or_not(),
                )
                .then(block.clone())
                .map_with(|(((generics, parameters), result), body), e| Located {
                    node: Expression::Function {
                        generics: generics.unwrap_or_default(),
                        parameters,
                        result,
                        body: Box::new(body),
                    },
                    span: e.span().into_range(),
                })
                .boxed();
            let description = function_type.try_map(|ty, span| {
                if matches!(&ty.node, TypeSyntax::Function(parameters, _) if parameters.iter().any(|p| p.annotation.is_none())) {
                    Err(Rich::custom(span, "function descriptions require typed parameters"))
                } else { Ok(Located { span: ty.span.clone(), node: Expression::Description(ty) }) }
            }).boxed();
            let tuple = expression
                .clone()
                .then(
                    comma
                        .clone()
                        .ignore_then(expression.clone())
                        .repeated()
                        .collect::<Vec<_>>(),
                )
                .then(comma.clone().or_not())
                .or_not()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map_with(|contents, e| {
                    let span = e.span().into_range();
                    match contents {
                        Some(((mut first, tail), None)) if tail.is_empty() => {
                            first.span = span;
                            first
                        }
                        Some(((first, tail), _)) => Located {
                            node: Expression::Tuple(std::iter::once(first).chain(tail).collect()),
                            span,
                        },
                        None => Located {
                            node: Expression::Tuple(Vec::new()),
                            span,
                        },
                    }
                })
                .boxed();
            let list = expression
                .clone()
                .separated_by(comma.clone())
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBracket), just(Token::RBracket))
                .map_with(|values, e| Located {
                    node: Expression::List(values),
                    span: e.span().into_range(),
                });
            let conditional = recursive(|conditional| {
                keyword("if")
                    .ignore_then(expression.clone())
                    .then(block.clone())
                    .then_ignore(keyword("else"))
                    .then(block.clone().or(conditional))
                    .map_with(|((condition, yes), no), e| Located {
                        node: Expression::If(Box::new(condition), Box::new(yes), Box::new(no)),
                        span: e.span().into_range(),
                    })
            })
            .boxed();
            let match_expression = keyword("match")
                .ignore_then(expression.clone())
                .then(
                    pattern
                        .then_ignore(just(Token::FatArrow))
                        .then(expression.clone())
                        .separated_by(comma.clone())
                        .allow_trailing()
                        .at_least(1)
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                )
                .map_with(|(value, arms), e| Located {
                    node: Expression::Match(Box::new(value), arms),
                    span: e.span().into_range(),
                });
            let primary = choice((
                literal.map_with(|literal, e| Located {
                    node: Expression::Literal(literal),
                    span: e.span().into_range(),
                }),
                function,
                conditional,
                match_expression.boxed(),
                value_path,
                description,
                tuple,
                list.boxed(),
                record,
                block,
            ))
            .boxed();
            let argument = name
                .clone()
                .then_ignore(just(Token::Colon))
                .or_not()
                .then(expression.clone())
                .boxed();
            let call = argument
                .separated_by(comma.clone())
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .try_map(|arguments, _span| {
                    let mut labelled = false;
                    for (label, value) in &arguments {
                        if label.is_some() {
                            labelled = true;
                        } else if labelled {
                            return Err(Rich::custom(
                                value.span.clone().into(),
                                "positional argument follows a labelled argument",
                            ));
                        }
                    }
                    Ok(Postfix::Call(arguments))
                })
                .boxed();
            let project = just(Token::Dot)
                .ignore_then(
                    name.clone()
                        .or(select! { Token::Integer(index) => index.to_string() }),
                )
                .map(Postfix::Project);
            let index = expression
                .clone()
                .delimited_by(just(Token::LBracket), just(Token::RBracket))
                .map(Postfix::Index);
            let postfix = primary
                .foldl_with(
                    choice((call, project.boxed(), index.boxed())).repeated(),
                    |value, suffix, e| Located {
                        span: e.span().into_range(),
                        node: match suffix {
                            Postfix::Call(arguments) => {
                                Expression::Call(Box::new(value), arguments)
                            }
                            Postfix::Project(field) => Expression::Project(Box::new(value), field),
                            Postfix::Index(index) => {
                                Expression::Index(Box::new(value), Box::new(index))
                            }
                        },
                    },
                )
                .boxed();
            expression_parsers.push(precedence(postfix));
            type_parser
        });
        type_parsers.push(type_expression);
        choice(expression_parsers)
    });
    let type_expression = choice(type_parsers).boxed();
    let generics = choice(generic_parsers).boxed();
    let item = recursive(|item| {
        item_grammar(
            item.boxed(),
            type_expression.clone(),
            expression.clone().boxed(),
            generics.clone(),
        )
    });
    item.repeated().collect::<Vec<_>>().then_ignore(end())
}

/// Parse a CDSL source module, preserving byte spans in syntax and diagnostics.
pub fn parse(source_name: &str, source: &str) -> Result<Vec<Item>, Vec<Diagnostic>> {
    let mut tokens = Vec::new();
    let mut errors = Vec::new();
    for (token, span) in Token::lexer(source).spanned() {
        match token {
            Ok(token) => tokens.push((token, SimpleSpan::from(span))),
            Err(message) => errors.push(Diagnostic::new(
                source_name,
                span,
                Phase::Lex,
                if message.is_empty() {
                    "invalid CDSL token".to_owned()
                } else {
                    message
                },
            )),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let input = Stream::from_iter(tokens)
        .map((source.len()..source.len()).into(), |(token, span)| {
            (token, span)
        });
    grammar().parse(input).into_result().map_err(|errors| {
        errors
            .into_iter()
            .map(|error| {
                Diagnostic::new(
                    source_name,
                    error.span().into_range(),
                    Phase::Parse,
                    format!("invalid syntax: {:?}", error.reason()),
                )
            })
            .collect()
    })
}
