//! A [`chumsky`]-based parser turning a token stream into the [`crate::ast`], in the
//! same style as the TMDL compiler's parser (combinators over a token slice
//! with `Rich` errors).
//!
//! The grammar covers scalar declarations, C's full expression precedence
//! ladder, structured control flow, switches, and labels/goto. Language-version
//! checks are driven by [`crate::lang_options::LangOptions`].
//!
//! Nodes are appended straight into the [`Ast`] DAG carried as parser state.
//! Because combinators run bottom-up, every child is added before its parent,
//! which is exactly the post-order layout the DAG requires.

use chumsky::input::{MapExtra, ValueInput};
use chumsky::inspector::SimpleState;
use chumsky::prelude::*;
use std::collections::{HashMap, HashSet};

use tir::graph::{Dag, MutDag, NodeId};

use crate::ast::*;
use crate::diagnostics::{
    Diagnostic, FileId, LanguageFeatureUnavailable, UnexpectedEof, UnexpectedToken,
};
use crate::lang_options::LangOptions;
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
    name_scopes: Vec<NameScope>,
    next_record: u32,
}

#[derive(Default)]
struct NameScope {
    typedefs: HashSet<String>,
    ordinary: HashSet<String>,
    tags: HashMap<String, (RecordKind, RecordId)>,
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

    fn is_typedef(&self, name: &str) -> bool {
        for scope in self.name_scopes.iter().rev() {
            if scope.ordinary.contains(name) {
                return false;
            }
            if scope.typedefs.contains(name) {
                return true;
            }
        }
        false
    }

    fn declare_typedef(&mut self, name: String) {
        self.name_scopes.last_mut().unwrap().typedefs.insert(name);
    }

    fn declare_ordinary(&mut self, name: String) {
        self.name_scopes.last_mut().unwrap().ordinary.insert(name);
    }

    fn record_id(&mut self, kind: RecordKind, name: Option<&str>, defining: bool) -> RecordId {
        if let Some(name) = name {
            if let Some((_, id)) = self.name_scopes.last().unwrap().tags.get(name) {
                return *id;
            }
            if !defining {
                for scope in self.name_scopes.iter().rev().skip(1) {
                    if let Some((_, id)) = scope.tags.get(name) {
                        return *id;
                    }
                }
            }
        }
        let id = RecordId::new(self.next_record);
        self.next_record += 1;
        if let Some(name) = name {
            self.name_scopes
                .last_mut()
                .unwrap()
                .tags
                .insert(name.to_string(), (kind, id));
        }
        id
    }

    fn push_scope(&mut self) {
        self.name_scopes.push(NameScope::default());
    }

    fn pop_scope(&mut self) {
        self.name_scopes.pop();
    }
}

/// Parse a stream of tokens, each paired with its byte [`crate::diagnostics::Span`]
/// in the source. Whitespace tokens are dropped first; on failure each parser
/// error is turned into a [`Diagnostic`] whose label points back at the source.
pub fn parse(
    tokens: &[(Token, crate::diagnostics::Span)],
    options: LangOptions,
) -> Result<Ast, Vec<Diagnostic>> {
    let mut version_diagnostics = Vec::new();
    for (token, span) in tokens {
        match token {
            Token::Comment(comment)
                if comment.starts_with("//")
                    && options.std_version == crate::lang_options::StdVersion::C89
                    && !options.gnu_extensions =>
            {
                version_diagnostics
                    .push(LanguageFeatureUnavailable::new(*span, "line comment", "C89").into());
            }
            Token::IntegerLiteral(literal)
                if literal.spelling.to_ascii_lowercase().starts_with("0b")
                    && options.std_version < crate::lang_options::StdVersion::C23
                    && !options.gnu_extensions =>
            {
                version_diagnostics.push(
                    LanguageFeatureUnavailable::new(*span, "binary integer literal", "C17").into(),
                );
            }
            Token::IntegerLiteral(literal)
                if literal.spelling.contains('\'')
                    && options.std_version < crate::lang_options::StdVersion::C23 =>
            {
                version_diagnostics
                    .push(LanguageFeatureUnavailable::new(*span, "digit separator", "C17").into());
            }
            _ => {}
        }
    }
    if !version_diagnostics.is_empty() {
        return Err(version_diagnostics);
    }
    let mut filtered = Vec::with_capacity(tokens.len());
    let mut byte_spans = Vec::with_capacity(tokens.len());
    for (tok, span) in tokens {
        if !matches!(tok, Token::Whitespace(_) | Token::Comment(_)) {
            filtered.push(keyword_for_standard(tok.clone(), options));
            byte_spans.push(*span);
        }
    }

    let mut state = SimpleState(ParseState {
        ast: Ast::new(),
        spans: byte_spans.clone(),
        name_scopes: vec![NameScope::default()],
        next_record: 0,
    });
    let (out, errors) = translation_unit()
        .parse_with_state(filtered.as_slice(), &mut state)
        .into_output_errors();

    match out {
        Some(_) if errors.is_empty() => {
            let ast = state.0.ast;
            let diagnostics = validate_language_version(&ast, options);
            if diagnostics.is_empty() {
                Ok(ast)
            } else {
                Err(diagnostics)
            }
        }
        _ => Err(errors
            .into_iter()
            .map(|e| rich_to_diagnostic(&e, &byte_spans))
            .collect()),
    }
}

