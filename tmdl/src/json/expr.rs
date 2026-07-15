use schemars::JsonSchema;
use serde::Serialize;

use crate::ast;

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// A checked TMDL expression. Numeric literal values retain their source spelling.
pub(super) enum Expr {
    /// Assignment to an lvalue expression.
    Assign {
        destination: Box<Expr>,
        value: Box<Expr>,
    },
    /// Binary operator application.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Unary operator application.
    Unary { op: UnOp, value: Box<Expr> },
    /// Ordered expression block.
    Block {
        /// Statements in source order; omitted when empty.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        statements: Vec<Expr>,
        /// Whether the final statement supplies the block value; omitted when false.
        #[serde(skip_serializing_if = "is_false")]
        returns_last: bool,
    },
    /// Function or builtin call.
    Call {
        callee: Box<Expr>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        arguments: Vec<Expr>,
    },
    /// Named member access.
    Field { base: Box<Expr>, member: String },
    /// Local or declaration identifier.
    Identifier { name: String },
    /// Conditional expression.
    If {
        condition: Box<Expr>,
        then: Box<Expr>,
        #[serde(rename = "else", skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Box<Expr>")]
        else_: Option<Box<Expr>>,
    },
    /// Constant index access.
    Index { base: Box<Expr>, index: u16 },
    /// Qualified name split into source-order segments.
    Path { segments: Vec<String> },
    /// String literal without its source quotes.
    String { value: String },
    /// Integer or bit literal retaining radix and source digits.
    Integer { value: String },
    /// Inclusive bit slice.
    Slice {
        base: Box<Expr>,
        start: u16,
        end: u16,
    },
    /// Precise-trap body and its ordered exception handlers.
    Try {
        body: Box<Expr>,
        handlers: Vec<ExceptClause>,
    },
    /// Builtin function used as a call target.
    Builtin { name: BuiltinFunction },
    /// Anonymous function used by iterator builtins.
    Lambda {
        parameters: Vec<String>,
        body: Box<Expr>,
    },
    /// Parser recovery node; checked successful inputs do not normally contain it.
    Invalid,
}

