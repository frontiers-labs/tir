//! The proved algebraic rewrites used to saturate the program e-graph before
//! covering, plus the small saturation driver over the [`tir_symbolic`] e-graph.

use tir::{Context, sem::SymKind};
use tir_symbolic::egraph::{EMatch, Pattern, PatternNode};

use super::node::{SemEGraph, SemNode};
use super::pattern::CompiledIselPattern;
use super::theory::enabled_axioms;

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
    let roots = |kind: SymKind| {
        patterns.iter().any(|compiled| {
            matches!(
                compiled.pattern.node(compiled.pattern.root()),
                PatternNode::Node(node) if node.kind == kind
            )
        })
    };
    enabled_axioms(roots)
        .into_iter()
        .map(|axiom| axiom.compile())
        .collect()
}