fn keyword_for_standard(token: Token, options: LangOptions) -> Token {
    use crate::lang_options::StdVersion;

    if options.std_version == StdVersion::C89 && !options.gnu_extensions {
        match token {
            Token::KwInline => return Token::Identifier("inline".to_string()),
            Token::KwRestrict => return Token::Identifier("restrict".to_string()),
            Token::KwUnderscoreBool => return Token::Identifier("_Bool".to_string()),
            _ => {}
        }
    }
    if options.std_version < StdVersion::C23 {
        match token {
            Token::KwAlignas => Token::Identifier("alignas".to_string()),
            Token::KwAlignof => Token::Identifier("alignof".to_string()),
            Token::KwBool => Token::Identifier("bool".to_string()),
            Token::KwConstexpr => Token::Identifier("constexpr".to_string()),
            Token::KwFalse => Token::Identifier("false".to_string()),
            Token::KwNullptr => Token::Identifier("nullptr".to_string()),
            Token::KwStaticAssert => Token::Identifier("static_assert".to_string()),
            Token::KwThreadLocal => Token::Identifier("thread_local".to_string()),
            Token::KwTrue => Token::Identifier("true".to_string()),
            Token::KwTypeof | Token::KwTypeofUnqual if !options.gnu_extensions => {
                Token::Identifier(token.to_string())
            }
            other => other,
        }
    } else {
        token
    }
}

fn validate_language_version(ast: &Ast, options: LangOptions) -> Vec<Diagnostic> {
    if options.gnu_extensions || options.std_version >= crate::lang_options::StdVersion::C99 {
        return Vec::new();
    }
    let mut diagnostics = Vec::new();
    let Some(root) = ast.root() else {
        return diagnostics;
    };
    for node in ast.postorder(root) {
        let ty = match ast.get_leaf_data(node) {
            Some(
                AstLeaf::Typedef { ty, .. }
                | AstLeaf::Global { ty, .. }
                | AstLeaf::Field { ty, .. }
                | AstLeaf::Param { ty, .. }
                | AstLeaf::Decl { ty, .. }
                | AstLeaf::Type(ty),
            ) => Some(ty),
            Some(AstLeaf::Function { ret, .. }) => Some(ret),
            _ => None,
        };
        if ty.is_some_and(ctype_contains_long_long) {
            diagnostics.push(
                LanguageFeatureUnavailable::new(
                    ast.get_node(node).span,
                    "long long integer type",
                    "C89",
                )
                .into(),
            );
        }
        if ast.get_node(node).kind == AstKind::For
            && let Some(init) = ast.children(node).next()
            && ast.get_node(init).kind == AstKind::Decl
        {
            diagnostics.push(
                LanguageFeatureUnavailable::new(
                    ast.get_node(init).span,
                    "declaration in for initializer",
                    "C89",
                )
                .into(),
            );
        }
        if matches!(ast.get_node(node).kind, AstKind::Block | AstKind::Function) {
            let mut saw_statement = false;
            for child in ast.children(node) {
                match ast.get_node(child).kind {
                    AstKind::Param | AstKind::VarArgs => {}
                    AstKind::Decl | AstKind::Typedef if saw_statement => diagnostics.push(
                        LanguageFeatureUnavailable::new(
                            ast.get_node(child).span,
                            "declaration after statement",
                            "C89",
                        )
                        .into(),
                    ),
                    AstKind::Decl | AstKind::Typedef => {}
                    _ => saw_statement = true,
                }
            }
        }
    }
    diagnostics
}

fn ctype_contains_long_long(ty: &CType) -> bool {
    match ty {
        CType::LongLong | CType::UnsignedLongLong => true,
        CType::Const(inner)
        | CType::Volatile(inner)
        | CType::Restrict(inner)
        | CType::Pointer(inner)
        | CType::Array(inner, _)
        | CType::Attributed(inner, _) => ctype_contains_long_long(inner),
        CType::Function { ret, params, .. } => {
            ctype_contains_long_long(ret)
                || params
                    .iter()
                    .any(|param| ctype_contains_long_long(&param.ty))
        }
        _ => false,
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
    let qualifier = choice((
        just(Token::KwConst),
        just(Token::KwRestrict),
        just(Token::KwVolatile),
    ));
    let builtin_atom = select! {
        tok @ Token::KwInt => tok,
        tok @ Token::KwVoid => tok,
        tok @ Token::KwChar => tok,
        tok @ Token::KwLong => tok,
        tok @ Token::KwShort => tok,
        tok @ Token::KwSigned => tok,
        tok @ Token::KwUnsigned => tok,
        tok @ Token::KwBool => tok,
        tok @ Token::KwUnderscoreBool => tok,
        tok @ Token::KwFloat => tok,
        tok @ Token::KwDouble => tok,
    };
    let builtin = builtin_atom
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|atoms| builtin_type(&atoms));
    let named = select! { Token::Identifier(name) => name }.try_map_with(
        |name, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            if e.state().0.is_typedef(&name) {
                Ok(CType::Named(name))
            } else {
                Err(Rich::custom(e.span(), "expected type name"))
            }
        },
    );
    let record = just(Token::KwStruct).ignore_then(ident()).map_with(
        |name, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let id = e
                .state()
                .0
                .record_id(RecordKind::Struct, Some(&name), false);
            CType::Record(RecordKind::Struct, id, Some(name))
        },
    );
    let base = choice((builtin, record, named));
    let pointer = just(Token::Star).ignore_then(qualifier.clone().repeated().collect::<Vec<_>>());

    qualifier
        .repeated()
        .collect::<Vec<_>>()
        .then(base)
        .then(pointer.repeated().collect::<Vec<_>>())
        .map(|((qualifiers, mut ty), pointers)| {
            ty = apply_qualifiers(ty, &qualifiers);
            for qualifiers in pointers {
                ty = CType::Pointer(Box::new(ty));
                ty = apply_qualifiers(ty, &qualifiers);
            }
            ty
        })
}

