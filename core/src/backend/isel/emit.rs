use std::collections::{HashMap, HashSet};

use tir::{BlockId, Context, OpId, TypeId, ValueId, analysis::DominatorTree};
use tir_symbolic::egraph::Id;

use super::{
    FunctionSelection, RuleMatch,
    cover::{BoundaryDemand, PbqpIselMatch},
    node::{class_int_binding, class_is_pure},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct BlockPlan {
    pub(crate) schedule: Vec<ScheduledEmit>,
    pub(crate) erase_ops: Vec<OpId>,
    pub(crate) value_remaps: Vec<(ValueId, ValueId)>,
    pub(crate) terminators: Vec<TerminatorPlan>,
}

#[derive(Clone, Debug)]
pub(crate) struct ScheduledEmit {
    pub(crate) rule_index: usize,
    pub(crate) m: RuleMatch,
    pub(crate) source_op: Option<OpId>,
    /// The original op to insert this instruction before: the source op's
    /// position, pulled earlier for pure tiles whose consumers precede it
    /// (a constant read by an earlier op through class merging), so surviving
    /// mid-block ops (calls) still read their operands in order. `None` inserts
    /// before the terminator.
    pub(crate) anchor: Option<OpId>,
    pub(crate) results: Vec<ValueId>,
    pub(crate) result_ty: Option<TypeId>,
}

#[derive(Clone, Debug)]
pub(crate) enum TerminatorPlan {
    Guard {
        op: OpId,
        branch: GuardBranch,
        true_dest: BlockId,
        true_args: Vec<ValueId>,
        false_dest: BlockId,
        false_args: Vec<ValueId>,
    },
    Jump {
        op: OpId,
        dest: BlockId,
        args: Vec<ValueId>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum GuardBranch {
    Fused { rule_index: usize, m: RuleMatch },
    Nonzero { condition: ValueId },
}

pub(crate) fn schedule_tiles(
    egraph: &super::SemEGraph,
    matches: &[PbqpIselMatch],
    selected: &HashMap<Id, usize>,
    positions: &HashMap<Id, usize>,
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
    let mut effects: Vec<_> = selected
        .iter()
        .filter(|(_, matched)| {
            matches[**matched]
                .bindings
                .pattern_nodes
                .iter()
                .any(|binding| !binding.is_boundary && !class_is_pure(egraph, binding.class))
        })
        .map(|(&class, _)| class)
        .collect();
    effects.sort_by_key(|class| (positions.get(class).copied(), *class));
    for pair in effects.windows(2) {
        dependencies.entry(pair[1]).or_default().insert(pair[0]);
    }

    let mut emitted = HashSet::new();
    let mut schedule = Vec::with_capacity(selected.len());
    while schedule.len() < selected.len() {
        let class = selected
            .keys()
            .copied()
            .filter(|class| !emitted.contains(class))
            .filter(|class| {
                dependencies
                    .get(class)
                    .is_none_or(|deps| deps.is_subset(&emitted))
            })
            .min_by_key(|class| (positions.get(class).copied(), *class))?;
        emitted.insert(class);
        schedule.push((class, selected[&class]));
    }
    Some(schedule)
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
