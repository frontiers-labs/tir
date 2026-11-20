use crate::{
    ast::{BinaryOp, Literal, Node, UnaryOp},
    syntax::{Fragment, SyntaxCache, SyntaxInput, SyntaxKind},
};
use std::{cell::RefCell, ops::Range};
use tir_cst::{BuildError, GreenNode};
pub(crate) type Expr = Node<Result<GreenNode<SyntaxKind>, BuildError>>;
pub(crate) type Pattern = Expr;
pub(crate) type DeclNode = Expr;
pub(crate) struct Context<'a> {
    pub(crate) input: SyntaxInput<'a>,
    pub(crate) cache: RefCell<&'a mut SyntaxCache>,
}
trait Children {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>);
}
impl Children for Expr {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        into.push(self.value.map(|green| Fragment {
            green,
            range: self.range,
        }));
    }
}
impl<T: Children> Children for Vec<T> {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        for child in self {
            child.children(into);
        }
    }
}
impl<T: Children> Children for Option<T> {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        if let Some(child) = self {
            child.children(into);
        }
    }
}
impl<T: Children> Children for Box<T> {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        (*self).children(into);
    }
}
impl<A: Children, B: Children> Children for (A, B) {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.0.children(into);
        self.1.children(into);
    }
}
impl Children for Result<Fragment, BuildError> {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        into.push(self);
    }
}
macro_rules! leaf { ($($ty:ty),*) => { $(impl Children for $ty { fn children(self, _: &mut Vec<Result<Fragment, BuildError>>) {} })* }; }
leaf!(String, bool, Literal, UnaryOp, BinaryOp);
pub(crate) type Path = Vec<String>;
pub(crate) type Record = Vec<(String, Expr)>;