fn apply_qualifiers(mut ty: CType, qualifiers: &[Token]) -> CType {
    if qualifiers.iter().any(|token| token == &Token::KwConst) {
        ty = CType::Const(Box::new(ty));
    }
    if qualifiers.iter().any(|token| token == &Token::KwVolatile) {
        ty = CType::Volatile(Box::new(ty));
    }
    if qualifiers.iter().any(|token| token == &Token::KwRestrict) {
        ty = CType::Restrict(Box::new(ty));
    }
    ty
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
        let assignment = recursive(|assignment| {
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
                call,
                var,
                expr.clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            ));
            let postfix = primary
                .then(
                    choice((
                        just(Token::PlusPlus).to((Some(AstKind::PostInc), None)),
                        just(Token::MinusMinus).to((Some(AstKind::PostDec), None)),
                        just(Token::Dot)
                            .ignore_then(ident())
                            .map(|name| (None, Some((false, name)))),
                        just(Token::Arrow)
                            .ignore_then(ident())
                            .map(|name| (None, Some((true, name)))),
                    ))
                    .repeated()
                    .collect::<Vec<_>>(),
                )
                .map_with(
                    |(operand, ops), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                        let tok = e.span().start;
                        let st = &mut e.state().0;
                        ops.into_iter().fold(operand, |child, (op, member)| {
                            if let Some(op) = op {
                                return unary(st, op, child, tok);
                            }
                            let (indirect, name) = member.unwrap();
                            let id = st.add(AstKind::Member, tok);
                            st.ast.set_leaf_data(id, AstLeaf::Member { name, indirect });
                            st.ast.add_edge(id, child);
                            id
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
            let conditional = logical_or
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
                .boxed();
            conditional
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
                .boxed()
        });
        binop(assignment, just(Token::Comma).to(AstKind::Comma))
    })
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
                st.declare_ordinary(name.clone());
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

fn close_scope<'src, I>() -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    just(Token::RBrace).map_with(|_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        e.state().0.pop_scope();
    })
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
            decl,
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

fn function<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let param = ctype().then(ident().or_not()).map_with(
        |(ty, name), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Param, tok);
            st.ast.set_leaf_data(
                id,
                AstLeaf::Param {
                    name: name.unwrap_or_default(),
                    ty,
                },
            );
            id
        },
    );
    let varargs =
        just(Token::Ellipsis).map_with(|_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            e.state().0.add(AstKind::VarArgs, tok)
        });

    let params = choice((param, varargs))
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen));
    let storage = choice((
        just(Token::KwExtern).ignored(),
        just(Token::KwStatic).ignored(),
        just(Token::KwInline).ignored(),
    ))
    .repeated()
    .ignored();

    let header = storage.ignore_then(ctype().then(ident()).then(params));
    let definition_header = header.clone().then_ignore(just(Token::LBrace)).map_with(
        |header, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let st = &mut e.state().0;
            st.push_scope();
            for &param in &header.1 {
                if let Some(AstLeaf::Param { name, .. }) = st.ast.get_leaf_data(param)
                    && !name.is_empty()
                {
                    st.declare_ordinary(name.clone());
                }
            }
            header
        },
    );
    let body = stmt()
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(close_scope());
    let definition = definition_header.then(body).map_with(
        |(((ret, name), params), body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Function, tok);
            let has_parameter_type_list = !params.is_empty();
            st.ast.set_leaf_data(
                id,
                AstLeaf::Function {
                    name,
                    ret,
                    has_parameter_type_list,
                },
            );
            let params = params
                .into_iter()
                .filter(|&param| !is_void_param(&st.ast, param))
                .collect::<Vec<_>>();
            for child in params.into_iter().chain(body) {
                st.ast.add_edge(id, child);
            }
            id
        },
    );
    let prototype = header.then_ignore(just(Token::Semicolon)).map_with(
        |((ret, name), params), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Prototype, tok);
            let has_parameter_type_list = !params.is_empty();
            st.ast.set_leaf_data(
                id,
                AstLeaf::Function {
                    name,
                    ret,
                    has_parameter_type_list,
                },
            );
            let params = params
                .into_iter()
                .filter(|&param| !is_void_param(&st.ast, param))
                .collect::<Vec<_>>();
            for param in params {
                st.ast.add_edge(id, param);
            }
            id
        },
    );

    choice((definition, prototype))
}

fn is_void_param(ast: &Ast, param: NodeId) -> bool {
    matches!(
        ast.get_leaf_data(param),
        Some(AstLeaf::Param {
            name,
            ty: CType::Void,
        }) if name.is_empty()
    )
}

struct DeclParser<'a> {
    tokens: &'a [Token],
    pos: usize,
    attrs: Vec<String>,
}

struct DeclSpecs {
    ty: CType,
    storage: Vec<Token>,
    record: Option<NodeId>,
}

struct Declarator {
    name: String,
    ty: CType,
}

