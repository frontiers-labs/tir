//! Trusted Kernel AST, Inductive Declarations, Semantic Values, and Extract Metadata.
//!
//! ZERO knowledge of any library/domain concept (no List, Rewrite, Op, Table,
//! Schedule, or Memo names anywhere in the kernel).

use std::{
    collections::HashMap,
    fmt::Write,
    hash::{BuildHasherDefault, Hasher},
    rc::Rc,
};

pub use crate::ast::{ExtractTarget, Sort};

/// Fast non-cryptographic hasher (FxHash) for internal compiler symbol/AST tables.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let mut h = self.hash;
        let (chunks, rem) = bytes.as_chunks::<8>();
        for chunk in chunks {
            let v = u64::from_ne_bytes(*chunk);
            h = (h.rotate_left(5) ^ v).wrapping_mul(K);
        }
        if !rem.is_empty() {
            let mut v = 0u64;
            for &b in rem {
                v = (v << 8) | b as u64;
            }
            h = (h.rotate_left(5) ^ v).wrapping_mul(K);
        }
        self.hash = h;
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        self.hash = (self.hash.rotate_left(5) ^ i as u64).wrapping_mul(K);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(K);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
}

pub type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// Interned symbol ID (O(1) copy & equality).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sym(pub u32);

/// String interner shared across elaboration, typechecking, NbE, erasure, and emission.
#[derive(Clone, Debug, Default)]
pub struct Interner {
    map: FxHashMap<Rc<str>, Sym>,
    strings: Vec<Rc<str>>,
}

impl Interner {
    pub fn new() -> Self {
        Self {
            map: FxHashMap::with_capacity_and_hasher(256, BuildHasherDefault::default()),
            strings: Vec::with_capacity(256),
        }
    }

