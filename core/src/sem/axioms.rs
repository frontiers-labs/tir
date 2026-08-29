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

use tir_adt::APInt;
use tir_symbolic::egraph::{EMatch, Id, Pattern, Var};

use crate::sem::{
    EquivalenceOracle, SemExpr, SemGraph, SmtOracle, SymKind, SymPayload, Value, con, execute, op,
    op_kind, parse, sym,
};
use crate::{Context, graph::NodeId};

use super::egraph::{SemEGraph, class_int_binding, class_width, is_comparison};
use super::node::{SemNode, template_node};
use super::rewrites::IselRewrite;

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
    /// Bind or check against the actual class width; false on mismatch.
    fn bind(&self, actual: u64, widths: &mut [Option<u64>]) -> bool {
        match self {
            WidthBinding::Lit(l) => *l == actual,
            WidthBinding::Name(i) => match widths[*i] {
                Some(bound) => bound == actual,
                None => {
                    widths[*i] = Some(actual);
                    true
                }
            },
        }
    }

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

enum Guard {
    Lt(WidthExpr, WidthExpr),
    Eq(WidthExpr, WidthExpr),
}

impl Guard {
    fn holds(&self, widths: &[u64]) -> bool {
        match self {
            Guard::Lt(a, b) => matches!(
                (a.eval(widths), b.eval(widths)),
                (Some(a), Some(b)) if a < b
            ),
            Guard::Eq(a, b) => matches!(
                (a.eval(widths), b.eval(widths)),
                (Some(a), Some(b)) if a == b
            ),
        }
    }
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

pub(crate) struct Axiom {
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
    guards: Vec<Guard>,
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
                                "<" => Guard::Lt(a, b),
                                "=" => Guard::Eq(a, b),
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

    /// Compile into an [`IselRewrite`]. Debug builds prove each width
    /// instantiation before asserting the invariant.
    pub(crate) fn compile(self) -> IselRewrite {
        let mut searcher = Pattern::<SemNode, u32>::new();
        let mut holes: HashMap<String, Id> = HashMap::new();
        let mut const_matches: Vec<(Id, WidthExpr)> = Vec::new();
        compile_lhs(
            &self.lhs,
            &mut searcher,
            &mut holes,
            &mut const_matches,
            &mut 0,
        );

        let const_var_ids: Vec<Id> = self
            .const_vars
            .iter()
            .map(|&i| holes[&self.vars[i].0])
            .collect();

        let name = format!("axiom-{}", self.name);
        let post_saturation = self.post_saturation;
        IselRewrite {
            name,
            searcher,
            post_saturation,
            // The applier below reads classes only through `m.binding(..)`.
            cone_bounded: true,
            apply: Box::new(move |ctx: &Context, eg: &mut SemEGraph, m: &EMatch<u32>| {
                let Some(widths) = self.resolve_widths(ctx, eg, m, &holes) else {
                    return;
                };
                if !self.guards.iter().all(|g| g.holds(&widths)) {
                    return;
                }
                // Constant-operand vars fire only on the immediate form.
                if const_var_ids
                    .iter()
                    .any(|&id| class_int_binding(eg, m.binding(id)).is_none())
                {
                    return;
                }
                for vg in &self.value_guards {
                    match class_int_binding(eg, m.binding(holes[&self.vars[vg.var].0])) {
                        Some(v)
                            if (if vg.unsigned {
                                fits_unsigned(&v, vg.bits)
                            } else {
                                fits_signed(&v, vg.bits)
                            }) == !vg.negated => {}
                        _ => return,
                    }
                }
                for (id, expr) in &const_matches {
                    let Some(expected) = expr.eval(&widths) else {
                        return;
                    };
                    match class_int_binding(eg, m.binding(*id)) {
                        Some(bound) if bound.to_u64() == expected => {}
                        _ => return,
                    }
                }
                if verify_axioms() {
                    self.verify(&widths);
                }
                if let Some(id) = self.instantiate(ctx, &self.rhs, eg, m, &holes, &widths) {
                    eg.union(m.root, id);
                }
            }),
        }
    }

