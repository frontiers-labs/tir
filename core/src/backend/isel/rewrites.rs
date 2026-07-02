//! The proved algebraic rewrites used to saturate the program e-graph before
//! covering, plus the small saturation driver over the [`tir_symbolic`] e-graph.

use tir::{
    Context,
    sem::{FuzzOracle, SymKind, SymPayload, confirm_extension_via_shifts},
};
use tir_adt::APInt;
use tir_symbolic::egraph::{EMatch, Pattern, PatternNode, Var};

use super::node::{SemEGraph, SemNode, class_width, template_node};
use super::pattern::{CompiledIselPattern, atomic_kinds};

/// The right-hand side of an [`IselRewrite`]: given the e-graph and a match, assert
/// the proven equivalence (typically by building nodes and unioning the result with
/// the match root).
pub type IselApplier = dyn Fn(&Context, &mut SemEGraph, &EMatch<u32>) + Send + Sync;

/// An imperative algebraic rewrite: e-match `searcher`, then call `apply` for each
/// match to assert the proven equivalence.
pub struct IselRewrite {
    pub name: String,
    pub searcher: Pattern<SemNode, u32>,
    pub apply: Box<IselApplier>,
}

/// Saturation budget: a cap on iterations and on e-class count.
#[derive(Clone, Copy, Debug)]
pub struct SaturationLimits {
    pub max_iterations: usize,
    pub max_classes: usize,
}

impl Default for SaturationLimits {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            max_classes: 10_000,
        }
    }
}

/// Saturate `eg` with `rewrites`. Each iteration searches every rewrite against the
/// same snapshot, applies all matches, then rebuilds — so a node born this iteration
/// is only visible to the next. Stops at a fixpoint (an iteration that changes
/// neither the class nor the node count) or once a limit is reached.
pub fn saturate(
    ctx: &Context,
    eg: &mut SemEGraph,
    rewrites: &[IselRewrite],
    limits: SaturationLimits,
) {
    for _ in 0..limits.max_iterations {
        let mut matches = Vec::new();
        for (index, rw) in rewrites.iter().enumerate() {
            for m in rw.searcher.search(eg) {
                matches.push((index, m));
            }
        }
        if matches.is_empty() {
            break;
        }

        let before = (eg.num_classes(), eg.total_size());
        for (index, m) in &matches {
            (rewrites[*index].apply)(ctx, eg, m);
        }
        eg.rebuild();

        if (eg.num_classes(), eg.total_size()) == before || eg.num_classes() >= limits.max_classes {
            break;
        }
    }
    eg.rebuild();
}

/// Discover the algebraic bridges the rule set needs to cover sub-word extensions.
/// If the target has `slli` plus the matching right shift, confirm the standard
/// shift-pair identity against the [`FuzzOracle`] and, on success, emit a
/// width-parameterized rewrite. No hand-written selection rule is involved — only a
/// proved bit-vector lemma the target's own instructions happen to realize.
pub(crate) fn discover_rewrites(patterns: &[CompiledIselPattern]) -> Vec<IselRewrite> {
    let atomics = atomic_kinds(patterns);
    let mut rewrites = Vec::new();

    if atomics.contains(&SymKind::ShiftLeft) {
        let oracle = FuzzOracle::default();
        for (ext_kind, shr_kind) in [
            (SymKind::SExt, SymKind::ShiftRightArithmetic),
            (SymKind::ZExt, SymKind::ShiftRightLogic),
        ] {
            if atomics.contains(&shr_kind)
                && confirm_extension_via_shifts(ext_kind, shr_kind, &oracle)
            {
                rewrites.push(extension_rewrite(ext_kind, shr_kind));
            }
        }
    }

    // If the rule set has an `If`-rooted materializer (the `slt`-style
    // "set register to comparison" instructions), bridge bare boolean
    // comparison classes to that shape via the width-1 identity
    // `c == If(c, zext(1, 1), zext(0, 1))`, so a `cmpi` value that must live
    // in a register is selectable without a hand-written rule.
    let has_if_materializer = patterns.iter().any(|compiled| {
        matches!(
            compiled.pattern.node(compiled.pattern.root()),
            PatternNode::Node(node) if node.kind == SymKind::If
        )
    });
    if has_if_materializer {
        for kind in [
            SymKind::Eq,
            SymKind::Ne,
            SymKind::Lt,
            SymKind::Le,
            SymKind::Gt,
            SymKind::Ge,
            SymKind::ULt,
            SymKind::ULe,
            SymKind::UGt,
            SymKind::UGe,
        ] {
            rewrites.push(bool_if_rewrite(kind));
        }
    }

    rewrites
}

