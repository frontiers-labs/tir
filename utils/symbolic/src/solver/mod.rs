//! Stateful QF_BV + Bool SMT solver: each `check-sat` lowers the current assertion
//! stack, bit-blasts it and runs the CDCL backend (non-incremental).

mod driver;

pub use driver::run_script;

use std::collections::HashMap;

use tir_adt::APInt;
use tir_graph::Dag;

use crate::bitblast::{SolveOutcome, blast};
use crate::sat::SatResult;
use crate::smtlib::ast::*;
use crate::smtlib::convert::lower_script;

/// The verdict of a `check-sat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckResult {
    Sat,
    Unsat,
    Unknown,
}

impl CheckResult {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckResult::Sat => "sat",
            CheckResult::Unsat => "unsat",
            CheckResult::Unknown => "unknown",
        }
    }
}

/// A satisfying assignment: each declared symbol's concrete value.
type Model = HashMap<String, APInt>;

/// An SMT solver for the QF_BV + Core (Bool) subset.
#[derive(Default)]
pub struct Solver {
    logic: Option<String>,
    decls: Vec<(String, Sort)>,
    defines: Vec<FunctionDef>,
    asserts: Vec<Term>,
    /// `(decls, defines, asserts)` lengths captured at each `push`.
    checkpoints: Vec<(usize, usize, usize)>,
    /// Cached model from the last `sat` check, invalidated by any state change.
    model: Option<Model>,
}

impl Solver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_logic(&mut self, name: String) {
        self.logic = Some(name);
    }

    pub fn declare_const(&mut self, name: String, sort: Sort) {
        self.decls.push((name, sort));
        self.model = None;
    }

    pub fn define_fun(&mut self, def: FunctionDef) {
        self.defines.push(def);
        self.model = None;
    }

    pub fn assert(&mut self, term: Term) {
        self.asserts.push(term);
        self.model = None;
    }

    pub fn push(&mut self, n: usize) {
        for _ in 0..n {
            self.checkpoints
                .push((self.decls.len(), self.defines.len(), self.asserts.len()));
        }
    }

    pub fn pop(&mut self, n: usize) {
        for _ in 0..n {
            if let Some((d, f, a)) = self.checkpoints.pop() {
                self.decls.truncate(d);
                self.defines.truncate(f);
                self.asserts.truncate(a);
            }
        }
        self.model = None;
    }

    pub fn reset(&mut self) {
        *self = Solver::new();
    }

    pub fn reset_assertions(&mut self) {
        self.decls.clear();
        self.defines.clear();
        self.asserts.clear();
        self.checkpoints.clear();
        self.model = None;
    }

    pub fn check_sat(&mut self) -> CheckResult {
        self.check_sat_assuming(&[])
    }

    /// Decide satisfiability of the current assertions plus `extra` assumptions.
    pub fn check_sat_assuming(&mut self, extra: &[Term]) -> CheckResult {
        self.model = None;
        let script = self.build_script(extra);
        let lo = match lower_script::<()>(&script) {
            Ok(lo) => lo,
            Err(_) => return CheckResult::Unknown,
        };
        let blasted = match blast(&lo.graph, &lo.widths) {
            Ok(b) => b,
            Err(_) => return CheckResult::Unknown,
        };
        match blasted.solve() {
            SolveOutcome::Sat(bits) => {
                self.model = Some(self.build_model(&lo.symbols, &bits));
                CheckResult::Sat
            }
            SolveOutcome::Unsat => CheckResult::Unsat,
            SolveOutcome::Unknown => CheckResult::Unknown,
        }
    }

    /// Model as an SMT-LIB `(define-fun ...)` list (one per constant), after a `sat` check.
    pub fn get_model(&self) -> Option<String> {
        let model = self.model.as_ref()?;
        let mut out = String::from("(");
        for (name, sort) in &self.decls {
            let value = model
                .get(name)
                .cloned()
                .unwrap_or_else(|| APInt::new(sort_width(sort), 0));
            out.push_str(&format!(
                "\n  (define-fun {name} () {sort} {})",
                format_value(&value, sort_is_bool(sort))
            ));
        }
        out.push_str("\n)");
        Some(out)
    }

    /// Values of `terms` under the current model as `((term value) ...)`, or `None` if no model
    /// or a term is outside the supported subset.
    pub fn get_value(&self, terms: &[Term]) -> Option<String> {
        let mut parts = Vec::with_capacity(terms.len());
        for t in terms {
            let (value, is_bool) = self.eval_term(t)?;
            parts.push(format!("({t} {})", format_value(&value, is_bool)));
        }
        Some(format!("({})", parts.join(" ")))
    }

    /// Push `declare-const`/`define-fun` commands for the current environment.
    fn push_env(&self, cmds: &mut Vec<Command>) {
        for (name, sort) in &self.decls {
            cmds.push(Command::DeclareConst(Symbol(name.clone()), sort.clone()));
        }
        for def in &self.defines {
            cmds.push(Command::DefineFun(def.clone()));
        }
    }

    fn build_script(&self, extra: &[Term]) -> Script {
        let mut cmds = Vec::new();
        if let Some(l) = &self.logic {
            cmds.push(Command::SetLogic(Symbol(l.clone())));
        }
        self.push_env(&mut cmds);
        for a in self.asserts.iter().chain(extra) {
            cmds.push(Command::Assert(a.clone()));
        }
        Script(cmds)
    }

    fn build_model(
        &self,
        symbols: &[crate::smtlib::convert::SymbolInfo],
        bits: &HashMap<u32, Vec<bool>>,
    ) -> Model {
        let mut model = Model::new();
        for (&sid, value_bits) in bits {
            let info = &symbols[sid as usize];
            let width = info.width.unwrap_or(value_bits.len() as u32);
            model.insert(info.name.clone(), bits_to_apint(width, value_bits));
        }
        // Declared but unconstrained symbols default to zero.
        for (name, sort) in &self.decls {
            if !model.contains_key(name) {
                model.insert(name.clone(), APInt::new(sort_width(sort), 0));
            }
        }
        model
    }

    /// Evaluate a term by pinning every symbol to its model value and re-reading the term's bits
    /// from the bit-blaster, keeping `get-value` identical to `check-sat`.
    fn eval_term(&self, t: &Term) -> Option<(APInt, bool)> {
        let model = self.model.as_ref()?;

        // `(= t t)` lowers `t` into a graph whose root's first child is `t`.
        let probe = Term::App(
            QualIdentifier::Plain(Identifier::simple("=")),
            vec![t.clone(), t.clone()],
        );
        let mut cmds = Vec::new();
        self.push_env(&mut cmds);
        cmds.push(Command::Assert(probe));
        let lo = lower_script::<()>(&Script(cmds)).ok()?;
        let mut blasted = blast(&lo.graph, &lo.widths).ok()?;

        for (&sid, lits) in &blasted.sym_bits {
            let value = model.get(&lo.symbols[sid as usize].name)?.to_u64();
            for (i, &lit) in lits.iter().enumerate() {
                let fixed = if (value >> i) & 1 == 1 {
                    lit
                } else {
                    lit.negate()
                };
                blasted.solver.add_clause(&[fixed]);
            }
        }
        match blasted.solver.solve() {
            SatResult::Sat(_) => {}
            _ => return None,
        }

        let root = lo.graph.root()?;
        let node = lo.graph.children(root).next()?;
        let width = lo.widths[node.index()]?;
        let lits = &blasted.node_bits[node.index()];
        let value = lits.iter().enumerate().fold(0u64, |acc, (i, &l)| {
            acc | ((blasted.lit_value(l) as u64) << i)
        });
        Some((APInt::new(width, value), self.term_is_bool(t)))
    }

    fn term_is_bool(&self, t: &Term) -> bool {
        match t {
            Term::Constant(_) => false,
            Term::Ident(q) => self.ident_is_bool(q.identifier()),
            Term::App(q, args) => self.app_is_bool(q.identifier(), args),
            Term::Let(_, body) => self.term_is_bool(body),
            Term::Annotated(inner, _) => self.term_is_bool(inner),
            Term::Forall(..) | Term::Exists(..) => true,
            Term::Match(..) => false,
        }
    }

    /// `Some(is_bool)` when `name` is a defined function, else `None`.
    fn defined_is_bool(&self, name: &str) -> Option<bool> {
        self.defines
            .iter()
            .find(|d| d.name.0 == name)
            .map(|d| sort_is_bool(&d.return_sort))
    }

    fn ident_is_bool(&self, id: &Identifier) -> bool {
        let name = id.symbol.0.as_str();
        if name == "true" || name == "false" {
            return true;
        }
        if let Some((_, sort)) = self.decls.iter().find(|(n, _)| n == name) {
            return sort_is_bool(sort);
        }
        self.defined_is_bool(name).unwrap_or(false)
    }

    fn app_is_bool(&self, id: &Identifier, args: &[Term]) -> bool {
        let name = id.symbol.0.as_str();
        if matches!(name, "and" | "or" | "not" | "=>" | "=" | "distinct" | "xor")
            || name.starts_with("bvult")
            || name.starts_with("bvule")
            || name.starts_with("bvugt")
            || name.starts_with("bvuge")
            || name.starts_with("bvslt")
            || name.starts_with("bvsle")
            || name.starts_with("bvsgt")
            || name.starts_with("bvsge")
        {
            return true;
        }
        if name == "ite" {
            return args.get(1).map(|a| self.term_is_bool(a)).unwrap_or(false);
        }
        self.defined_is_bool(name).unwrap_or(false)
    }
}

