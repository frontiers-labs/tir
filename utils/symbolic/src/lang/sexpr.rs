use std::fmt;

use tir_adt::APInt;
use tir_graph::{MutDag, NodeId};

use crate::lang::{SymKind, SymPayload, scalar_op, scalar_op_named};

/// The fixed-arity operator vocabulary of the s-expression surface, shared by
/// the op-sem builder and the isel axiom DSL; operand count is
/// [`SymKind::arity`]. Excludes the context-dependent forms [`build`] resolves
/// itself (unary `sext`/`zext`/`trunc` taking the result width, `(concat
/// iter)`, `map`/`reduce` lambdas).
const OP_VOCABULARY: &[(&str, SymKind)] = &[
    ("fadd", SymKind::FAdd),
    ("fsub", SymKind::FSub),
    ("fmul", SymKind::FMul),
    ("fdiv", SymKind::FDiv),
    ("fmin", SymKind::FMin),
    ("fmax", SymKind::FMax),
    ("asfloat", SymKind::AsFloat),
    ("fcvt", SymKind::FCvt),
    ("sitofp", SymKind::SIToFP),
    ("uitofp", SymKind::UIToFP),
    ("fptosi", SymKind::FPToSI),
    ("fptoui", SymKind::FPToUI),
    ("zip", SymKind::Zip),
    ("split", SymKind::Split),
    ("iota", SymKind::Iota),
    ("sext", SymKind::SExt),
    ("zext", SymKind::ZExt),
    ("extract", SymKind::Extract),
    ("bitcast", SymKind::Bitcast),
    ("if", SymKind::If),
    ("theta", SymKind::Theta),
    ("loop", SymKind::Loop),
    ("port", SymKind::Port),
    ("switch", SymKind::Switch),
];

/// The [`SymKind`] an operator atom names, if any.
pub fn op_kind(name: &str) -> Option<SymKind> {
    scalar_op_named(name).map(|op| op.kind).or_else(|| {
        OP_VOCABULARY
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, k)| k)
    })
}

/// The operator atom naming a [`SymKind`]; inverse of [`op_kind`].
pub fn op_name(kind: SymKind) -> Option<&'static str> {
    scalar_op(kind).map(|op| op.name).or_else(|| {
        OP_VOCABULARY
            .iter()
            .find(|&&(_, k)| k == kind)
            .map(|&(n, _)| n)
    })
}

/// Parsed s-expression: surface syntax of an op's `sem = "..."`; [`build`] lowers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemExpr {
    Atom(String),
    List(Vec<SemExpr>),
}

impl SemExpr {
    /// Names of every `$splice` atom, in first-seen order.
    pub fn splice_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_splices(&mut out);
        out
    }

    fn collect_splices(&self, out: &mut Vec<String>) {
        match self {
            SemExpr::Atom(name) => {
                if let Some(method) = name.strip_prefix('$')
                    && !out.iter().any(|n| n == method)
                {
                    out.push(method.to_string());
                }
            }
            SemExpr::List(items) => {
                for item in items {
                    item.collect_splices(out);
                }
            }
        }
    }
}

/// Parse an s-expression; tokens are whitespace/paren delimited, no quotes or escapes.
pub fn parse(input: &str) -> Option<SemExpr> {
    fn parse_list(chars: &[char], pos: &mut usize) -> Option<SemExpr> {
        if *pos >= chars.len() || chars[*pos] != '(' {
            return None;
        }
        *pos += 1;
        let mut items = Vec::new();
        loop {
            while *pos < chars.len() && chars[*pos].is_whitespace() {
                *pos += 1;
            }
            if *pos >= chars.len() {
                return None;
            }
            if chars[*pos] == ')' {
                *pos += 1;
                break;
            }
            if chars[*pos] == '(' {
                items.push(parse_list(chars, pos)?);
                continue;
            }
            let start = *pos;
            while *pos < chars.len()
                && !chars[*pos].is_whitespace()
                && chars[*pos] != '('
                && chars[*pos] != ')'
            {
                *pos += 1;
            }
            items.push(SemExpr::Atom(chars[start..*pos].iter().collect()));
        }
        Some(SemExpr::List(items))
    }

    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    let expr = parse_list(&chars, &mut pos)?;
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    if pos == chars.len() { Some(expr) } else { None }
}

