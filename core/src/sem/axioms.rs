//! Target-independent selection axioms: the algebraic bridges of
//! [`super::rewrites`] declared as s-expressions instead of hand-written
//! appliers. Debug builds use the [`SmtOracle`] to validate every concrete
//! width instantiation before asserting it. Release builds trust the declared
//! invariants.
//!
//! ```text
//! (axiom <name>
//!   (vars (<var> <width>)...)    ; pattern vars whose class width binds <width>
//!   (root <width|int>)           ; the matched root class's width
//!   (where (< <a> <b>) (= <a> <b>)...) ; guards over bound widths
//!   (lhs (<kind> <operand>...))  ; matched shape; undeclared atoms are wildcards,
//!                                ;   integer/`(- ..)`/`(ones ..)` operands match a
//!                                ;   `Constant` class equal to the expression
//!   (rhs <template>))            ; equivalent form unioned with the root
//! ```
//!
//! An RHS template references declared vars, `root` (the matched class),
//! nested `(<kind> ...)` nodes, and integer expressions over bound widths
//! (names, `-`, `(ones <e>)` for `2^e - 1`). A bare expression is an untyped
//! immediate ([`ConstWidth::Register`]); `(const <expr> <width>)` pins the
//! width. Node kinds are the op-sem surface's fixed-arity vocabulary
//! ([`op_kind`]).
//!
//! The proof obligation depends on what the RHS reads. Referencing only
//! `root`, the lemma quantifies over an opaque root value of the root's width
//! (`eq-via-if`: *any* 1-bit `c` equals `If(c, 1, 0)`, whatever the operand
//! widths). Referencing vars, each var of class width `n` is realized as the
//! low `n` bits of a fresh register-wide symbol that the RHS reads whole — so
//! the proof also covers the undefined upper register bits the emitted
//! instructions actually see.
//!
//! An axiom over a loop-carried `theta` is proved by induction instead: the
//! identity is discharged once with every `theta` read as its `init` port (the
//! base case) and once as its `next` port (the step). An RHS that nests a
//! `theta` under a `theta` unrolls the loop and never saturates, so it is
//! rejected at parse.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, OnceLock};

use smallvec::{SmallVec, smallvec};
use tir_adt::APInt;
use tir_relational::{Atom, Cmp, ColumnId, Expr, Guard, HeadOp, LabelFill, Plan, Query, Source};
use tir_symbolic::egraph::Id;

use crate::builtin::{FloatType, IntegerType};
use crate::sem::{
    EquivalenceOracle, SemExpr, SemGraph, SmtOracle, SymKind, SymPayload, Value, con, execute, op,
    op_kind, parse, sym,
};
use crate::{Context, TypeId, graph::NodeId};

use super::egraph::{is_comparison, type_width};
use super::node::{SemNode, field, template_node};

/// Whether the SMT obligations behind the semantic invariants and the guarded
/// relaxations are discharged. They validate the target description, they are not
/// inputs to selection, so running hundreds of SAT queries on every compile buys
/// nothing: `TIR_VERIFY_AXIOMS` turns them on for the target-definition test runs
/// that actually need the check.
pub(crate) fn verify_axioms() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TIR_VERIFY_AXIOMS").is_some())
}

/// A width position in `vars`/`root`: a literal to check or a name to bind.
#[derive(Clone)]
enum WidthBinding {
    Lit(u64),
    Name(usize),
}

impl WidthBinding {
    fn value(&self, widths: &[u64]) -> u64 {
        match self {
            WidthBinding::Lit(l) => *l,
            WidthBinding::Name(i) => widths[*i],
        }
    }
}

/// An integer expression over bound widths.
#[derive(Clone)]
enum WidthExpr {
    Lit(u64),
    Name(usize),
    Sub(Box<WidthExpr>, Box<WidthExpr>),
    /// `(ones e)`: the all-ones value of `e` bits, `2^e - 1`.
    Ones(Box<WidthExpr>),
}

impl WidthExpr {
    fn eval(&self, widths: &[u64]) -> Option<u64> {
        match self {
            WidthExpr::Lit(v) => Some(*v),
            WidthExpr::Name(i) => Some(widths[*i]),
            WidthExpr::Sub(a, b) => a.eval(widths)?.checked_sub(b.eval(widths)?),
            WidthExpr::Ones(e) => match e.eval(widths)? {
                64 => Some(u64::MAX),
                v if v < 64 => Some((1u64 << v) - 1),
                _ => None,
            },
        }
    }
}

enum Guard_ {
    Lt(WidthExpr, WidthExpr),
    Eq(WidthExpr, WidthExpr),
}

/// A predicate over a matched constant's *value* (not its width): whether the
/// bound constant `var` fits a signed `bits`-bit immediate. A `materialize`
/// decomposition axiom guards on the negation so it fires only on constants too
/// wide for the target's immediate, bounding the saturation descent.
struct ValueGuard {
    var: usize,
    bits: u32,
    unsigned: bool,
    negated: bool,
}

/// `v`'s low `width` bits read as a two's-complement signed value.
fn sign_extend(v: u64, width: u32) -> i64 {
    let shift = 64 - width.min(64);
    ((v << shift) as i64) >> shift
}

/// Whether `v`, read as two's-complement at its own width, is within the signed
/// `bits`-bit range `[-2^(bits-1), 2^(bits-1))`.
fn fits_signed(v: &APInt, bits: u32) -> bool {
    let signed = sign_extend(v.to_u64(), v.width());
    let bound = 1i128 << (bits - 1);
    (-bound..bound).contains(&i128::from(signed))
}

fn fits_unsigned(v: &APInt, bits: u32) -> bool {
    bits == 64 || v.to_u64() < (1u64 << bits)
}

/// The width a template constant materializes at.
#[derive(Clone, Copy)]
enum ConstWidth {
    /// A bare expression: an untyped immediate — proved at the register width,
    /// instantiated at the e-graph's 64-bit introduced-constant convention.
    Register,
    /// An explicit `(const <expr> <width>)`.
    Fixed(u32),
}

/// One template tree shared by both sides; which leaves are legal where is
/// enforced at parse by [`Side`].
enum AxNode {
    /// An LHS capture hole — a declared var (`Some(index)`, also referencable
    /// from the RHS), or a width name / wildcard (`None`). In proofs a
    /// width-name hole realizes as the constant carrying that width.
    Hole(String, Option<usize>),
    /// The matched root class (RHS only).
    Root,
    /// An integer expression materialized as a constant (RHS only).
    Const(WidthExpr, ConstWidth),
    /// An LHS constant operand: matches only a class holding a `Constant` equal
    /// to the expression, evaluated after widths resolve.
    ConstMatch(WidthExpr),
    Node(SymKind, Vec<AxNode>),
    /// A materialize-axiom RHS node kept structural (an emitted instruction),
    /// wrapping a [`AxNode::Node`]; unmarked RHS nodes fold to constants. Purely
    /// an instantiation directive — semantically transparent to the proof.
    Keep(Box<AxNode>),
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Lhs,
    Rhs,
}