#[derive(Clone, Debug)]
pub(crate) enum Declaration {
    Use {
        public: bool,
        path: Path,
        glob: bool,
        alias: Option<String>,
    },
    Mod {
        public: bool,
        name: String,
        declarations: Option<Vec<DeclNode>>,
    },
    Let {
        public: bool,
        binding: Binding,
    },
    Interface {
        public: bool,
        binding: Binding,
    },
    Enum {
        public: bool,
        name: String,
        generics: Vec<Binder>,
        variants: Vec<Variant>,
    },
    Axiom {
        public: bool,
        name: String,
        generics: Vec<RuleBinder>,
        ty: Expr,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Binder {
    pub(crate) name: String,
    pub(crate) ty: Expr,
}

#[derive(Clone, Debug)]
pub(crate) struct Binding {
    pub(crate) path: Path,
    pub(crate) generics: Vec<Binder>,
    pub(crate) ty: Option<Expr>,
    pub(crate) value: Expr,
}

#[derive(Clone, Debug)]
pub(crate) struct Variant {
    pub(crate) name: String,
    pub(crate) generics: Vec<Binder>,
    pub(crate) arguments: Option<Vec<Expr>>,
    pub(crate) ty: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) enum Expression {
    Concrete(Result<Fragment, BuildError>),
    Literal(Literal),
    Path(Path),
    Unit,
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    Record(Record),
    Block(Block),
    Function(Box<Function>),
    If {
        condition: Box<Expr>,
        then: Block,
        otherwise: Box<Expr>,
    },
    Match {
        value: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
    },
    Op(Vec<Expr>),
    Ir {
        bindings: Vec<Binding>,
        value: Box<Expr>,
    },
    Limit(Record),
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Generic {
        value: Box<Expr>,
        arguments: Vec<Expr>,
    },
    Call {
        value: Box<Expr>,
        arguments: Vec<Expr>,
    },
    Member {
        value: Box<Expr>,
        name: String,
    },
    RecordCall {
        value: Box<Expr>,
        fields: Record,
        arguments: Vec<Expr>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub(crate) bindings: Vec<Binding>,
    pub(crate) value: Box<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct Function {
    pub(crate) generics: Vec<Binder>,
    pub(crate) parameters: Vec<Binder>,
    pub(crate) result: Option<Expr>,
    pub(crate) decreases: Option<Expr>,
    pub(crate) body: Block,
}

#[derive(Clone, Debug)]
pub(crate) enum PatternKind {
    Wildcard,
    Literal(Literal),
    Path(Path),
    Constructor { path: Path, arguments: Vec<Pattern> },
    Unit,
    Tuple(Vec<Pattern>),
}

#[derive(Clone, Debug)]
pub(crate) enum OpField {
    Signature {
        parameters: Vec<Binder>,
        result: Expr,
        results: Vec<String>,
    },
    Field {
        name: String,
        value: Expr,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct RuleBinder {
    pub(crate) name: String,
    pub(crate) ty: Option<Expr>,
}
impl Children for Declaration {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        match self {
            Self::Use {
                public,
                path,
                glob,
                alias,
            } => {
                public.children(into);
                path.children(into);
                glob.children(into);
                alias.children(into);
            }
            Self::Mod {
                public,
                name,
                declarations,
            } => {
                declarations.children(into);
                public.children(into);
                name.children(into);
            }
            Self::Let { public, binding } | Self::Interface { public, binding } => {
                public.children(into);
                binding.children(into);
            }
            Self::Enum {
                public,
                name,
                generics,
                variants,
            } => {
                public.children(into);
                name.children(into);
                generics.children(into);
                variants.children(into);
            }
            Self::Axiom {
                public,
                name,
                generics,
                ty,
            } => {
                public.children(into);
                name.children(into);
                generics.children(into);
                ty.children(into);
            }
        }
    }
}
impl Children for Binder {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.name.children(into);
        self.ty.children(into);
    }
}
impl Children for Binding {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.path.children(into);
        self.generics.children(into);
        self.ty.children(into);
        self.value.children(into);
    }
}
impl Children for Variant {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.name.children(into);
        self.generics.children(into);
        self.arguments.children(into);
        self.ty.children(into);
    }
}
impl Children for Expression {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        match self {
            Self::Concrete(v0) => {
                v0.children(into);
            }
            Self::Literal(v0) => {
                v0.children(into);
            }
            Self::Path(v0) => {
                v0.children(into);
            }
            Self::Unit => {}
            Self::Tuple(v0) => {
                v0.children(into);
            }
            Self::Array(v0) => {
                v0.children(into);
            }
            Self::Record(v0) => {
                v0.children(into);
            }
            Self::Block(v0) => {
                v0.children(into);
            }
            Self::Function(v0) => {
                v0.children(into);
            }
            Self::If {
                condition,
                then,
                otherwise,
            } => {
                condition.children(into);
                then.children(into);
                otherwise.children(into);
            }
            Self::Match { value, arms } => {
                value.children(into);
                arms.children(into);
            }
            Self::Op(v0) => {
                v0.children(into);
            }
            Self::Ir { bindings, value } => {
                bindings.children(into);
                value.children(into);
            }
            Self::Limit(v0) => {
                v0.children(into);
            }
            Self::Unary { op, value } => {
                op.children(into);
                value.children(into);
            }
            Self::Binary { op, left, right } => {
                op.children(into);
                left.children(into);
                right.children(into);
            }
            Self::Generic { value, arguments } => {
                value.children(into);
                arguments.children(into);
            }
            Self::Call { value, arguments } => {
                value.children(into);
                arguments.children(into);
            }
            Self::Member { value, name } => {
                value.children(into);
                name.children(into);
            }
            Self::RecordCall {
                value,
                fields,
                arguments,
            } => {
                value.children(into);
                fields.children(into);
                arguments.children(into);
            }
        }
    }
}
impl Children for Block {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.bindings.children(into);
        self.value.children(into);
    }
}
impl Children for Function {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.generics.children(into);
        self.parameters.children(into);
        self.result.children(into);
        self.decreases.children(into);
        self.body.children(into);
    }
}
impl Children for PatternKind {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        match self {
            Self::Constructor { path, arguments } => {
                path.children(into);
                arguments.children(into);
            }
            Self::Wildcard => {}
            Self::Literal(v0) => {
                v0.children(into);
            }
            Self::Path(v0) => {
                v0.children(into);
            }
            Self::Unit => {}
            Self::Tuple(v0) => {
                v0.children(into);
            }
        }
    }
}
impl Children for OpField {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        match self {
            Self::Signature {
                parameters,
                result,
                results,
            } => {
                parameters.children(into);
                result.children(into);
                results.children(into);
            }
            Self::Field { name, value } => {
                name.children(into);
                value.children(into);
            }
        }
    }
}
impl Children for RuleBinder {
    fn children(self, into: &mut Vec<Result<Fragment, BuildError>>) {
        self.name.children(into);
        self.ty.children(into);
    }
}
pub(crate) trait Build {
    type Built;
    fn build(self, ctx: &Context<'_>, range: Range<usize>) -> Self::Built;
}
macro_rules! syntax_build {
    ($ty:ty, $kind:expr) => {
        impl Build for $ty {
            type Built = Result<GreenNode<SyntaxKind>, BuildError>;
            fn build(self, ctx: &Context<'_>, range: Range<usize>) -> Self::Built {
                let kind = ($kind)(&self);
                let mut children = Vec::new();
                self.children(&mut children);
                ctx.input
                    .fragment(
                        &mut ctx.cache.borrow_mut(),
                        kind,
                        range,
                        children.into_iter().collect::<Result<Vec<_>, _>>()?,
                    )
                    .map(|fragment| fragment.green)
            }
        }
    };
}
syntax_build!(Expression, |expression: &Expression| match expression {
    Expression::Binary { .. } => SyntaxKind::BinaryExpression,
    _ => SyntaxKind::Expression,
});
syntax_build!(PatternKind, |_: &PatternKind| SyntaxKind::Pattern);
syntax_build!(Declaration, |_: &Declaration| SyntaxKind::Declaration);
syntax_build!(OpField, |_: &OpField| SyntaxKind::Binding);
impl Build for Vec<Binder> {
    type Built = Self;
    fn build(self, _: &Context<'_>, _: Range<usize>) -> Self {
        self
    }
}
impl Build for UnaryOp {
    type Built = Self;
    fn build(self, _: &Context<'_>, _: Range<usize>) -> Self {
        self
    }
}
pub(crate) fn finish<T: Build>(ctx: &Context<'_>, value: T, range: Range<usize>) -> Node<T::Built> {
    Node {
        value: value.build(ctx, range.clone()),
        range,
    }
}
pub(crate) fn ungroup(expr: Expr) -> Expression {
    Expression::Concrete(expr.value.map(|green| Fragment {
        green,
        range: expr.range,
    }))
}
