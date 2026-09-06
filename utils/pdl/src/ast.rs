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
    /// `None` takes the namespace default: [`Proof::Smt`] for a rule whose terms
    /// are all semantic, [`Proof::Trusted`] once a dialect op term appears.
    pub proof: Option<Proof>,
    pub post_saturation: bool,
    pub span: Span,
}

/// How a rule's equivalence is discharged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    /// Bit-blasted and checked under `TIR_VERIFY_AXIOMS`.
    Smt,
    /// Asserted by the target description; nothing checks it.
    Trusted,
    /// A law of an algebra the prover has no model for, such as memory.
    Definitional,
}

impl Rule {
    /// The stated proof mode, or the default for the rule's vocabulary.
    pub fn proof(&self) -> Proof {
        self.proof.unwrap_or_else(|| {
            if names_dialect_op(&self.lhs) || names_dialect_op(&self.rhs) {
                Proof::Trusted
            } else {
                Proof::Smt
            }
        })
    }

    /// Whether the left-hand side is a bare constant binder, which matches every
    /// constant class so a wide constant can be decomposed in place.
    pub fn materializes(&self) -> bool {
        matches!(
            &self.lhs.kind,
            TermKind::Binder {
                ty: Some(BindingType::Constant(_)),
                ..
            }
        )
    }
}

fn names_dialect_op(term: &Term) -> bool {
    match &term.kind {
        TermKind::Operation {
            operator, operands, ..
        } => matches!(operator, Operator::Dialect { .. }) || operands.iter().any(names_dialect_op),
        TermKind::Keep(inner) => names_dialect_op(inner),
        _ => false,
    }
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
        /// The dependency operands, spelled after `|` as the printer does.
        dependencies: Vec<Term>,
    },
    Binder {
        name: String,
        ty: Option<BindingType>,
    },
    /// An integer expression over widths and literals. On a left-hand side it
    /// matches a constant class equal to the expression; on a right-hand side it
    /// materializes one.
    Value(Expr),
    String(String),
    Constant {
        width: Expr,
        value: Expr,
    },
    /// The matched root class. Right-hand side only.
    Root,
    /// A right-hand-side node a materialize rule keeps as an instruction instead
    /// of folding to the constant it computes.
    Keep(Box<Term>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operator {
    Dialect { dialect: String, name: String },
    Semantic(String),
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