/// Op-specific callbacks resolving context-dependent atoms: `$splice` subexprs and result width.
pub trait SemBuilderHooks<G> {
    /// Build the subexpr a `$name` atom stands for, or `None` if unprovided.
    fn splice(&self, name: &str, g: &mut G) -> Option<NodeId>;

    /// Width `sext`/`zext`/`trunc` extend to; `None` if op has no result width.
    fn result_width(&self) -> Option<u64>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    Parse,
    /// The top-level form was not `(set <dst> <rhs>)`.
    NotSet,
    UnknownAtom(String),
    BadForm(String),
    /// A `$name` atom had no matching [`SemBuilderHooks::splice`].
    MissingSplice(String),
    /// A width-changing op was used by an op with no result width.
    MissingWidth,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Parse => write!(f, "malformed s-expression"),
            BuildError::NotSet => write!(f, "expected a top-level (set <dst> <rhs>) form"),
            BuildError::UnknownAtom(a) => write!(f, "unknown atom `{a}`"),
            BuildError::BadForm(s) => write!(f, "malformed `{s}` form"),
            BuildError::MissingSplice(n) => write!(f, "no splice provided for `${n}`"),
            BuildError::MissingWidth => write!(f, "width-changing op needs a result width"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Lower a `sem = "(set <dst> <rhs>)"` declaration into a [`SymKind`] graph.
pub fn build<V, G, H>(
    g: &mut G,
    sem: &str,
    symbols: &[(&str, u32)],
    hooks: &H,
) -> Result<NodeId, BuildError>
where
    G: MutDag<Node = SymKind, Leaf = SymPayload<V>>,
    H: SemBuilderHooks<G>,
{
    let parsed = parse(sem).ok_or(BuildError::Parse)?;
    let SemExpr::List(items) = &parsed else {
        return Err(BuildError::NotSet);
    };
    let [SemExpr::Atom(set_kw), SemExpr::Atom(_dst), rhs] = items.as_slice() else {
        return Err(BuildError::NotSet);
    };
    if set_kw != "set" {
        return Err(BuildError::NotSet);
    }
    let mut lambda_params: Vec<Vec<String>> = Vec::new();
    build_node(g, rhs, symbols, &mut lambda_params, hooks)
}

fn leaf<V, G>(g: &mut G, kind: SymKind, data: SymPayload<V>) -> NodeId
where
    G: MutDag<Node = SymKind, Leaf = SymPayload<V>>,
{
    let n = g.add_node(kind);
    g.set_leaf_data(n, data);
    n
}

fn node<V, G>(g: &mut G, kind: SymKind, children: &[NodeId]) -> NodeId
where
    G: MutDag<Node = SymKind, Leaf = SymPayload<V>>,
{
    let n = g.add_node(kind);
    for &child in children {
        g.add_edge(n, child);
    }
    n
}

fn build_node<V, G, H>(
    g: &mut G,
    expr: &SemExpr,
    symbols: &[(&str, u32)],
    lambda_params: &mut Vec<Vec<String>>,
    hooks: &H,
) -> Result<NodeId, BuildError>
where
    G: MutDag<Node = SymKind, Leaf = SymPayload<V>>,
    H: SemBuilderHooks<G>,
{
    match expr {
        SemExpr::Atom(name) => {
            if let Some(method) = name.strip_prefix('$') {
                hooks
                    .splice(method, g)
                    .ok_or_else(|| BuildError::MissingSplice(method.to_string()))
            } else if let Some(idx) = lambda_params
                .last()
                .and_then(|ps| ps.iter().position(|p| p == name))
            {
                // Lambda param reference lowers to an `Arg` leaf carrying its position.
                Ok(leaf(
                    g,
                    SymKind::Arg,
                    SymPayload::Int(APInt::new(32, idx as u64)),
                ))
            } else if let Some(&(_, idx)) = symbols.iter().find(|(s, _)| *s == name) {
                Ok(leaf(g, SymKind::Symbol, SymPayload::SymbolId(idx)))
            } else if let Ok(i) = name.parse::<i64>() {
                Ok(leaf(
                    g,
                    SymKind::Constant,
                    SymPayload::Int(APInt::new_signed(64, i)),
                ))
            } else {
                Err(BuildError::UnknownAtom(name.clone()))
            }
        }
        SemExpr::List(items) => build_list(g, items, symbols, lambda_params, hooks),
    }
}

fn build_list<V, G, H>(
    g: &mut G,
    items: &[SemExpr],
    symbols: &[(&str, u32)],
    lambda_params: &mut Vec<Vec<String>>,
    hooks: &H,
) -> Result<NodeId, BuildError>
where
    G: MutDag<Node = SymKind, Leaf = SymPayload<V>>,
    H: SemBuilderHooks<G>,
{
    // `(concat iter)`: matched before width-changing ops to avoid the single-operand clash.
    if let [SemExpr::Atom(op), arg] = items
        && op == "concat"
    {
        let inner = build_node(g, arg, symbols, lambda_params, hooks)?;
        return Ok(node(g, SymKind::IterConcat, &[inner]));
    }

    // Unary width-changing ops take width from the result type, not an operand
    // (their explicit-width forms fall through to the generic vocabulary).
    if let [SemExpr::Atom(op), arg] = items
        && let Some(kind) = match op.as_str() {
            "sext" => Some(Some(SymKind::SExt)),
            "zext" => Some(Some(SymKind::ZExt)),
            "trunc" => Some(None),
            "fptosi" => Some(Some(SymKind::FPToSI)),
            "fptoui" => Some(Some(SymKind::FPToUI)),
            _ => None,
        }
    {
        let inner = build_node(g, arg, symbols, lambda_params, hooks)?;
        let width = hooks.result_width().ok_or(BuildError::MissingWidth)?;
        return Ok(match kind {
            Some(kind) => {
                let width_bits = if matches!(kind, SymKind::FPToSI | SymKind::FPToUI) {
                    (u64::BITS - width.leading_zeros()).max(1)
                } else {
                    16
                };
                let w = leaf(
                    g,
                    SymKind::Constant,
                    SymPayload::Int(APInt::new(width_bits, width)),
                );
                node(g, kind, &[inner, w])
            }
            None => {
                // trunc x == extract(x, result_width - 1, 0)
                let hi = leaf(
                    g,
                    SymKind::Constant,
                    SymPayload::Int(APInt::new(16, width.saturating_sub(1))),
                );
                let lo = leaf(g, SymKind::Constant, SymPayload::Int(APInt::new(16, 0)));
                node(g, SymKind::Extract, &[inner, hi, lo])
            }
        });
    }

    // `(map iter (lambda (x) body))` / `(reduce iter (lambda (acc x) body))`.
    if let [SemExpr::Atom(op), iter, lambda] = items
        && (op == "map" || op == "reduce")
    {
        let SemExpr::List(parts) = lambda else {
            return Err(BuildError::BadForm(op.clone()));
        };
        let [SemExpr::Atom(lam_kw), SemExpr::List(param_nodes), body] = parts.as_slice() else {
            return Err(BuildError::BadForm(op.clone()));
        };
        if lam_kw != "lambda" {
            return Err(BuildError::BadForm(op.clone()));
        }
        let mut params = Vec::with_capacity(param_nodes.len());
        for p in param_nodes {
            let SemExpr::Atom(p) = p else {
                return Err(BuildError::BadForm(op.clone()));
            };
            params.push(p.clone());
        }

        let iter_node = build_node(g, iter, symbols, lambda_params, hooks)?;
        lambda_params.push(params);
        let body_res = build_node(g, body, symbols, lambda_params, hooks);
        lambda_params.pop();
        let body_node = body_res?;

        let kind = if op == "map" {
            SymKind::Map
        } else {
            SymKind::Reduce
        };
        return Ok(node(g, kind, &[iter_node, body_node]));
    }

    let [SemExpr::Atom(op), args @ ..] = items else {
        return Err(BuildError::BadForm("expression".to_string()));
    };
    let kind = op_kind(op).ok_or_else(|| BuildError::UnknownAtom(op.to_string()))?;
    if !kind.accepts_arity(args.len()) {
        return Err(BuildError::BadForm(op.to_string()));
    }
    let children = args
        .iter()
        .map(|a| build_node(g, a, symbols, lambda_params, hooks))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(node(g, kind, &children))
}