    /// Resolve every width name from the matched classes; `None` if a needed
    /// class width is unknown or a binding conflicts.
    fn resolve_widths(
        &self,
        ctx: &Context,
        eg: &SemEGraph,
        m: &EMatch<u32>,
        holes: &HashMap<String, Id>,
    ) -> Option<Vec<u64>> {
        let mut widths = vec![None; self.width_names.len()];
        let root_width = class_width(ctx, eg, m.root)?;
        if !self.root_width.bind(root_width as u64, &mut widths) {
            return None;
        }
        for (var, binding) in &self.vars {
            let class = m.binding(holes[var]);
            let actual = class_width(ctx, eg, class)?;
            if !binding.bind(actual as u64, &mut widths) {
                return None;
            }
        }
        widths.into_iter().collect()
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

    /// Build the RHS in the e-graph from a match's bindings.
    fn instantiate(
        &self,
        ctx: &Context,
        node: &AxNode,
        eg: &mut SemEGraph,
        m: &EMatch<u32>,
        holes: &HashMap<String, Id>,
        widths: &[u64],
    ) -> Option<Id> {
        Some(match node {
            AxNode::Root => m.root,
            AxNode::ConstMatch(..) => unreachable!("const-match holes are lhs-only"),
            AxNode::Hole(name, _) => m.binding(holes[name]),
            AxNode::Const(e, width) => {
                let width = match width {
                    ConstWidth::Register => 64,
                    ConstWidth::Fixed(w) => *w,
                };
                eg.add(template_node(
                    SymKind::Constant,
                    Some(SymPayload::Int(APInt::new(width, e.eval(widths)?))),
                    None,
                ))
            }
            // A kept materialize node stays structural (an emitted instruction),
            // typed at the root width so its shift/add tiles the class.
            AxNode::Keep(inner) => {
                let AxNode::Node(kind, node_children) = &**inner else {
                    unreachable!("keep wraps a node");
                };
                let children = node_children
                    .iter()
                    .map(|c| self.instantiate(ctx, c, eg, m, holes, widths))
                    .collect::<Option<Vec<_>>>()?;
                let width = self.root_width.value(widths) as u32;
                let ty = tir::builtin::IntegerType::new(ctx, width);
                let mut n = template_node(*kind, None, Some(ty));
                n.children = children;
                eg.add(n)
            }
            AxNode::Node(kind, children) => {
                // An unmarked subtree of a materialize axiom is evaluated purely
                // numerically at the *root width* — the width the identity was
                // proved at — and becomes one *typed* constant class: a clean
                // recursion target the axiom can decompose again, with no
                // back-reference to the wide root (which would make the cover's
                // class graph cyclic) and no junk classes for the deconstruction
                // intermediates. The value is stored sign-extended the same way
                // program constants are interned. Only the kept reconstruction
                // nodes survive as instructions.
                if self.materialize {
                    let width = self.root_width.value(widths) as u32;
                    let ty = tir::builtin::IntegerType::new(ctx, width);
                    let value = self.eval_at(node, eg, m, holes, widths, width)?;
                    return Some(eg.add(template_node(
                        SymKind::Constant,
                        Some(SymPayload::Int(APInt::new_signed(64, value))),
                        Some(ty),
                    )));
                }
                let children = children
                    .iter()
                    .map(|c| self.instantiate(ctx, c, eg, m, holes, widths))
                    .collect::<Option<Vec<_>>>()?;
                fold_constant_op(*kind, &children, eg).unwrap_or_else(|| {
                    // Conversions carry the result type named by their format or
                    // width operands; every other node is register-wide integer
                    // (comparisons a single bit).
                    let ty = if matches!(*kind, SymKind::SIToFP | SymKind::UIToFP) {
                        conversion_float_type(ctx, eg, &children)
                    } else if matches!(
                        *kind,
                        SymKind::FPToSI | SymKind::FPToUI | SymKind::ZExt | SymKind::SExt
                    ) {
                        conversion_integer_type(ctx, eg, &children)
                    } else if *kind == SymKind::Extract {
                        extract_integer_type(ctx, eg, &children, self.register_width(widths))
                    } else {
                        let width = if is_comparison(*kind) {
                            1
                        } else {
                            self.register_width(widths)
                        };
                        tir::builtin::IntegerType::new(ctx, width)
                    };
                    let mut n = template_node(*kind, None, Some(ty));
                    n.children = children;
                    eg.add(n)
                })
            }
        })
    }

    /// Numerically evaluate an unmarked materialize-RHS subtree at `width` (see
    /// [`fold_values_at`]); `None` if a leaf is not a bound constant.
    fn eval_at(
        &self,
        node: &AxNode,
        eg: &SemEGraph,
        m: &EMatch<u32>,
        holes: &HashMap<String, Id>,
        widths: &[u64],
        width: u32,
    ) -> Option<i64> {
        match node {
            AxNode::Hole(name, _) => {
                class_int_binding(eg, m.binding(holes[name])).map(|v| v.to_i64())
            }
            AxNode::Const(e, _) | AxNode::ConstMatch(e) => e.eval(widths).map(|v| v as i64),
            AxNode::Node(kind, children) => {
                let values = children
                    .iter()
                    .map(|c| self.eval_at(c, eg, m, holes, widths, width))
                    .collect::<Option<Vec<_>>>()?;
                fold_values_at(*kind, &values, width)
            }
            AxNode::Root | AxNode::Keep(..) => None,
        }
    }
}

/// The IEEE result type of a conversion node from its exponent/mantissa operand
/// classes; falls back to double when either is not a bound constant.
fn conversion_float_type(ctx: &Context, eg: &SemEGraph, children: &[Id]) -> tir::TypeId {
    let format = |slot: usize| {
        children
            .get(slot)
            .and_then(|&c| class_int_binding(eg, c))
            .map(|v| v.to_u64() as u32)
    };
    let exp = format(1).unwrap_or(11);
    let mant = format(2).unwrap_or(52);
    tir::builtin::FloatType::new(ctx, exp, mant)
}

fn conversion_integer_type(ctx: &Context, eg: &SemEGraph, children: &[Id]) -> tir::TypeId {
    let width = children
        .get(1)
        .and_then(|&c| class_int_binding(eg, c))
        .map(|v| v.to_u64() as u32)
        .unwrap_or(64);
    tir::builtin::IntegerType::new(ctx, width)
}

fn extract_integer_type(
    ctx: &Context,
    eg: &SemEGraph,
    children: &[Id],
    fallback: u32,
) -> tir::TypeId {
    let bound = |slot: usize| {
        children
            .get(slot)
            .and_then(|&c| class_int_binding(eg, c))
            .map(|v| v.to_u64() as u32)
    };
    let width = match (bound(1), bound(2)) {
        (Some(hi), Some(lo)) if hi >= lo => hi - lo + 1,
        _ => fallback,
    };
    tir::builtin::IntegerType::new(ctx, width)
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

/// Fold a pure op whose operands are all constants into a single constant, so an
/// immediate consumer can bind the result — e.g. `Sub(x, c) -> Add(x, neg(c))`
/// yields `neg(const)`, which folds to the negated immediate `addi` reads.
/// `None` when an operand is not constant or the kind is not a foldable pure op.
fn fold_constant_op(kind: SymKind, children: &[Id], eg: &mut SemEGraph) -> Option<Id> {
    use SymKind::*;
    if !matches!(
        kind,
        Add | Sub
            | Mul
            | And
            | Or
            | Xor
            | ShiftLeft
            | ShiftRightLogic
            | ShiftRightArithmetic
            | Neg
            | Not
    ) {
        return None;
    }
    let operands: Vec<(u64, u32)> = children
        .iter()
        .map(|&c| class_int_binding(eg, c).map(|v| (v.to_u64(), v.width())))
        .collect::<Option<Vec<_>>>()?;
    let result = execute_fold(kind, &operands)?;
    Some(eg.add(template_node(
        SymKind::Constant,
        Some(SymPayload::Int(result)),
        None,
    )))
}

/// Lower the LHS into a search pattern: holes become capture vars (one per
/// name), nodes become untyped templates. The LHS root is added last, so it is
/// the pattern root.
fn compile_lhs(
    node: &AxNode,
    searcher: &mut Pattern<SemNode, u32>,
    holes: &mut HashMap<String, Id>,
    const_matches: &mut Vec<(Id, WidthExpr)>,
    next_symbol: &mut u32,
) -> Id {
    match node {
        AxNode::Hole(name, _) => {
            if let Some(&id) = holes.get(name) {
                return id;
            }
            let id = searcher.var(Var::Symbol(*next_symbol));
            *next_symbol += 1;
            holes.insert(name.clone(), id);
            id
        }
        AxNode::ConstMatch(e) => {
            let id = searcher.var(Var::Symbol(*next_symbol));
            *next_symbol += 1;
            const_matches.push((id, e.clone()));
            id
        }
        AxNode::Node(kind, children) => {
            let children: Vec<Id> = children
                .iter()
                .map(|c| compile_lhs(c, searcher, holes, const_matches, next_symbol))
                .collect();
            let mut n = template_node(*kind, None, None);
            n.children = children;
            searcher.add(n)
        }
        AxNode::Root | AxNode::Const(..) | AxNode::Keep(..) => {
            unreachable!("rejected when parsing the lhs")
        }
    }
}