enum ProofObligation {
    /// Discharged by the [`SmtOracle`] over the realized LHS and RHS.
    Equivalence,
    /// An identity over a loop-carried value, discharged by induction over the
    /// iterations as a pair of [`ProofObligation::Equivalence`] obligations: the
    /// property holds of the `init` port (base case) and of the `next` port
    /// (step). The step needs no explicit hypothesis: an axiom's operands are
    /// opaque holes, so the term realized at iteration `t + 1` is `next` itself,
    /// with no occurrence of the iteration-`t` value for a hypothesis to
    /// constrain.
    ThetaInvariant,
}

/// Which loop-carried port a `theta` node realizes as in one proof instance.
#[derive(Clone, Copy)]
enum ThetaPort {
    Init,
    Next,
}

/// The fixed context of one realized proof instance.
struct Realization<'a> {
    widths: &'a [u64],
    register_width: u32,
    side: Side,
    root_sym: Option<NodeId>,
    /// `None` for an obligation whose realization holds no `theta`.
    theta_port: Option<ThetaPort>,
}

pub struct Axiom {
    pub(crate) name: String,
    /// Width names in declaration order; a resolved `Vec<u64>` in this order is
    /// the proof-memo key.
    width_names: Vec<String>,
    /// Declared pattern vars (name, class-width binding); a var's `SymbolId` in
    /// proof graphs is its index here.
    vars: Vec<(String, WidthBinding)>,
    /// Indices into `vars` of operands that must match a `Constant` class — so a
    /// rule fires only on the immediate form. The proof treats them as ordinary
    /// symbols (the identity holds for any value); the applier checks constness.
    const_vars: Vec<usize>,
    root_width: WidthBinding,
    guards: Vec<Guard_>,
    /// Value predicates gating on a matched constant's magnitude (see
    /// [`ValueGuard`]); only meaningful for `materialize` axioms.
    value_guards: Vec<ValueGuard>,
    lhs: AxNode,
    rhs: AxNode,
    /// The RHS references the matched root itself (excludes var references).
    uses_root: bool,
    obligation: ProofObligation,
    /// Declared `(phase post-saturation)`: applied once after the iterative
    /// fixpoint instead of participating in it.
    post_saturation: bool,
    /// A materialize axiom: its LHS root is a bare `consts` var, so it matches
    /// every constant class, and its RHS structure is unioned *with* the folded
    /// constant instead of collapsing to it (keeps the shift/add tiling live).
    materialize: bool,
}

fn atom(e: &SemExpr) -> Option<&str> {
    match e {
        SemExpr::Atom(a) => Some(a),
        SemExpr::List(_) => None,
    }
}

