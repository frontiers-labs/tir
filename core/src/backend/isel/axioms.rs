//! Target-independent selection axioms: the algebraic bridges of
//! [`super::rewrites`] declared as s-expressions instead of hand-written
//! appliers. An axiom compiles to an [`IselRewrite`] whose applier proves the
//! exact width instantiation it is about to assert with the [`SmtOracle`]
//! (memoized per instantiation) before unioning — an unproved equivalence
//! never reaches the e-graph, and there is no generalization gap between what
//! is proved and what is applied.
//!
//! ```text
//! (axiom <name>
//!   (vars (<var> <width>)...)    ; pattern vars whose class width binds <width>
//!   (root <width|int>)           ; the matched root class's width
//!   (where (< <a> <b>)...)       ; guards over bound widths
//!   (lhs (<kind> <operand>...))  ; matched shape; undeclared atoms are wildcards
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
use std::sync::Mutex;

use tir::{
    Context,
    graph::NodeId,
    sem::{
        EquivalenceOracle, SemExpr, SemGraph, SmtOracle, SymKind, SymPayload, con, op, op_kind,
        parse, sym,
    },
};
use tir_adt::APInt;
use tir_symbolic::egraph::{EMatch, Id, Pattern, Var};

use super::node::{SemEGraph, SemNode, class_width, template_node};
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
}

impl Guard {
    fn holds(&self, widths: &[u64]) -> bool {
        match self {
            Guard::Lt(a, b) => matches!(
                (a.eval(widths), b.eval(widths)),
                (Some(a), Some(b)) if a < b
            ),
        }
    }
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
    Node(SymKind, Vec<AxNode>),
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
    root_width: WidthBinding,
    guards: Vec<Guard>,
    lhs: AxNode,
    rhs: AxNode,
    /// The RHS references the matched root itself (excludes var references).
    uses_root: bool,
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
    let mut root_width = None;
    let mut guards = Vec::new();
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
            "vars" => {
                for entry in rest {
                    let SemExpr::List(pair) = entry else {
                        return Err("vars entries must be (<var> <width>)".into());
                    };
                    let [SemExpr::Atom(v), SemExpr::Atom(w)] = pair.as_slice() else {
                        return Err("vars entries must be (<var> <width>)".into());
                    };
                    let w = binding(w, &mut width_names);
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
                        return Err("guards must be (< <a> <b>)".into());
                    };
                    let [SemExpr::Atom(cmp), a, b] = parts.as_slice() else {
                        return Err("guards must be (< <a> <b>)".into());
                    };
                    if cmp != "<" {
                        return Err(format!("unknown guard `{cmp}`"));
                    }
                    guards.push(Guard::Lt(
                        parse_width_expr(a, &width_names)?,
                        parse_width_expr(b, &width_names)?,
                    ));
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
    if !matches!(lhs, AxNode::Node(..)) {
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
        root_width,
        guards,
        lhs,
        rhs,
        uses_root,
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
                    Err("integer literals cannot be lhs operands".into())
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
                "-" | "ones" if side == Side::Rhs => Ok(AxNode::Const(
                    parse_width_expr(e, width_names)?,
                    ConstWidth::Register,
                )),
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
        AxNode::Hole(_, None) | AxNode::Const(..) => {}
        AxNode::Node(_, children) => {
            for c in children {
                references(c, uses_root, vars);
            }
        }
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
        AxNode::Root | AxNode::Const(..) => {}
    }
}

impl Axiom {
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Every node kind the RHS introduces, for target-capability gating.
    pub(crate) fn rhs_kinds(&self) -> HashSet<SymKind> {
        fn walk(node: &AxNode, out: &mut HashSet<SymKind>) {
            if let AxNode::Node(kind, children) = node {
                out.insert(*kind);
                for c in children {
                    walk(c, out);
                }
            }
        }
        let mut kinds = HashSet::new();
        walk(&self.rhs, &mut kinds);
        kinds
    }

