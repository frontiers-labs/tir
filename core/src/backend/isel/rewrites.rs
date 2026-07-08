//! The proved algebraic rewrites used to saturate the program e-graph before
//! covering, plus the small saturation driver over the [`tir_symbolic`] e-graph.

use tir::{Context, sem::SymKind};
use tir_symbolic::egraph::{EMatch, Pattern, PatternNode, Var};

use super::axioms::bool_materialize_axioms;
use super::node::{SemEGraph, SemNode, class_int_binding, class_width, template_node};
use super::pattern::CompiledIselPattern;

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

/// The target-independent axioms every rule set gets: the boolean materializer
/// bridges, included when the rule set has an `If`-rooted materializer (the
/// `slt`-style "set register to comparison" instructions). Target-specific
/// bridges are discovered offline by the `tir axioms` utility and installed
/// through [`super::InstructionSelectPass::with_axioms`]. Every axiom still
/// proves each width instantiation before it unions (see [`super::axioms`]).
pub(crate) fn discover_rewrites(patterns: &[CompiledIselPattern]) -> Vec<IselRewrite> {
    let mut rewrites = vec![low_bits_extract_rewrite()];
    let has_if_materializer = patterns.iter().any(|compiled| {
        matches!(
            compiled.pattern.node(compiled.pattern.root()),
            PatternNode::Node(node) if node.kind == SymKind::If
        )
    });
    if has_if_materializer {
        rewrites.extend(bool_materialize_axioms().into_iter().map(|a| a.compile()));
    }
    rewrites
}

fn low_bits_extract_rewrite() -> IselRewrite {
    let mut searcher = Pattern::<SemNode, u32>::new();
    let x = searcher.var(Var::Symbol(0));
    let hi = searcher.var(Var::Symbol(1));
    let lo = searcher.var(Var::Symbol(2));
    let mut root = template_node(SymKind::Extract, None, None);
    root.children = vec![x, hi, lo];
    searcher.add(root);

    IselRewrite {
        name: "trunc-low-bits".to_string(),
        searcher,
        apply: Box::new(move |ctx, eg, m| {
            let x_class = eg.find(m.binding(x));
            let Some(result_width) = class_width(ctx, eg, m.root) else {
                return;
            };
            let Some(input_width) = class_width(ctx, eg, x_class) else {
                return;
            };
            if input_width <= result_width || result_width == 0 {
                return;
            }
            let Some(hi_value) = class_int_binding(eg, m.binding(hi)) else {
                return;
            };
            let Some(lo_value) = class_int_binding(eg, m.binding(lo)) else {
                return;
            };
            if hi_value.to_u64() == u64::from(result_width - 1) && lo_value.to_u64() == 0 {
                eg.union(m.root, x_class);
            }
        }),
    }
}