/// `(width, is_bool)` for the supported sorts, `None` otherwise.
fn sort_bits(sort: &Sort) -> Option<(u32, bool)> {
    if !sort.params.is_empty() {
        return None;
    }
    match sort.id.symbol.0.as_str() {
        "Bool" if sort.id.indices.is_empty() => Some((1, true)),
        "BitVec" => match sort.id.indices.as_slice() {
            [Index::Numeral(n)] => Some((*n as u32, false)),
            _ => None,
        },
        _ => None,
    }
}

fn sort_is_bool(sort: &Sort) -> bool {
    sort_bits(sort).map(|(_, b)| b).unwrap_or(false)
}

fn sort_width(sort: &Sort) -> u32 {
    sort_bits(sort).map(|(w, _)| w).unwrap_or(1)
}

fn bits_to_apint(width: u32, bits: &[bool]) -> APInt {
    let value = bits
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, &b)| acc | ((b as u64) << i));
    APInt::new(width, value)
}

/// Format a value as an SMT-LIB constant: `true`/`false`, `#x..` for nibble widths, else `#b..`.
fn format_value(value: &APInt, is_bool: bool) -> String {
    if is_bool {
        return if value.to_u64() & 1 == 1 {
            "true"
        } else {
            "false"
        }
        .to_string();
    }
    let width = value.width();
    let raw = value.to_u64();
    if width.is_multiple_of(4) {
        format!("#x{:0>width$x}", raw, width = (width / 4) as usize)
    } else {
        format!("#b{:0>width$b}", raw, width = width as usize)
    }
}
