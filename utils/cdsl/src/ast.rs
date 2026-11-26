//! Zero-copy, flat-arena surface AST for CDSL v3.
//!
//! All identifiers and string literals borrow `&'src str` directly from the input
//! source, and all child lists live in contiguous flat `Vec` slices indexed by
//! compact `u32` IDs so parsing makes zero per-node heap allocations.

use std::ops::Range;

/// Index into `Ast::exprs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExprId(pub u32);

/// Index into `Ast::decls`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeclId(pub u32);

/// Contiguous slice range inside one of `Ast`'s flat pools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Slice {
    pub start: u32,
    pub len: u32,
}

impl Slice {
    pub const EMPTY: Self = Self { start: 0, len: 0 };

    #[inline]
    pub fn as_range(self) -> Range<usize> {
        let start = self.start as usize;
        start..start + self.len as usize
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sort {
    Type,
    Prop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExtractTarget {
    Rust,
    Egglog,
    Lean,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lit<'src> {
    Int(&'src str),
    Bool(bool),
    Str(&'src str),
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum BinOp {
    #[default]
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
    StrCat,
}

impl BinOp {
    #[inline]
    pub const fn prec(self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
            BinOp::Add | BinOp::Sub | BinOp::StrCat => 4,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pattern<'src> {
    Wild,
    Var(&'src str),
    Lit(Lit<'src>),
    Ctor {
        path: Slice,   // slice into `idents`
        fields: Slice, // slice into `idents`
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expr<'src> {
    Path {
        path: Slice,    // slice into `idents`
        ty_args: Slice, // slice into `expr_lists`
    },
    Lit(Lit<'src>),
    Sort(Sort),
    Pi {
        param: &'src str,
        dom: ExprId,
        cod: ExprId,
    },
    Lam {
        params: Slice, // slice into `opt_params`
        ret: Option<ExprId>,
        body: ExprId,
    },
    RecFix {
        params: Slice, // slice into `opt_params`
        ret: Option<ExprId>,
        body: ExprId,
    },
    Memo {
        params: Slice, // slice into `opt_params`
        body: ExprId,
    },
    App {
        func: ExprId,
        args: Slice, // slice into `expr_lists`
    },
    Block {
        stmts: Slice, // slice into `stmts`
        final_expr: ExprId,
    },
    Record(Slice),   // slice into `fields`
    RecordTy(Slice), // slice into `fields`
    Proj {
        obj: ExprId,
        field: &'src str,
    },
    Match {
        scrut: ExprId,
        arms: Slice, // slice into `arms`
    },
    If {
        cond: ExprId,
        then_expr: ExprId,
        else_expr: ExprId,
    },
    Bin {
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    Not(ExprId),
    Neg(ExprId),
    Try(ExprId),
    OpSugar(Slice), // slice into `fields`
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stmt<'src> {
    Let {
        name: &'src str,
        ty: Option<ExprId>,
        rhs: ExprId,
    },
    Rule {
        name: &'src str,
        vars: Slice, // slice into `idents`
        kind: &'src str,
        lhs: Slice, // slice into `expr_lists`
        rhs: Slice, // slice into `expr_lists`
    },
    Expr(ExprId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctor<'src> {
    pub name: &'src str,
    pub fields: Slice, // slice into `fields` (name, type_expr)
    pub ret: Option<ExprId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UseItems<'src> {
    Single,
    Alias(&'src str),
    Glob,
    Group(Slice), // slice into `idents`
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decl<'src> {
    Mod {
        public: bool,
        name: &'src str,
        body: Option<Slice>, // slice into `decl_lists`
    },
    Use {
        public: bool,
        path: Slice, // slice into `idents`
        items: UseItems<'src>,
    },
    Type {
        public: bool,
        name: &'src str,
        params: Slice, // slice into `fields` (name, type_expr)
        sort: Sort,
    },
    Enum {
        public: bool,
        name: &'src str,
        params: Slice, // slice into `fields` (name, type_expr)
        sort: Sort,
        ctors: Slice, // slice into `ctors`
    },
    Let {
        public: bool,
        name: &'src str,
        tparams: Slice, // slice into `fields`
        ty: Option<ExprId>,
        body: Option<ExprId>,
    },
    Rule {
        public: bool,
        name: &'src str,
        vars: Slice, // slice into `idents`
        kind: &'src str,
        lhs: Slice, // slice into `expr_lists`
        rhs: Slice, // slice into `expr_lists`
    },
    Extract {
        target: ExtractTarget,
        path: Slice, // slice into `idents`
        rhs: &'src str,
        ctors: Option<Slice>, // slice into `ctor_maps`
    },
}

/// Flat-arena AST owning all parsed nodes in contiguous vectors.
#[derive(Clone, Debug)]
pub struct Ast<'src> {
    pub root: Slice, // slice into `decl_lists`
    pub decls: Vec<Decl<'src>>,
    pub decl_spans: Vec<(u32, u32)>,
    pub decl_lists: Vec<DeclId>,
    pub exprs: Vec<Expr<'src>>,
    pub expr_lists: Vec<ExprId>,
    pub idents: Vec<&'src str>,
    pub fields: Vec<(&'src str, ExprId)>,
    pub opt_params: Vec<(&'src str, Option<ExprId>)>,
    pub stmts: Vec<Stmt<'src>>,
    pub arms: Vec<(Pattern<'src>, ExprId)>,
    pub ctors: Vec<Ctor<'src>>,
    pub ctor_maps: Vec<(&'src str, &'src str)>,
}

impl<'src> Ast<'src> {
    pub fn with_token_capacity(token_count: usize) -> Self {
        Self {
            root: Slice::EMPTY,
            decls: Vec::with_capacity(token_count / 24 + 4),
            decl_spans: Vec::with_capacity(token_count / 24 + 4),
            decl_lists: Vec::with_capacity(token_count / 24 + 4),
            exprs: Vec::with_capacity(token_count / 4 + 8),
            expr_lists: Vec::with_capacity(token_count / 6 + 8),
            idents: Vec::with_capacity(token_count / 4 + 8),
            fields: Vec::with_capacity(token_count / 16 + 4),
            opt_params: Vec::with_capacity(token_count / 24 + 4),
            stmts: Vec::with_capacity(token_count / 32 + 4),
            arms: Vec::with_capacity(token_count / 32 + 4),
            ctors: Vec::with_capacity(token_count / 32 + 4),
            ctor_maps: Vec::with_capacity(token_count / 64 + 4),
        }
    }

    #[inline]
    pub fn push_expr(&mut self, expr: Expr<'src>) -> ExprId {
        let id = ExprId(self.exprs.len() as u32);
        self.exprs.push(expr);
        id
    }

    #[inline]
    pub fn push_decl(&mut self, decl: Decl<'src>, span: (u32, u32)) -> DeclId {
        let id = DeclId(self.decls.len() as u32);
        self.decls.push(decl);
        self.decl_spans.push(span);
        id
    }

    #[inline]
    pub fn push_idents(&mut self, slice: &[&'src str]) -> Slice {
        let start = self.idents.len() as u32;
        self.idents.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_ident_one(&mut self, ident: &'src str) -> Slice {
        let start = self.idents.len() as u32;
        self.idents.push(ident);
        Slice { start, len: 1 }
    }

    #[inline]
    pub fn push_exprs(&mut self, slice: &[ExprId]) -> Slice {
        let start = self.expr_lists.len() as u32;
        self.expr_lists.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_expr_one(&mut self, id: ExprId) -> Slice {
        let start = self.expr_lists.len() as u32;
        self.expr_lists.push(id);
        Slice { start, len: 1 }
    }

    #[inline]
    pub fn push_decls(&mut self, slice: &[DeclId]) -> Slice {
        let start = self.decl_lists.len() as u32;
        self.decl_lists.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_fields(&mut self, slice: &[(&'src str, ExprId)]) -> Slice {
        let start = self.fields.len() as u32;
        self.fields.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_opt_params(&mut self, slice: &[(&'src str, Option<ExprId>)]) -> Slice {
        let start = self.opt_params.len() as u32;
        self.opt_params.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_stmts(&mut self, slice: &[Stmt<'src>]) -> Slice {
        let start = self.stmts.len() as u32;
        self.stmts.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_arms(&mut self, slice: &[(Pattern<'src>, ExprId)]) -> Slice {
        let start = self.arms.len() as u32;
        self.arms.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_ctors(&mut self, slice: &[Ctor<'src>]) -> Slice {
        let start = self.ctors.len() as u32;
        self.ctors.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn push_ctor_maps(&mut self, slice: &[(&'src str, &'src str)]) -> Slice {
        let start = self.ctor_maps.len() as u32;
        self.ctor_maps.extend_from_slice(slice);
        Slice {
            start,
            len: slice.len() as u32,
        }
    }

    #[inline]
    pub fn expr(&self, id: ExprId) -> Expr<'src> {
        self.exprs[id.0 as usize]
    }

    #[inline]
    pub fn decl(&self, id: DeclId) -> Decl<'src> {
        self.decls[id.0 as usize]
    }

    #[inline]
    pub fn root_decls(&self) -> &[DeclId] {
        &self.decl_lists[self.root.as_range()]
    }

    #[inline]
    pub fn ident_slice(&self, s: Slice) -> &[&'src str] {
        &self.idents[s.as_range()]
    }

    #[inline]
    pub fn expr_slice(&self, s: Slice) -> &[ExprId] {
        &self.expr_lists[s.as_range()]
    }

    #[inline]
    pub fn decl_slice(&self, s: Slice) -> &[DeclId] {
        &self.decl_lists[s.as_range()]
    }

    #[inline]
    pub fn field_slice(&self, s: Slice) -> &[(&'src str, ExprId)] {
        &self.fields[s.as_range()]
    }

    #[inline]
    pub fn opt_param_slice(&self, s: Slice) -> &[(&'src str, Option<ExprId>)] {
        &self.opt_params[s.as_range()]
    }

    #[inline]
    pub fn stmt_slice(&self, s: Slice) -> &[Stmt<'src>] {
        &self.stmts[s.as_range()]
    }

    #[inline]
    pub fn arm_slice(&self, s: Slice) -> &[(Pattern<'src>, ExprId)] {
        &self.arms[s.as_range()]
    }

    #[inline]
    pub fn ctor_slice(&self, s: Slice) -> &[Ctor<'src>] {
        &self.ctors[s.as_range()]
    }

    #[inline]
    pub fn ctor_map_slice(&self, s: Slice) -> &[(&'src str, &'src str)] {
        &self.ctor_maps[s.as_range()]
    }
}
