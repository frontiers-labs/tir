//! The C frontend's post-order abstract syntax tree. It preserves scalar type
//! ranks, declarators, literal spelling, expressions, and control-flow syntax;
//! codegen still lowers only its original subset.
//!
//! The tree is stored in core's [`PostOrderDag`], the same cache-friendly,
//! post-order layout the semantic-expression graph uses: node *kinds* live in a
//! flat vector while the variable-sized payload (names, literals, types) sits in
//! a sparse side table keyed by node id. Children always precede their parent,
//! so the root is the last node.

use tir::graph::{Dag, NodeId, PostOrderDag};

use crate::diagnostics::Span;
use crate::lexer::{FloatingLiteral, IntegerLiteral};

/// The AST: node payloads ([`AstNode`], kind + source span) live in the DAG's
/// dense vector, while the variable-sized leaf payload sits in its side table.
pub type Ast = PostOrderDag<AstNode, AstLeaf, crate::sema::NodeSemantics>;

/// A node's dense payload: its structural [`AstKind`] and the source [`Span`]
/// where the construct begins, used to point diagnostics at the offending code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AstNode {
    pub kind: AstKind,
    pub span: Span,
}

impl AstNode {
    pub fn new(kind: AstKind, span: Span) -> Self {
        AstNode { kind, span }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CType {
    Invalid(String),
    Int,
    Void,
    Char,
    SignedChar,
    UnsignedChar,
    Short,
    UnsignedShort,
    UnsignedInt,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
    Bool,
    Float,
    Double,
    LongDouble,
    Builtin(String),
    Named(String),
    Record(RecordKind, RecordId, Option<String>),
    Enum(Option<String>),
    Const(Box<CType>),
    Volatile(Box<CType>),
    Restrict(Box<CType>),
    Pointer(Box<CType>),
    Array(Box<CType>, Option<String>),
    Function {
        ret: Box<CType>,
        params: Vec<CParam>,
        varargs: bool,
        has_parameter_type_list: bool,
    },
    Attributed(Box<CType>, Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId(u32);

impl RecordId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub(crate) fn number(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    Struct,
    Union,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CParam {
    pub name: String,
    pub ty: CType,
}

/// The structural kind of an AST node. How its children are interpreted depends
/// solely on the kind; payload data lives in the matching [`AstLeaf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstKind {
    /// Children: the translation unit's functions.
    TranslationUnit,
    /// Children: declarations parsed from one C declaration statement.
    DeclGroup,
    /// Children: field declarations.
    RecordDecl,
    Typedef,
    Global,
    Field,
    Attribute,
    /// Children: parameters.
    Prototype,
    /// Children: parameters, then body statements.
    Function,
    Param,
    VarArgs,
    /// Child: the optional initializer expression.
    Decl,
    /// Child: the assigned value expression.
    Assign,
    AssignExpr,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    ShlAssign,
    ShrAssign,
    AndAssign,
    XorAssign,
    OrAssign,
    /// Child: the optional returned expression.
    Return,
    /// Children: the block's statements.
    Block,
    /// Child: the wrapped expression (an expression used as a statement).
    ExprStmt,
    /// Children: condition, then-branch, and an optional else-branch.
    If,
    /// Children: condition, body.
    While,
    /// Children: body, condition.
    DoWhile,
    /// Children: init, condition, step, body. Omitted clauses are [`AstKind::Empty`].
    For,
    Switch,
    Case,
    Default,
    Goto,
    Label,
    Break,
    Continue,
    /// A placeholder for an omitted `for` clause or a null statement.
    Empty,
    /// Children: left-hand side, right-hand side.
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    LogAnd,
    LogOr,
    Conditional,
    Comma,
    Cast,
    SizeofExpr,
    SizeofType,
    /// Child: the single operand.
    Neg,
    Pos,
    Not,
    BitNot,
    PreInc,
    PreDec,
    PostInc,
    PostDec,
    /// Children: the argument expressions. Callee name lives in [`AstLeaf::Call`].
    Call,
    /// Child: the base expression. Field name and access form live in [`AstLeaf::Member`].
    Member,
    Var,
    String,
    Int,
    FloatLiteral,
    Character,
}

/// Payload for the nodes that carry one. Indexed by node id through
/// [`Dag::get_leaf_data`]; structural nodes ([`AstKind::TranslationUnit`],
/// [`AstKind::Return`], the binary operators) have none.
#[derive(Debug, Clone, PartialEq)]
pub enum AstLeaf {
    Record {
        id: RecordId,
        kind: RecordKind,
        name: Option<String>,
    },
    Typedef {
        name: String,
        ty: CType,
    },
    Global {
        name: String,
        ty: CType,
    },
    Field {
        name: String,
        ty: CType,
    },
    Attribute(String),
    Function {
        name: String,
        ret: CType,
        has_parameter_type_list: bool,
    },
    Param {
        name: String,
        ty: CType,
    },
    Decl {
        name: String,
        ty: CType,
    },
    Assign(String),
    Label(String),
    Call(String),
    Member {
        name: String,
        indirect: bool,
    },
    Var(String),
    String(String),
    Int(IntegerLiteral),
    Float(FloatingLiteral),
    Character(String),
    Type(CType),
}

fn render_ctype(ty: &CType) -> String {
    match ty {
        CType::Invalid(spelling) => format!("Invalid({spelling})"),
        CType::Int => "Int".to_string(),
        CType::Void => "Void".to_string(),
        CType::Char => "Char".to_string(),
        CType::SignedChar => "SignedChar".to_string(),
        CType::UnsignedChar => "UnsignedChar".to_string(),
        CType::Short => "Short".to_string(),
        CType::UnsignedShort => "UnsignedShort".to_string(),
        CType::UnsignedInt => "UnsignedInt".to_string(),
        CType::Long => "Long".to_string(),
        CType::UnsignedLong => "UnsignedLong".to_string(),
        CType::LongLong => "LongLong".to_string(),
        CType::UnsignedLongLong => "UnsignedLongLong".to_string(),
        CType::Bool => "Bool".to_string(),
        CType::Float => "Float".to_string(),
        CType::Double => "Double".to_string(),
        CType::LongDouble => "LongDouble".to_string(),
        CType::Builtin(name) => format!("Builtin({name})"),
        CType::Named(name) => format!("Named({name})"),
        CType::Record(kind, _, name) => {
            let kind = match kind {
                RecordKind::Struct => "struct",
                RecordKind::Union => "union",
            };
            match name {
                Some(name) => format!("Record({kind} {name})"),
                None => format!("Record({kind})"),
            }
        }
        CType::Enum(name) => match name {
            Some(name) => format!("Enum({name})"),
            None => "Enum".to_string(),
        },
        CType::Const(inner) => format!("Const({})", render_ctype(inner)),
        CType::Volatile(inner) => format!("Volatile({})", render_ctype(inner)),
        CType::Restrict(inner) => format!("Restrict({})", render_ctype(inner)),
        CType::Pointer(inner) => format!("Ptr({})", render_ctype(inner)),
        CType::Array(inner, Some(len)) => format!("Array({}, {len})", render_ctype(inner)),
        CType::Array(inner, None) => format!("Array({})", render_ctype(inner)),
        CType::Function {
            ret,
            params,
            varargs,
            ..
        } => {
            let mut parts = params
                .iter()
                .map(|p| render_ctype(&p.ty))
                .collect::<Vec<_>>();
            if *varargs {
                parts.push("...".to_string());
            }
            format!("Fn({}) -> {}", parts.join(", "), render_ctype(ret))
        }
        CType::Attributed(inner, attrs) => {
            format!("Attr({}; {})", attrs.join(", "), render_ctype(inner))
        }
    }
}

/// Render the tree as an indented outline, used by the `--stage ast` output.
pub fn render(ast: &Ast) -> String {
    let mut out = String::new();
    if let Some(root) = ast.root() {
        render_node(ast, root, 0, &mut out);
    }
    out
}

fn render_node(ast: &Ast, id: NodeId, depth: usize, out: &mut String) {
    use std::fmt::Write;

    let label = match ast.get_node(id).kind {
        AstKind::TranslationUnit => "TranslationUnit".to_string(),
        AstKind::DeclGroup => "DeclGroup".to_string(),
        AstKind::RecordDecl => match ast.get_leaf_data(id) {
            Some(AstLeaf::Record { kind, name, .. }) => {
                let kind = match kind {
                    RecordKind::Struct => "Struct",
                    RecordKind::Union => "Union",
                };
                match name {
                    Some(name) => format!("{kind} {name:?}"),
                    None => kind.to_string(),
                }
            }
            _ => unreachable!(),
        },
        AstKind::Typedef => match ast.get_leaf_data(id) {
            Some(AstLeaf::Typedef { name, ty }) => {
                format!("Typedef {name:?}: {}", render_ctype(ty))
            }
            _ => unreachable!(),
        },
        AstKind::Global => match ast.get_leaf_data(id) {
            Some(AstLeaf::Global { name, ty }) => {
                format!("Global {name:?}: {}", render_ctype(ty))
            }
            _ => unreachable!(),
        },
        AstKind::Field => match ast.get_leaf_data(id) {
            Some(AstLeaf::Field { name, ty }) if name.is_empty() => {
                format!("Field _: {}", render_ctype(ty))
            }
            Some(AstLeaf::Field { name, ty }) => {
                format!("Field {name:?}: {}", render_ctype(ty))
            }
            _ => unreachable!(),
        },
        AstKind::Attribute => match ast.get_leaf_data(id) {
            Some(AstLeaf::Attribute(value)) => format!("Attribute {value:?}"),
            _ => unreachable!(),
        },
        AstKind::Prototype => match ast.get_leaf_data(id) {
            Some(AstLeaf::Function { name, ret, .. }) => {
                format!("Prototype {name:?} -> {}", render_ctype(ret))
            }
            _ => unreachable!(),
        },
        AstKind::Function => match ast.get_leaf_data(id) {
            Some(AstLeaf::Function { name, ret, .. }) => {
                format!("Function {name:?} -> {}", render_ctype(ret))
            }
            _ => unreachable!(),
        },
        AstKind::Param => match ast.get_leaf_data(id) {
            Some(AstLeaf::Param { name, ty }) if name.is_empty() => {
                format!("Param _: {}", render_ctype(ty))
            }
            Some(AstLeaf::Param { name, ty }) => format!("Param {name:?}: {}", render_ctype(ty)),
            _ => unreachable!(),
        },
        AstKind::VarArgs => "VarArgs".to_string(),
        AstKind::Decl => match ast.get_leaf_data(id) {
            Some(AstLeaf::Decl { name, ty }) => format!("Decl {name:?}: {}", render_ctype(ty)),
            _ => unreachable!(),
        },
        AstKind::Assign => match ast.get_leaf_data(id) {
            Some(AstLeaf::Assign(name)) => format!("Assign {name:?}"),
            _ => unreachable!(),
        },
        AstKind::AssignExpr => "AssignExpr".to_string(),
        AstKind::AddAssign => "AddAssign".to_string(),
        AstKind::SubAssign => "SubAssign".to_string(),
        AstKind::MulAssign => "MulAssign".to_string(),
        AstKind::DivAssign => "DivAssign".to_string(),
        AstKind::ModAssign => "ModAssign".to_string(),
        AstKind::ShlAssign => "ShlAssign".to_string(),
        AstKind::ShrAssign => "ShrAssign".to_string(),
        AstKind::AndAssign => "AndAssign".to_string(),
        AstKind::XorAssign => "XorAssign".to_string(),
        AstKind::OrAssign => "OrAssign".to_string(),
        AstKind::Return => "Return".to_string(),
        AstKind::Block => "Block".to_string(),
        AstKind::ExprStmt => "ExprStmt".to_string(),
        AstKind::If => "If".to_string(),
        AstKind::While => "While".to_string(),
        AstKind::DoWhile => "DoWhile".to_string(),
        AstKind::For => "For".to_string(),
        AstKind::Switch => "Switch".to_string(),
        AstKind::Case => "Case".to_string(),
        AstKind::Default => "Default".to_string(),
        AstKind::Goto => match ast.get_leaf_data(id) {
            Some(AstLeaf::Label(name)) => format!("Goto {name:?}"),
            _ => unreachable!(),
        },
        AstKind::Label => match ast.get_leaf_data(id) {
            Some(AstLeaf::Label(name)) => format!("Label {name:?}"),
            _ => unreachable!(),
        },
        AstKind::Break => "Break".to_string(),
        AstKind::Continue => "Continue".to_string(),
        AstKind::Empty => "Empty".to_string(),
        AstKind::Add => "Add".to_string(),
        AstKind::Sub => "Sub".to_string(),
        AstKind::Mul => "Mul".to_string(),
        AstKind::Div => "Div".to_string(),
        AstKind::Mod => "Mod".to_string(),
        AstKind::Shl => "Shl".to_string(),
        AstKind::Shr => "Shr".to_string(),
        AstKind::Lt => "Lt".to_string(),
        AstKind::Gt => "Gt".to_string(),
        AstKind::Le => "Le".to_string(),
        AstKind::Ge => "Ge".to_string(),
        AstKind::Eq => "Eq".to_string(),
        AstKind::Ne => "Ne".to_string(),
        AstKind::BitAnd => "BitAnd".to_string(),
        AstKind::BitXor => "BitXor".to_string(),
        AstKind::BitOr => "BitOr".to_string(),
        AstKind::LogAnd => "LogAnd".to_string(),
        AstKind::LogOr => "LogOr".to_string(),
        AstKind::Conditional => "Conditional".to_string(),
        AstKind::Comma => "Comma".to_string(),
        AstKind::Cast => match ast.get_leaf_data(id) {
            Some(AstLeaf::Type(ty)) => format!("Cast {}", render_ctype(ty)),
            _ => unreachable!(),
        },
        AstKind::SizeofExpr => "SizeofExpr".to_string(),
        AstKind::SizeofType => match ast.get_leaf_data(id) {
            Some(AstLeaf::Type(ty)) => format!("SizeofType {}", render_ctype(ty)),
            _ => unreachable!(),
        },
        AstKind::Neg => "Neg".to_string(),
        AstKind::Pos => "Pos".to_string(),
        AstKind::Not => "Not".to_string(),
        AstKind::BitNot => "BitNot".to_string(),
        AstKind::PreInc => "PreInc".to_string(),
        AstKind::PreDec => "PreDec".to_string(),
        AstKind::PostInc => "PostInc".to_string(),
        AstKind::PostDec => "PostDec".to_string(),
        AstKind::Call => match ast.get_leaf_data(id) {
            Some(AstLeaf::Call(name)) => format!("Call {name:?}"),
            _ => unreachable!(),
        },
        AstKind::Member => match ast.get_leaf_data(id) {
            Some(AstLeaf::Member { name, indirect }) => {
                format!("Member {}{name}", if *indirect { "->" } else { "." })
            }
            _ => unreachable!(),
        },
        AstKind::Var => match ast.get_leaf_data(id) {
            Some(AstLeaf::Var(name)) => format!("Var {name:?}"),
            _ => unreachable!(),
        },
        AstKind::String => match ast.get_leaf_data(id) {
            Some(AstLeaf::String(value)) => format!("String {value:?}"),
            _ => unreachable!(),
        },
        AstKind::Int => match ast.get_leaf_data(id) {
            Some(AstLeaf::Int(value)) => format!("Int {}", value.spelling),
            _ => unreachable!(),
        },
        AstKind::FloatLiteral => match ast.get_leaf_data(id) {
            Some(AstLeaf::Float(value)) => format!("Float {}", value.spelling),
            _ => unreachable!(),
        },
        AstKind::Character => match ast.get_leaf_data(id) {
            Some(AstLeaf::Character(value)) => format!("Character {value}"),
            _ => unreachable!(),
        },
    };

    writeln!(out, "{:indent$}{label}", "", indent = depth * 2).unwrap();
    for child in ast.children(id) {
        render_node(ast, child, depth + 1, out);
    }
}