/// Split an axiom file (`;` line comments, one `(axiom ...)` form per
/// balanced-paren span) into its forms.
pub(crate) fn axiom_forms(file: &str) -> Vec<String> {
    let text: String = file
        .lines()
        .filter(|line| !line.trim_start().starts_with(';'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut forms = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    for (i, c) in text.char_indices() {
        match c {
            '(' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(s) = start.take()
                {
                    forms.push(text[s..=i].to_string());
                }
            }
            _ => {}
        }
    }
    forms
}

pub(crate) fn parse_axiom(text: &str) -> Result<Axiom, String> {
    let parsed = parse(text).ok_or("malformed s-expression")?;
    let SemExpr::List(items) = &parsed else {
        return Err("expected a top-level list".into());
    };
    let [head, name, sections @ ..] = items.as_slice() else {
        return Err("expected (axiom <name> <section>...)".into());
    };
    if atom(head) != Some("axiom") {
        return Err("expected the `axiom` keyword".into());
    }
    let name = atom(name).ok_or("axiom name must be an atom")?.to_string();

    let mut width_names: Vec<String> = Vec::new();
    let binding = |w: &str, width_names: &mut Vec<String>| {
        if let Ok(v) = w.parse::<u64>() {
            WidthBinding::Lit(v)
        } else {
            WidthBinding::Name(intern(width_names, w))
        }
    };

    let mut vars: Vec<(String, WidthBinding)> = Vec::new();
    let mut const_vars: Vec<usize> = Vec::new();
    let mut root_width = None;
    let mut guards = Vec::new();
    let mut value_guards = Vec::new();
    let mut lhs_expr = None;
    let mut rhs_expr = None;
    let mut post_saturation = false;

    for section in sections {
        let SemExpr::List(parts) = section else {
            return Err("axiom sections must be lists".into());
        };
        let [SemExpr::Atom(section_head), rest @ ..] = parts.as_slice() else {
            return Err("axiom section must start with a keyword".into());
        };
        match section_head.as_str() {
            "vars" | "consts" => {
                for entry in rest {
                    let SemExpr::List(pair) = entry else {
                        return Err("var entries must be (<var> <width>)".into());
                    };
                    let [SemExpr::Atom(v), SemExpr::Atom(w)] = pair.as_slice() else {
                        return Err("var entries must be (<var> <width>)".into());
                    };
                    let w = binding(w, &mut width_names);
                    if section_head == "consts" {
                        const_vars.push(vars.len());
                    }
                    vars.push((v.clone(), w));
                }
            }
            "root" => {
                let [SemExpr::Atom(w)] = rest else {
                    return Err("root section must be (root <width>)".into());
                };
                root_width = Some(binding(w, &mut width_names));
            }
            "phase" => {
                let [SemExpr::Atom(phase)] = rest else {
                    return Err("phase section must be (phase post-saturation)".into());
                };
                if phase != "post-saturation" {
                    return Err(format!("unknown phase `{phase}`"));
                }
                post_saturation = true;
            }
            "where" => {
                for g in rest {
                    let SemExpr::List(parts) = g else {
                        return Err("guards must be (< <a> <b>) or ([u]fits <var> <bits>)".into());
                    };
                    // `(not ...)` unwraps to its inner guard; only `[u]fits` may
                    // be negated, which the match below enforces.
                    let (parts, negated) = match parts.as_slice() {
                        [SemExpr::Atom(kw), SemExpr::List(inner)] if kw == "not" => {
                            (inner.as_slice(), true)
                        }
                        parts => (parts, false),
                    };
                    match parts {
                        [SemExpr::Atom(kw), SemExpr::Atom(var), SemExpr::Atom(bits)]
                            if kw == "fits" || kw == "ufits" =>
                        {
                            value_guards.push(parse_value_guard(
                                var,
                                bits,
                                kw == "ufits",
                                negated,
                                &vars,
                            )?);
                        }
                        [SemExpr::Atom(cmp), a, b] if !negated => {
                            let a = parse_width_expr(a, &width_names)?;
                            let b = parse_width_expr(b, &width_names)?;
                            guards.push(match cmp.as_str() {
                                "<" => Guard_::Lt(a, b),
                                "=" => Guard_::Eq(a, b),
                                other => return Err(format!("unknown guard `{other}`")),
                            });
                        }
                        _ => {
                            return Err("guards must be (< <a> <b>), ([u]fits <var> <bits>), \
                                 or (not ([u]fits <var> <bits>))"
                                .into());
                        }
                    }
                }
            }
            "lhs" => {
                let [e] = rest else {
                    return Err("lhs section must hold one pattern".into());
                };
                lhs_expr = Some(e);
            }
            "rhs" => {
                let [e] = rest else {
                    return Err("rhs section must hold one template".into());
                };
                rhs_expr = Some(e);
            }
            other => return Err(format!("unknown section `{other}`")),
        }
    }

    let lhs = parse_node(
        lhs_expr.ok_or("missing lhs section")?,
        Side::Lhs,
        &vars,
        &width_names,
    )?;
    // A bare `consts` var as the LHS root marks a materialize axiom: it matches
    // every constant class so a wide constant can be decomposed in place.
    let materialize = matches!(&lhs, AxNode::Hole(_, Some(i)) if const_vars.contains(i));
    if !materialize && !matches!(lhs, AxNode::Node(..)) {
        return Err("lhs must be a pattern node, not a bare atom".into());
    }
    let root_width = root_width.ok_or("missing root section")?;
    let rhs = parse_node(
        rhs_expr.ok_or("missing rhs section")?,
        Side::Rhs,
        &vars,
        &width_names,
    )?;

    let mut uses_root = false;
    let mut used_vars = HashSet::new();
    references(&rhs, &mut uses_root, &mut used_vars);
    if uses_root && !used_vars.is_empty() {
        return Err("rhs may reference `root` or vars, not both".into());
    }
    let mut lhs_holes = Vec::new();
    holes_of(&lhs, &mut lhs_holes);
    for &i in &used_vars {
        if !lhs_holes.iter().any(|(_, v)| *v == Some(i)) {
            return Err(format!("rhs var `{}` never bound by the lhs", vars[i].0));
        }
    }
    if !used_vars.is_empty() {
        // The proof realizes the whole LHS, so every hole needs a known width.
        for (name, var) in &lhs_holes {
            if var.is_none() && !width_names.contains(name) {
                return Err(format!("lhs atom `{name}` must be declared to be provable"));
            }
        }
    }
    let obligation = if contains_kind(&lhs, SymKind::Theta) || contains_kind(&rhs, SymKind::Theta) {
        // An RHS theta nested under a theta re-grows the shape it matched, so
        // saturation would never reach a fixpoint. Unrolling is a structural
        // transform, not a rewrite rule.
        if grows_theta_under_theta(&rhs) {
            return Err(format!("axiom `{name}` unrolls a theta under itself"));
        }
        ProofObligation::ThetaInvariant
    } else {
        ProofObligation::Equivalence
    };

    Ok(Axiom {
        name,
        width_names,
        vars,
        const_vars,
        root_width,
        guards,
        value_guards,
        lhs,
        rhs,
        uses_root,
        obligation,
        post_saturation,
        materialize,
    })
}

fn contains_kind(node: &AxNode, expected: SymKind) -> bool {
    match node {
        AxNode::Node(kind, children) => {
            *kind == expected || children.iter().any(|child| contains_kind(child, expected))
        }
        AxNode::Keep(inner) => contains_kind(inner, expected),
        _ => false,
    }
}

/// Whether a `theta` in `node` has another `theta` below it.
fn grows_theta_under_theta(node: &AxNode) -> bool {
    match node {
        AxNode::Node(SymKind::Theta, children) => children
            .iter()
            .any(|child| contains_kind(child, SymKind::Theta)),
        AxNode::Node(_, children) => children.iter().any(grows_theta_under_theta),
        AxNode::Keep(inner) => grows_theta_under_theta(inner),
        _ => false,
    }
}

fn parse_value_guard(
    var: &str,
    bits: &str,
    unsigned: bool,
    negated: bool,
    vars: &[(String, WidthBinding)],
) -> Result<ValueGuard, String> {
    let var = vars
        .iter()
        .position(|(v, _)| v == var)
        .ok_or_else(|| format!("fits var `{var}` is not declared"))?;
    let bits = bits
        .parse::<u32>()
        .map_err(|_| "fits bit count must be an integer".to_string())?;
    if !(1..=64).contains(&bits) {
        return Err("fits bit count must be in 1..=64".to_string());
    }
    Ok(ValueGuard {
        var,
        bits,
        unsigned,
        negated,
    })
}

fn intern(names: &mut Vec<String>, name: &str) -> usize {
    names.iter().position(|n| n == name).unwrap_or_else(|| {
        names.push(name.to_string());
        names.len() - 1
    })
}

fn parse_width_expr(e: &SemExpr, width_names: &[String]) -> Result<WidthExpr, String> {
    match e {
        SemExpr::Atom(a) => {
            if let Ok(v) = a.parse::<u64>() {
                Ok(WidthExpr::Lit(v))
            } else if let Some(i) = width_names.iter().position(|n| n == a) {
                Ok(WidthExpr::Name(i))
            } else {
                Err(format!("unknown width `{a}`"))
            }
        }
        SemExpr::List(parts) => match parts.as_slice() {
            [SemExpr::Atom(minus), a, b] if minus == "-" => Ok(WidthExpr::Sub(
                Box::new(parse_width_expr(a, width_names)?),
                Box::new(parse_width_expr(b, width_names)?),
            )),
            [SemExpr::Atom(ones), e] if ones == "ones" => {
                Ok(WidthExpr::Ones(Box::new(parse_width_expr(e, width_names)?)))
            }
            _ => Err("width expressions are atoms, (- <a> <b>), or (ones <e>)".into()),
        },
    }
}

/// Parse one template tree; atoms resolve to holes on the LHS and to var
/// references / constants on the RHS, node heads through the shared op-sem
/// vocabulary ([`op_kind`]).
fn parse_node(
    e: &SemExpr,
    side: Side,
    vars: &[(String, WidthBinding)],
    width_names: &[String],
) -> Result<AxNode, String> {
    match e {
        SemExpr::Atom(a) => {
            if a == "root" {
                return match side {
                    Side::Lhs => Err("`root` cannot appear in the lhs".into()),
                    Side::Rhs => Ok(AxNode::Root),
                };
            }
            let var = vars.iter().position(|(v, _)| v == a);
            match side {
                Side::Lhs if a.parse::<u64>().is_ok() => {
                    Ok(AxNode::ConstMatch(parse_width_expr(e, width_names)?))
                }
                Side::Lhs => Ok(AxNode::Hole(a.clone(), var)),
                Side::Rhs => match var {
                    Some(i) => Ok(AxNode::Hole(a.clone(), Some(i))),
                    None => Ok(AxNode::Const(
                        parse_width_expr(e, width_names)?,
                        ConstWidth::Register,
                    )),
                },
            }
        }
        SemExpr::List(parts) => {
            let [SemExpr::Atom(head), rest @ ..] = parts.as_slice() else {
                return Err("template nodes must be (<kind> <operand>...)".into());
            };
            match head.as_str() {
                "-" | "ones" if side == Side::Lhs => {
                    Ok(AxNode::ConstMatch(parse_width_expr(e, width_names)?))
                }
                "-" | "ones" if side == Side::Rhs => Ok(AxNode::Const(
                    parse_width_expr(e, width_names)?,
                    ConstWidth::Register,
                )),
                "keep" if side == Side::Rhs => {
                    let [inner] = rest else {
                        return Err("keep form is (keep <node>)".into());
                    };
                    let inner = parse_node(inner, side, vars, width_names)?;
                    if !matches!(inner, AxNode::Node(..)) {
                        return Err("keep wraps a node, not a bare atom".into());
                    }
                    Ok(AxNode::Keep(Box::new(inner)))
                }
                "const" if side == Side::Rhs => {
                    let [value, SemExpr::Atom(width)] = rest else {
                        return Err("const form is (const <expr> <width>)".into());
                    };
                    let width: u32 = width
                        .parse()
                        .map_err(|_| "const width must be an integer")?;
                    Ok(AxNode::Const(
                        parse_width_expr(value, width_names)?,
                        ConstWidth::Fixed(width),
                    ))
                }
                _ => {
                    let kind = op_kind(head).ok_or_else(|| format!("unknown kind `{head}`"))?;
                    if kind.arity() != rest.len() {
                        return Err(format!("`{head}` expects {} operands", kind.arity()));
                    }
                    let children = rest
                        .iter()
                        .map(|c| parse_node(c, side, vars, width_names))
                        .collect::<Result<_, _>>()?;
                    Ok(AxNode::Node(kind, children))
                }
            }
        }
    }
}

/// What the RHS reads: the matched root and/or declared vars.
fn references(node: &AxNode, uses_root: &mut bool, vars: &mut HashSet<usize>) {
    match node {
        AxNode::Root => *uses_root = true,
        AxNode::Hole(_, Some(i)) => {
            vars.insert(*i);
        }
        AxNode::Hole(_, None) | AxNode::Const(..) | AxNode::ConstMatch(..) => {}
        AxNode::Node(_, children) => {
            for c in children {
                references(c, uses_root, vars);
            }
        }
        AxNode::Keep(inner) => references(inner, uses_root, vars),
    }
}

fn holes_of(node: &AxNode, out: &mut Vec<(String, Option<usize>)>) {
    match node {
        AxNode::Hole(name, var) => out.push((name.clone(), *var)),
        AxNode::Node(_, children) => {
            for c in children {
                holes_of(c, out);
            }
        }
        AxNode::Keep(inner) => holes_of(inner, out),
        AxNode::Root | AxNode::Const(..) | AxNode::ConstMatch(..) => {}
    }
}

impl Axiom {
    pub(crate) fn materializes_constants(&self) -> bool {
        self.materialize
    }

    /// Compile into a rule: the left-hand side as atoms, the declared widths and
    /// value predicates as guards, the right-hand side as a head. Debug builds
    /// prove each width instantiation before asserting the invariant, through
    /// the [`call::VERIFY`] guard.
    pub(crate) fn compile(
        &self,
        index: usize,
        folds: &mut Vec<SymKind>,
        assume: Folding,
    ) -> Option<(tir_relational::Rule<SemNode>, bool)> {
        let mut low = Lowering::new(self, folds, assume);
        low.left(self);
        low.widths(self);
        low.predicates(self, index);
        let head = low.right(self)?;
        let assumed = low.assumed;
        Some((
            tir_relational::Rule {
                name: format!("axiom-{}", self.name),
                plan: Plan::compile(Query {
                    vars: low.slots.vars,
                    scalars: low.slots.scalars,
                    root: 0,
                    atoms: low.atoms,
                    guards: low.guards,
                    nots: Vec::new(),
                }),
                head,
                post_saturation: self.post_saturation,
            },
            assumed,
        ))
    }

    /// Discharge this instantiation's proof obligation, panicking on an unsound
    /// axiom. Results are memoized process-wide: the same axiom recurs at the same
    /// widths across every block, function and freshly built pass.
    fn verify(&self, widths: &[u64]) {
        type ProofCache = Mutex<HashMap<(String, Vec<u64>), bool>>;
        static PROOFS: LazyLock<ProofCache> = LazyLock::new(Mutex::default);
        let key = (self.name.clone(), widths.to_vec());
        let cached = PROOFS.lock().unwrap().get(&key).copied();
        let proven = cached.unwrap_or_else(|| {
            let proven = self.prove(widths);
            PROOFS.lock().unwrap().insert(key, proven);
            proven
        });
        assert!(
            proven,
            "invalid semantic invariant `{}` for widths {widths:?}",
            self.name
        );
    }

    /// Prove one width instantiation with the [`SmtOracle`]; `widths` follows
    /// the width names' declaration order (`vars` first, then `root`).
    pub(crate) fn prove(&self, widths: &[u64]) -> bool {
        match self.obligation {
            ProofObligation::Equivalence => self.prove_instance(widths, None),
            ProofObligation::ThetaInvariant => {
                self.prove_instance(widths, Some(ThetaPort::Init))
                    && self.prove_instance(widths, Some(ThetaPort::Next))
            }
        }
    }

    /// One equivalence instance of this axiom's obligation, with every `theta`
    /// realized as `theta_port`.
    fn prove_instance(&self, widths: &[u64], theta_port: Option<ThetaPort>) -> bool {
        let register_width = self.register_width(widths);
        let mut lhs = SemGraph::new();
        let mut rhs = SemGraph::new();
        let realization = |side, root_sym| Realization {
            widths,
            register_width,
            side,
            root_sym,
            theta_port,
        };
        let (built, symbol_count) = if self.uses_root {
            // Lemma over an opaque root value: lhs is a bare symbol, `root` in
            // the rhs is the same symbol.
            let root_sym = sym(&mut rhs, 0);
            sym(&mut lhs, 0);
            let built = self
                .realize(&self.rhs, &mut rhs, &realization(Side::Rhs, Some(root_sym)))
                .is_some();
            (built, 1)
        } else {
            // Register realization: each var is the low bits of a full-width
            // register symbol in the lhs; the rhs reads the register whole.
            let built = self
                .realize(&self.lhs, &mut lhs, &realization(Side::Lhs, None))
                .is_some()
                && self
                    .realize(&self.rhs, &mut rhs, &realization(Side::Rhs, None))
                    .is_some();
            (built, self.vars.len())
        };
        if !built {
            return false;
        }
        let symbol_widths = vec![register_width; symbol_count];
        SmtOracle.refines(&lhs, &rhs, &symbol_widths)
    }

    fn register_width(&self, widths: &[u64]) -> u32 {
        self.vars
            .iter()
            .map(|(_, binding)| binding.value(widths))
            .chain([self.root_width.value(widths)])
            .max()
            .unwrap_or_else(|| self.root_width.value(widths)) as u32
    }

    /// Build one side of the proof. A declared var is a register-wide symbol —
    /// narrowed to its class width through an extract on the LHS, read whole on
    /// the RHS; a width-name hole is the constant carrying that width. A `theta`
    /// realizes as the port this instance proves.
    fn realize(&self, node: &AxNode, g: &mut SemGraph, r: &Realization) -> Option<NodeId> {
        let widths = r.widths;
        match node {
            AxNode::Root => r.root_sym,
            AxNode::Hole(_, Some(i)) => {
                let s = sym(g, *i as u32);
                let class_w = self.vars[*i].1.value(widths) as u32;
                if r.side == Side::Rhs || class_w == r.register_width {
                    Some(s)
                } else if class_w < r.register_width {
                    let hi = con(g, (class_w - 1) as u64, 16);
                    let lo = con(g, 0, 16);
                    Some(op(g, SymKind::Extract, &[s, hi, lo]))
                } else {
                    None
                }
            }
            AxNode::Hole(name, None) => {
                // A width name: the constant operand carrying that width.
                let i = self.width_names.iter().position(|n| n == name)?;
                Some(con(g, widths[i], 16))
            }
            AxNode::Const(e, width) => {
                let width = match width {
                    ConstWidth::Register => r.register_width,
                    ConstWidth::Fixed(w) => *w,
                };
                Some(con(g, e.eval(widths)?, width))
            }
            AxNode::ConstMatch(e) => Some(con(g, e.eval(widths)?, r.register_width)),
            AxNode::Node(SymKind::Theta, children) => {
                let port = match r.theta_port? {
                    ThetaPort::Init => 0,
                    ThetaPort::Next => 1,
                };
                self.realize(&children[port], g, r)
            }
            AxNode::Node(kind, children) => {
                let children = children
                    .iter()
                    .map(|c| self.realize(c, g, r))
                    .collect::<Option<Vec<_>>>()?;
                Some(op(g, *kind, &children))
            }
            AxNode::Keep(inner) => self.realize(inner, g, r),
        }
    }
}

/// Execute a pure op over `(value, width)` constant operands via a throwaway
/// [`SemGraph`]; `None` when the result is not an integer.
fn execute_fold(kind: SymKind, operands: &[(u64, u32)]) -> Option<APInt> {
    let mut g = SemGraph::new();
    let ids: Vec<NodeId> = operands.iter().map(|&(v, w)| con(&mut g, v, w)).collect();
    op(&mut g, kind, &ids);
    match execute(&g, &[]) {
        Value::Int(result) => Some(result),
        _ => None,
    }
}

/// Evaluate a pure op over integer operands at bit-width `width`: operands are
/// truncated to `width`, the op executed there, and the result returned
/// sign-extended to i64 — the convention program constants are interned with,
/// so a recursion constant compares and binds like an original one.
fn fold_values_at(kind: SymKind, values: &[i64], width: u32) -> Option<i64> {
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    let operands: Vec<(u64, u32)> = values.iter().map(|&v| ((v as u64) & mask, width)).collect();
    let result = execute_fold(kind, &operands)?;
    Some(sign_extend(result.to_u64(), width))
}

/// Where the next class variable and the next scalar of a lowered axiom come
/// from.
#[derive(Default)]
struct Slots {
    vars: u32,
    scalars: u32,
}

impl Slots {
    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    fn scalar(&mut self) -> u32 {
        self.scalars += 1;
        self.scalars - 1
    }
}

/// The host functions an axiom's guards and head call. Every one reads the
/// `Context` — a type's width, a type for a width, the value of a pure op over
/// constants — and none reads the e-graph, which is what keeps a match's
/// existence a function of its atoms.
pub(crate) mod call {
    pub(crate) const WIDTH_OF: u32 = 0;
    pub(crate) const INT_TYPE: u32 = 1;
    pub(crate) const FLOAT_TYPE: u32 = 2;
    pub(crate) const EXTRACT_TYPE: u32 = 3;
    pub(crate) const MAX: u32 = 4;
    pub(crate) const FITS: u32 = 5;
    /// One per pure op a head folds over constants, so the id names the kind.
    pub(crate) const FOLD: u32 = 8;
    /// One per pure op the folding rules execute, by the same table as
    /// [`FOLD`], but over `(value, width)` operands rather than a common width.
    pub(crate) const EXECUTE: u32 = 512;
    /// One per axiom, so a proof obligation names the axiom it belongs to.
    pub(crate) const VERIFY: u32 = 1024;
}

/// The `Context` reads an axiom's guards and head make: a type's width, the type
/// for a width, the value of a pure op over constants, and — under
/// `TIR_VERIFY_AXIOMS` — the proof obligation itself. None of them reads the
/// e-graph, which is what keeps a match's existence a function of its atoms.
pub struct Interpretation<'a> {
    context: &'a Context,
    axioms: &'a [Axiom],
    folds: &'a [SymKind],
}