impl<'a> DeclParser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            attrs: Vec::new(),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, tok: &Token) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Token) -> Result<(), String> {
        self.eat(tok)
            .then_some(())
            .ok_or_else(|| format!("expected {tok}"))
    }

    fn is_done(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn parse_specs(&mut self, st: &mut ParseState, tok: usize) -> Result<DeclSpecs, String> {
        let mut storage = Vec::new();
        let mut qualifiers = Vec::new();
        let mut spec_tokens = Vec::new();
        let mut record = None;
        let mut ty = None;

        loop {
            match self.peek() {
                Some(Token::KwTypedef | Token::KwExtern | Token::KwStatic | Token::KwInline) => {
                    storage.push(self.next().unwrap());
                }
                Some(Token::KwConst | Token::KwVolatile | Token::KwRestrict) => {
                    qualifiers.push(self.next().unwrap());
                }
                Some(Token::KwStruct) => {
                    self.next();
                    let (record_ty, record_node) =
                        self.parse_record(st, tok, RecordKind::Struct)?;
                    ty = Some(record_ty);
                    record = record_node;
                    break;
                }
                Some(Token::KwUnion) => {
                    self.next();
                    let (record_ty, record_node) = self.parse_record(st, tok, RecordKind::Union)?;
                    ty = Some(record_ty);
                    record = record_node;
                    break;
                }
                Some(Token::KwEnum) => {
                    self.next();
                    let name = match self.peek() {
                        Some(Token::Identifier(_)) => match self.next().unwrap() {
                            Token::Identifier(name) => Some(name),
                            _ => unreachable!(),
                        },
                        _ => None,
                    };
                    if self.eat(&Token::LBrace) {
                        self.skip_balanced(Token::LBrace, Token::RBrace)?;
                    }
                    ty = Some(CType::Enum(name));
                    break;
                }
                Some(
                    Token::KwVoid
                    | Token::KwBool
                    | Token::KwUnderscoreBool
                    | Token::KwChar
                    | Token::KwShort
                    | Token::KwInt
                    | Token::KwLong
                    | Token::KwSigned
                    | Token::KwUnsigned
                    | Token::KwFloat
                    | Token::KwDouble,
                ) => spec_tokens.push(self.next().unwrap()),
                Some(Token::Identifier(name)) if is_decl_attr_name(name) => {
                    let attr = self.parse_attr()?;
                    self.attrs.push(attr);
                }
                Some(Token::Identifier(_)) if spec_tokens.is_empty() && ty.is_none() => {
                    if let Token::Identifier(name) = self.next().unwrap() {
                        ty = Some(CType::Named(name));
                    }
                    break;
                }
                _ => break,
            }
        }

        if ty.is_none() && !spec_tokens.is_empty() {
            ty = Some(builtin_type(&spec_tokens));
        }

        let mut ty = ty.ok_or_else(|| "expected declaration type".to_string())?;
        for qualifier in qualifiers.into_iter().rev() {
            ty = match qualifier {
                Token::KwConst => CType::Const(Box::new(ty)),
                Token::KwVolatile => CType::Volatile(Box::new(ty)),
                Token::KwRestrict => CType::Restrict(Box::new(ty)),
                _ => ty,
            };
        }
        if !self.attrs.is_empty() {
            ty = CType::Attributed(Box::new(ty), std::mem::take(&mut self.attrs));
        }
        Ok(DeclSpecs {
            ty,
            storage,
            record,
        })
    }

    fn parse_record(
        &mut self,
        st: &mut ParseState,
        tok: usize,
        kind: RecordKind,
    ) -> Result<(CType, Option<NodeId>), String> {
        let name = match self.peek() {
            Some(Token::Identifier(_)) => match self.next().unwrap() {
                Token::Identifier(name) => Some(name),
                _ => unreachable!(),
            },
            _ => None,
        };
        let defining = self.peek() == Some(&Token::LBrace);
        let record_id = st.record_id(kind, name.as_deref(), defining);
        let mut record = None;
        if self.eat(&Token::LBrace) {
            let mut fields = Vec::new();
            while !self.eat(&Token::RBrace) {
                if self.is_done() {
                    return Err("unterminated record declaration".to_string());
                }
                fields.extend(self.parse_field_decl(st, tok)?);
            }
            let id = st.add(AstKind::RecordDecl, tok);
            st.ast.set_leaf_data(
                id,
                AstLeaf::Record {
                    id: record_id,
                    kind,
                    name: name.clone(),
                },
            );
            for field in fields {
                st.ast.add_edge(id, field);
            }
            record = Some(id);
        }
        Ok((CType::Record(kind, record_id, name), record))
    }

    fn parse_field_decl(&mut self, st: &mut ParseState, tok: usize) -> Result<Vec<NodeId>, String> {
        let specs = self.parse_specs(st, tok)?;
        let mut fields = Vec::new();
        loop {
            self.consume_attrs()?;
            let mut decl = self.parse_declarator(specs.ty.clone())?;
            self.consume_attrs()?;
            decl.ty = self.take_attrs(decl.ty);
            self.consume_bitfield()?;
            let id = st.add(AstKind::Field, tok);
            st.ast.set_leaf_data(
                id,
                AstLeaf::Field {
                    name: decl.name,
                    ty: decl.ty,
                },
            );
            fields.push(id);
            if self.eat(&Token::Comma) {
                continue;
            }
            self.expect(&Token::Semicolon)?;
            break;
        }
        Ok(fields)
    }

    fn parse_declarator(&mut self, mut base: CType) -> Result<Declarator, String> {
        while self.eat(&Token::Star) {
            let attrs = self.consume_pointer_attrs()?;
            base = CType::Pointer(Box::new(base));
            if !attrs.is_empty() {
                base = CType::Attributed(Box::new(base), attrs);
            }
        }

        let mut decl = if self.eat(&Token::LParen) {
            if self.eat(&Token::Star) {
                let attrs = self.consume_pointer_attrs()?;
                let name = self.parse_name()?;
                self.expect(&Token::RParen)?;
                let ty = if self.eat(&Token::LParen) {
                    let (params, varargs, has_parameter_type_list) = self.parse_param_list()?;
                    CType::Pointer(Box::new(CType::Function {
                        ret: Box::new(base),
                        params,
                        varargs,
                        has_parameter_type_list,
                    }))
                } else {
                    CType::Pointer(Box::new(base))
                };
                let ty = if attrs.is_empty() {
                    ty
                } else {
                    CType::Attributed(Box::new(ty), attrs)
                };
                Declarator { name, ty }
            } else {
                let decl = self.parse_declarator(base)?;
                self.expect(&Token::RParen)?;
                decl
            }
        } else {
            Declarator {
                name: self.parse_name()?,
                ty: base,
            }
        };

        loop {
            if self.eat(&Token::LBracket) {
                let len = self.collect_until_matching(Token::LBracket, Token::RBracket)?;
                let len = (!len.is_empty()).then_some(tokens_text(&len));
                decl.ty = CType::Array(Box::new(decl.ty), len);
            } else if self.eat(&Token::LParen) {
                let (params, varargs, has_parameter_type_list) = self.parse_param_list()?;
                decl.ty = CType::Function {
                    ret: Box::new(decl.ty),
                    params,
                    varargs,
                    has_parameter_type_list,
                };
            } else {
                break;
            }
        }

        Ok(decl)
    }

    fn parse_name(&mut self) -> Result<String, String> {
        self.consume_attrs()?;
        match self.next() {
            Some(Token::Identifier(name)) => Ok(name),
            Some(tok) => Err(format!("expected declarator name, found {tok}")),
            None => Err("expected declarator name".to_string()),
        }
    }

    fn parse_param_list(&mut self) -> Result<(Vec<CParam>, bool, bool), String> {
        let mut params = Vec::new();
        let mut varargs = false;
        if self.eat(&Token::RParen) {
            return Ok((params, varargs, false));
        }
        loop {
            if self.eat(&Token::Ellipsis) {
                varargs = true;
            } else {
                let specs = self.parse_specs_for_param()?;
                let param = if matches!(self.peek(), Some(Token::Comma | Token::RParen)) {
                    CParam {
                        name: String::new(),
                        ty: specs,
                    }
                } else {
                    let pos = self.pos;
                    let attrs = self.attrs.clone();
                    let decl = match self.parse_declarator(specs.clone()) {
                        Ok(decl) => decl,
                        Err(_) => {
                            self.pos = pos;
                            self.attrs = attrs;
                            self.parse_abstract_declarator(specs)?
                        }
                    };
                    CParam {
                        name: decl.name,
                        ty: decl.ty,
                    }
                };
                if !matches!(param.ty, CType::Void) || !param.name.is_empty() {
                    params.push(param);
                }
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            self.expect(&Token::RParen)?;
            break;
        }
        Ok((params, varargs, true))
    }

    fn parse_abstract_declarator(&mut self, mut base: CType) -> Result<Declarator, String> {
        while self.eat(&Token::Star) {
            let attrs = self.consume_pointer_attrs()?;
            base = CType::Pointer(Box::new(base));
            if !attrs.is_empty() {
                base = CType::Attributed(Box::new(base), attrs);
            }
        }
        self.consume_attrs()?;

        while self.eat(&Token::LBracket) {
            let len = self.collect_until_matching(Token::LBracket, Token::RBracket)?;
            let len = (!len.is_empty()).then_some(tokens_text(&len));
            base = CType::Array(Box::new(base), len);
        }

        if !matches!(self.peek(), Some(Token::Comma | Token::RParen)) {
            return Err("expected abstract declarator".to_string());
        }
        Ok(Declarator {
            name: String::new(),
            ty: base,
        })
    }

    fn parse_specs_for_param(&mut self) -> Result<CType, String> {
        let mut scratch = ParseState {
            ast: Ast::new(),
            spans: Vec::new(),
            name_scopes: vec![NameScope::default()],
            next_record: 0,
        };
        self.parse_specs(&mut scratch, 0).map(|specs| specs.ty)
    }

    fn consume_bitfield(&mut self) -> Result<(), String> {
        if self.eat(&Token::Colon) {
            while !matches!(self.peek(), Some(Token::Comma | Token::Semicolon) | None) {
                self.next();
            }
        }
        Ok(())
    }

    fn consume_pointer_attrs(&mut self) -> Result<Vec<String>, String> {
        let mut attrs = Vec::new();
        loop {
            match self.peek() {
                Some(Token::KwConst | Token::KwVolatile | Token::KwRestrict) => {
                    attrs.push(self.next().unwrap().to_string());
                }
                Some(Token::Identifier(name)) if is_decl_attr_name(name) => {
                    attrs.push(self.parse_attr()?);
                }
                _ => break,
            }
        }
        Ok(attrs)
    }

    fn consume_attrs(&mut self) -> Result<(), String> {
        while matches!(self.peek(), Some(Token::Identifier(name)) if is_decl_attr_name(name)) {
            let attr = self.parse_attr()?;
            self.attrs.push(attr);
        }
        Ok(())
    }

    fn take_attrs(&mut self, ty: CType) -> CType {
        if self.attrs.is_empty() {
            ty
        } else {
            CType::Attributed(Box::new(ty), std::mem::take(&mut self.attrs))
        }
    }

    fn parse_attr(&mut self) -> Result<String, String> {
        let name = match self.next() {
            Some(Token::Identifier(name)) => name,
            _ => unreachable!(),
        };
        let name = match name.as_str() {
            "__restrict" | "__restrict__" => "restrict".to_string(),
            _ => name,
        };
        if self.eat(&Token::LParen) {
            let args = self.collect_until_matching(Token::LParen, Token::RParen)?;
            Ok(format!("{name}({})", tokens_text(&args)))
        } else {
            Ok(name)
        }
    }

    fn skip_balanced(&mut self, open: Token, close: Token) -> Result<(), String> {
        let mut depth = 1usize;
        while let Some(tok) = self.next() {
            if tok == open {
                depth += 1;
            } else if tok == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
        }
        Err(format!("expected {close}"))
    }

    fn collect_until_matching(&mut self, open: Token, close: Token) -> Result<Vec<Token>, String> {
        let mut depth = 1usize;
        let mut out = Vec::new();
        while let Some(tok) = self.next() {
            if tok == open {
                depth += 1;
                out.push(tok);
            } else if tok == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
                out.push(tok);
            } else {
                out.push(tok);
            }
        }
        Err(format!("expected {close}"))
    }
}

