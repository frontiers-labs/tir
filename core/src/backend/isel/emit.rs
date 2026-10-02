use std::collections::{HashMap, HashSet};

use tir::{Context, OpId, TypeId, ValueId};
use tir_relational::ClassId as Id;

use super::{
    FunctionSelection, RuleMatch,
    builder::ControlSlot,
    cover::{BoundaryDemand, PbqpIselMatch},
    problem::{HasRegister, RegionProblem},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct RegionPlan {
    /// The region's operations in the order they were solved in: the place
    /// each tile's root held, which the tile inherits.
    pub(crate) order: Vec<OpId>,
    pub(crate) schedule: Vec<ScheduledEmit>,
    pub(crate) erase_ops: Vec<OpId>,
    pub(crate) value_remaps: Vec<(ValueId, ValueId)>,
    /// What this region leaves a destruction to read: the branch each test
    /// selected into.
    pub(crate) aux: Vec<(OpId, ControlSlot, AuxEmit)>,
}

/// What a destruction emits for one of a structured operation's tests.
#[derive(Clone, Debug)]
pub(crate) enum AuxEmit {
    Branch(GuardBranch),
    /// A test the region's assumptions already decided: the edge it picks is
    /// taken unconditionally, and nothing is computed or branched on.
    Decided(bool),
}

#[derive(Clone, Debug)]
pub(crate) struct ScheduledEmit {
    pub(crate) rule_index: usize,
    pub(crate) m: RuleMatch,
    pub(crate) source_op: Option<OpId>,
    /// The resource state ports of the operations this tile covers.
    pub(crate) states: Vec<super::StatePorts>,
    /// The value each result stands for; `None` for one nothing reads.
    pub(crate) results: Vec<Option<ValueId>>,
    pub(crate) result_ty: Option<TypeId>,
}

/// How a destruction's branch tests its condition: fused into a selected
/// conditional-branch instruction, or the target's branch-if-nonzero over the
/// materialized condition.
#[derive(Clone, Debug)]
pub(crate) enum GuardBranch {
    Fused {
        rule_index: usize,
        m: RuleMatch,
        inverse: Option<(usize, RuleMatch)>,
    },
    Nonzero {
        condition: ValueId,
    },
}

/// The order the cover's tiles are emitted in: a topological order of the
/// registers they pass each other, ties broken by `rank` — the place of the
/// operation each tile is rooted at, and `None` for one this block roots at no
/// operation of its own, which is a pure value and goes first. `None` when the
/// tiles read each other's registers in a cycle.
///
/// This is a reference order, not the block's: commit merges the surviving
/// operations into it and derives the block's own order from the whole
/// dependence graph. What it has to be is an order the values admit, because
/// anti- and output edges are read off it.
pub(crate) fn order_tiles(
    matches: &[PbqpIselMatch],
    selected: &HashMap<Id, usize>,
    rank: impl Fn(Id) -> Option<usize>,
) -> Option<Vec<(Id, usize)>> {
    let mut dependencies: HashMap<Id, HashSet<Id>> = HashMap::new();
    for (&class, &match_id) in selected {
        for binding in &matches[match_id].bindings.pattern_nodes {
            let child = binding.class;
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

/// Bind a selected match's operands: the constant a class is proven to be,
/// and the register holding it — one a tile of this region defines, or one
/// that already exists where `consumer` reads it.
pub(crate) fn resolve_match(
    fs: &FunctionSelection,
    context: &Context,
    problem: &RegionProblem,
    consumer: Option<OpId>,
    matched: &PbqpIselMatch,
    destinations: &HashMap<Id, ValueId>,
    has_register: HasRegister,
) -> RuleMatch {
    let mut ints = Vec::new();
    let mut values = Vec::new();
    for (symbol, class) in &matched.bindings.captures.entries {
        let facts = problem.facts(*class);
        if let Some(value) = &facts.int {
            ints.push((*symbol, value.clone()));
        }
        // A low-extract capture reads its chased source's register, which may
        // be defined by a tile scheduled in this region, unless the view was
        // tiled in its own right.
        if let Some(value) = destinations
            .get(class)
            .or_else(|| destinations.get(&facts.source))
            .copied()
            .or_else(|| {
                fs.register_value(
                    context,
                    &facts.binding_members,
                    problem.region,
                    consumer,
                    false,
                    has_register,
                )
            })
        {
            values.push((*symbol, value));
        }
    }
    RuleMatch::new(ints, values)
}
