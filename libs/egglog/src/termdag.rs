// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/termdag.rs. See libs/egglog/LICENSE-MIT.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write;

use tir_adt::FxBuildHasher;

use crate::ast::{Expr, Literal, Span};

pub type TermId = usize;

/// An expression whose children are ids into a [`TermDag`], so equal
/// subterms are shared.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Term {
    Lit(Literal),
    Var(String),
    App(String, Vec<TermId>),
}

/// A hash-consing arena of [`Term`]s.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TermDag {
    nodes: Vec<Term>,
    ids: HashMap<Term, TermId, FxBuildHasher>,
}

/// A term of a [`TermDag`] ordered by [`TermDag::ast_cmp`], for ordered
/// collections. Compare only terms of one dag.
#[derive(Copy, Clone)]
pub struct OrdTerm<'a> {
    termdag: &'a TermDag,
    id: TermId,
}

impl OrdTerm<'_> {
    pub fn id(&self) -> TermId {
        self.id
    }
}

impl PartialEq for OrdTerm<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for OrdTerm<'_> {}

impl PartialOrd for OrdTerm<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdTerm<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.termdag.ast_cmp(self.id, other.id)
    }
}

// Shared subterms of at least this many nodes get a `let`.
const MIN_SHARED_TERM_SIZE: usize = 4;

impl TermDag {
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// The id of `node`, which must be in the dag.
    pub fn lookup(&self, node: &Term) -> TermId {
        self.ids[node]
    }

    /// The term of `id`, which must be of this dag.
    pub fn get(&self, id: TermId) -> &Term {
        &self.nodes[id]
    }

    /// An order on terms by structure: literals by value, then variables by
    /// name, then applications by head, arity and children in turn.
    pub fn ast_cmp(&self, a: TermId, b: TermId) -> Ordering {
        let mut work = vec![(a, b)];
        while let Some((a, b)) = work.pop() {
            if a == b {
                continue;
            }
            let order = match (self.get(a), self.get(b)) {
                (Term::Lit(x), Term::Lit(y)) => x.cmp(y),
                (Term::Lit(_), _) => Ordering::Less,
                (_, Term::Lit(_)) => Ordering::Greater,
                (Term::Var(x), Term::Var(y)) => x.cmp(y),
                (Term::Var(_), _) => Ordering::Less,
                (_, Term::Var(_)) => Ordering::Greater,
                (Term::App(f, xs), Term::App(g, ys)) => {
                    let order = f.cmp(g).then(xs.len().cmp(&ys.len()));
                    if order.is_eq() {
                        work.extend(xs.iter().copied().zip(ys.iter().copied()).rev());
                    }
                    order
                }
            };
            if order.is_ne() {
                return order;
            }
        }
        Ordering::Equal
    }

    pub fn sort_terms_by_ast(&self, terms: &mut [TermId]) {
        terms.sort_by(|a, b| self.ast_cmp(*a, *b));
    }

    pub fn ord_term(&self, id: TermId) -> OrdTerm<'_> {
        OrdTerm { termdag: self, id }
    }

    /// The application of `sym` to `children`, which must be of this dag.
    pub fn app(&mut self, sym: String, children: Vec<TermId>) -> TermId {
        assert!(
            children.iter().all(|&c| c < self.nodes.len()),
            "children of this dag"
        );
        self.add(Term::App(sym, children))
    }

    pub fn lit(&mut self, lit: Literal) -> TermId {
        self.add(Term::Lit(lit))
    }

    pub fn var(&mut self, sym: String) -> TermId {
        self.add(Term::Var(sym))
    }

    fn add(&mut self, node: Term) -> TermId {
        if let Some(&id) = self.ids.get(&node) {
            return id;
        }
        self.nodes.push(node.clone());
        self.ids.insert(node, self.nodes.len() - 1);
        self.nodes.len() - 1
    }

    pub fn expr_to_term(&mut self, expr: &Expr) -> TermId {
        match expr {
            Expr::Lit(_, lit) => self.lit(lit.clone()),
            Expr::Var(_, var) => self.var(var.clone()),
            Expr::Call(_, op, args) => {
                let args = args.iter().map(|arg| self.expr_to_term(arg)).collect();
                self.app(op.clone(), args)
            }
        }
    }

    pub fn term_to_expr(&self, term: &TermId, span: Span) -> Expr {
        match self.get(*term) {
            Term::Lit(lit) => Expr::Lit(span, lit.clone()),
            Term::Var(var) => Expr::Var(span, var.clone()),
            Term::App(op, args) => {
                let args = args
                    .iter()
                    .map(|arg| self.term_to_expr(arg, span))
                    .collect();
                Expr::Call(span, op.clone(), args)
            }
        }
    }

    /// The term as egglog text.
    pub fn to_string(&self, term: TermId) -> String {
        let mut out = String::new();
        self.write(term, &mut out, &HashMap::default());
        out
    }

    fn write(
        &self,
        term: TermId,
        out: &mut String,
        names: &HashMap<TermId, String, FxBuildHasher>,
    ) {
        if let Some(name) = names.get(&term) {
            out.push_str(name);
            return;
        }
        match self.get(term) {
            Term::Lit(lit) => write!(out, "{lit}").expect("a string takes writes"),
            Term::Var(var) => out.push_str(var),
            Term::App(head, children) => {
                out.push('(');
                out.push_str(head);
                for &child in children {
                    out.push(' ');
                    self.write(child, out, names);
                }
                out.push(')');
            }
        }
    }

    /// The term with each subterm it shares, of four nodes or more, bound by
    /// a `let` line before it.
    pub fn to_string_with_let(&self, term: TermId) -> String {
        self.to_string_with_let_and_hint(term, "t")
    }

    /// Like [`TermDag::to_string_with_let`], naming the bindings `hint0`,
    /// `hint1` and so on.
    pub fn to_string_with_let_and_hint(&self, term: TermId, hint: &str) -> String {
        let (mut uses, mut sizes) = (vec![0usize; self.size()], vec![0usize; self.size()]);
        let mut order = Vec::new();
        self.stats(term, &mut uses, &mut sizes, &mut order);
        let (mut names, mut lets) = (HashMap::default(), String::new());
        for id in order {
            if id != term && uses[id] > 1 && sizes[id] >= MIN_SHARED_TERM_SIZE {
                let name = format!("{hint}{}", names.len());
                let mut body = String::new();
                self.write(id, &mut body, &names);
                writeln!(lets, "(let {name} {body})").expect("a string takes writes");
                names.insert(id, name);
            }
        }
        let mut out = lets;
        self.write(term, &mut out, &names);
        out
    }

    // Counts the uses of each subterm and its size; `order` lists children
    // before parents.
    fn stats(
        &self,
        term: TermId,
        uses: &mut [usize],
        sizes: &mut [usize],
        order: &mut Vec<TermId>,
    ) {
        uses[term] += 1;
        if uses[term] > 1 {
            return;
        }
        let mut size = 1;
        if let Term::App(_, children) = self.get(term) {
            for &child in children {
                self.stats(child, uses, sizes, order);
                size += sizes[child];
            }
        }
        sizes[term] = size;
        order.push(term);
    }
}
