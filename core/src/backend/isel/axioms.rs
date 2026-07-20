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

use std::collections::{HashMap, HashSet};
#[cfg(debug_assertions)]
use std::sync::Mutex;

#[cfg(debug_assertions)]
use tir::sem::{EquivalenceOracle, SmtOracle, sym};
use tir::{
    Context,
    graph::NodeId,
    sem::{SemExpr, SemGraph, SymKind, SymPayload, Value, con, execute, op, op_kind, parse},
};
use tir_adt::APInt;
use tir_symbolic::egraph::{EMatch, Id, Pattern, Var};

use super::node::{
    SemEGraph, SemNode, class_int_binding, class_width, is_comparison, template_node,
};
use super::rewrites::IselRewrite;

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

pub(crate) struct Axiom {
    name: String,
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
    #[cfg(debug_assertions)]
    uses_root: bool,
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
            "where" => {
                for g in rest {
                    let SemExpr::List(parts) = g else {
                        return Err("guards must be (< <a> <b>) or (fits <var> <bits>)".into());
                    };
                    // `(not ...)` unwraps to its inner guard; only `fits` may be
                    // negated, which the match below enforces.
                    let (parts, negated) = match parts.as_slice() {
                        [SemExpr::Atom(kw), SemExpr::List(inner)] if kw == "not" => {
                            (inner.as_slice(), true)
                        }
                        parts => (parts, false),
                    };
                    match parts {
                        [SemExpr::Atom(kw), SemExpr::Atom(var), SemExpr::Atom(bits)]
                            if kw == "fits" =>
                        {
                            value_guards.push(parse_value_guard(var, bits, negated, &vars)?);
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
                            return Err("guards must be (< <a> <b>), (fits <var> <bits>), \
                                 or (not (fits <var> <bits>))"
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
        #[cfg(debug_assertions)]
        uses_root,
        materialize,
    })
}

fn parse_value_guard(
    var: &str,
    bits: &str,
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
    Ok(ValueGuard { var, bits, negated })
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
        #[cfg(debug_assertions)]
        let proofs: Mutex<HashMap<Vec<u64>, bool>> = Mutex::default();
        IselRewrite {
            name,
            searcher,
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
                        Some(v) if fits_signed(&v, vg.bits) == !vg.negated => {}
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
                #[cfg(debug_assertions)]
                {
                    let mut proofs = proofs.lock().unwrap();
                    let proven = match proofs.get(&widths) {
                        Some(&p) => p,
                        None => {
                            let p = self.prove(&widths);
                            proofs.insert(widths.clone(), p);
                            p
                        }
                    };
                    assert!(
                        proven,
                        "invalid semantic invariant `{}` for widths {widths:?}",
                        self.name
                    );
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

    /// Prove one width instantiation with the [`SmtOracle`]; `widths` follows
    /// the width names' declaration order (`vars` first, then `root`).
    #[cfg(debug_assertions)]
    pub(crate) fn prove(&self, widths: &[u64]) -> bool {
        let register_width = self.register_width(widths);
        let mut lhs = SemGraph::new();
        let mut rhs = SemGraph::new();
        let (built, symbol_count) = if self.uses_root {
            // Lemma over an opaque root value: lhs is a bare symbol, `root` in
            // the rhs is the same symbol.
            let root_sym = sym(&mut rhs, 0);
            sym(&mut lhs, 0);
            let built = self
                .realize(
                    &self.rhs,
                    &mut rhs,
                    widths,
                    register_width,
                    Side::Rhs,
                    Some(root_sym),
                )
                .is_some();
            (built, 1)
        } else {
            // Register realization: each var is the low bits of a full-width
            // register symbol in the lhs; the rhs reads the register whole.
            let built = self
                .realize(&self.lhs, &mut lhs, widths, register_width, Side::Lhs, None)
                .is_some()
                && self
                    .realize(&self.rhs, &mut rhs, widths, register_width, Side::Rhs, None)
                    .is_some();
            (built, self.vars.len())
        };
        if !built {
            return false;
        }
        let symbol_widths = vec![register_width; symbol_count];
        SmtOracle.equivalent(&lhs, &rhs, &symbol_widths)
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
    /// the RHS; a width-name hole is the constant carrying that width.
    #[cfg(debug_assertions)]
    fn realize(
        &self,
        node: &AxNode,
        g: &mut SemGraph,
        widths: &[u64],
        register_width: u32,
        side: Side,
        root_sym: Option<NodeId>,
    ) -> Option<NodeId> {
        match node {
            AxNode::Root => root_sym,
            AxNode::Hole(_, Some(i)) => {
                let s = sym(g, *i as u32);
                let class_w = self.vars[*i].1.value(widths) as u32;
                if side == Side::Rhs || class_w == register_width {
                    Some(s)
                } else if class_w < register_width {
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
                    ConstWidth::Register => register_width,
                    ConstWidth::Fixed(w) => *w,
                };
                Some(con(g, e.eval(widths)?, width))
            }
            AxNode::ConstMatch(e) => Some(con(g, e.eval(widths)?, 16)),
            AxNode::Node(kind, children) => {
                let children = children
                    .iter()
                    .map(|c| self.realize(c, g, widths, register_width, side, root_sym))
                    .collect::<Option<Vec<_>>>()?;
                Some(op(g, *kind, &children))
            }
            AxNode::Keep(inner) => self.realize(inner, g, widths, register_width, side, root_sym),
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
                    let width = if is_comparison(*kind) {
                        1
                    } else {
                        self.register_width(widths)
                    };
                    let mut n = template_node(
                        *kind,
                        None,
                        Some(tir::builtin::IntegerType::new(ctx, width)),
                    );
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

/// The boolean materializer bridges: any width-1 comparison equals the
/// `If(c, 1, 0)` shape TMDL derives for `slt`-style instructions.
#[cfg(test)]
pub(crate) fn bool_materialize_axioms() -> Vec<Axiom> {
    super::theory::axioms()
        .into_iter()
        .filter(|axiom| axiom.name.ends_with("-via-if"))
        .collect()
}

/// Bridge the six comparison values a `slt`/`sltu`-class target cannot root
/// directly onto the four it can (`lt`/`gt`/`ult`/`ugt`) plus `xor`:
///   `eq(a,b)  == ult(xor(a,b), 1)`   `ne(a,b)  == xor(ult(xor(a,b), 1), 1)`
///   `ge(a,b)  == xor(lt(a,b), 1)`    `le(a,b)  == xor(lt(b,a), 1)`
///   `uge(a,b) == xor(ult(a,b), 1)`   `ule(a,b) == xor(ult(b,a), 1)`
/// Each a proved identity; the introduced `lt`/`ult` in turn materialize through
/// the boolean `via-if` bridges. `ne` is `not(eq)` — a constant in the `ult`
/// low operand would match neither `sltu` (register) nor `sltiu` (imm is high).
#[cfg(test)]
pub(crate) fn comparison_materialize_axioms() -> Vec<Axiom> {
    super::theory::axioms()
        .into_iter()
        .filter(|axiom| axiom.name.ends_with("-via-cmp"))
        .collect()
}

/// `x - c == x + (-c)` for a constant `c`: a target without a subtract-immediate
/// covers `sub` with an immediate operand through `add`, since `neg(const)` folds
/// to the negated immediate. The `consts` operand keeps it off register `sub`.
#[cfg(test)]
pub(crate) fn sub_via_add_neg_axiom() -> Axiom {
    super::theory::axioms()
        .into_iter()
        .find(|axiom| axiom.name == "sub-via-add-neg")
        .expect("sub-immediate family is declared")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tir::builtin::IntegerType;

    /// Apply `rewrite` to every current match and rebuild.
    fn apply_all(ctx: &Context, eg: &mut SemEGraph, rewrite: &IselRewrite) {
        let matches: Vec<_> = rewrite.searcher.search(eg);
        for m in &matches {
            (rewrite.apply)(ctx, eg, m);
        }
        eg.rebuild();
    }

    /// `ext_kind(symbol : i16, 64) : i64` — the shape the extension axioms match.
    fn extension_egraph(ctx: &Context, ext_kind: SymKind) -> (SemEGraph, Id) {
        let i16 = IntegerType::new(ctx, 16);
        let i64 = IntegerType::new(ctx, 64);
        let mut eg = SemEGraph::new();
        let v = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(0)),
            Some(i16),
        ));
        let width = eg.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(64, 64))),
            None,
        ));
        let mut ext = template_node(ext_kind, None, Some(i64));
        ext.children = vec![v, width];
        let root = eg.add(ext);
        (eg, root)
    }

    fn class_kinds(eg: &SemEGraph, class: Id) -> HashSet<SymKind> {
        eg.nodes(class).iter().map(|n| n.kind).collect()
    }

    fn shift_pair_axiom(ext: &str, shr: &str) -> Axiom {
        parse_axiom(&format!(
            "(axiom {ext}-via-shifts
               (vars (x n)) (root w) (where (< n w))
               (lhs ({ext} x w))
               (rhs ({shr} (shl x (- w n)) (- w n))))"
        ))
        .unwrap()
    }

    #[test]
    fn proved_extension_axiom_unions_shift_pair() {
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = extension_egraph(&ctx, SymKind::ZExt);
        let axiom = shift_pair_axiom("zext", "lshr");
        apply_all(&ctx, &mut eg, &axiom.compile());
        assert!(class_kinds(&eg, root).contains(&SymKind::ShiftRightLogic));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "invalid semantic invariant")]
    fn unsound_axiom_fails_debug_validation() {
        // zext realized with an *arithmetic* right shift copies the sign bit —
        // false, so debug validation must reject it.
        let axiom = parse_axiom(
            "(axiom zext-via-ashr
               (vars (x n)) (root w) (where (< n w))
               (lhs (zext x w))
               (rhs (ashr (shl x (- w n)) (- w n))))",
        )
        .unwrap();
        let ctx = Context::with_default_dialects();
        let (mut eg, _) = extension_egraph(&ctx, SymKind::ZExt);
        apply_all(&ctx, &mut eg, &axiom.compile());
    }

    #[test]
    fn guard_blocks_widths_outside_the_lemma() {
        // sext to the value's own width: `(< n w)` fails, nothing is asserted
        // (the underflowing `w - n = 0` shift is never even built).
        let ctx = Context::with_default_dialects();
        let i64 = IntegerType::new(&ctx, 64);
        let mut eg = SemEGraph::new();
        let v = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(0)),
            Some(i64),
        ));
        let width = eg.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(64, 64))),
            None,
        ));
        let mut ext = template_node(SymKind::SExt, None, Some(i64));
        ext.children = vec![v, width];
        let root = eg.add(ext);

        let axiom = shift_pair_axiom("sext", "ashr");
        apply_all(&ctx, &mut eg, &axiom.compile());
        assert_eq!(class_kinds(&eg, root), HashSet::from([SymKind::SExt]));
    }

    #[test]
    fn bool_axiom_bridges_a_comparison_class() {
        let ctx = Context::with_default_dialects();
        let i1 = IntegerType::new(&ctx, 1);
        let i32 = IntegerType::new(&ctx, 32);
        let mut eg = SemEGraph::new();
        let a = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(0)),
            Some(i32),
        ));
        let b = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(1)),
            Some(i32),
        ));
        let mut cmp = template_node(SymKind::Lt, None, Some(i1));
        cmp.children = vec![a, b];
        let root = eg.add(cmp);

        let axiom = bool_materialize_axioms()
            .into_iter()
            .find(|a| a.name == "lt-via-if")
            .unwrap();
        apply_all(&ctx, &mut eg, &axiom.compile());
        assert!(class_kinds(&eg, root).contains(&SymKind::If));
    }

    /// `extract(symbol : i32, hi, lo)` with the width-16 constant convention;
    /// the result type carries the `hi - lo + 1` narrowed width.
    fn extract_egraph(ctx: &Context, hi: u64, lo: u64) -> (SemEGraph, Id) {
        let i32 = IntegerType::new(ctx, 32);
        let result_ty = IntegerType::new(ctx, (hi - lo + 1) as u32);
        let mut eg = SemEGraph::new();
        let x = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(0)),
            Some(i32),
        ));
        let hi_c = eg.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(16, hi))),
            None,
        ));
        let lo_c = eg.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(16, lo))),
            None,
        ));
        let mut ex = template_node(SymKind::Extract, None, Some(result_ty));
        ex.children = vec![x, hi_c, lo_c];
        let root = eg.add(ex);
        (eg, root)
    }

    fn narrowing_axiom() -> Axiom {
        parse_axiom(
            "(axiom trunc-and-ones
               (vars (x w)) (root n) (where (< n w))
               (lhs (extract x (- n 1) 0))
               (rhs (extract (and x (ones n)) (- n 1) 0)))",
        )
        .unwrap()
    }

    #[test]
    fn narrowing_axiom_unions_a_masked_form() {
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = extract_egraph(&ctx, 15, 0);
        assert_eq!(eg.nodes(root).len(), 1);
        apply_all(&ctx, &mut eg, &narrowing_axiom().compile());
        assert!(
            eg.nodes(root).len() > 1,
            "the proved masked form must union into the low-extract class"
        );
    }

    /// `sub(symbol : i64, right)` where `right` is a constant or a symbol.
    fn sub_egraph(ctx: &Context, right_const: bool) -> (SemEGraph, Id) {
        let i64 = IntegerType::new(ctx, 64);
        let mut eg = SemEGraph::new();
        let a = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(0)),
            Some(i64),
        ));
        let right = if right_const {
            eg.add(template_node(
                SymKind::Constant,
                Some(SymPayload::Int(APInt::new(64, 10))),
                Some(i64),
            ))
        } else {
            eg.add(template_node(
                SymKind::Symbol,
                Some(SymPayload::SymbolId(1)),
                Some(i64),
            ))
        };
        let mut sub = template_node(SymKind::Sub, None, Some(i64));
        sub.children = vec![a, right];
        let root = eg.add(sub);
        (eg, root)
    }

    #[test]
    fn equality_bridges_to_a_typed_xor_slt_composite() {
        // `eq(a,b) == ult(xor(a,b), 1)`; the introduced `ult` must be typed i1
        // so the `sltiu`-class materializer pattern still matches it.
        let ctx = Context::with_default_dialects();
        let i1 = IntegerType::new(&ctx, 1);
        let i64 = IntegerType::new(&ctx, 64);
        let mut eg = SemEGraph::new();
        let a = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(0)),
            Some(i64),
        ));
        let b = eg.add(template_node(
            SymKind::Symbol,
            Some(SymPayload::SymbolId(1)),
            Some(i64),
        ));
        let mut eq = template_node(SymKind::Eq, None, Some(i1));
        eq.children = vec![a, b];
        let root = eg.add(eq);

        let axiom = comparison_materialize_axioms()
            .into_iter()
            .find(|a| a.name == "eq-via-cmp")
            .unwrap();
        apply_all(&ctx, &mut eg, &axiom.compile());
        let ult = eg
            .nodes(root)
            .iter()
            .find(|n| n.kind == SymKind::ULt)
            .expect("eq must bridge to ult(xor, 1)");
        assert_eq!(
            ult.ty,
            Some(i1),
            "the introduced comparison must be typed i1"
        );
    }

    #[test]
    fn constant_operand_axiom_bridges_only_a_constant() {
        let ctx = Context::with_default_dialects();

        let (mut const_eg, root) = sub_egraph(&ctx, true);
        apply_all(&ctx, &mut const_eg, &sub_via_add_neg_axiom().compile());
        assert!(
            class_kinds(&const_eg, root).contains(&SymKind::Add),
            "a constant operand bridges `sub` to `add`"
        );

        let (mut reg_eg, root) = sub_egraph(&ctx, false);
        apply_all(&ctx, &mut reg_eg, &sub_via_add_neg_axiom().compile());
        assert!(
            !class_kinds(&reg_eg, root).contains(&SymKind::Add),
            "a register operand is left as `sub`"
        );
    }

    #[test]
    fn equality_guard_gates_on_a_width() {
        // `(= n 16)` fires only when the narrowed width is 16.
        let axiom = || {
            parse_axiom(
                "(axiom trunc-when-16
                   (vars (x w)) (root n) (where (< n w) (= n 16))
                   (lhs (extract x (- n 1) 0))
                   (rhs (extract (and x (ones n)) (- n 1) 0)))",
            )
            .unwrap()
        };
        let ctx = Context::with_default_dialects();

        let (mut blocked, root) = extract_egraph(&ctx, 7, 0);
        apply_all(&ctx, &mut blocked, &axiom().compile());
        assert_eq!(blocked.nodes(root).len(), 1, "width 8 fails `(= n 16)`");

        let (mut fired, root) = extract_egraph(&ctx, 15, 0);
        apply_all(&ctx, &mut fired, &axiom().compile());
        assert!(fired.nodes(root).len() > 1, "width 16 satisfies `(= n 16)`");
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "invalid semantic invariant")]
    fn unsound_narrowing_axiom_fails_debug_validation() {
        // Masking to `n - 1` bits drops bit `n - 1`, so it does not equal the
        // low `n` bits, so debug validation must reject it.
        let axiom = parse_axiom(
            "(axiom trunc-off-by-one
               (vars (x w)) (root n) (where (< n w))
               (lhs (extract x (- n 1) 0))
               (rhs (extract (and x (ones (- n 1))) (- n 1) 0)))",
        )
        .unwrap();
        let ctx = Context::with_default_dialects();
        let (mut eg, _) = extract_egraph(&ctx, 15, 0);
        apply_all(&ctx, &mut eg, &axiom.compile());
    }

    #[test]
    fn narrowing_axiom_skips_a_non_low_slice() {
        // `extract(x, 16, 1)` is a slice, not a low truncation: the `0` const
        // hole must reject `lo = 1`, so nothing unions.
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = extract_egraph(&ctx, 16, 1);
        apply_all(&ctx, &mut eg, &narrowing_axiom().compile());
        assert_eq!(
            eg.nodes(root).len(),
            1,
            "a slice must not match the low-extract axiom"
        );
    }

    /// The universal low-12-bit split identity, guarded to fire only on
    /// constants too wide for a 12-bit signed immediate.
    fn wide_const_axiom() -> Axiom {
        parse_axiom(
            "(axiom wide-const
               (consts (v w)) (root w) (where (not (fits v 12)))
               (lhs v)
               (rhs (keep (add
                    (keep (shl (ashr (sub v (ashr (shl v (- w 12)) (- w 12))) 12) 12))
                    (ashr (shl v (- w 12)) (- w 12))))))",
        )
        .unwrap()
    }

    fn constant_egraph_at(ctx: &Context, value: u64, width: u32) -> (SemEGraph, Id) {
        let ty = IntegerType::new(ctx, width);
        let mut eg = SemEGraph::new();
        let root = eg.add(template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(width, value))),
            Some(ty),
        ));
        (eg, root)
    }

    fn constant_egraph(ctx: &Context, value: u64) -> (SemEGraph, Id) {
        constant_egraph_at(ctx, value, 64)
    }

    /// The `shl` operand class of the decomposition's root `add`, when present.
    fn shl_operand(eg: &SemEGraph, root: Id) -> Option<Id> {
        eg.nodes(root)
            .iter()
            .find(|n| n.kind == SymKind::Add)
            .and_then(|add| {
                add.children.iter().copied().find(|&c| {
                    eg.nodes(eg.find(c))
                        .iter()
                        .any(|n| n.kind == SymKind::ShiftLeft)
                })
            })
    }

    #[test]
    fn materialize_axiom_decomposes_a_narrow_typed_constant() {
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = constant_egraph_at(&ctx, 74565, 32);
        apply_all(&ctx, &mut eg, &wide_const_axiom().compile());
        assert!(
            class_kinds(&eg, root).contains(&SymKind::Add) && shl_operand(&eg, root).is_some(),
            "an i32 constant must decompose at its own width, got {:?}",
            class_kinds(&eg, root)
        );
    }

    #[test]
    fn materialize_axiom_decomposes_a_wide_constant() {
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = constant_egraph(&ctx, 0x8000_0000);
        apply_all(&ctx, &mut eg, &wide_const_axiom().compile());
        assert!(
            class_kinds(&eg, root).contains(&SymKind::Add) && shl_operand(&eg, root).is_some(),
            "the shift/add tiling must be unioned into the constant class, got {:?}",
            class_kinds(&eg, root)
        );
        assert_eq!(
            class_int_binding(&eg, root).map(|v| v.to_u64()),
            Some(0x8000_0000),
            "union-with-fold must keep the constant value in the class"
        );
    }

    #[test]
    fn materialize_axiom_skips_a_fitting_constant() {
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = constant_egraph(&ctx, 5);
        apply_all(&ctx, &mut eg, &wide_const_axiom().compile());
        assert_eq!(
            class_kinds(&eg, root),
            HashSet::from([SymKind::Constant]),
            "a constant fitting the 12-bit immediate must not be decomposed"
        );
    }

    #[test]
    fn fits_guard_rejects_invalid_bit_counts_and_supports_64_bits() {
        let axiom = |bits| {
            parse_axiom(&format!(
                "(axiom fits-{bits}
                   (consts (v w)) (root w) (where (fits v {bits}))
                   (lhs v) (rhs v))"
            ))
        };

        assert!(axiom(0).is_err());
        assert!(axiom(65).is_err());
        assert!(axiom(64).is_ok());
        assert!(fits_signed(&APInt::new_signed(64, i64::MIN), 64));
        assert!(fits_signed(&APInt::new_signed(64, i64::MAX), 64));
    }
}
