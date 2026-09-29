//! AST mirroring SMT-LIB 2.7 grammar; theory meaning is resolved at conversion, not baked in here.

use std::collections::HashSet;

/// Non-alphanumeric characters permitted in a simple (unquoted) symbol.
pub const SYMBOL_CHARS: &str = "+-/*=%?!.$_~&^<>@";

/// Whether `name` is a valid simple symbol and so needs no `|...|` quoting.
pub fn is_simple_symbol(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || SYMBOL_CHARS.contains(first) => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || SYMBOL_CHARS.contains(c))
}

/// `<spec_constant>`. Hex/Binary keep prefix-less digits so width survives round-trip; numerals capped at u128.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecConstant {
    Numeral(u128),
    Decimal(String),
    Hexadecimal(String),
    Binary(String),
    String(String),
}

/// A `<symbol>`: logical name without `|...|` quotes; quoting is a printing concern.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Symbol(pub String);

/// A `<keyword>`, stored without the leading `:`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Keyword(pub String);

/// An `<index>`: the `7`/`0` in `(_ extract 7 0)` or a symbolic index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Index {
    Numeral(u128),
    Symbol(Symbol),
}

/// An `<identifier>`: a bare symbol, or an indexed `(_ symbol index+)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identifier {
    pub symbol: Symbol,
    pub indices: Vec<Index>,
}

impl Identifier {
    pub fn simple(name: impl Into<String>) -> Self {
        Identifier {
            symbol: Symbol(name.into()),
            indices: Vec::new(),
        }
    }

    pub fn is_simple(&self) -> bool {
        self.indices.is_empty()
    }
}

/// A `<sort>`: an identifier optionally applied to argument sorts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sort {
    pub id: Identifier,
    pub params: Vec<Sort>,
}

impl Sort {
    pub fn simple(id: Identifier) -> Self {
        Sort {
            id,
            params: Vec::new(),
        }
    }

    pub fn bool() -> Self {
        Sort::simple(Identifier::simple("Bool"))
    }

    /// `(_ BitVec width)`.
    pub fn bitvec(width: u32) -> Self {
        Sort::simple(Identifier {
            symbol: Symbol("BitVec".into()),
            indices: vec![Index::Numeral(u128::from(width))],
        })
    }
}

/// A `<qual_identifier>`: an identifier, optionally `(as id sort)`-annotated to disambiguate overloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QualIdentifier {
    Plain(Identifier),
    Annotated(Identifier, Sort),
}

impl QualIdentifier {
    pub fn identifier(&self) -> &Identifier {
        match self {
            QualIdentifier::Plain(id) | QualIdentifier::Annotated(id, _) => id,
        }
    }
}

/// An `<s_expr>`: the generic untyped form for attribute values and other nested expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SExpr {
    Constant(SpecConstant),
    Symbol(Symbol),
    Keyword(Keyword),
    List(Vec<SExpr>),
}

/// An `<attribute_value>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeValue {
    Constant(SpecConstant),
    Symbol(Symbol),
    List(Vec<SExpr>),
}

/// An `<attribute>`: a keyword optionally carrying a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub keyword: Keyword,
    pub value: Option<AttributeValue>,
}

/// A `(symbol term)` binding inside `let`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VarBinding {
    pub var: Symbol,
    pub term: Term,
}

/// A `(symbol sort)` binding inside a function definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortedVar {
    pub var: Symbol,
    pub sort: Sort,
}

/// A `<term>` of the Core + FixedSizeBitVectors fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    Constant(SpecConstant),
    Ident(QualIdentifier),
    App(QualIdentifier, Vec<Term>),
    Let(Vec<VarBinding>, Box<Term>),
    Exists(Vec<SortedVar>, Box<Term>),
    Annotated(Box<Term>, Vec<Attribute>),
}

impl Term {
    /// A constant, variable or nullary function by name.
    pub fn ident(name: impl Into<String>) -> Self {
        Term::Ident(QualIdentifier::Plain(Identifier::simple(name)))
    }

    pub fn bool(value: bool) -> Self {
        Term::ident(if value { "true" } else { "false" })
    }

    /// The bit-vector literal `(_ bv<value> width)`.
    pub fn bv(value: u128, width: u32) -> Self {
        Term::Ident(QualIdentifier::Plain(Identifier {
            symbol: Symbol(format!("bv{value}")),
            indices: vec![Index::Numeral(u128::from(width))],
        }))
    }

    pub fn app(op: &str, args: Vec<Term>) -> Self {
        Term::App(QualIdentifier::Plain(Identifier::simple(op)), args)
    }

    /// An application of an indexed function such as `(_ extract 7 0)`.
    pub fn indexed_app(op: &str, indices: &[u128], args: Vec<Term>) -> Self {
        Term::App(
            QualIdentifier::Plain(Identifier {
                symbol: Symbol(op.into()),
                indices: indices.iter().map(|&index| Index::Numeral(index)).collect(),
            }),
            args,
        )
    }

    /// Simple symbols this term names outside its own `let` and `exists`
    /// binders: its free constants and variables.
    pub fn free_symbols(&self) -> HashSet<&str> {
        let mut free = HashSet::new();
        self.collect_free(&mut Vec::new(), &mut free);
        free
    }

    fn collect_free<'a>(&'a self, bound: &mut Vec<&'a str>, free: &mut HashSet<&'a str>) {
        match self {
            Term::Constant(_) => {}
            Term::Ident(id) => {
                let id = id.identifier();
                if id.is_simple() && !bound.contains(&id.symbol.0.as_str()) {
                    free.insert(&id.symbol.0);
                }
            }
            Term::App(_, args) => {
                for arg in args {
                    arg.collect_free(bound, free);
                }
            }
            // `let` binds in parallel: its terms see only the enclosing scope.
            Term::Let(binds, body) => {
                for bind in binds {
                    bind.term.collect_free(bound, free);
                }
                let depth = bound.len();
                bound.extend(binds.iter().map(|bind| bind.var.0.as_str()));
                body.collect_free(bound, free);
                bound.truncate(depth);
            }
            Term::Exists(vars, body) => {
                let depth = bound.len();
                bound.extend(vars.iter().map(|var| var.var.0.as_str()));
                body.collect_free(bound, free);
                bound.truncate(depth);
            }
            Term::Annotated(term, _) => term.collect_free(bound, free),
        }
    }
}

/// A `function_def`: `symbol (sorted_var*) sort term`, as used by `define-fun`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: Symbol,
    pub params: Vec<SortedVar>,
    pub return_sort: Sort,
    pub body: Term,
}

/// A `prop_literal`: `symbol` or `(not symbol)`, used by `check-sat-assuming`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropLiteral {
    pub symbol: Symbol,
    pub negated: bool,
}

/// A top-level `<command>`. Sort, datatype, array and string declarations are out of scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    SetLogic(Symbol),
    SetOption(Attribute),
    SetInfo(Attribute),
    DeclareConst(Symbol, Sort),
    DeclareFun(Symbol, Vec<Sort>, Sort),
    DefineFun(FunctionDef),
    Assert(Term),
    CheckSat,
    CheckSatAssuming(Vec<PropLiteral>),
    GetModel,
    GetValue(Vec<Term>),
    Push(u128),
    Pop(u128),
    Reset,
    ResetAssertions,
    Echo(String),
    Exit,
}

/// A full SMT-LIB script: a sequence of commands.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Script(pub Vec<Command>);