impl<'a> Interpretation<'a> {
    pub fn new(context: &'a Context, axioms: &'a [Axiom], folds: &'a [SymKind]) -> Self {
        Self {
            context,
            axioms,
            folds,
        }
    }
}

impl tir_relational::Externs<SemNode> for Interpretation<'_> {
    fn call(&self, id: u32, _terms: &[&SemNode], args: &[u64], out: &mut [u64]) -> bool {
        match id {
            call::WIDTH_OF => match type_width(self.context, TypeId::from_number(args[0] as u32)) {
                Some(width) => {
                    out[0] = width as u64;
                    true
                }
                None => false,
            },
            call::INT_TYPE => {
                let width = args[0] as u32;
                if !(1..=64).contains(&width) {
                    return false;
                }
                out[0] = IntegerType::new(self.context, width).number() as u64;
                true
            }
            call::FLOAT_TYPE => {
                out[0] =
                    FloatType::new(self.context, args[0] as u32, args[1] as u32).number() as u64;
                true
            }
            call::EXTRACT_TYPE => {
                let (hi, lo, known, fallback) =
                    (args[0] as u32, args[1] as u32, args[2] != 0, args[3] as u32);
                let width = if known && hi >= lo {
                    hi - lo + 1
                } else {
                    fallback
                };
                if !(1..=64).contains(&width) {
                    return false;
                }
                out[0] = IntegerType::new(self.context, width).number() as u64;
                true
            }
            call::MAX => {
                out[0] = args.iter().copied().max().unwrap_or(0);
                true
            }
            call::FITS => {
                let value = APInt::new(args[1] as u32, args[0]);
                let (bits, unsigned, negated) = (args[2] as u32, args[3] != 0, args[4] != 0);
                let fits = if unsigned {
                    fits_unsigned(&value, bits)
                } else {
                    fits_signed(&value, bits)
                };
                fits == !negated
            }
            proof if proof >= call::VERIFY => {
                if verify_axioms() {
                    self.axioms[(proof - call::VERIFY) as usize].verify(args);
                }
                true
            }
            execute if execute >= call::EXECUTE => {
                let kind = self.folds[(execute - call::EXECUTE) as usize];
                let operands: Vec<(u64, u32)> = args
                    .chunks(2)
                    .map(|pair| (pair[0], pair[1] as u32))
                    .collect();
                match execute_fold(kind, &operands) {
                    Some(value) => {
                        out[0] = value.to_u64();
                        out[1] = value.width() as u64;
                        true
                    }
                    None => false,
                }
            }
            fold => {
                let kind = self.folds[(fold - call::FOLD) as usize];
                let width = args[0] as u32;
                let values: Vec<i64> = args[1..].iter().map(|&v| v as i64).collect();
                match fold_values_at(kind, &values, width) {
                    Some(value) => {
                        out[0] = value as u64;
                        true
                    }
                    None => false,
                }
            }
        }
    }
}