impl From<&ast::Expr> for Expr {
    fn from(expr: &ast::Expr) -> Self {
        match expr {
            ast::Expr::Assign(assign) => Self::Assign {
                destination: Box::new(Expr::from(assign.dest.as_ref())),
                value: Box::new(Expr::from(assign.value.as_ref())),
            },
            ast::Expr::Binary(binary) => Self::Binary {
                op: BinOp::from(binary.op.clone()),
                lhs: Box::new(Expr::from(binary.lhs.as_ref())),
                rhs: Box::new(Expr::from(binary.rhs.as_ref())),
            },
            ast::Expr::Unary(unary) => Self::Unary {
                op: UnOp::from(unary.op.clone()),
                value: Box::new(Expr::from(unary.x.as_ref())),
            },
            ast::Expr::Block(block) => Self::Block {
                statements: block.stmts.iter().map(Expr::from).collect(),
                returns_last: block.last_expr_return,
            },
            ast::Expr::Call(call) => Self::Call {
                callee: Box::new(Expr::from(call.callee.as_ref())),
                arguments: call.arguments.iter().map(Expr::from).collect(),
            },
            ast::Expr::Field(field) => Self::Field {
                base: Box::new(Expr::from(field.base.as_ref())),
                member: field.member.clone(),
            },
            ast::Expr::Ident(identifier) => Self::Identifier {
                name: identifier.name.clone(),
            },
            ast::Expr::If(if_) => Self::If {
                condition: Box::new(Expr::from(if_.cond.as_ref())),
                then: Box::new(Expr::from(if_.then.as_ref())),
                else_: if_.else_.as_deref().map(Expr::from).map(Box::new),
            },
            ast::Expr::IndexAccess(index) => Self::Index {
                base: Box::new(Expr::from(index.base.as_ref())),
                index: index.index,
            },
            ast::Expr::Path(path) => {
                let mut segments = Vec::with_capacity(path.remainder.len() + 1);
                segments.push(path.base.clone());
                segments.extend(path.remainder.clone());
                Self::Path { segments }
            }
            ast::Expr::Lit(ast::Lit::Str(value)) => Self::String {
                value: value.value().to_string(),
            },
            ast::Expr::Lit(ast::Lit::Int(value)) => Self::Integer {
                value: value.value().to_string(),
            },
            ast::Expr::Slice(slice) => Self::Slice {
                base: Box::new(Expr::from(slice.base.as_ref())),
                start: slice.start,
                end: slice.end,
            },
            ast::Expr::Try(try_) => Self::Try {
                body: Box::new(Expr::from(try_.body.as_ref())),
                handlers: try_.handlers.iter().map(ExceptClause::from).collect(),
            },
            ast::Expr::BuiltinFunction(function) => Self::Builtin {
                name: BuiltinFunction::from(function.clone()),
            },
            ast::Expr::Lambda(lambda) => Self::Lambda {
                parameters: lambda.params.clone(),
                body: Box::new(Expr::from(lambda.body.as_ref())),
            },
            ast::Expr::Invalid => Self::Invalid,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// One exception handler in a `try` expression.
pub(super) struct ExceptClause {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    binding: Option<String>,
    body: Expr,
}

impl From<&ast::ExceptClause> for ExceptClause {
    fn from(clause: &ast::ExceptClause) -> Self {
        Self {
            kind: clause.kind.clone(),
            binding: clause.binding.clone(),
            body: Expr::from(&clause.body),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(super) enum BinOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    UnsignedDivide,
    Equal,
    NotEqual,
    LessThan,
    GreaterThan,
    LessThanEqual,
    GreaterThanEqual,
    UnsignedLessThan,
    UnsignedGreaterThan,
    UnsignedLessThanEqual,
    UnsignedGreaterThanEqual,
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    ShiftLeftLogical,
    ShiftRightLogical,
    ShiftRightArithmetic,
}

impl From<ast::BinOp> for BinOp {
    fn from(op: ast::BinOp) -> Self {
        match op {
            ast::BinOp::Add => Self::Add,
            ast::BinOp::Sub => Self::Subtract,
            ast::BinOp::Mul => Self::Multiply,
            ast::BinOp::Div => Self::Divide,
            ast::BinOp::UnsignedDiv => Self::UnsignedDivide,
            ast::BinOp::Equal => Self::Equal,
            ast::BinOp::NotEqual => Self::NotEqual,
            ast::BinOp::LessThan => Self::LessThan,
            ast::BinOp::GreaterThan => Self::GreaterThan,
            ast::BinOp::LessThenEqual => Self::LessThanEqual,
            ast::BinOp::GreaterThanEqual => Self::GreaterThanEqual,
            ast::BinOp::UnsignedLessThan => Self::UnsignedLessThan,
            ast::BinOp::UnsignedGreaterThan => Self::UnsignedGreaterThan,
            ast::BinOp::UnsignedLessThenEqual => Self::UnsignedLessThanEqual,
            ast::BinOp::UnsignedGreaterThanEqual => Self::UnsignedGreaterThanEqual,
            ast::BinOp::BitwiseAnd => Self::BitwiseAnd,
            ast::BinOp::BitwiseOr => Self::BitwiseOr,
            ast::BinOp::BitwiseXor => Self::BitwiseXor,
            ast::BinOp::ShiftLeftLogical => Self::ShiftLeftLogical,
            ast::BinOp::ShiftRightLogical => Self::ShiftRightLogical,
            ast::BinOp::ShiftRightArithmetic => Self::ShiftRightArithmetic,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(super) enum UnOp {
    BitwiseNot,
}

impl From<ast::UnOp> for UnOp {
    fn from(op: ast::UnOp) -> Self {
        match op {
            ast::UnOp::BitwiseNot => Self::BitwiseNot,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub(super) enum BuiltinFunction {
    Clamp,
    Extract,
    #[serde(rename = "log2_ceil")]
    Log2Ceil,
    Regnum,
    #[serde(rename = "sext")]
    SExt,
    #[serde(rename = "zext")]
    ZExt,
    Load,
    Store,
    LoadReserved,
    StoreConditional,
    AtomicRmw,
    Fence,
    FenceI,
    Trap,
    Split,
    Concat,
    Map,
    Reduce,
    Zip,
    #[serde(rename = "fadd")]
    FAdd,
    #[serde(rename = "fsub")]
    FSub,
    #[serde(rename = "fmul")]
    FMul,
    #[serde(rename = "fdiv")]
    FDiv,
    Todo,
}

impl From<ast::BuiltinFunction> for BuiltinFunction {
    fn from(function: ast::BuiltinFunction) -> Self {
        match function {
            ast::BuiltinFunction::Clamp => Self::Clamp,
            ast::BuiltinFunction::Extract => Self::Extract,
            ast::BuiltinFunction::Log2Ceil => Self::Log2Ceil,
            ast::BuiltinFunction::Regnum => Self::Regnum,
            ast::BuiltinFunction::SExt => Self::SExt,
            ast::BuiltinFunction::ZExt => Self::ZExt,
            ast::BuiltinFunction::Load => Self::Load,
            ast::BuiltinFunction::Store => Self::Store,
            ast::BuiltinFunction::LoadReserved => Self::LoadReserved,
            ast::BuiltinFunction::StoreConditional => Self::StoreConditional,
            ast::BuiltinFunction::AtomicRmw => Self::AtomicRmw,
            ast::BuiltinFunction::Fence => Self::Fence,
            ast::BuiltinFunction::FenceI => Self::FenceI,
            ast::BuiltinFunction::Trap => Self::Trap,
            ast::BuiltinFunction::Split => Self::Split,
            ast::BuiltinFunction::Concat => Self::Concat,
            ast::BuiltinFunction::Map => Self::Map,
            ast::BuiltinFunction::Reduce => Self::Reduce,
            ast::BuiltinFunction::Zip => Self::Zip,
            ast::BuiltinFunction::FAdd => Self::FAdd,
            ast::BuiltinFunction::FSub => Self::FSub,
            ast::BuiltinFunction::FMul => Self::FMul,
            ast::BuiltinFunction::FDiv => Self::FDiv,
            ast::BuiltinFunction::Todo => Self::Todo,
        }
    }
}
