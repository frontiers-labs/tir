//! Placement under constraints.
//!
//! A [`Problem`] is a set of [`WorkItem`]s, one [`Domain`] of slots each may
//! take, [`Preceded`] edges between them, [`Capacity`] limits over slots, and a
//! [`CostFn`] scoring a whole [`Placement`]. [`solve`] returns the admissible
//! placement of least cost, ties broken by the enumeration order — so the same
//! problem always gives the same answer.
//!
//! Nothing here names an instruction, a loop, a register or a cycle. A slot is
//! whatever the caller decided a slot is.
//!
//! # What the types are meant to express
//!
//! The contract has to hold the instances that are not written yet, or it is not
//! a contract. Each of these is expressible in the types above as they stand:
//!
//! - **Scheduling.** Items are operations, slots are cycles, `Preceded` carries
//!   the latency between a producer and its consumer, `Capacity` is an issue
//!   width over the slots of one cycle.
//! - **Loop scheduling** (the first instance). One item per nest, slots are the
//!   `(permutation, tiling)` pairs the dependence vectors admit, and the cost is
//!   the locality of the placement. One item needs no edges — which is why the
//!   two below are spelled out here rather than discovered later.
//! - **Fusion.** Two nests are two items whose slots are the loop levels they
//!   are placed at; a `Preceded` between them with `distance` the inter-nest
//!   dependence distance says how far apart the levels must stay. Fusing is the
//!   placement that puts them at the same level, which the edge admits exactly
//!   when the distance is zero.
//! - **Skewing.** The slots of a nest item are affine maps rather than
//!   permutations. Nothing in these types reads a slot's contents, so a wider
//!   slot alphabet costs the engine nothing; only the caller's legality filter
//!   and cost function grow.
//!
//! If an instance ever needs a constraint none of the four types state, that is
//! a contract change, and it is made here rather than worked around in a caller.

/// One position a work item may be placed at. What it means is the caller's.
pub type SlotId = usize;

/// A work item, named by its index in [`Problem::work`].
pub type ItemId = usize;

/// One thing to place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WorkItem {
    pub id: ItemId,
}

/// The slots `item` may take, in the order they are tried.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Domain {
    pub item: ItemId,
    pub slots: Vec<SlotId>,
}

/// `after` may take no slot below `before`'s plus `distance`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Preceded {
    pub before: ItemId,
    pub after: ItemId,
    pub distance: i64,
}

/// At most `limit` items may take slots from `slots`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Capacity {
    pub slots: Vec<SlotId>,
    pub limit: usize,
}

/// What a whole placement costs. Lower is better.
pub type CostFn = Box<dyn Fn(&Placement) -> i64>;

/// Where every work item went, indexed by [`ItemId`].
pub type Placement = Vec<SlotId>;

pub struct Problem {
    pub work: Vec<WorkItem>,
    pub domain: Vec<Domain>,
    pub precedence: Vec<Preceded>,
    pub capacity: Vec<Capacity>,
    pub cost: CostFn,
}

/// How many placements the search looks at before it gives up and reports that
/// it could not decide. v1 enumerates; a problem too big to enumerate wants a
/// better search behind this same contract, not a bigger budget here.
pub const BUDGET: usize = 1 << 20;

/// The admissible placement of least cost, or `None` where the problem admits
/// none — an empty domain, constraints nothing satisfies, or a search space
/// past [`BUDGET`].
pub fn solve(problem: &Problem) -> Option<Placement> {
    let domains: Vec<&[SlotId]> = problem
        .work
        .iter()
        .map(|item| slots_of(problem, item.id))
        .collect();
    if domains.iter().any(|slots| slots.is_empty()) {
        return None;
    }
    if domains
        .iter()
        .try_fold(1usize, |total, slots| total.checked_mul(slots.len()))
        .is_none_or(|total| total > BUDGET)
    {
        return None;
    }

    let mut placement = Vec::with_capacity(domains.len());
    let mut best: Option<(i64, Placement)> = None;
    search(problem, &domains, &mut placement, &mut best);
    best.map(|(_, placement)| placement)
}

