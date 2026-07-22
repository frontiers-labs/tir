use crate::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Group(Group),
    Rule(Box<Rule>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub alternatives: Vec<Type>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub lhs: Term,
    pub direction: Direction,
    pub rhs: Term,
    pub guards: Vec<Expr>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Bidirectional,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub kind: TermKind,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermKind {
    Operation {
        operator: Operator,
        attributes: Vec<Attribute>,
        operands: Vec<Term>,
    },
    Binder {
        name: String,
        ty: Option<BindingType>,
    },
    Integer(i64),
    String(String),
    Constant {
        width: Expr,
        value: Expr,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operator {
    Dialect { dialect: String, name: String },
    Gate(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeValue {
    Integer(i64),
    String(String),
    Binder(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingType {
    Type(Type),
    Constant(Option<Expr>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    Integer(Width),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Width {
    Concrete(u32),
    Named(String),
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprKind {
    Integer(i64),
    Name(String),
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Multiply,
    Divide,
    Remainder,
    Add,
    Subtract,
    ShiftLeft,
    ShiftRight,
    BitAnd,
    BitXor,
    BitOr,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    LogicalAnd,
    LogicalOr,
}
