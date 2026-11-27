//! Lossless CDSL syntax storage. Parser ranges count significant tokens; green
//! trees and positioned handles use UTF-8 bytes.
use std::ops::Range;

use tir_cst::{BuildError, GreenElement, GreenNode, NodeCache, NodeOrToken};

use crate::lexer::{SourceToken, Token, TokenStream};

/// CDSL node and trivia classifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyntaxKind {
    /// Complete source, including leading and trailing trivia.
    Module,
    /// A top-level declaration.
    Declaration,
    /// An expression, including explicit parentheses.
    Expression,
    /// A binary expression with grammar-defined precedence.
    BinaryExpression,
    /// A pattern.
    Pattern,
    /// A binder or local binding.
    Binding,
    /// A grammar token. Its spelling remains exact.
    Token,
    /// Source whitespace.
    Whitespace,
    /// A line comment, without its newline.
    LineComment,
    /// A possibly nested block comment.
    BlockComment,
}

/// An owned positioned CDSL tree, supporting persistent subtree replacement.
pub type SyntaxTree = tir_cst::SyntaxTree<SyntaxKind>;
/// An owned positioned CDSL node.
pub type SyntaxNode = tir_cst::SyntaxNode<SyntaxKind>;
/// A reusable weak interning cache. Retain roots to retain shared allocations.
pub type SyntaxCache = NodeCache<SyntaxKind>;

/// A parser output with significant-token coordinates and independently owned syntax.
#[derive(Clone, Debug)]
pub(crate) struct Fragment {
    pub green: GreenNode<SyntaxKind>,
    pub range: Range<usize>,
}

/// Source data used only by the lossless output constructors.
pub(crate) struct SyntaxInput<'a> {
    pub source: &'a str,
    pub tokens: &'a [SourceToken<'a>],
    /// Significant token index to raw token index, with an end sentinel.
    pub significant: Vec<usize>,
}

impl<'a> SyntaxInput<'a> {
    pub fn new(source: &'a str, tokens: &'a [SourceToken<'a>]) -> Self {
        let mut significant: Vec<_> = tokens
            .iter()
            .enumerate()
            .filter_map(|(index, token)| (!token.token.is_trivia()).then_some(index))
            .collect();
        significant.push(tokens.len());
        Self {
            source,
            tokens,
            significant,
        }
    }

    pub fn fragment(
        &self,
        cache: &mut SyntaxCache,
        kind: SyntaxKind,
        range: Range<usize>,
        children: impl IntoIterator<Item = Fragment>,
    ) -> Result<Fragment, BuildError> {
        let raw = if kind == SyntaxKind::Module {
            0..self.tokens.len()
        } else {
            self.significant[range.start]..if range.is_empty() {
                self.significant[range.start]
            } else {
                self.significant[range.end - 1] + 1
            }
        };
        let mut cursor = raw.start;
        let mut elements = Vec::new();
        for child in children {
            let start = self.significant[child.range.start];
            self.raw_tokens(cache, cursor..start, &mut elements)?;
            cursor = if child.range.is_empty() {
                start
            } else {
                self.significant[child.range.end - 1] + 1
            };
            elements.push(NodeOrToken::Node(child.green));
        }
        self.raw_tokens(cache, cursor..raw.end, &mut elements)?;
        Ok(Fragment {
            green: cache.node(kind, elements)?,
            range,
        })
    }

    fn raw_tokens(
        &self,
        cache: &mut SyntaxCache,
        range: Range<usize>,
        elements: &mut Vec<GreenElement<SyntaxKind>>,
    ) -> Result<(), BuildError> {
        for token in &self.tokens[range] {
            let kind = match token.token {
                Token::Whitespace => SyntaxKind::Whitespace,
                Token::LineComment => SyntaxKind::LineComment,
                Token::BlockComment => SyntaxKind::BlockComment,
                _ => SyntaxKind::Token,
            };
            elements.push(NodeOrToken::Token(
                cache.token(kind, &self.source[token.range()])?,
            ));
        }
        Ok(())
    }
}

/// A lexical, syntactic, or storage failure while building lossless syntax.
#[derive(Debug)]
pub enum SyntaxError {
    /// Invalid source tokens.
    Lex(Vec<crate::lexer::LexError>),
    /// Source does not match the shared CDSL grammar.
    Parse(Vec<crate::parser::ParseError>),
    /// Source exceeds storage coordinate limits.
    Storage(BuildError),
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lex(errors) => write!(f, "CDSL lexical errors: {errors:?}"),
            Self::Parse(errors) => write!(f, "CDSL syntax errors: {errors:?}"),
            Self::Storage(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SyntaxError {}

/// Parses a complete source with a retained sharing cache.
pub fn parse_source(source: &str, cache: &mut SyntaxCache) -> Result<SyntaxTree, SyntaxError> {
    let tokens = crate::lexer::lex_source(source).map_err(SyntaxError::Lex)?;
    let stream = TokenStream {
        tokens: tokens
            .iter()
            .filter(|token| !token.token.is_trivia())
            .map(|token| token.token)
            .collect(),
        spans: tokens
            .iter()
            .filter(|token| !token.token.is_trivia())
            .map(|token| (token.start as u32, token.end as u32))
            .collect(),
        end: source.len() as u32,
    };
    let context = crate::parser_syntax::Context {
        input: SyntaxInput::new(source, &tokens),
        cache: std::cell::RefCell::new(cache),
    };
    let declarations =
        crate::parser::parse_lossless(&stream, &context).map_err(SyntaxError::Parse)?;
    let children = declarations
        .into_iter()
        .map(|node| {
            node.value.map(|green| Fragment {
                green,
                range: node.range,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(SyntaxError::Storage)?;
    let root = context
        .input
        .fragment(
            &mut context.cache.borrow_mut(),
            SyntaxKind::Module,
            0..context.input.significant.len() - 1,
            children,
        )
        .map_err(SyntaxError::Storage)?;
    Ok(SyntaxTree::new(root.green))
}

macro_rules! typed_node {
    ($name:ident, $($kind:ident)|+, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug)]
        pub struct $name(SyntaxNode);
        impl tir_cst::AstNode for $name {
            type Kind = SyntaxKind;
            fn can_cast(kind: SyntaxKind) -> bool {
                matches!(kind, $(SyntaxKind::$kind)|+)
            }
            fn cast(syntax: SyntaxNode) -> Option<Self> {
                Self::can_cast(syntax.kind()).then_some(Self(syntax))
            }
            fn syntax(&self) -> &SyntaxNode {
                &self.0
            }
        }
    };
}

typed_node!(ModuleNode, Module, "Typed view over a CDSL module root.");
typed_node!(
    DeclarationNode,
    Declaration,
    "Typed view over a CDSL top-level declaration."
);
typed_node!(
    ExpressionNode,
    Expression | BinaryExpression,
    "Typed view over a CDSL expression."
);
typed_node!(PatternNode, Pattern, "Typed view over a CDSL pattern.");
typed_node!(
    BindingNode,
    Binding,
    "Typed view over a CDSL binder or local binding."
);