fn builtin_type(tokens: &[Token]) -> CType {
    if !valid_builtin_type(tokens) {
        return CType::Invalid(tokens_text(tokens));
    }
    let unsigned = tokens.iter().any(|tok| matches!(tok, Token::KwUnsigned));
    let signed = tokens.iter().any(|tok| matches!(tok, Token::KwSigned));
    let long_count = tokens
        .iter()
        .filter(|tok| matches!(tok, Token::KwLong))
        .count();
    if tokens.iter().any(|tok| matches!(tok, Token::KwVoid)) {
        CType::Void
    } else if tokens
        .iter()
        .any(|tok| matches!(tok, Token::KwBool | Token::KwUnderscoreBool))
    {
        CType::Bool
    } else if tokens.iter().any(|tok| matches!(tok, Token::KwFloat)) {
        CType::Float
    } else if tokens.iter().any(|tok| matches!(tok, Token::KwDouble)) {
        if long_count > 0 {
            CType::LongDouble
        } else {
            CType::Double
        }
    } else if tokens.iter().any(|tok| matches!(tok, Token::KwChar)) {
        if unsigned {
            CType::UnsignedChar
        } else if signed {
            CType::SignedChar
        } else {
            CType::Char
        }
    } else if tokens.iter().any(|tok| matches!(tok, Token::KwShort)) {
        if unsigned {
            CType::UnsignedShort
        } else {
            CType::Short
        }
    } else if long_count >= 2 {
        if unsigned {
            CType::UnsignedLongLong
        } else {
            CType::LongLong
        }
    } else if long_count == 1 {
        if unsigned {
            CType::UnsignedLong
        } else {
            CType::Long
        }
    } else if unsigned {
        CType::UnsignedInt
    } else if tokens
        .iter()
        .any(|tok| matches!(tok, Token::KwInt | Token::KwSigned))
    {
        CType::Int
    } else {
        CType::Builtin(tokens_text(tokens))
    }
}