    /// Compile into an [`IselRewrite`]. The applier resolves the width names
    /// from the matched classes, checks the guards, proves the instantiation
    /// (memoized), and only then instantiates the RHS and unions it with the
    /// matched root.
    pub(crate) fn compile(self) -> IselRewrite {
        let mut searcher = Pattern::<SemNode, u32>::new();
        let mut holes: HashMap<String, Id> = HashMap::new();
        compile_lhs(&self.lhs, &mut searcher, &mut holes, &mut 0);

        let name = format!("axiom-{}", self.name);
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
                let proven = {
                    let mut proofs = proofs.lock().unwrap();
                    match proofs.get(&widths) {
                        Some(&p) => p,
                        None => {
                            let p = self.prove(&widths);
                            proofs.insert(widths.clone(), p);
                            p
                        }
                    }
                };
                if !proven {
                    return;
                }
                if let Some(id) = self.instantiate(&self.rhs, eg, m, &holes, &widths) {
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
    pub(crate) fn prove(&self, widths: &[u64]) -> bool {
        let register_width = self.root_width.value(widths) as u32;
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

    /// Build one side of the proof. A declared var is a register-wide symbol —
    /// narrowed to its class width through an extract on the LHS, read whole on
    /// the RHS; a width-name hole is the constant carrying that width.
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
            AxNode::Node(kind, children) => {
                let children = children
                    .iter()
                    .map(|c| self.realize(c, g, widths, register_width, side, root_sym))
                    .collect::<Option<Vec<_>>>()?;
                Some(op(g, *kind, &children))
            }
        }
    }

    /// Build the RHS in the e-graph from a match's bindings.
    fn instantiate(
        &self,
        node: &AxNode,
        eg: &mut SemEGraph,
        m: &EMatch<u32>,
        holes: &HashMap<String, Id>,
        widths: &[u64],
    ) -> Option<Id> {
        Some(match node {
            AxNode::Root => m.root,
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
            AxNode::Node(kind, children) => {
                let children = children
                    .iter()
                    .map(|c| self.instantiate(c, eg, m, holes, widths))
                    .collect::<Option<Vec<_>>>()?;
                let mut n = template_node(*kind, None, None);
                n.children = children;
                eg.add(n)
            }
        })
    }
}

/// Lower the LHS into a search pattern: holes become capture vars (one per
/// name), nodes become untyped templates. The LHS root is added last, so it is
/// the pattern root.
fn compile_lhs(
    node: &AxNode,
    searcher: &mut Pattern<SemNode, u32>,
    holes: &mut HashMap<String, Id>,
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
        AxNode::Node(kind, children) => {
            let children: Vec<Id> = children
                .iter()
                .map(|c| compile_lhs(c, searcher, holes, next_symbol))
                .collect();
            let mut n = template_node(*kind, None, None);
            n.children = children;
            searcher.add(n)
        }
        AxNode::Root | AxNode::Const(..) => unreachable!("rejected when parsing the lhs"),
    }
}

/// The boolean materializer bridges: any width-1 comparison equals the
/// `If(c, 1, 0)` shape TMDL derives for `slt`-style instructions.
pub(crate) fn bool_materialize_axioms() -> Vec<Axiom> {
    [
        "eq", "ne", "lt", "le", "gt", "ge", "ult", "ule", "ugt", "uge",
    ]
    .iter()
    .map(|kind| {
        parse_axiom(&format!(
            "(axiom {kind}-via-if (root 1)
               (lhs ({kind} a b))
               (rhs (if root (zext (const 1 1) (const 1 1))
                             (zext (const 0 1) (const 1 1)))))"
        ))
        .expect("builtin axiom must parse")
    })
    .collect()
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

    #[test]
    fn unsound_axiom_is_refused() {
        // zext realized with an *arithmetic* right shift copies the sign bit —
        // false, and the fuzz samples of old would also have caught it; here the
        // instantiation proof fails and the applier must not union.
        let axiom = parse_axiom(
            "(axiom zext-via-ashr
               (vars (x n)) (root w) (where (< n w))
               (lhs (zext x w))
               (rhs (ashr (shl x (- w n)) (- w n))))",
        )
        .unwrap();
        let ctx = Context::with_default_dialects();
        let (mut eg, root) = extension_egraph(&ctx, SymKind::ZExt);
        apply_all(&ctx, &mut eg, &axiom.compile());
        assert_eq!(
            class_kinds(&eg, root),
            HashSet::from([SymKind::ZExt]),
            "a refuted instantiation must leave the class untouched"
        );
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
            .find(|a| a.name() == "lt-via-if")
            .unwrap();
        apply_all(&ctx, &mut eg, &axiom.compile());
        assert!(class_kinds(&eg, root).contains(&SymKind::If));
    }

    #[test]
    fn extension_axiom_reports_its_rhs_kinds() {
        assert_eq!(
            shift_pair_axiom("sext", "ashr").rhs_kinds(),
            HashSet::from([SymKind::ShiftRightArithmetic, SymKind::ShiftLeft])
        );
        assert_eq!(
            shift_pair_axiom("zext", "lshr").rhs_kinds(),
            HashSet::from([SymKind::ShiftRightLogic, SymKind::ShiftLeft])
        );
    }
}
