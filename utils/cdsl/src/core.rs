//! Read-only checked code for compiler output backends.
use crate::ast::{Literal, Span};
use crate::types::{Constraint, Type};
use std::collections::BTreeMap;

/// A checked expression with its decoded type.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Term {
    /// Source range used by diagnostics.
    pub span: Span,
    /// Checked expression form.
    pub kind: TermKind,
    /// Decoded expression type after inference.
    pub ty: Type,
}
/// Domain-independent checked expression forms.
#[derive(Clone, Debug)]
pub enum TermKind {
    /// A scalar literal.
    Literal(Literal),
    /// A lexically bound value.
    Local(String),
    /// A checked definition, its generic arguments, and its inferred row substitution.
    Global(String, BTreeMap<String, Type>, BTreeMap<u32, Type>),
    /// A typed description of the enclosed decoded type.
    Description(Type),
    /// A function with ordered flattened parameter names.
    Function(Vec<String>, Box<Term>),
    /// A call with arguments in parameter order.
    Call(Box<Term>, Vec<Term>),
    /// A checked representation conversion to the enclosing term's type.
    Coerce(Box<Term>),
    /// A named trusted standard-library implementation.
    Trusted(String),
    /// A field projection.
    Project(Box<Term>, String),
    /// A total list index returning an Option.
    Index(Box<Term>, Box<Term>),
    /// An ordered record.
    Record(Vec<(String, Term)>),
    /// A product.
    Tuple(Vec<Term>),
    /// A homogeneous list.
    List(Vec<Term>),
    /// A checked lexical binding.
    Let(Pattern, Box<Term>, Box<Term>),
    /// A structurally recursive lexical function and its continuation.
    LetRec(String, Vec<String>, Box<Term>, Box<Term>),
    /// A conditional.
    If(Box<Term>, Box<Term>, Box<Term>),
    /// An exhaustive match.
    Match(Box<Term>, Vec<(Pattern, Term)>),
    /// A statically selected unary operation.
    Unary(String, Box<Term>),
    /// A statically selected binary operation and its operand type.
    Binary(String, Box<Term>, Box<Term>, Type),
    /// A nominal constructor and its arity.
    Constructor(String, usize),
    /// A stable package-qualified opaque host identity.
    Foreign(String),
    /// A qualified definition identity and its payload.
    Named(String, Box<Term>),
}
/// A checked exhaustive binding or match pattern.
#[derive(Clone, Debug)]
pub enum Pattern {
    /// A pattern that ignores its value.
    Wildcard,
    /// A lexical value binding.
    Bind(String),
    /// A scalar literal.
    Literal(Literal),
    /// A nominal constructor and its field patterns.
    Constructor(String, Vec<Pattern>),
    /// A product.
    Tuple(Vec<Pattern>),
    /// An ordered record.
    Record(Vec<(String, Pattern)>),
}
/// The explicit generic binders, type and dictionary requirements of a definition.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Scheme {
    /// Ordered generic parameters and their kinds.
    pub parameters: Vec<(String, Type)>,
    /// Decoded value type.
    pub ty: Type,
    /// Statically selected dictionary obligations.
    pub constraints: Vec<Constraint>,
}
/// A definition admitted by checking every part of its body.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Definition {
    /// Source identity for diagnostics.
    pub source: String,
    /// Resolved nominal namespace of the original qualified declaration, if any.
    pub namespace: Option<String>,
    /// Checked code, preserving opaque host identities.
    pub term: Term,
    /// Quantified definition type.
    pub scheme: Scheme,
}

/// An externally declared nominal family and its checked category.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct NominalDefinition {
    /// Ordered generic parameters and their kinds.
    pub parameters: Vec<(String, Type)>,
    /// Checked category, retaining its resolved nominal identity.
    pub category: Type,
}

impl Term {
    pub(crate) fn map_types(&mut self, f: &mut impl FnMut(&Type) -> Type) {
        self.ty = f(&self.ty);
        match &mut self.kind {
            TermKind::Description(ty) => *ty = f(ty),
            TermKind::Global(_, arguments, rows) => {
                for ty in arguments.values_mut().chain(rows.values_mut()) {
                    *ty = f(ty);
                }
            }
            TermKind::Binary(_, left, right, ty) => {
                *ty = f(ty);
                left.map_types(f);
                right.map_types(f);
            }
            TermKind::Call(function, args) => {
                function.map_types(f);
                for arg in args {
                    arg.map_types(f);
                }
            }
            TermKind::Function(_, body)
            | TermKind::Coerce(body)
            | TermKind::Project(body, _)
            | TermKind::Unary(_, body)
            | TermKind::Named(_, body) => body.map_types(f),
            TermKind::Index(left, right)
            | TermKind::Let(_, left, right)
            | TermKind::LetRec(_, _, left, right) => {
                left.map_types(f);
                right.map_types(f);
            }
            TermKind::Tuple(values) | TermKind::List(values) => {
                for value in values {
                    value.map_types(f);
                }
            }
            TermKind::Record(fields) => {
                for (_, value) in fields {
                    value.map_types(f);
                }
            }
            TermKind::If(condition, yes, no) => {
                condition.map_types(f);
                yes.map_types(f);
                no.map_types(f);
            }
            TermKind::Match(value, arms) => {
                value.map_types(f);
                for (_, arm) in arms {
                    arm.map_types(f);
                }
            }
            _ => {}
        }
    }
}