fn valid_builtin_type(tokens: &[Token]) -> bool {
    let count = |needle: &Token| tokens.iter().filter(|token| *token == needle).count();
    let void = count(&Token::KwVoid);
    let boolean = count(&Token::KwUnderscoreBool) + count(&Token::KwBool);
    let float = count(&Token::KwFloat);
    let double = count(&Token::KwDouble);
    let char_ = count(&Token::KwChar);
    let short = count(&Token::KwShort);
    let long = count(&Token::KwLong);
    let signed = count(&Token::KwSigned);
    let unsigned = count(&Token::KwUnsigned);
    let int = count(&Token::KwInt);

    if signed > 1 || unsigned > 1 || signed + unsigned > 1 || int > 1 || short > 1 || long > 2 {
        return false;
    }
    if void + boolean + float > 0 {
        return tokens.len() == 1;
    }
    if double > 0 {
        return double == 1 && long <= 1 && tokens.len() == 1 + long;
    }
    if char_ > 0 {
        return char_ == 1 && tokens.len() == 1 + signed + unsigned;
    }
    if short > 0 {
        return long == 0 && tokens.len() == short + signed + unsigned + int;
    }
    if long > 0 {
        return tokens.len() == long + signed + unsigned + int;
    }
    !tokens.is_empty() && tokens.len() == signed + unsigned + int
}

fn is_decl_attr_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        name,
        "_Nullable"
            | "_Nonnull"
            | "_Null_unspecified"
            | "__restrict"
            | "__restrict__"
            | "__THROW"
            | "__THROWNL"
            | "__wur"
            | "__nonnull"
            | "__attribute_malloc__"
            | "__attr_dealloc"
            | "__COLD"
            | "__fortified_attr_access"
            | "__attribute__"
            | "__asm"
            | "__asm__"
            | "__swift_nonisolated_unsafe"
            | "__swift_unavailable"
            | "__ptr"
    ) || name.starts_with("__")
        && (lower.contains("like")
            || lower.contains("alias")
            || lower.contains("availability")
            || lower.contains("deprecated"))
        || name.starts_with("_LIBC_")
}

