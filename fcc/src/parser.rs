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

use tir::NodeId;

use crate::ast::*;
use crate::diagnostics::{
    Diagnostic, FileId, UnexpectedEof, UnexpectedToken, file_source, intern_file,
};
use crate::lang_options::LangOptions;
use crate::lexer::{
    FloatingLiteral, IntegerLiteral, Token, parse_floating_literal, parse_integer_literal,
};

mod declarations;
mod expressions;
mod language;
mod statements;

use declarations::{external_decl, function, top_level_attr, top_level_marker};
use language::{keyword_for_standard, validate_language_version};

/// Index-based span over the token slice (we parse already-lexed tokens, so
/// byte offsets are not available — token indices are the natural span).
type Span = SimpleSpan<usize>;
type Extra<'src> = extra::Full<Rich<'src, Token<'src>, Span>, SimpleState<ParseState>, ()>;

/// Parser state: the tree under construction plus the byte span of every input
/// token, so each node can record where its construct starts in the source.
struct ParseState {
    ast: Ast,
    spans: Vec<crate::diagnostics::Span>,
    token_offset: usize,
    name_scopes: Vec<NameScope>,
    next_record: u32,
}

#[derive(Clone, Default)]
struct NameScope {
    typedefs: HashSet<String>,
    ordinary: HashSet<String>,
    tags: HashMap<String, (RecordKind, RecordId)>,
}

impl ParseState {
    fn add(&mut self, kind: AstKind, tok: usize) -> NodeId {
        let span = self
            .spans
            .get(tok + self.token_offset)
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
pub fn parse<'src>(
    tokens: &[(Token<'src>, crate::diagnostics::Span)],
    options: LangOptions,
) -> Result<Ast, Vec<Diagnostic>> {
    let version_diagnostics = language::validate_tokens(tokens, options);
    if !version_diagnostics.is_empty() {
        return Err(version_diagnostics);
    }
    let mut filtered: Vec<Token<'src>> = Vec::with_capacity(tokens.len());
    let mut byte_spans = Vec::with_capacity(tokens.len());
    let mut tokens = tokens
        .iter()
        .filter(|(tok, _)| !matches!(tok, Token::Whitespace(_) | Token::Comment(_)))
        .peekable();
    while let Some((tok, span)) = tokens.next() {
        let mut token = keyword_for_standard(*tok, options);
        if let Token::StringLiteral(first) = token
            && let Some((Token::StringLiteral(_), _)) = tokens.peek()
        {
            let mut joined = first.to_string();
            while let Some((Token::StringLiteral(next), _)) = tokens.peek() {
                joined.push_str(next);
                tokens.next();
            }
            token = Token::StringLiteral(file_source(intern_file("<scratch>", &joined)));
        }
        filtered.push(token);
        byte_spans.push(*span);
    }

    let mut state = SimpleState(ParseState {
        ast: Ast::new(),
        spans: byte_spans.clone(),
        token_offset: 0,
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

fn rich_to_diagnostic(
    err: &Rich<'_, Token<'_>, Span>,
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

fn ident<'src, I>() -> impl Parser<'src, I, String, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! { Token::Identifier(name) => name.to_string() }
}

fn integer_literal<'src, I>() -> impl Parser<'src, I, IntegerLiteral, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! { Token::IntegerLiteral(spelling) => spelling }.try_map(|spelling, span| {
        parse_integer_literal(spelling).ok_or_else(|| Rich::custom(span, "invalid integer literal"))
    })
}

fn floating_literal<'src, I>() -> impl Parser<'src, I, FloatingLiteral, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! { Token::FloatingLiteral(spelling) => spelling }.try_map(|spelling, span| {
        parse_floating_literal(spelling)
            .ok_or_else(|| Rich::custom(span, "invalid floating literal"))
    })
}

fn translation_unit<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
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