/// A right-hand side node as the head builds it: the class variable it lands in,
/// and — where the axiom's own text determines it — the scalar holding its value.
#[derive(Clone, Copy)]
struct Built {
    var: u32,
    /// Scalars holding the value and width of the constant this node is, where
    /// the axiom's own text says it is one.
    value: Option<(u32, u32)>,
    /// Whether the node is an operand the graph may turn out to have made a
    /// constant, so a fold may ask for it to be one.
    assumable: bool,
}

/// One axiom's left-hand side as atoms and guards, and the numbering the head
/// reads back.
struct Lowering<'a> {
    slots: Slots,
    atoms: Vec<Atom<SemNode>>,
    guards: Vec<Guard>,
    /// Class variable per capture name, so a name written twice is one variable.
    holes: HashMap<String, u32>,
    /// Class variable per declared var, in declaration order.
    declared: Vec<u32>,
    /// Scalars holding each declared const var's value and width.
    const_values: HashMap<usize, (u32, u32)>,
    /// Scalar per width name.
    widths: Vec<u32>,
    /// Scalar holding the register width, once something asks for it.
    register: Option<u32>,
    /// The pure ops any head folds over constants, shared across the theory so
    /// one extern id names one kind.
    folds: &'a mut Vec<SymKind>,
    /// Whether an operand the axiom does not declare constant may be assumed to
    /// be one, at the cost of an atom that requires it.
    assume: Folding,
    /// Whether that assumption was taken, so a caller can tell the two readings
    /// apart.
    assumed: bool,
    /// Value and width scalars per class already assumed constant.
    assumptions: HashMap<u32, (u32, u32)>,
}