fn tokens_text(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn add_param_node(st: &mut ParseState, tok: usize, mut param: CParam) -> NodeId {
    let id = st.add(AstKind::Param, tok);
    param.name.clear();
    st.ast.set_leaf_data(
        id,
        AstLeaf::Param {
            name: param.name,
            ty: param.ty,
        },
    );
    id
}

fn add_varargs_node(st: &mut ParseState, tok: usize) -> NodeId {
    st.add(AstKind::VarArgs, tok)
}

fn split_function_type(ty: CType) -> Result<(CType, Vec<CParam>, bool, bool), CType> {
    match ty {
        CType::Function {
            ret,
            params,
            varargs,
            has_parameter_type_list,
        } => Ok((*ret, params, varargs, has_parameter_type_list)),
        CType::Attributed(inner, attrs) => match *inner {
            CType::Function {
                ret,
                params,
                varargs,
                has_parameter_type_list,
            } => Ok((
                CType::Attributed(ret, attrs),
                params,
                varargs,
                has_parameter_type_list,
            )),
            other => Err(CType::Attributed(Box::new(other), attrs)),
        },
        other => Err(other),
    }
}

fn top_level_attr<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let group = recursive(|group| {
        let atom = any()
            .filter(|tok: &Token| !matches!(tok, Token::LParen | Token::RParen))
            .map(|tok| vec![tok]);
        choice((
            group
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .map(|parts| {
                    let mut toks = vec![Token::LParen];
                    toks.extend(parts.into_iter().flatten());
                    toks.push(Token::RParen);
                    toks
                })
                .delimited_by(just(Token::LParen), just(Token::RParen)),
            atom,
        ))
    });

    select! { Token::Identifier(name) => name }
        .try_map(|name, span| {
            is_decl_attr_name(&name)
                .then_some(name)
                .ok_or_else(|| Rich::custom(span, "expected top-level attribute"))
        })
        .then(
            group
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map_with(|(name, args), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Attribute, tok);
            let args = args.into_iter().flatten().collect::<Vec<_>>();
            st.ast.set_leaf_data(
                id,
                AstLeaf::Attribute(format!("{name}({})", tokens_text(&args))),
            );
            id
        })
}

fn top_level_marker<'src, I>() -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    select! { Token::Identifier(name) => name }
        .try_map(|name, span| {
            is_top_level_marker(&name)
                .then_some(())
                .ok_or_else(|| Rich::custom(span, "expected top-level marker"))
        })
        .then_ignore(just(Token::Semicolon).or_not())
}

fn parse_external_tokens(
    st: &mut ParseState,
    tok: usize,
    tokens: &[Token],
) -> Result<NodeId, String> {
    let mut parser = DeclParser::new(tokens);
    let specs = parser.parse_specs(st, tok)?;
    let is_typedef = specs
        .storage
        .iter()
        .any(|tok| matches!(tok, Token::KwTypedef));
    let is_extern = specs
        .storage
        .iter()
        .any(|tok| matches!(tok, Token::KwExtern));
    let mut nodes = Vec::new();
    if let Some(record) = specs.record {
        nodes.push(record);
    }

    if parser.is_done() {
        if nodes.len() == 1 {
            return Ok(nodes[0]);
        }
        if let CType::Record(kind, record_id, name) = specs.ty {
            let id = st.add(AstKind::RecordDecl, tok);
            st.ast.set_leaf_data(
                id,
                AstLeaf::Record {
                    id: record_id,
                    kind,
                    name,
                },
            );
            return Ok(id);
        }
        return Err("record declaration has no declarator".to_string());
    }

    loop {
        parser.consume_attrs()?;
        let mut decl = parser.parse_declarator(specs.ty.clone())?;
        parser.consume_attrs()?;
        decl.ty = parser.take_attrs(decl.ty);
        let ty = decl.ty;
        if !is_typedef
            && let Ok((ret, params, varargs, has_parameter_type_list)) =
                split_function_type(ty.clone())
        {
            let params = params
                .into_iter()
                .map(|param| add_param_node(st, tok, param))
                .collect::<Vec<_>>();
            let varargs = varargs.then(|| add_varargs_node(st, tok));
            let id = st.add(AstKind::Prototype, tok);
            st.ast.set_leaf_data(
                id,
                AstLeaf::Function {
                    name: decl.name,
                    ret,
                    has_parameter_type_list,
                },
            );
            for param in params {
                st.ast.add_edge(id, param);
            }
            if let Some(varargs) = varargs {
                st.ast.add_edge(id, varargs);
            }
            nodes.push(id);
        } else {
            match ty {
                ty if is_typedef => {
                    st.declare_typedef(decl.name.clone());
                    let id = st.add(AstKind::Typedef, tok);
                    st.ast.set_leaf_data(
                        id,
                        AstLeaf::Typedef {
                            name: decl.name,
                            ty,
                        },
                    );
                    nodes.push(id);
                }
                ty if is_extern => {
                    let id = st.add(AstKind::Global, tok);
                    st.ast.set_leaf_data(
                        id,
                        AstLeaf::Global {
                            name: decl.name,
                            ty,
                        },
                    );
                    nodes.push(id);
                }
                _ => return Err("unsupported top-level declaration".to_string()),
            }
        }

        if parser.eat(&Token::Comma) {
            continue;
        }
        if parser.is_done() {
            break;
        }
        return Err(format!(
            "unexpected token in declaration: {}",
            parser.peek().unwrap()
        ));
    }

    if nodes.len() == 1 {
        Ok(nodes[0])
    } else {
        let group = st.add(AstKind::DeclGroup, tok);
        for node in nodes {
            st.ast.add_edge(group, node);
        }
        Ok(group)
    }
}