/// Build the rewrite `c -> If(c, zext(1, 1), zext(0, 1))` for a boolean
/// comparison class `c` (width 1). This is the shape TMDL derives for
/// `slt`-style instructions (`rd = if rs1 < rs2 { 1 } else { 0 }`), so adding
/// it to the class lets those patterns match and materialize the boolean.
pub(crate) fn bool_if_rewrite(kind: SymKind) -> IselRewrite {
    let mut searcher = Pattern::<SemNode, u32>::new();
    let lhs = searcher.var(Var::Symbol(0));
    let rhs = searcher.var(Var::Symbol(1));
    let mut root = template_node(kind, None, None);
    root.children = vec![lhs, rhs];
    searcher.add(root);

    IselRewrite {
        name: format!("{kind:?}-bool-materialize"),
        searcher,
        apply: Box::new(
            move |ctx: &Context, egraph: &mut SemEGraph, m: &EMatch<u32>| {
                let root_class = m.root;
                if class_width(ctx, egraph, root_class) != Some(1) {
                    return;
                }
                let one = egraph.add(template_node(
                    SymKind::Constant,
                    Some(SymPayload::Int(APInt::new(1, 1))),
                    None,
                ));
                let zero = egraph.add(template_node(
                    SymKind::Constant,
                    Some(SymPayload::Int(APInt::new(1, 0))),
                    None,
                ));
                let mut zext = |value| {
                    let mut node = template_node(SymKind::ZExt, None, None);
                    node.children = vec![value, one];
                    egraph.add(node)
                };
                let then_branch = zext(one);
                let else_branch = zext(zero);
                let mut if_node = template_node(SymKind::If, None, None);
                if_node.children = vec![root_class, then_branch, else_branch];
                let boolean = egraph.add(if_node);
                egraph.union(root_class, boolean);
            },
        ),
    }
}

/// Build the rewrite `ext_kind(v, W) -> shr_kind(shl(v, W - n), W - n)` with
/// `n = width(v)`. The introduced shift nodes are left untyped so they match the
/// target's width-agnostic shift patterns, and the shift amount is a fresh constant.
pub(crate) fn extension_rewrite(ext_kind: SymKind, shr_kind: SymKind) -> IselRewrite {
    let mut searcher = Pattern::<SemNode, u32>::new();
    let value = searcher.var(Var::Symbol(0));
    let width = searcher.var(Var::Symbol(1));
    let mut root = template_node(ext_kind, None, None);
    root.children = vec![value, width];
    searcher.add(root);

    IselRewrite {
        name: format!("{ext_kind:?}-via-shifts"),
        searcher,
        apply: Box::new(
            move |ctx: &Context, egraph: &mut SemEGraph, m: &EMatch<u32>| {
                let root_class = m.root;
                let value_class = m.binding(value);
                let (Some(w), Some(n)) = (
                    class_width(ctx, egraph, root_class),
                    class_width(ctx, egraph, value_class),
                ) else {
                    return;
                };
                if n >= w {
                    return;
                }
                let shift_amount = egraph.add(template_node(
                    SymKind::Constant,
                    Some(SymPayload::Int(APInt::new(64, (w - n) as u64))),
                    None,
                ));
                let mut add_binop = |kind, children| {
                    let mut node = template_node(kind, None, None);
                    node.children = children;
                    egraph.add(node)
                };
                let shl = add_binop(SymKind::ShiftLeft, vec![value_class, shift_amount]);
                let shr = add_binop(shr_kind, vec![shl, shift_amount]);
                egraph.union(root_class, shr);
            },
        ),
    }
}