/// Whether a head may fold an operand the axiom's own text does not say is a
/// constant.
///
/// An axiom's right-hand side is one shape; what its operands turn out to be is
/// the graph's business. `sext(x, w)` becomes a pair of shifts, and where `x` is
/// a constant those shifts *are* a constant — the one an immediate consumer
/// binds. Both readings are true, so both are rules: one requires the operand to
/// be known constant and folds, one does not and does not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Folding {
    Never,
    Assume,
}

impl<'a> Lowering<'a> {
    fn new(axiom: &Axiom, folds: &'a mut Vec<SymKind>, assume: Folding) -> Self {
        let mut slots = Slots::default();
        // Variable zero is the matched root, as every plan's is.
        slots.var();
        Self {
            slots,
            atoms: Vec::new(),
            guards: Vec::new(),
            holes: HashMap::new(),
            declared: vec![u32::MAX; axiom.vars.len()],
            const_values: HashMap::new(),
            widths: Vec::new(),
            register: None,
            folds,
            assume,
            assumed: false,
            assumptions: HashMap::new(),
        }
    }

    /// The left-hand side, root atom first and then a depth-first walk, which is
    /// the order the goal stack popped its nodes in and so the order the ids a
    /// saturation mints still depend on.
    fn left(&mut self, axiom: &Axiom) {
        match &axiom.lhs {
            // A materialize axiom's left-hand side is a bare constant var: it
            // matches every constant class so a wide one can be decomposed where
            // it stands.
            AxNode::Hole(name, var) => {
                self.holes.insert(name.clone(), 0);
                if let Some(index) = var {
                    self.declared[*index] = 0;
                }
            }
            node => self.node(node, 0),
        }
    }

    fn node(&mut self, node: &AxNode, class: u32) {
        let AxNode::Node(kind, children) = node else {
            unreachable!("only a template node is matched")
        };
        let args: Vec<u32> = children.iter().map(|child| self.child(child)).collect();
        let mut template = template_node(*kind, None, None);
        template.children = args.iter().map(|&var| Id::from_raw(var)).collect();
        self.atoms.push(Atom::Node {
            template,
            args: args.iter().copied().collect(),
            class,
            row: None,
        });
        for (child, &var) in children.iter().zip(&args) {
            match child {
                AxNode::Node(..) => self.node(child, var),
                AxNode::ConstMatch(expr) => self.const_match(var, expr),
                _ => {}
            }
        }
    }

    /// The variable a child binds, reusing the one a capture name already has.
    fn child(&mut self, node: &AxNode) -> u32 {
        match node {
            AxNode::Hole(name, var) => {
                if let Some(&existing) = self.holes.get(name) {
                    return existing;
                }
                let fresh = self.slots.var();
                self.holes.insert(name.clone(), fresh);
                if let Some(index) = var {
                    self.declared[*index] = fresh;
                }
                fresh
            }
            _ => self.slots.var(),
        }
    }

    /// An operand that must be the constant `expr` evaluates to.
    fn const_match(&mut self, class: u32, expr: &WidthExpr) {
        let (value, _) = self.constant(class);
        let want = self.slots.scalar();
        self.guards.push(Guard::Let {
            out: want,
            value: width_expr(expr, &self.widths),
        });
        self.guards
            .push(Guard::Cmp(Cmp::Eq, Expr::Scalar(value), Expr::Scalar(want)));
    }

    /// Bind `class`'s constant, its value and its width.
    fn constant(&mut self, class: u32) -> (u32, u32) {
        let label = self.slots.scalar();
        let value = self.slots.scalar();
        let width = self.slots.scalar();
        self.atoms.push(Atom::Fact {
            column: ColumnId::Const,
            key: class,
            value: label,
        });
        self.guards.push(Guard::Read {
            term: Source::Label(label),
            field: field::INT_VALUE,
            out: value,
        });
        self.guards.push(Guard::Read {
            term: Source::Label(label),
            field: field::INT_WIDTH,
            out: width,
        });
        (value, width)
    }

    /// Resolve every width name from the matched classes, root first and then the
    /// declared vars in order — a name met twice is a comparison, not a rebinding.
    fn widths(&mut self, axiom: &Axiom) {
        self.widths = vec![u32::MAX; axiom.width_names.len()];
        let root = self.width_of(0);
        self.bind(&axiom.root_width, root);
        for (index, (_, binding)) in axiom.vars.iter().enumerate() {
            let width = self.width_of(self.declared[index]);
            self.bind(binding, width);
        }
    }

    /// The width of the type `class`'s terms carry.
    fn width_of(&mut self, class: u32) -> u32 {
        let ty = self.slots.scalar();
        let width = self.slots.scalar();
        self.atoms.push(Atom::Fact {
            column: ColumnId::Type,
            key: class,
            value: ty,
        });
        self.guards.push(Guard::Extern {
            call: call::WIDTH_OF,
            terms: SmallVec::new(),
            args: smallvec![Expr::Scalar(ty)],
            out: smallvec![width],
        });
        width
    }

    fn bind(&mut self, binding: &WidthBinding, actual: u32) {
        match binding {
            WidthBinding::Lit(literal) => self.guards.push(Guard::Cmp(
                Cmp::Eq,
                Expr::Scalar(actual),
                Expr::Lit(*literal as i64),
            )),
            WidthBinding::Name(index) => match self.widths[*index] {
                u32::MAX => self.widths[*index] = actual,
                bound => self.guards.push(Guard::Cmp(
                    Cmp::Eq,
                    Expr::Scalar(actual),
                    Expr::Scalar(bound),
                )),
            },
        }
    }

