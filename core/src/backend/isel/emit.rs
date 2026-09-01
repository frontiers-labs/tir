use std::collections::{HashMap, HashSet};

use tir::{
    BlockId, Context, OpId, TypeId, ValueId, analysis::DominatorTree,
    sem::egraph::class_int_binding,
};
use tir_relational::ClassId as Id;

use super::{
    FunctionSelection, RuleMatch,
    builder::AuxSlot,
    cover::{BoundaryDemand, PbqpIselMatch},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct BlockPlan {
    pub(crate) schedule: Vec<ScheduledEmit>,
    pub(crate) erase_ops: Vec<OpId>,
    pub(crate) value_remaps: Vec<(ValueId, ValueId)>,
    /// What this block leaves a destruction to read: the branch each test selected
    /// into, and the register a counter's advance landed in.
    pub(crate) aux: Vec<(OpId, AuxSlot, AuxEmit)>,
}

/// What a destruction emits for one of a region-carrying operation's values.
#[derive(Clone, Debug)]
pub(crate) enum AuxEmit {
    Branch(GuardBranch),
    Value(ValueId),
    /// A test the block's assumptions already decided: the edge it picks is
    /// taken unconditionally, and nothing is computed or branched on.
    Decided(bool),
}

#[derive(Clone, Debug)]
pub(crate) struct ScheduledEmit {
    pub(crate) rule_index: usize,
    pub(crate) m: RuleMatch,
    pub(crate) source_op: Option<OpId>,
    /// The state ports of the access this tile covers, where it covers one.
    pub(crate) state: Option<super::StatePorts>,
    pub(crate) results: Vec<ValueId>,
    pub(crate) result_ty: Option<TypeId>,
}

/// How a destruction's branch tests its condition: fused into a selected
/// conditional-branch instruction, or the target's branch-if-nonzero over the
/// materialized condition.
#[derive(Clone, Debug)]
pub(crate) enum GuardBranch {
    Fused { rule_index: usize, m: RuleMatch },
    Nonzero { condition: ValueId },
}

/// The order the cover's tiles are emitted in: a topological order of the
/// registers they pass each other, ties broken by `rank` — the place of the
/// operation each tile is rooted at, and `None` for one this block roots at no
/// operation of its own, which is a pure value and goes first.
///
/// This is a reference order, not the block's: commit merges the surviving
/// operations into it and derives the block's own order from the whole
/// dependence graph. What it has to be is an order the values admit, because
/// anti- and output edges are read off it.
pub(crate) fn order_tiles(
    egraph: &super::SemEGraph,
    matches: &[PbqpIselMatch],
    selected: &HashMap<Id, usize>,
    rank: impl Fn(Id) -> Option<usize>,
) -> Option<Vec<(Id, usize)>> {
    let mut dependencies: HashMap<Id, HashSet<Id>> = HashMap::new();
    for (&class, &match_id) in selected {
        for binding in &matches[match_id].bindings.pattern_nodes {
            let child = egraph.find(binding.class);
            if child != class
                && binding.is_boundary
                && binding.demand == BoundaryDemand::Register
                && selected.contains_key(&child)
            {
                dependencies.entry(class).or_default().insert(child);
            }
        }
    }
    let mut emitted = HashSet::new();
    let mut order = Vec::with_capacity(selected.len());
    while order.len() < selected.len() {
        let class = selected
            .keys()
            .copied()
            .filter(|class| !emitted.contains(class))
            .filter(|class| {
                dependencies
                    .get(class)
                    .is_none_or(|deps| deps.is_subset(&emitted))
            })
            .min_by_key(|class| (rank(*class), *class))?;
        emitted.insert(class);
        order.push((class, selected[&class]));
    }
    Some(order)
}

pub(crate) fn resolve_match(
    fs: &FunctionSelection,
    dom: &DominatorTree,
    context: &Context,
    block: BlockId,
    consumer: OpId,
    matched: &PbqpIselMatch,
    destinations: &HashMap<Id, ValueId>,
) -> RuleMatch {
    let mut ints = Vec::new();
    let mut values = Vec::new();
    for (symbol, class) in &matched.bindings.captures.entries {
        let class = fs.egraph.find(*class);
        if let Some(value) = class_int_binding(&fs.egraph, class) {
            ints.push((*symbol, value));
        }
        // A low-extract capture reads its chased source's register, which may
        // be defined by a tile scheduled in this block.
        let chased = fs.chase_low_extract(class);
        if let Some(value) = destinations.get(&chased).copied().or_else(|| {
            fs.resolve_binding(dom, context, class, block, consumer, false)
                .value
        }) {
            values.push((*symbol, value));
        }
    }
    RuleMatch::new(ints, values)
}