fn external_decl<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let group = recursive(|group| {
        let atom = any()
            .filter(|tok: &Token| {
                !matches!(
                    tok,
                    Token::LBrace
                        | Token::RBrace
                        | Token::LParen
                        | Token::RParen
                        | Token::LBracket
                        | Token::RBracket
                )
            })
            .map(|tok| vec![tok]);
        choice((
            group
                .clone()
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .map(|parts| {
                    let mut toks = vec![Token::LBrace];
                    toks.extend(parts.into_iter().flatten());
                    toks.push(Token::RBrace);
                    toks
                })
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
            group
                .clone()
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .map(|parts| {
                    let mut toks = vec![Token::LParen];
                    toks.extend(parts.into_iter().flatten());
                    toks.push(Token::RParen);
                    toks
                })
                .delimited_by(just(Token::LParen), just(Token::RParen)),
            group
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .map(|parts| {
                    let mut toks = vec![Token::LBracket];
                    toks.extend(parts.into_iter().flatten());
                    toks.push(Token::RBracket);
                    toks
                })
                .delimited_by(just(Token::LBracket), just(Token::RBracket)),
            atom,
        ))
    });
    let outer_atom = any()
        .filter(|tok: &Token| {
            !matches!(
                tok,
                Token::Semicolon
                    | Token::LBrace
                    | Token::RBrace
                    | Token::LParen
                    | Token::RParen
                    | Token::LBracket
                    | Token::RBracket
            )
        })
        .map(|tok| vec![tok]);
    choice((
        group
            .clone()
            .repeated()
            .collect::<Vec<Vec<Token>>>()
            .map(|parts| {
                let mut toks = vec![Token::LBrace];
                toks.extend(parts.into_iter().flatten());
                toks.push(Token::RBrace);
                toks
            })
            .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        group
            .clone()
            .repeated()
            .collect::<Vec<Vec<Token>>>()
            .map(|parts| {
                let mut toks = vec![Token::LParen];
                toks.extend(parts.into_iter().flatten());
                toks.push(Token::RParen);
                toks
            })
            .delimited_by(just(Token::LParen), just(Token::RParen)),
        group
            .repeated()
            .collect::<Vec<Vec<Token>>>()
            .map(|parts| {
                let mut toks = vec![Token::LBracket];
                toks.extend(parts.into_iter().flatten());
                toks.push(Token::RBracket);
                toks
            })
            .delimited_by(just(Token::LBracket), just(Token::RBracket)),
        outer_atom,
    ))
    .repeated()
    .collect::<Vec<Vec<Token>>>()
    .then_ignore(just(Token::Semicolon))
    .try_map_with(|parts, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        let tok = e.span().start;
        let tokens = parts.into_iter().flatten().collect::<Vec<_>>();
        let st = &mut e.state().0;
        parse_external_tokens(st, tok, &tokens).map_err(|msg| Rich::custom(e.span(), msg))
    })
}

fn translation_unit<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>>
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    choice((
        top_level_marker().to(None),
        top_level_attr().map(Some),
        external_decl().map(Some),
        function().map(Some),
    ))
    .repeated()
    .collect::<Vec<_>>()
    .then_ignore(end())
    .map_with(|functions, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        let tok = e.span().start;
        let st = &mut e.state().0;
        let id = st.add(AstKind::TranslationUnit, tok);
        for item in functions.into_iter().flatten() {
            st.ast.add_edge(id, item);
        }
        id
    })
}

fn is_top_level_marker(name: &str) -> bool {
    matches!(name, "__BEGIN_DECLS" | "__END_DECLS")
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
        assert!(parse(&lex("int main(void) { return 0; }"), Default::default()).is_ok());
    }

    #[test]
    fn accepts_local_struct_object() {
        assert!(
            parse(
                &lex("struct Pair { int value; }; int main(void) { struct Pair pair; return 0; }"),
                Default::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn accepts_struct_member_access() {
        assert!(
            parse(
                &lex("struct Pair { int value; }; int read(void) { struct Pair pair; return pair.value; }"),
                Default::default(),
            )
            .is_ok()
        );
    }

    fn errors(src: &str) -> Vec<Code> {
        match parse(&lex(src), Default::default()) {
            Ok(_) => panic!("expected parse to fail for {src:?}"),
            Err(diags) => diags.iter().map(|d| d.code()).collect(),
        }
    }

    #[test]
    fn missing_semicolon_is_rejected() {
        assert!(!errors("int main(void) { return 0 }").is_empty());
    }

    #[test]
    fn missing_closing_brace_is_unexpected_eof() {
        assert!(errors("int main(void) { return 0;").contains(&Code::UnexpectedEof));
    }

    #[test]
    fn c89_rejects_a_declaration_in_for_init() {
        let options = crate::lang_options::LangOptions {
            std_version: crate::lang_options::StdVersion::C89,
            gnu_extensions: false,
        };
        assert!(
            parse(
                &lex("int main(void) { for (int i = 0; i < 1; i++) {} }"),
                options,
            )
            .is_err()
        );
    }

    #[test]
    fn c89_rejects_a_declaration_after_a_statement() {
        let options = crate::lang_options::LangOptions {
            std_version: crate::lang_options::StdVersion::C89,
            gnu_extensions: false,
        };
        assert!(
            parse(
                &lex("int main(void) { int first; first = 0; int second; }"),
                options,
            )
            .is_err()
        );
    }

    #[test]
    fn c89_rejects_long_long() {
        let options = crate::lang_options::LangOptions {
            std_version: crate::lang_options::StdVersion::C89,
            gnu_extensions: false,
        };
        assert!(parse(&lex("long long value(void);"), options).is_err());
    }

    #[test]
    fn c17_treats_bool_as_an_identifier() {
        let options = crate::lang_options::LangOptions {
            std_version: crate::lang_options::StdVersion::C17,
            gnu_extensions: false,
        };
        assert!(parse(&lex("int bool(void);"), options).is_ok());
    }

    #[test]
    fn c89_rejects_line_comments() {
        let options = crate::lang_options::LangOptions {
            std_version: crate::lang_options::StdVersion::C89,
            gnu_extensions: false,
        };
        assert!(parse(&lex("int main(void) { // comment\n return 0; }"), options,).is_err());
    }

    #[test]
    fn c17_rejects_binary_integer_literals() {
        let options = crate::lang_options::LangOptions {
            std_version: crate::lang_options::StdVersion::C17,
            gnu_extensions: false,
        };
        assert!(parse(&lex("int main(void) { return 0b1; }"), options).is_err());
    }
}