    /// The declared guards, the constant-operand requirement, the value
    /// predicates, and the proof obligation.
    fn predicates(&mut self, axiom: &Axiom, index: usize) {
        for guard in &axiom.guards {
            let (cmp, a, b) = match guard {
                Guard_::Lt(a, b) => (Cmp::Lt, a, b),
                Guard_::Eq(a, b) => (Cmp::Eq, a, b),
            };
            let widths = self.widths.clone();
            self.guards.push(Guard::Cmp(
                cmp,
                width_expr(a, &widths),
                width_expr(b, &widths),
            ));
        }
        // Constant-operand vars fire only on the immediate form.
        for &var in &axiom.const_vars {
            let class = self.declared[var];
            let constant = self.constant(class);
            self.const_values.insert(var, constant);
        }
        for predicate in &axiom.value_guards {
            let class = self.declared[predicate.var];
            let (value, _) = *self
                .const_values
                .get(&predicate.var)
                .expect("a value predicate names a constant var");
            let width = self.width_of(class);
            self.guards.push(Guard::Extern {
                call: call::FITS,
                terms: SmallVec::new(),
                args: smallvec![
                    Expr::Scalar(value),
                    Expr::Scalar(width),
                    Expr::Lit(predicate.bits as i64),
                    Expr::Lit(i64::from(predicate.unsigned)),
                    Expr::Lit(i64::from(predicate.negated)),
                ],
                out: SmallVec::new(),
            });
        }
        // The obligation is a debug-build check on the target description, not
        // an input to selection, so it is only in the rule when it is asked for.
        if verify_axioms() {
            self.guards.push(Guard::Extern {
                call: call::VERIFY + index as u32,
                terms: SmallVec::new(),
                args: self.widths.iter().map(|&w| Expr::Scalar(w)).collect(),
                out: SmallVec::new(),
            });
        }
    }

    /// The width the identity was proved at: the widest of the declared vars and
    /// the root.
    fn register_width(&mut self, axiom: &Axiom) -> u32 {
        if let Some(width) = self.register {
            return width;
        }
        let widths = self.widths.clone();
        let args: SmallVec<[Expr; 4]> = axiom
            .vars
            .iter()
            .map(|(_, binding)| binding_expr(binding, &widths))
            .chain([binding_expr(&axiom.root_width, &widths)])
            .collect();
        let out = self.slots.scalar();
        self.guards.push(Guard::Extern {
            call: call::MAX,
            terms: SmallVec::new(),
            args,
            out: smallvec![out],
        });
        self.register = Some(out);
        out
    }

    fn right(&mut self, axiom: &Axiom) -> Option<Vec<HeadOp<SemNode>>> {
        let mut head = Vec::new();
        let built = self.build(axiom, &axiom.rhs, &mut head)?;
        head.push(HeadOp::Union(0, built.var));
        Some(head)
    }

    fn build(
        &mut self,
        axiom: &Axiom,
        node: &AxNode,
        head: &mut Vec<HeadOp<SemNode>>,
    ) -> Option<Built> {
        Some(match node {
            AxNode::Root => Built {
                var: 0,
                value: None,
                assumable: false,
            },
            AxNode::ConstMatch(..) => unreachable!("const-match holes are lhs-only"),
            AxNode::Hole(name, var) => {
                let class = self.holes[name];
                Built {
                    var: class,
                    value: var.and_then(|index| self.const_values.get(&index).copied()),
                    assumable: true,
                }
            }
            AxNode::Const(expr, width) => {
                let value = self.slots.scalar();
                let widths = self.widths.clone();
                self.guards.push(Guard::Let {
                    out: value,
                    value: width_expr(expr, &widths),
                });
                let bits = match width {
                    ConstWidth::Register => 64,
                    ConstWidth::Fixed(width) => *width,
                };
                let width = self.constant_width(bits);
                let var = self.literal(value, width, None, head);
                Built {
                    var,
                    value: Some((value, width)),
                    assumable: false,
                }
            }
            // A kept materialize node stays structural — an emitted instruction —
            // typed at the root width so its shift and add tile the class.
            AxNode::Keep(inner) => {
                let AxNode::Node(kind, children) = &**inner else {
                    unreachable!("keep wraps a node")
                };
                let args: Vec<u32> = children
                    .iter()
                    .map(|child| self.build(axiom, child, head).map(|built| built.var))
                    .collect::<Option<_>>()?;
                let widths = self.widths.clone();
                let width = self.let_expr(binding_expr(&axiom.root_width, &widths));
                let ty = self.int_type(width);
                Built {
                    var: self.insert(*kind, &args, Some(ty), head),
                    value: None,
                    assumable: false,
                }
            }
            AxNode::Node(kind, children) => {
                // An unmarked subtree of a materialize axiom is evaluated purely
                // numerically at the root width — the width the identity was
                // proved at — and becomes one typed constant class: a clean
                // recursion target with no back-reference to the wide root and no
                // junk classes for the deconstruction intermediates.
                if axiom.materialize {
                    let widths = self.widths.clone();
                    let width = self.let_expr(binding_expr(&axiom.root_width, &widths));
                    let values: SmallVec<[Expr; 4]> = children
                        .iter()
                        .map(|child| {
                            self.build(axiom, child, &mut Vec::new())
                                .and_then(|built| built.value)
                                .map(|(value, _)| Expr::Scalar(value))
                        })
                        .collect::<Option<_>>()?;
                    let value = self.fold(*kind, width, values);
                    let ty = self.int_type(width);
                    let sixty_four = self.let_expr(Expr::Lit(64));
                    return Some(Built {
                        var: self.literal(value, sixty_four, Some(ty), head),
                        value: Some((value, sixty_four)),
                        assumable: false,
                    });
                }
                let built: Vec<Built> = children
                    .iter()
                    .map(|child| self.build(axiom, child, head))
                    .collect::<Option<_>>()?;
                // A pure op the axiom's own text says is over constants folds
                // where the head builds it, so an immediate consumer binds the
                // result: `sub(x, c)` becomes `add(x, neg(c))`, and `neg(c)` is
                // the negated immediate an `addi` reads. A `keep` node is exempt
                // by construction — it is an instruction, not a value.
                if let Some(folded) = self.fold_operands(*kind, &built) {
                    let (value, width) = folded;
                    return Some(Built {
                        var: self.literal(value, width, None, head),
                        value: Some(folded),
                        assumable: false,
                    });
                }
                let args: Vec<u32> = built.iter().map(|built| built.var).collect();
                let ty = self.result_type(axiom, *kind, &built);
                Built {
                    var: self.insert(*kind, &args, Some(ty), head),
                    value: None,
                    assumable: false,
                }
            }
        })
    }

    /// The value and width a pure op takes over operands the axiom's text
    /// already says are constants; `None` when it is not such an op, or an
    /// operand is not one.
    fn fold_operands(&mut self, kind: SymKind, children: &[Built]) -> Option<(u32, u32)> {
        if !FOLDABLE.contains(&kind) {
            return None;
        }
        let values: SmallVec<[(u32, u32); 4]> = children
            .iter()
            .map(|built| match built.value {
                Some(known) => Some(known),
                None if built.assumable => self.assumed_constant(built.var),
                None => None,
            })
            .collect::<Option<_>>()?;
        let operands: SmallVec<[Expr; 4]> = values
            .iter()
            .flat_map(|&(value, width)| [Expr::Scalar(value), Expr::Scalar(width)])
            .collect();
        let slot = self.fold_slot(kind);
        let (value, width) = (self.slots.scalar(), self.slots.scalar());
        self.guards.push(Guard::Extern {
            call: call::EXECUTE + slot,
            terms: SmallVec::new(),
            args: operands,
            out: smallvec![value, width],
        });
        Some((value, width))
    }