/// Extend `placement` by one item, in domain order, keeping the first placement
/// of least cost. The prefix checks prune a branch as soon as an edge it already
/// violates is decided.
fn search(
    problem: &Problem,
    domains: &[&[SlotId]],
    placement: &mut Placement,
    best: &mut Option<(i64, Placement)>,
) {
    let Some(&slots) = domains.get(placement.len()) else {
        let cost = (problem.cost)(placement);
        if best.as_ref().is_none_or(|(lowest, _)| cost < *lowest) {
            *best = Some((cost, placement.clone()));
        }
        return;
    };
    for &slot in slots {
        placement.push(slot);
        if admissible(problem, placement) {
            search(problem, domains, placement, best);
        }
        placement.pop();
    }
}

/// Whether every constraint both of whose ends are decided holds.
fn admissible(problem: &Problem, placement: &Placement) -> bool {
    let placed = |item: ItemId| placement.get(item).copied();
    let precedence =
        problem
            .precedence
            .iter()
            .all(|edge| match (placed(edge.before), placed(edge.after)) {
                (Some(before), Some(after)) => after as i64 >= before as i64 + edge.distance,
                _ => true,
            });
    precedence
        && problem.capacity.iter().all(|capacity| {
            placement
                .iter()
                .filter(|slot| capacity.slots.contains(slot))
                .count()
                <= capacity.limit
        })
}

fn slots_of(problem: &Problem, item: ItemId) -> &[SlotId] {
    problem
        .domain
        .iter()
        .find(|domain| domain.item == item)
        .map_or(&[], |domain| &domain.slots)
}

#[cfg(test)]
mod tests {
    //! The engine, on problems that name nothing about the IR.

    use super::*;

    fn problem(domains: Vec<Vec<usize>>, cost: CostFn) -> Problem {
        Problem {
            work: (0..domains.len()).map(|id| WorkItem { id }).collect(),
            domain: domains
                .into_iter()
                .enumerate()
                .map(|(item, slots)| Domain { item, slots })
                .collect(),
            precedence: Vec::new(),
            capacity: Vec::new(),
            cost,
        }
    }

    #[test]
    fn the_cheapest_placement_wins() {
        let solved = solve(&problem(
            vec![vec![0, 1, 2]],
            Box::new(|placement| -(placement[0] as i64)),
        ));
        assert_eq!(solved, Some(vec![2]));
    }

    /// Every item's domain is enumerated in the order it was given, and a tie is
    /// broken by the first placement reached — so the answer never depends on
    /// iteration order elsewhere.
    #[test]
    fn ties_break_by_enumeration_order() {
        let flat: CostFn = Box::new(|_| 0);
        assert_eq!(
            solve(&problem(vec![vec![3, 1, 2], vec![7, 5]], flat)),
            Some(vec![3, 7])
        );
    }

    #[test]
    fn precedence_prunes_what_it_forbids() {
        let mut p = problem(
            vec![vec![0, 1, 2, 3], vec![0, 1, 2, 3]],
            Box::new(|placement| placement.iter().map(|&slot| slot as i64).sum()),
        );
        p.precedence.push(Preceded {
            before: 0,
            after: 1,
            distance: 2,
        });
        // Cost pulls both to zero; the edge holds the second two slots above the
        // first, and the cheapest pair that survives is (0, 2).
        assert_eq!(solve(&p), Some(vec![0, 2]));
    }

    #[test]
    fn capacity_limits_what_may_share_slots() {
        let mut p = problem(
            vec![vec![0, 1], vec![0, 1]],
            Box::new(|placement| placement.iter().map(|&slot| slot as i64).sum()),
        );
        p.capacity.push(Capacity {
            slots: vec![0],
            limit: 1,
        });
        assert_eq!(solve(&p), Some(vec![0, 1]));
    }

    #[test]
    fn an_empty_domain_has_no_placement() {
        assert_eq!(solve(&problem(vec![vec![]], Box::new(|_| 0))), None);
        assert_eq!(solve(&problem(vec![], Box::new(|_| 0))), Some(vec![]));
    }

    #[test]
    fn a_space_past_the_budget_is_refused() {
        let wide: Vec<Vec<usize>> = (0..8).map(|_| (0..64).collect()).collect();
        assert_eq!(solve(&problem(wide, Box::new(|_| 0))), None);
    }
}
