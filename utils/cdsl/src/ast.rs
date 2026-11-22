use std::ops::Range;

use crate::lexer::Span;

/// Node ranges index `Ast::token_spans`, retaining either input's original spans.
#[derive(Clone)]
pub struct Node<T> {
    pub value: T,
    pub range: Range<usize>,
}

impl<T: std::fmt::Debug> std::fmt::Debug for Node<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub struct Ast {
    pub declarations: Vec<Node<Declaration>>,
    pub token_spans: Vec<Span>,
    pub end: Span,
}

pub type Expr = Node<Expression>;
pub type Pattern = Node<PatternKind>;
pub type Path = Vec<String>;
pub type Record = Vec<(String, Expr)>;

#[derive(Clone, Debug)]
pub enum Declaration {
    Use {
        public: bool,
        path: Path,
        glob: bool,
        alias: Option<String>,
    },
    Mod {
        public: bool,
        name: String,
        declarations: Option<Vec<Node<Declaration>>>,
    },
    Let {
        public: bool,
        binding: Binding,
    },
    /// A named operation interface whose contract is a record type.
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
pub struct Binder {
    pub name: String,
    pub ty: Expr,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub path: Path,
    pub generics: Vec<Binder>,
    pub ty: Option<Expr>,
    pub value: Expr,
}

impl Binding {
    pub(crate) fn operation_fields(&self) -> Option<&[Node<OpField>]> {
        match &self.value.value {
            Expression::Op(fields) => Some(fields),
            _ => None,
        }
    }

    pub(crate) fn operation_name(&self, module: &[String]) -> Option<String> {
        if self.operation_fields().is_some() {
            let dialect = match module {
                [.., namespace, dialect] if namespace == "dialects" => dialect,
                _ => "builtin",
            };
            return Some(format!("{dialect}.{}", self.path.last()?));
        }
        let Expression::Record(fields) = &self.value.value else {
            return None;
        };
        let field = |name| {
            fields.iter().find_map(|(key, value)| {
                if key == name
                    && let Expression::Literal(Literal::String(value)) = &value.value
                {
                    Some(value.as_str())
                } else {
                    None
                }
            })
        };
        field("rust_name")?;
        Some(format!("{}.{}", field("dialect")?, field("name")?))
    }
}

#[derive(Clone, Debug)]
pub struct Variant {
    pub name: String,
    pub generics: Vec<Binder>,
    pub arguments: Option<Vec<Expr>>,
    pub ty: Option<Expr>,
}

#[derive(Clone, Debug)]
pub enum Literal {
    Natural(String),
    String(String),
    Bool(bool),
}

#[derive(Clone, Copy, Debug)]
pub enum UnaryOp {
    Not,
    Negate,
}

#[derive(Clone, Copy, Debug)]
pub enum BinaryOp {
    Arrow,
    Or,
    And,
    Equal,
    NotEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

#[derive(Clone, Debug)]
pub enum Expression {
    Literal(Literal),
    Path(Path),
    Unit,
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    Record(Record),
    Block(Block),
    Function(Box<Function>),
    LocalTable(Box<Expr>),
    Run(Box<Expr>),
    Rule {
        binders: Vec<RuleBinder>,
        clause: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then: Block,
        otherwise: Box<Expr>,
    },
    Match {
        value: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
    },
    Op(Vec<Node<OpField>>),
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
pub struct Block {
    pub bindings: Vec<Binding>,
    pub value: Box<Expr>,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub memo: bool,
    pub generics: Vec<Binder>,
    pub parameters: Vec<Binder>,
    pub result: Option<Expr>,
    pub decreases: Option<Expr>,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    Wildcard,
    Literal(Literal),
    Path(Path),
    Constructor { path: Path, arguments: Vec<Pattern> },
    Unit,
    Tuple(Vec<Pattern>),
}

#[derive(Clone, Debug)]
pub enum OpField {
    Signature {
        parameters: Vec<Binder>,
        result: Expr,
        results: Vec<String>,
    },
    Field {
        name: String,
        value: Expr,
    },
    /// An ordinary interfaces field after resolving its labels.
    Interfaces {
        receiver: String,
        entries: Vec<(String, Path, Expr)>,
    },
}

#[derive(Clone, Debug)]
pub struct RuleBinder {
    pub name: String,
    pub ty: Option<Expr>,
}