    #[inline]
    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = Sym(self.strings.len() as u32);
        let owned: Rc<str> = Rc::from(s);
        self.strings.push(owned.clone());
        self.map.insert(owned, id);
        id
    }

    #[inline]
    pub fn lookup(&self, s: &str) -> Option<Sym> {
        self.map.get(s).copied()
    }

    #[inline]
    pub fn resolve(&self, sym: Sym) -> &str {
        &self.strings[sym.0 as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Lit {
    Int(i64),
    Bool(bool),
    Str(Rc<str>),
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrimOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    StrCat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Wild,
    Var(Sym),
    Ctor(Sym, Vec<Sym>),
    Lit(Lit),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    Var(Sym),
    Global(Sym),
    Sort(Sort),
    Pi(Sym, Box<Term>, Rc<Term>),
    Lam(Sym, Option<Box<Term>>, Rc<Term>),
    App(Box<Term>, Box<Term>),
    Let(Sym, Option<Box<Term>>, Box<Term>, Box<Term>),
    Ind(Sym, Vec<Term>),
    Ctor(Sym, Sym, Vec<Term>),
    Hole,
    Match(Box<Term>, Rc<Vec<(Pattern, Term)>>),
    Fix(Sym, Box<Term>, Rc<Term>),
    Lit(Lit),
    Prim(PrimOp, Vec<Term>),
    Rec(Vec<(Sym, Term)>),
    RecTy(Vec<(Sym, Term)>),
    Proj(Box<Term>, Sym),
    Opaque(Sym, Box<Term>),
}

#[derive(Clone, Debug)]
pub struct CtorDecl {
    pub cname: Sym,
    pub cparams: Vec<(Sym, Term)>,
    pub cret: Term,
}

#[derive(Clone, Debug)]
pub struct IndDecl {
    pub iname: Sym,
    pub iparams: Vec<(Sym, Term)>,
    pub isort: Sort,
    pub ictors: Vec<CtorDecl>,
}

#[derive(Clone, Debug)]
pub enum ExtractSpec {
    Const(String),
    Inductive(String, Vec<(Sym, String)>),
    Tag(String, Vec<(Sym, String)>),
}

#[derive(Clone, Debug)]
pub enum Decl {
    Ind(IndDecl),
    Type(Sym, Vec<(Sym, Term)>, Sort),
    Def(Sym, Option<Term>, Term),
    Axiom(Sym, Term),
    Extract(ExtractTarget, Sym, ExtractSpec),
}

/// Persistent linked environment for O(1) extension in NbE.
#[derive(Clone, Debug)]
pub struct EnvNode {
    pub name: Sym,
    pub val: Rc<Value>,
    pub parent: Option<Rc<EnvNode>>,
}

pub type Env = Option<Rc<EnvNode>>;

#[inline]
pub fn env_extend(env: &Env, name: Sym, val: Rc<Value>) -> Env {
    Some(Rc::new(EnvNode {
        name,
        val,
        parent: env.clone(),
    }))
}

#[inline]
pub fn env_lookup(mut env: Option<&EnvNode>, name: Sym) -> Option<Rc<Value>> {
    while let Some(node) = env {
        if node.name == name {
            return Some(node.val.clone());
        }
        env = node.parent.as_deref();
    }
    None
}

/// Defunctionalized closure for NbE (`Pi`, `Lam`, and lazy recursive `Fix`).
#[derive(Clone, Debug)]
pub enum Closure {
    Term {
        env: Env,
        param: Sym,
        body: Rc<Term>,
    },
    FixLam {
        env: Env,
        self_name: Sym,
        param: Sym,
        body: Rc<Term>,
    },
}

/// Semantic domain for Normalization by Evaluation (NbE).
#[derive(Clone, Debug)]
pub enum Value {
    Sort(Sort),
    Pi(Sym, Rc<Value>, Closure),
    Lam(Sym, Closure),
    Ind(Sym, Vec<Rc<Value>>),
    Ctor(Sym, Sym, Vec<Rc<Value>>),
    Lit(Lit),
    Rec(Vec<(Sym, Rc<Value>)>),
    RecTy(Vec<(Sym, Rc<Value>)>),
    Neutral(Neutral),
}

#[derive(Clone, Debug)]
pub enum Neutral {
    Var(Sym, u32),
    Global(Sym),
    App(Box<Neutral>, Rc<Value>),
    Match(Box<Neutral>, Env, Rc<Vec<(Pattern, Term)>>),
    Proj(Box<Neutral>, Sym),
    Prim(PrimOp, Vec<Rc<Value>>),
}

pub fn pp_term(interner: &Interner, t: &Term) -> String {
    let mut out = String::new();
    pp_term_into(interner, t, &mut out);
    out
}

fn pp_term_into(interner: &Interner, t: &Term, out: &mut String) {
    match t {
        Term::Var(x) | Term::Global(x) | Term::Opaque(x, _) => out.push_str(interner.resolve(*x)),
        Term::Sort(Sort::Type) => out.push_str("Type"),
        Term::Sort(Sort::Prop) => out.push_str("Prop"),
        Term::Pi(x, a, b) => {
            out.push('(');
            let xs = interner.resolve(*x);
            if xs != "_" {
                out.push_str(xs);
                out.push_str(": ");
            }
            pp_term_into(interner, a, out);
            out.push_str(" -> ");
            pp_term_into(interner, b, out);
            out.push(')');
        }
        Term::Lam(x, ann, b) => {
            out.push_str("(|");
            out.push_str(interner.resolve(*x));
            if let Some(a) = ann {
                out.push_str(": ");
                pp_term_into(interner, a, out);
            }
            out.push_str("| ");
            pp_term_into(interner, b, out);
            out.push(')');
        }
        Term::App(f, a) => {
            out.push('(');
            pp_term_into(interner, f, out);
            out.push(' ');
            pp_term_into(interner, a, out);
            out.push(')');
        }
        Term::Let(x, _, v, b) => {
            out.push_str("(let ");
            out.push_str(interner.resolve(*x));
            out.push_str(" = ");
            pp_term_into(interner, v, out);
            out.push_str(" in ");
            pp_term_into(interner, b, out);
            out.push(')');
        }
        Term::Ind(name, args) => {
            out.push_str(interner.resolve(*name));
            if !args.is_empty() {
                out.push('<');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    pp_term_into(interner, a, out);
                }
                out.push('>');
            }
        }
        Term::Ctor(tname, cname, args) => {
            out.push_str(interner.resolve(*tname));
            out.push_str("::");
            out.push_str(interner.resolve(*cname));
            if !args.is_empty() {
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    pp_term_into(interner, a, out);
                }
                out.push(')');
            }
        }
        Term::Match(s, _) => {
            out.push_str("(match ");
            pp_term_into(interner, s, out);
            out.push_str(" { ... })");
        }
        Term::Fix(f, _, b) => {
            out.push_str("(fix ");
            out.push_str(interner.resolve(*f));
            out.push_str(" = ");
            pp_term_into(interner, b, out);
            out.push(')');
        }
        Term::Lit(Lit::Int(n)) => {
            let _ = write!(out, "{n}");
        }
        Term::Lit(Lit::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        Term::Lit(Lit::Str(s)) => {
            let _ = write!(out, "{s:?}");
        }
        Term::Lit(Lit::Unit) => out.push_str("()"),
        Term::Prim(op, args) => {
            let sym = match op {
                PrimOp::Add => "+",
                PrimOp::Sub => "-",
                PrimOp::Mul => "*",
                PrimOp::Div => "/",
                PrimOp::Rem => "%",
                PrimOp::Eq => "==",
                PrimOp::Neq => "!=",
                PrimOp::Lt => "<",
                PrimOp::Le => "<=",
                PrimOp::Gt => ">",
                PrimOp::Ge => ">=",
                PrimOp::And => "&&",
                PrimOp::Or => "||",
                PrimOp::Not => "!",
                PrimOp::StrCat => "++",
            };
            out.push('(');
            out.push_str(sym);
            for a in args {
                out.push(' ');
                pp_term_into(interner, a, out);
            }
            out.push(')');
        }
        Term::Rec(fs) => {
            out.push_str("{ ");
            for (i, (k, v)) in fs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(interner.resolve(*k));
                out.push_str(": ");
                pp_term_into(interner, v, out);
            }
            out.push_str(" }");
        }
        Term::RecTy(fs) => {
            out.push_str("rec_ty{ ");
            for (i, (k, v)) in fs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(interner.resolve(*k));
                out.push_str(": ");
                pp_term_into(interner, v, out);
            }
            out.push_str(" }");
        }
        Term::Proj(e, f) => {
            pp_term_into(interner, e, out);
            out.push('.');
            out.push_str(interner.resolve(*f));
        }
        Term::Hole => out.push('_'),
    }
}