    /// The value of a class the axiom does not declare constant, under the
    /// reading that assumes it is — which the atom this adds then requires.
    fn assumed_constant(&mut self, class: u32) -> Option<(u32, u32)> {
        if self.assume == Folding::Never {
            return None;
        }
        if let Some(&known) = self.assumptions.get(&class) {
            return Some(known);
        }
        let constant = self.constant(class);
        self.assumptions.insert(class, constant);
        self.assumed = true;
        Some(constant)
    }

    fn fold_slot(&mut self, kind: SymKind) -> u32 {
        match self.folds.iter().position(|&seen| seen == kind) {
            Some(slot) => slot as u32,
            None => {
                self.folds.push(kind);
                self.folds.len() as u32 - 1
            }
        }
    }

    /// The type a right-hand side node carries. A conversion names it through
    /// its own format or width operands; a comparison is one bit; everything
    /// else is register-wide.
    fn result_type(&mut self, axiom: &Axiom, kind: SymKind, children: &[Built]) -> u32 {
        let operand = |slot: usize| {
            children
                .get(slot)
                .and_then(|built| built.value)
                .map(|(value, _)| value)
        };
        match kind {
            SymKind::SIToFP | SymKind::UIToFP => {
                let exponent = operand(1).map_or(Expr::Lit(11), Expr::Scalar);
                let mantissa = operand(2).map_or(Expr::Lit(52), Expr::Scalar);
                self.extern_type(call::FLOAT_TYPE, smallvec![exponent, mantissa])
            }
            SymKind::FPToSI | SymKind::FPToUI | SymKind::ZExt | SymKind::SExt => {
                let width = operand(1).map_or(Expr::Lit(64), Expr::Scalar);
                self.extern_type(call::INT_TYPE, smallvec![width])
            }
            SymKind::Extract => {
                let register = self.register_width(axiom);
                let (hi, lo) = (operand(1), operand(2));
                let known = i64::from(hi.is_some() && lo.is_some());
                self.extern_type(
                    call::EXTRACT_TYPE,
                    smallvec![
                        hi.map_or(Expr::Lit(0), Expr::Scalar),
                        lo.map_or(Expr::Lit(0), Expr::Scalar),
                        Expr::Lit(known),
                        Expr::Scalar(register),
                    ],
                )
            }
            kind if is_comparison(kind) => {
                let one = self.let_expr(Expr::Lit(1));
                self.int_type(one)
            }
            _ => {
                let register = self.register_width(axiom);
                self.int_type(register)
            }
        }
    }

    fn insert(
        &mut self,
        kind: SymKind,
        args: &[u32],
        ty: Option<u32>,
        head: &mut Vec<HeadOp<SemNode>>,
    ) -> u32 {
        let mut template = template_node(kind, None, None);
        template.children = args.iter().map(|&var| Id::from_raw(var)).collect();
        let into = self.slots.var();
        head.push(HeadOp::Insert {
            label: LabelFill {
                template,
                fills: ty.map(|ty| (field::TY, ty)).into_iter().collect(),
            },
            args: args.iter().copied().collect(),
            into,
        });
        into
    }

    /// A constant of `value` at `width`, optionally typed.
    fn literal(
        &mut self,
        value: u32,
        width: u32,
        ty: Option<u32>,
        head: &mut Vec<HeadOp<SemNode>>,
    ) -> u32 {
        let mut fills: SmallVec<[(u32, u32); 2]> =
            smallvec![(field::INT_VALUE, value), (field::INT_WIDTH, width)];
        fills.extend(ty.map(|ty| (field::TY, ty)));
        let into = self.slots.var();
        head.push(HeadOp::Insert {
            label: LabelFill {
                template: template_node(
                    SymKind::Constant,
                    Some(SymPayload::Int(APInt::new(1, 0))),
                    None,
                ),
                fills,
            },
            args: SmallVec::new(),
            into,
        });
        into
    }

    fn constant_width(&mut self, bits: u32) -> u32 {
        self.let_expr(Expr::Lit(bits as i64))
    }

    fn let_expr(&mut self, value: Expr) -> u32 {
        let out = self.slots.scalar();
        self.guards.push(Guard::Let { out, value });
        out
    }

    fn int_type(&mut self, width: u32) -> u32 {
        self.extern_type(call::INT_TYPE, smallvec![Expr::Scalar(width)])
    }

    fn extern_type(&mut self, call: u32, args: SmallVec<[Expr; 4]>) -> u32 {
        let out = self.slots.scalar();
        self.guards.push(Guard::Extern {
            call,
            terms: SmallVec::new(),
            args,
            out: smallvec![out],
        });
        out
    }

    /// The value a pure op takes over constant operands, at `width`.
    fn fold(&mut self, kind: SymKind, width: u32, values: SmallVec<[Expr; 4]>) -> u32 {
        let out = self.slots.scalar();
        let slot = self.fold_slot(kind);
        let mut args: SmallVec<[Expr; 4]> = smallvec![Expr::Scalar(width)];
        args.extend(values);
        self.guards.push(Guard::Extern {
            call: call::FOLD + slot,
            terms: SmallVec::new(),
            args,
            out: smallvec![out],
        });
        out
    }
}

/// The pure ops a head can introduce over constant operands. An axiom's
/// right-hand side builds the shape; what its operands turn out to be is the
/// graph's business, so folding it is a rule rather than a step inside the head
/// — `sub(x, c)` becomes `add(x, neg(c))`, and `neg(c)` is the negated immediate
/// an `addi` reads only once something says so.
pub(crate) const FOLDABLE: [SymKind; 11] = [
    SymKind::Add,
    SymKind::Sub,
    SymKind::Mul,
    SymKind::And,
    SymKind::Or,
    SymKind::Xor,
    SymKind::ShiftLeft,
    SymKind::ShiftRightLogic,
    SymKind::ShiftRightArithmetic,
    SymKind::Neg,
    SymKind::Not,
];

/// A width expression over the scalars the width names bound.
fn width_expr(expr: &WidthExpr, widths: &[u32]) -> Expr {
    match expr {
        WidthExpr::Lit(value) => Expr::Lit(*value as i64),
        WidthExpr::Name(index) => Expr::Scalar(widths[*index]),
        WidthExpr::Sub(a, b) => Expr::Sub(
            Box::new(width_expr(a, widths)),
            Box::new(width_expr(b, widths)),
        ),
        WidthExpr::Ones(e) => Expr::Ones(Box::new(width_expr(e, widths))),
    }
}

fn binding_expr(binding: &WidthBinding, widths: &[u32]) -> Expr {
    match binding {
        WidthBinding::Lit(value) => Expr::Lit(*value as i64),
        WidthBinding::Name(index) => Expr::Scalar(widths[*index]),
    }
}
