//! Existing language-mode checks and keyword classification.

use crate::ast::{Ast, AstKind, AstLeaf, CType};
use crate::diagnostics::{Diagnostic, LanguageFeatureUnavailable};
use crate::lang_options::LangOptions;
use crate::lexer::Token;
use tir::graph::Dag;

pub(super) fn validate_tokens(
    tokens: &[(Token, crate::diagnostics::Span)],
    options: LangOptions,
) -> Vec<Diagnostic> {
    let mut version_diagnostics = Vec::new();
    for (token, span) in tokens {
        match token {
            Token::Comment(comment) if comment.starts_with("//") && options.is_strict_c89() => {
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
    version_diagnostics
}

pub(super) fn keyword_for_standard(token: Token, options: LangOptions) -> Token {
    use crate::lang_options::StdVersion;

    if options.is_strict_c89() {
        match token {
            Token::KwInline => return Token::Identifier("inline".to_string()),
            Token::KwRestrict => return Token::Identifier("restrict".to_string()),
            Token::KwUnderscoreBool => return Token::Identifier("_Bool".to_string()),
            Token::KwComplex => return Token::Identifier("_Complex".to_string()),
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

pub(super) fn validate_language_version(ast: &Ast, options: LangOptions) -> Vec<Diagnostic> {
    if !options.is_strict_c89() {
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
