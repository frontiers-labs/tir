use crate::Span;
use tir_symbolic::lang::{SymKind, op_kind};

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
    pub kind: RuleKind,
    pub lhs: Term,
    pub rhs: Term,
    pub guards: Vec<Expr>,
    pub requirements: Vec<RefinementRequirement>,
    /// `None` uses [`Proof::Smt`] for floating-point and semantic rules, and
    /// [`Proof::Trusted`] for other dialect rules.
    pub proof: Option<Proof>,
    pub post_saturation: bool,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefinementAction {
    pub name: String,
    pub lhs: Term,
    pub rhs: Term,
    pub requirements: Vec<RefinementRequirement>,
    pub proof: Option<Proof>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleKind {
    Equality(Direction),
    Refinement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefinementRequirement {
    SameRoundRegion,
    Permits(ContractPermission),
    CompatibleFormatsAndRounding,
    PermittedEffectChange,
    SatisfiesDomainAndEffectContract,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractPermission {
    ContractMulAdd,
    CrossStatement,
    Reassociation,
    Reciprocal,
    ApprovedApproximation,
}

/// How a rule's proof or directional contract is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    /// Bit-blasted and checked under `TIR_VERIFY_AXIOMS`.
    Smt,
    /// Asserted by the target description; nothing checks it.
    Trusted,
    /// A law of an algebra the prover has no model for, such as memory.
    Definitional,
    /// Checked against the structural contract for a directional refinement.
    Contract,
}

impl Term {
    /// Whether the term matches only an integer constant: a number written
    /// in place, bare or as `const<W>(...)`, or a binder declared `const`.
    pub fn is_constant(&self) -> bool {
        matches!(
            self.kind,
            TermKind::Value(_)
                | TermKind::Constant { .. }
                | TermKind::Binder {
                    ty: Some(BindingType::Constant(_)),
                    ..
                }
        )
    }

    /// Whether the term is the literal zero, bare or as `const<W>(0)`.
    pub fn is_zero(&self) -> bool {
        let zero = |value: &Expr| matches!(value.kind, ExprKind::Integer(0));
        match &self.kind {
            TermKind::Value(value) | TermKind::Constant { value, .. } => zero(value),
            _ => false,
        }
    }

    /// Whether this operand can match at position `operand` of an operator
    /// with the given offset law. A constant with a coefficient is moved out
    /// of the node into its value, leaving the zero behind, which an identity
    /// at that position dissolves as well; so only a literal zero where there
    /// is no identity is ever held.
    pub fn held_under(&self, coefficient: Option<i64>, identity: Option<u64>) -> bool {
        !self.is_constant() || coefficient.is_none() || (self.is_zero() && identity.is_none())
    }
}

impl Rule {
    pub fn is_floating_point(&self) -> bool {
        names_fp_op(&self.lhs)
            || names_fp_op(&self.rhs)
            || names_fp_semantic_op(&self.lhs)
            || names_fp_semantic_op(&self.rhs)
            || contains_float_type(&self.lhs)
            || contains_float_type(&self.rhs)
            || contains_fp_env_state(&self.lhs)
            || contains_fp_env_state(&self.rhs)
    }

    /// The stated proof mode, or the default for the rule's vocabulary.
    pub fn proof(&self) -> Proof {
        self.proof.unwrap_or_else(|| {
            if self.is_floating_point() {
                Proof::Smt
            } else if names_dialect_op(&self.lhs) || names_dialect_op(&self.rhs) {
                Proof::Trusted
            } else {
                Proof::Smt
            }
        })
    }

    pub fn refinement_action(&self) -> Option<RefinementAction> {
        matches!(self.kind, RuleKind::Refinement).then(|| RefinementAction {
            name: self.name.clone(),
            lhs: self.lhs.clone(),
            rhs: self.rhs.clone(),
            requirements: self.requirements.clone(),
            proof: self.proof,
        })
    }
}

fn names_fp_op(term: &Term) -> bool {
    match &term.kind {
        TermKind::Operation {
            operator,
            operands,
            dependencies,
            ..
        } => {
            matches!(operator, Operator::Dialect { dialect, .. } if dialect == "fp")
                || operands.iter().chain(dependencies).any(names_fp_op)
        }
        _ => false,
    }
}

fn names_dialect_op(term: &Term) -> bool {
    match &term.kind {
        TermKind::Operation {
            operator, operands, ..
        } => matches!(operator, Operator::Dialect { .. }) || operands.iter().any(names_dialect_op),
        _ => false,
    }
}

fn names_fp_semantic_op(term: &Term) -> bool {
    match &term.kind {
        TermKind::Operation {
            operator: Operator::Semantic(name),
            operands,
            dependencies,
            ..
        } => {
            matches!(
                op_kind(name),
                Some(
                    SymKind::Fma
                        | SymKind::FAdd
                        | SymKind::FSub
                        | SymKind::FMul
                        | SymKind::FDiv
                        | SymKind::SIToFP
                        | SymKind::UIToFP
                        | SymKind::FPToSI
                        | SymKind::FPToUI
                        | SymKind::FMin
                        | SymKind::FMax
                        | SymKind::AsFloat
                        | SymKind::FCvt
                        | SymKind::FAddRound
                        | SymKind::FSubRound
                        | SymKind::FMulRound
                        | SymKind::FDivRound
                        | SymKind::FmaRound
                        | SymKind::Sqrt
                        | SymKind::SqrtRound
                        | SymKind::FCvtRound
                        | SymKind::SIToFPRound
                        | SymKind::UIToFPRound
                        | SymKind::FPToSIRound
                        | SymKind::FPToUIRound
                        | SymKind::FPFlags
                )
            ) || operands
                .iter()
                .chain(dependencies)
                .any(names_fp_semantic_op)
        }
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => operands
            .iter()
            .chain(dependencies)
            .any(names_fp_semantic_op),
        _ => false,
    }
}

fn contains_float_type(term: &Term) -> bool {
    if matches!(term.ty, Some(Type::Float(_) | Type::ShapedFloat { .. })) {
        return true;
    }
    match &term.kind {
        TermKind::Binder {
            ty: Some(BindingType::Type(Type::Float(_) | Type::ShapedFloat { .. })),
            ..
        } => true,
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => operands.iter().chain(dependencies).any(contains_float_type),
        _ => false,
    }
}

fn contains_fp_env_state(term: &Term) -> bool {
    if matches!(term.ty, Some(Type::State(Resource::FpEnv))) {
        return true;
    }
    match &term.kind {
        TermKind::Binder {
            ty: Some(BindingType::Type(Type::State(Resource::FpEnv))),
            ..
        } => true,
        TermKind::Operation {
            operands,
            dependencies,
            ..
        } => operands
            .iter()
            .chain(dependencies)
            .any(contains_fp_env_state),
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
    Float(FloatFormat),
    ShapedFloat {
        format: FloatFormat,
        shape: Vec<Expr>,
    },
    State(Resource),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    Memory,
    FpEnv,
    Named(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatFormat {
    Binary32,
    Binary64,
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
