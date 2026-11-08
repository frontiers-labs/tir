//! Source syntax shared by the text adapter and checker.
use std::ops::Range;

/// Byte range in a source module.
pub type Span = Range<usize>;

/// Syntax paired with its original source range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located<T> {
    /// Source syntax.
    pub node: T,
    /// Byte range in the source.
    pub span: Span,
}

/// A possibly absolute, qualified source name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path {
    /// Whether the path starts at the package root.
    pub absolute: bool,
    /// Name components.
    pub segments: Vec<String>,
}
impl Path {
    /// Construct a relative single-component path.
    pub fn name(name: impl Into<String>) -> Self {
        Self {
            absolute: false,
            segments: vec![name.into()],
        }
    }
    /// Render the qualified name.
    pub fn text(&self) -> String {
        format!(
            "{}{}",
            if self.absolute { "::" } else { "" },
            self.segments.join("::")
        )
    }
}

/// Explicit erased or static parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generic {
    /// Parameter name.
    pub name: String,
    /// Kind, defaulting to `Type`.
    pub kind: Option<TypeExpr>,
}
/// A function parameter or labelled type component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameter {
    /// Source label.
    pub name: String,
    /// Optional annotation.
    pub annotation: Option<TypeExpr>,
}
/// Type syntax.
pub type TypeExpr = Located<TypeSyntax>;
/// Type forms; schema arguments remain ordinary expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeSyntax {
    /// Named type with erased or static arguments.
    Path(Path, Vec<GenericArg>),
    /// Flattened function type.
    Function(Vec<Parameter>, Box<TypeExpr>),
    /// Product type; labels distinguish named singleton products.
    Tuple(Vec<(Option<String>, TypeExpr)>),
}
/// Explicit generic argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenericArg {
    /// A type or a static parameter name.
    Type(TypeExpr),
    /// A literal, schema, or computed static argument.
    Value(Expr),
}
/// Literal source value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    /// Nonnegative integer; negation is a separate expression.
    Integer(i128),
    /// Semantic Boolean.
    Bool(bool),
    /// Decoded string.
    String(String),
}
/// Expression syntax.
pub type Expr = Located<Expression>;
/// Domain-independent expression forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expression {
    /// Literal.
    Literal(Literal),
    /// Name or explicit generic specialization.
    Path(Path, Vec<GenericArg>),
    /// Immutable function.
    Function {
        generics: Vec<Generic>,
        parameters: Vec<Parameter>,
        result: Option<TypeExpr>,
        body: Box<Expr>,
    },
    /// Type/schema-description value.
    Description(TypeExpr),
    /// Function application.
    Call(Box<Expr>, Vec<(Option<String>, Expr)>),
    /// Static projection.
    Project(Box<Expr>, String),
    /// Total list indexing, returning an option.
    Index(Box<Expr>, Box<Expr>),
    /// Ordered immutable record.
    Record(Vec<(String, Expr)>),
    /// Product value.
    Tuple(Vec<Expr>),
    /// Homogeneous list.
    List(Vec<Expr>),
    /// Lexical bindings followed by a final expression.
    Block(Vec<Local>, Box<Expr>),
    /// Conditional.
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Exhaustive elimination.
    Match(Box<Expr>, Vec<(Pattern, Expr)>),
    /// Statically selected unary operation.
    Unary(String, Box<Expr>),
    /// Statically selected binary operation.
    Binary(String, Box<Expr>, Box<Expr>),
}
/// Pattern syntax.
pub type Pattern = Located<PatternSyntax>;
/// Exhaustive source patterns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternSyntax {
    /// Wildcard.
    Wildcard,
    /// Binding.
    Bind(String),
    /// Literal pattern.
    Literal(Literal),
    /// Nominal constructor pattern.
    Constructor(Path, Vec<Pattern>),
    /// Product pattern.
    Tuple(Vec<Pattern>),
    /// Record pattern.
    Record(Vec<(String, Pattern)>),
}
/// Lexical definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    /// Recursive function marker.
    pub recursive: bool,
    /// Bound pattern.
    pub pattern: Pattern,
    /// Local generic binders, rejected by the v0.1 monomorphic local checker.
    pub generics: Vec<Generic>,
    /// Optional type annotation.
    pub annotation: Option<TypeExpr>,
    /// Initializer.
    pub value: Expr,
}
/// One source item.
pub type Item = Located<Declaration>;
/// Visibility and declaration payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// Export to other modules.
    pub public: bool,
    /// Declaration.
    pub kind: ItemKind,
}
/// Import selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Import {
    /// One binding or module.
    Name(Option<String>),
    /// All public bindings.
    Glob,
    /// Selected public bindings and optional aliases.
    Group(Vec<(String, Option<String>)>),
}
/// Enum constructor declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    /// Constructor name.
    pub name: String,
    /// Constructor-local generic binders.
    pub generics: Vec<Generic>,
    /// Positional fields.
    pub fields: Vec<TypeExpr>,
    /// Optional indexed constructor result.
    pub result: Option<TypeExpr>,
}
/// Top-level declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// Source import.
    Use(Path, Import),
    /// Inline or source-map module.
    Module(String, Option<Vec<Item>>),
    /// Abstract nominal identity in a category.
    ExternalType(String, Vec<Generic>, TypeExpr),
    /// Opaque typed host binding.
    ExternalValue(Path, Vec<Generic>, TypeExpr),
    /// Transparent type alias.
    Type(String, Vec<Generic>, TypeExpr),
    /// Strictly positive inductive type.
    Enum(String, Vec<Generic>, Vec<Variant>),
    /// Value definition.
    Let {
        path: Path,
        generics: Vec<Generic>,
        recursive: bool,
        annotation: Option<TypeExpr>,
        value: Expr,
    },
}
