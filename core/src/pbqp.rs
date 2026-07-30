use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const INF_COST: u64 = u64::MAX / 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PbqpNodeId(u32);

impl PbqpNodeId {
    pub fn from_index(index: usize) -> Self {
        Self(index as u32)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PbqpAlternative {
    pub node: PbqpNodeId,
    pub alternative: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PbqpMatrix {
    rows: usize,
    cols: usize,
    costs: Vec<u64>,
}

impl PbqpMatrix {
    pub fn new(rows: usize, cols: usize, costs: Vec<u64>) -> Self {
        assert_eq!(rows * cols, costs.len(), "invalid PBQP matrix shape");
        Self { rows, cols, costs }
    }

    pub fn zero(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            costs: vec![0; rows * cols],
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn get(&self, row: usize, col: usize) -> u64 {
        self.costs[row * self.cols + col]
    }

    pub fn set(&mut self, row: usize, col: usize, cost: u64) {
        self.costs[row * self.cols + col] = cost;
    }

    fn add_assign(&mut self, row: usize, col: usize, cost: u64) {
        let idx = row * self.cols + col;
        self.costs[idx] = add_cost(self.costs[idx], cost);
    }

    fn is_zero(&self) -> bool {
        self.costs.iter().all(|&cost| cost == 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PbqpProblem {
    node_costs: Vec<Vec<u64>>,
    edges: BTreeMap<(usize, usize), PbqpMatrix>,
    coherence_sets: Vec<Vec<PbqpAlternative>>,
}

impl PbqpProblem {
    pub fn new() -> Self {
        Self {
            node_costs: Vec::new(),
            edges: BTreeMap::new(),
            coherence_sets: Vec::new(),
        }
    }

    pub fn add_node(&mut self, costs: Vec<u64>) -> PbqpNodeId {
        assert!(!costs.is_empty(), "PBQP node must have alternatives");
        let id = PbqpNodeId::from_index(self.node_costs.len());
        self.node_costs.push(costs);
        id
    }

    pub fn add_edge(&mut self, lhs: PbqpNodeId, rhs: PbqpNodeId, matrix: PbqpMatrix) {
        assert_ne!(lhs, rhs, "PBQP self-edges are not supported");
        let (a, b, matrix) = orient_matrix(lhs, rhs, matrix);
        assert_eq!(self.node_costs[a].len(), matrix.rows());
        assert_eq!(self.node_costs[b].len(), matrix.cols());

        self.edges
            .entry((a, b))
            .and_modify(|existing| {
                for row in 0..existing.rows() {
                    for col in 0..existing.cols() {
                        existing.add_assign(row, col, matrix.get(row, col));
                    }
                }
            })
            .or_insert(matrix);
    }

    pub fn add_coherence_set(&mut self, alternatives: Vec<PbqpAlternative>) {
        if alternatives.len() > 1 {
            self.coherence_sets.push(alternatives);
        }
    }

    pub fn node_count(&self) -> usize {
        self.node_costs.len()
    }
}

impl Default for PbqpProblem {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PbqpSolution {
    pub choices: Vec<usize>,
    pub total_cost: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PbqpSolveError {
    Infeasible { node: PbqpNodeId },
    InvalidProblem(String),
}

#[derive(Clone, Debug)]
enum Reduction {
    Fixed {
        node: usize,
        alternative: usize,
    },
    R1 {
        node: usize,
        neighbor: usize,
        choices_by_neighbor_alt: Vec<Option<usize>>,
    },
    R2 {
        node: usize,
        left: usize,
        right: usize,
        right_alternatives: usize,
        choices_by_neighbor_alts: Vec<Option<usize>>,
    },
}

enum Undo {
    NodeCost {
        node: usize,
        alternative: usize,
        old_cost: u64,
    },
    EdgeAdded {
        lhs: usize,
        rhs: usize,
    },
    EdgeRemoved {
        lhs: usize,
        rhs: usize,
        matrix: PbqpMatrix,
    },
    EdgeChanged {
        lhs: usize,
        rhs: usize,
        old_matrix: PbqpMatrix,
    },
    NodeDeactivated {
        node: usize,
    },
}

#[derive(Clone, Copy)]
struct Checkpoint {
    undo_len: usize,
    reductions_len: usize,
}

pub fn solve(problem: &PbqpProblem) -> Result<PbqpSolution, PbqpSolveError> {
    Solver::new(problem.clone()).solve(problem)
}

struct Solver {
    problem: PbqpProblem,
    active: Vec<bool>,
    active_count: usize,
    reductions: Vec<Reduction>,
    /// Per-node neighbor set, maintained alongside `problem.edges` so neighbor
    /// queries are O(degree) rather than a full O(edges) scan — the difference
    /// between the solver being usable and unusable at register-allocation scale.
    adjacency: Vec<BTreeSet<usize>>,
    degrees: Vec<usize>,
    reducible: BTreeSet<usize>,
    finite_alternatives: Vec<usize>,
    coherence_groups_by_alternative: Vec<Vec<Vec<usize>>>,
    infeasible: BTreeSet<usize>,
    recording_undo: bool,
    undo: Vec<Undo>,
}

impl Solver {
    fn new(problem: PbqpProblem) -> Self {
        let node_count = problem.node_count();
        let active = vec![true; node_count];
        let mut adjacency = vec![BTreeSet::new(); node_count];
        for &(a, b) in problem.edges.keys() {
            adjacency[a].insert(b);
            adjacency[b].insert(a);
        }
        let degrees: Vec<_> = adjacency.iter().map(BTreeSet::len).collect();
        let reducible = degrees
            .iter()
            .enumerate()
            .filter_map(|(node, &degree)| (degree <= 2).then_some(node))
            .collect();
        let finite_alternatives: Vec<usize> = problem
            .node_costs
            .iter()
            .map(|costs| costs.iter().filter(|&&cost| cost < INF_COST).count())
            .collect();
        let infeasible = finite_alternatives
            .iter()
            .enumerate()
            .filter_map(|(node, &finite)| (finite == 0).then_some(node))
            .collect();
        let mut coherence_groups_by_alternative: Vec<Vec<Vec<usize>>> = problem
            .node_costs
            .iter()
            .map(|costs| vec![Vec::new(); costs.len()])
            .collect();
        for (group_index, group) in problem.coherence_sets.iter().enumerate() {
            for alternative in group {
                let node = alternative.node.index();
                if let Some(groups) = coherence_groups_by_alternative
                    .get_mut(node)
                    .and_then(|alternatives| alternatives.get_mut(alternative.alternative))
                {
                    groups.push(group_index);
                }
            }
        }
        Self {
            problem,
            active,
            active_count: node_count,
            reductions: Vec::new(),
            adjacency,
            degrees,
            reducible,
            finite_alternatives,
            coherence_groups_by_alternative,
            infeasible,
            recording_undo: false,
            undo: Vec::new(),
        }
    }

    fn solve(mut self, original: &PbqpProblem) -> Result<PbqpSolution, PbqpSolveError> {
        self.validate()?;

        // Normalize costs and propagate impossible alternatives once, up front. This
        // is where INF node costs (pre-coloring) and coherence-set fragments get
        // pruned. Re-running the global propagation after every reduction is
        // quadratic in the accumulated INF entries and dominated solve time at
        // register-allocation scale; the per-node reductions below already respect
        // INF through saturating cost arithmetic, so a single pass suffices.
        self.normalize_and_propagate()?;
        self.undo.clear();

        self.solve_prepared(original)
    }

    fn solve_prepared(&mut self, original: &PbqpProblem) -> Result<PbqpSolution, PbqpSolveError> {
        while self.active_count > 0 {
            let node = self
                .next_active_node()
                .expect("an active PBQP node must be available");
            match self.degree(node) {
                0 => self.reduce_fixed(node)?,
                1 => self.reduce_r1(node)?,
                2 => self.reduce_r2(node)?,
                _ => return self.solve_rn(node, original),
            }
        }

        let choices = self.reconstruct()?;
        let total_cost = evaluate_solution(original, &choices)?;
        Ok(PbqpSolution {
            choices,
            total_cost,
        })
    }

    fn validate(&self) -> Result<(), PbqpSolveError> {
        for (node, costs) in self.problem.node_costs.iter().enumerate() {
            if costs.is_empty() {
                return Err(PbqpSolveError::InvalidProblem(format!(
                    "node {node} has no alternatives"
                )));
            }
        }

        for alternatives in &self.problem.coherence_sets {
            for alternative in alternatives {
                let node = alternative.node.index();
                if node >= self.problem.node_costs.len()
                    || alternative.alternative >= self.problem.node_costs[node].len()
                {
                    return Err(PbqpSolveError::InvalidProblem(
                        "coherence set references an unknown alternative".to_string(),
                    ));
                }
            }
        }

        Ok(())
    }

    fn normalize_and_propagate(&mut self) -> Result<(), PbqpSolveError> {
        loop {
            let normalized = self.normalize_edges();
            self.rebuild_infeasible();
            let propagated = self.propagate_infinities();
            self.ensure_feasible()?;
            if !normalized && !propagated {
                return Ok(());
            }
        }
    }

    fn normalize_edges(&mut self) -> bool {
        let mut changed = false;
        let keys: Vec<_> = self.problem.edges.keys().copied().collect();
        let mut zero_edges = Vec::new();

        for key @ (lhs, rhs) in keys {
            if !self.active[lhs] || !self.active[rhs] {
                continue;
            }

            let matrix = self.problem.edges.get_mut(&key).unwrap();
            for row in 0..matrix.rows() {
                if self.problem.node_costs[lhs][row] >= INF_COST {
                    for col in 0..matrix.cols() {
                        matrix.set(row, col, INF_COST);
                    }
                    continue;
                }

                let min = (0..matrix.cols())
                    .map(|col| matrix.get(row, col))
                    .min()
                    .unwrap_or(INF_COST);
                if min >= INF_COST {
                    add_tracked_cost(
                        &mut self.problem.node_costs[lhs][row],
                        &mut self.finite_alternatives[lhs],
                        INF_COST,
                    );
                    changed = true;
                } else if min > 0 {
                    add_tracked_cost(
                        &mut self.problem.node_costs[lhs][row],
                        &mut self.finite_alternatives[lhs],
                        min,
                    );
                    for col in 0..matrix.cols() {
                        let cost = matrix.get(row, col);
                        if cost < INF_COST {
                            matrix.set(row, col, cost - min);
                        }
                    }
                    changed = true;
                }
            }

            for col in 0..matrix.cols() {
                if self.problem.node_costs[rhs][col] >= INF_COST {
                    for row in 0..matrix.rows() {
                        matrix.set(row, col, INF_COST);
                    }
                    continue;
                }

                let min = (0..matrix.rows())
                    .map(|row| matrix.get(row, col))
                    .min()
                    .unwrap_or(INF_COST);
                if min >= INF_COST {
                    add_tracked_cost(
                        &mut self.problem.node_costs[rhs][col],
                        &mut self.finite_alternatives[rhs],
                        INF_COST,
                    );
                    changed = true;
                } else if min > 0 {
                    add_tracked_cost(
                        &mut self.problem.node_costs[rhs][col],
                        &mut self.finite_alternatives[rhs],
                        min,
                    );
                    for row in 0..matrix.rows() {
                        let cost = matrix.get(row, col);
                        if cost < INF_COST {
                            matrix.set(row, col, cost - min);
                        }
                    }
                    changed = true;
                }
            }

            if matrix.is_zero() {
                zero_edges.push(key);
            }
        }

        for key in zero_edges {
            self.remove_edge(key.0, key.1);
            changed = true;
        }

        changed
    }

    fn propagate_infinities(&mut self) -> bool {
        let mut queue: VecDeque<PbqpAlternative> = self
            .problem
            .node_costs
            .iter()
            .enumerate()
            .flat_map(|(node, costs)| {
                costs
                    .iter()
                    .enumerate()
                    .filter(|(_, cost)| **cost >= INF_COST)
                    .map(move |(alternative, _)| PbqpAlternative {
                        node: PbqpNodeId::from_index(node),
                        alternative,
                    })
            })
            .collect();
        self.propagate_queue(&mut queue)
    }

    fn propagate_queue(&mut self, queue: &mut VecDeque<PbqpAlternative>) -> bool {
        let mut changed = false;

        while let Some(impossible) = queue.pop_front() {
            let node = impossible.node.index();
            let group_indices =
                self.coherence_groups_by_alternative[node][impossible.alternative].clone();
            for group_index in group_indices {
                let coherent_members = self.problem.coherence_sets[group_index].clone();
                for member in coherent_members {
                    if self.mark_impossible(member) {
                        queue.push_back(member);
                        changed = true;
                    }
                }
            }

            for neighbor in self.neighbors(node) {
                for alternative in 0..self.problem.node_costs[neighbor].len() {
                    let candidate = PbqpAlternative {
                        node: PbqpNodeId::from_index(neighbor),
                        alternative,
                    };
                    if self.problem.node_costs[neighbor][alternative] >= INF_COST {
                        continue;
                    }
                    if !self.has_supported_pair(candidate, node) && self.mark_impossible(candidate)
                    {
                        queue.push_back(candidate);
                        changed = true;
                    }
                }
            }
        }

        changed
    }

    fn propagate_new_infinities(
        &mut self,
        mut queue: VecDeque<PbqpAlternative>,
    ) -> Result<(), PbqpSolveError> {
        self.propagate_queue(&mut queue);
        self.ensure_feasible()
    }

    fn has_supported_pair(&self, alternative: PbqpAlternative, neighbor: usize) -> bool {
        let node = alternative.node.index();
        (0..self.problem.node_costs[neighbor].len()).any(|neighbor_alt| {
            self.problem.node_costs[neighbor][neighbor_alt] < INF_COST
                && self.edge_cost(node, alternative.alternative, neighbor, neighbor_alt) < INF_COST
        })
    }

    fn mark_impossible(&mut self, alternative: PbqpAlternative) -> bool {
        let node = alternative.node.index();
        let old_cost = self.problem.node_costs[node][alternative.alternative];
        if old_cost >= INF_COST {
            return false;
        }
        if self.recording_undo {
            self.undo.push(Undo::NodeCost {
                node,
                alternative: alternative.alternative,
                old_cost,
            });
        }
        self.problem.node_costs[node][alternative.alternative] = INF_COST;
        self.finite_alternatives[node] -= 1;
        self.refresh_feasibility(node);
        true
    }

    fn add_node_cost(&mut self, node: usize, alternative: usize, cost: u64) -> bool {
        let old_cost = self.problem.node_costs[node][alternative];
        let new_cost = add_cost(old_cost, cost);
        if new_cost == old_cost {
            return false;
        }
        if self.recording_undo {
            self.undo.push(Undo::NodeCost {
                node,
                alternative,
                old_cost,
            });
        }
        self.problem.node_costs[node][alternative] = new_cost;
        if old_cost < INF_COST && new_cost >= INF_COST {
            self.finite_alternatives[node] -= 1;
            self.refresh_feasibility(node);
            true
        } else {
            false
        }
    }

    fn ensure_feasible(&self) -> Result<(), PbqpSolveError> {
        if let Some(&node) = self.infeasible.first() {
            return Err(PbqpSolveError::Infeasible {
                node: PbqpNodeId::from_index(node),
            });
        }
        Ok(())
    }

    fn next_active_node(&self) -> Option<usize> {
        self.reducible
            .first()
            .copied()
            .or_else(|| self.active.iter().position(|active| *active))
    }

    fn degree(&self, node: usize) -> usize {
        self.degrees[node]
    }

    fn neighbors(&self, node: usize) -> Vec<usize> {
        self.adjacency[node].iter().copied().collect()
    }

    fn reduce_fixed(&mut self, node: usize) -> Result<(), PbqpSolveError> {
        let alternative = self.cheapest_alternative(node)?;
        self.reductions.push(Reduction::Fixed { node, alternative });
        self.deactivate(node);
        Ok(())
    }

    fn reduce_r1(&mut self, node: usize) -> Result<(), PbqpSolveError> {
        let neighbor = self.neighbors(node)[0];
        let mut choices = vec![None; self.problem.node_costs[neighbor].len()];
        let mut impossible = VecDeque::new();

        for (neighbor_alt, choice) in choices.iter_mut().enumerate() {
            let mut best = INF_COST;
            let mut best_alt = None;
            for node_alt in 0..self.problem.node_costs[node].len() {
                let cost = add_cost(
                    self.problem.node_costs[node][node_alt],
                    self.edge_cost(node, node_alt, neighbor, neighbor_alt),
                );
                if cost < best {
                    best = cost;
                    best_alt = Some(node_alt);
                }
            }
            if self.add_node_cost(neighbor, neighbor_alt, best) {
                impossible.push_back(PbqpAlternative {
                    node: PbqpNodeId::from_index(neighbor),
                    alternative: neighbor_alt,
                });
            }
            *choice = best_alt;
        }

        self.remove_incident_edges(node);
        self.deactivate(node);
        self.reductions.push(Reduction::R1 {
            node,
            neighbor,
            choices_by_neighbor_alt: choices,
        });
        self.propagate_new_infinities(impossible)
    }

    fn reduce_r2(&mut self, node: usize) -> Result<(), PbqpSolveError> {
        let mut neighbors = self.neighbors(node);
        neighbors.sort_unstable();
        let left = neighbors[0];
        let right = neighbors[1];
        let mut folded = PbqpMatrix::zero(
            self.problem.node_costs[left].len(),
            self.problem.node_costs[right].len(),
        );
        let mut choices = vec![None; folded.rows() * folded.cols()];

        for left_alt in 0..folded.rows() {
            for right_alt in 0..folded.cols() {
                let mut best = INF_COST;
                let mut best_alt = None;
                for node_alt in 0..self.problem.node_costs[node].len() {
                    let left_cost = self.edge_cost(left, left_alt, node, node_alt);
                    let right_cost = self.edge_cost(node, node_alt, right, right_alt);
                    let cost = add_cost(
                        self.problem.node_costs[node][node_alt],
                        add_cost(left_cost, right_cost),
                    );
                    if cost < best {
                        best = cost;
                        best_alt = Some(node_alt);
                    }
                }
                folded.set(left_alt, right_alt, best);
                choices[left_alt * folded.cols() + right_alt] = best_alt;
            }
        }

        self.remove_incident_edges(node);
        self.deactivate(node);
        self.add_or_accumulate_edge(left, right, folded);
        self.reductions.push(Reduction::R2 {
            node,
            left,
            right,
            right_alternatives: self.problem.node_costs[right].len(),
            choices_by_neighbor_alts: choices,
        });
        let mut impossible = VecDeque::new();
        self.prune_unsupported_alternatives(left, right, &mut impossible);
        self.propagate_new_infinities(impossible)
    }

    fn solve_rn(
        &mut self,
        node: usize,
        original: &PbqpProblem,
    ) -> Result<PbqpSolution, PbqpSolveError> {
        let alternatives = self.locally_ordered_alternatives(node)?;
        self.recording_undo = true;
        let checkpoint = self.checkpoint();
        for alternative in alternatives {
            self.rollback(checkpoint);
            if let Err(PbqpSolveError::Infeasible { .. }) = self.reduce_rn(node, alternative) {
                continue;
            }
            match self.solve_prepared(original) {
                Ok(solution) => return Ok(solution),
                Err(PbqpSolveError::Infeasible { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        self.rollback(checkpoint);
        Err(PbqpSolveError::Infeasible {
            node: PbqpNodeId::from_index(node),
        })
    }

    fn reduce_rn(&mut self, node: usize, alternative: usize) -> Result<(), PbqpSolveError> {
        let mut impossible = VecDeque::new();
        for neighbor in self.neighbors(node) {
            for neighbor_alt in 0..self.problem.node_costs[neighbor].len() {
                let cost = self.edge_cost(node, alternative, neighbor, neighbor_alt);
                if self.add_node_cost(neighbor, neighbor_alt, cost) {
                    impossible.push_back(PbqpAlternative {
                        node: PbqpNodeId::from_index(neighbor),
                        alternative: neighbor_alt,
                    });
                }
            }
        }

        self.remove_incident_edges(node);
        self.deactivate(node);
        self.reductions.push(Reduction::Fixed { node, alternative });
        self.propagate_new_infinities(impossible)
    }

    fn cheapest_alternative(&self, node: usize) -> Result<usize, PbqpSolveError> {
        self.problem.node_costs[node]
            .iter()
            .enumerate()
            .filter(|(_, cost)| **cost < INF_COST)
            .min_by_key(|(alternative, cost)| (*cost, *alternative))
            .map(|(alternative, _)| alternative)
            .ok_or(PbqpSolveError::Infeasible {
                node: PbqpNodeId::from_index(node),
            })
    }

    fn locally_ordered_alternatives(&self, node: usize) -> Result<Vec<usize>, PbqpSolveError> {
        let neighbors = self.neighbors(node);
        let mut alternatives: Vec<(usize, u64)> = self.problem.node_costs[node]
            .iter()
            .enumerate()
            .filter(|(_, cost)| **cost < INF_COST)
            .map(|(alternative, &base)| {
                let edge_costs = neighbors.iter().copied().fold(0, |acc, neighbor| {
                    let best = (0..self.problem.node_costs[neighbor].len())
                        .filter(|&neighbor_alt| {
                            self.problem.node_costs[neighbor][neighbor_alt] < INF_COST
                        })
                        .map(|neighbor_alt| {
                            add_cost(
                                self.problem.node_costs[neighbor][neighbor_alt],
                                self.edge_cost(node, alternative, neighbor, neighbor_alt),
                            )
                        })
                        .min()
                        .unwrap_or(INF_COST);
                    add_cost(acc, best)
                });
                (alternative, add_cost(base, edge_costs))
            })
            .collect();
        if alternatives.is_empty() {
            return Err(PbqpSolveError::Infeasible {
                node: PbqpNodeId::from_index(node),
            });
        }
        alternatives.sort_by_key(|(alternative, cost)| (*cost, *alternative));
        Ok(alternatives
            .into_iter()
            .map(|(alternative, _)| alternative)
            .collect())
    }

    fn edge_cost(&self, lhs: usize, lhs_alt: usize, rhs: usize, rhs_alt: usize) -> u64 {
        let (a, a_alt, b, b_alt) = if lhs < rhs {
            (lhs, lhs_alt, rhs, rhs_alt)
        } else {
            (rhs, rhs_alt, lhs, lhs_alt)
        };
        self.problem
            .edges
            .get(&(a, b))
            .map(|matrix| matrix.get(a_alt, b_alt))
            .unwrap_or(0)
    }

    fn add_or_accumulate_edge(&mut self, lhs: usize, rhs: usize, matrix: PbqpMatrix) {
        let (a, b, matrix) = orient_matrix(
            PbqpNodeId::from_index(lhs),
            PbqpNodeId::from_index(rhs),
            matrix,
        );
        if let Some(existing) = self.problem.edges.get_mut(&(a, b)) {
            if self.recording_undo {
                self.undo.push(Undo::EdgeChanged {
                    lhs: a,
                    rhs: b,
                    old_matrix: existing.clone(),
                });
            }
            for row in 0..existing.rows() {
                for col in 0..existing.cols() {
                    existing.add_assign(row, col, matrix.get(row, col));
                }
            }
        } else {
            self.insert_edge_raw(a, b, matrix);
            if self.recording_undo {
                self.undo.push(Undo::EdgeAdded { lhs: a, rhs: b });
            }
        }
    }

    fn prune_unsupported_alternatives(
        &mut self,
        lhs: usize,
        rhs: usize,
        impossible: &mut VecDeque<PbqpAlternative>,
    ) {
        for (node, neighbor) in [(lhs, rhs), (rhs, lhs)] {
            for alternative in 0..self.problem.node_costs[node].len() {
                let candidate = PbqpAlternative {
                    node: PbqpNodeId::from_index(node),
                    alternative,
                };
                if self.problem.node_costs[node][alternative] < INF_COST
                    && !self.has_supported_pair(candidate, neighbor)
                    && self.mark_impossible(candidate)
                {
                    impossible.push_back(candidate);
                }
            }
        }
    }

    fn remove_incident_edges(&mut self, node: usize) {
        let neighbors: Vec<usize> = self.adjacency[node].iter().copied().collect();
        for neighbor in neighbors {
            self.remove_edge(node, neighbor);
        }
    }

    fn remove_edge(&mut self, lhs: usize, rhs: usize) {
        let key = if lhs < rhs { (lhs, rhs) } else { (rhs, lhs) };
        if let Some(matrix) = self.remove_edge_raw(key.0, key.1)
            && self.recording_undo
        {
            self.undo.push(Undo::EdgeRemoved {
                lhs: key.0,
                rhs: key.1,
                matrix,
            });
        }
    }

    fn remove_edge_raw(&mut self, lhs: usize, rhs: usize) -> Option<PbqpMatrix> {
        let matrix = self.problem.edges.remove(&(lhs, rhs))?;
        self.adjacency[lhs].remove(&rhs);
        self.adjacency[rhs].remove(&lhs);
        self.degrees[lhs] -= 1;
        self.degrees[rhs] -= 1;
        self.refresh_reducible(lhs);
        self.refresh_reducible(rhs);
        Some(matrix)
    }

    fn insert_edge_raw(&mut self, lhs: usize, rhs: usize, matrix: PbqpMatrix) {
        let previous = self.problem.edges.insert((lhs, rhs), matrix);
        debug_assert!(previous.is_none());
        self.adjacency[lhs].insert(rhs);
        self.adjacency[rhs].insert(lhs);
        self.degrees[lhs] += 1;
        self.degrees[rhs] += 1;
        self.refresh_reducible(lhs);
        self.refresh_reducible(rhs);
    }

    fn deactivate(&mut self, node: usize) {
        debug_assert_eq!(self.degrees[node], 0);
        if self.recording_undo {
            self.undo.push(Undo::NodeDeactivated { node });
        }
        self.active[node] = false;
        self.active_count -= 1;
        self.reducible.remove(&node);
        self.refresh_feasibility(node);
    }

    fn refresh_reducible(&mut self, node: usize) {
        if self.active[node] && self.degrees[node] <= 2 {
            self.reducible.insert(node);
        } else {
            self.reducible.remove(&node);
        }
    }

    fn refresh_feasibility(&mut self, node: usize) {
        if self.active[node] && self.finite_alternatives[node] == 0 {
            self.infeasible.insert(node);
        } else {
            self.infeasible.remove(&node);
        }
    }

    fn rebuild_infeasible(&mut self) {
        self.infeasible.clear();
        for node in 0..self.active.len() {
            if self.active[node] && self.finite_alternatives[node] == 0 {
                self.infeasible.insert(node);
            }
        }
    }

    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            undo_len: self.undo.len(),
            reductions_len: self.reductions.len(),
        }
    }

    fn rollback(&mut self, checkpoint: Checkpoint) {
        while self.undo.len() > checkpoint.undo_len {
            match self.undo.pop().unwrap() {
                Undo::NodeCost {
                    node,
                    alternative,
                    old_cost,
                } => {
                    let cost = &mut self.problem.node_costs[node][alternative];
                    if *cost >= INF_COST && old_cost < INF_COST {
                        self.finite_alternatives[node] += 1;
                    } else if *cost < INF_COST && old_cost >= INF_COST {
                        self.finite_alternatives[node] -= 1;
                    }
                    *cost = old_cost;
                    self.refresh_feasibility(node);
                }
                Undo::EdgeAdded { lhs, rhs } => {
                    self.remove_edge_raw(lhs, rhs)
                        .expect("an added PBQP edge must exist during rollback");
                }
                Undo::EdgeRemoved { lhs, rhs, matrix } => {
                    self.insert_edge_raw(lhs, rhs, matrix);
                }
                Undo::EdgeChanged {
                    lhs,
                    rhs,
                    old_matrix,
                } => {
                    *self
                        .problem
                        .edges
                        .get_mut(&(lhs, rhs))
                        .expect("a changed PBQP edge must exist during rollback") = old_matrix;
                }
                Undo::NodeDeactivated { node } => {
                    self.active[node] = true;
                    self.active_count += 1;
                    self.refresh_reducible(node);
                    self.refresh_feasibility(node);
                }
            }
        }
        self.reductions.truncate(checkpoint.reductions_len);
    }

    fn reconstruct(&self) -> Result<Vec<usize>, PbqpSolveError> {
        let mut choices = vec![None; self.problem.node_count()];

        for reduction in self.reductions.iter().rev() {
            match reduction {
                Reduction::Fixed { node, alternative } => {
                    choices[*node] = Some(*alternative);
                }
                Reduction::R1 {
                    node,
                    neighbor,
                    choices_by_neighbor_alt,
                } => {
                    let neighbor_alt = choices[*neighbor].ok_or_else(|| {
                        PbqpSolveError::InvalidProblem("missing R1 neighbor choice".to_string())
                    })?;
                    choices[*node] = choices_by_neighbor_alt[neighbor_alt];
                }
                Reduction::R2 {
                    node,
                    left,
                    right,
                    right_alternatives,
                    choices_by_neighbor_alts,
                } => {
                    let left_alt = choices[*left].ok_or_else(|| {
                        PbqpSolveError::InvalidProblem("missing R2 left choice".to_string())
                    })?;
                    let right_alt = choices[*right].ok_or_else(|| {
                        PbqpSolveError::InvalidProblem("missing R2 right choice".to_string())
                    })?;
                    choices[*node] =
                        choices_by_neighbor_alts[left_alt * *right_alternatives + right_alt];
                }
            }
        }

        choices
            .into_iter()
            .enumerate()
            .map(|(node, choice)| {
                choice.ok_or_else(|| {
                    PbqpSolveError::InvalidProblem(format!("missing choice for node {node}"))
                })
            })
            .collect()
    }
}

fn orient_matrix(
    lhs: PbqpNodeId,
    rhs: PbqpNodeId,
    matrix: PbqpMatrix,
) -> (usize, usize, PbqpMatrix) {
    if lhs.index() < rhs.index() {
        (lhs.index(), rhs.index(), matrix)
    } else {
        let mut transposed = PbqpMatrix::zero(matrix.cols(), matrix.rows());
        for row in 0..matrix.rows() {
            for col in 0..matrix.cols() {
                transposed.set(col, row, matrix.get(row, col));
            }
        }
        (rhs.index(), lhs.index(), transposed)
    }
}

fn add_cost(lhs: u64, rhs: u64) -> u64 {
    if lhs >= INF_COST || rhs >= INF_COST {
        INF_COST
    } else {
        lhs.saturating_add(rhs).min(INF_COST)
    }
}

fn add_tracked_cost(cost: &mut u64, finite_alternatives: &mut usize, added: u64) -> bool {
    let old_cost = *cost;
    let new_cost = add_cost(old_cost, added);
    *cost = new_cost;
    if old_cost < INF_COST && new_cost >= INF_COST {
        *finite_alternatives -= 1;
        true
    } else {
        false
    }
}

fn evaluate_solution(problem: &PbqpProblem, choices: &[usize]) -> Result<u64, PbqpSolveError> {
    let mut total = 0;
    for (node, &choice) in choices.iter().enumerate() {
        let Some(cost) = problem.node_costs[node].get(choice) else {
            return Err(PbqpSolveError::InvalidProblem(format!(
                "choice for node {node} is out of range"
            )));
        };
        total = add_cost(total, *cost);
    }

    for (&(lhs, rhs), matrix) in &problem.edges {
        total = add_cost(total, matrix.get(choices[lhs], choices[rhs]));
    }

    if total >= INF_COST {
        Err(PbqpSolveError::Infeasible {
            node: PbqpNodeId::from_index(0),
        })
    } else {
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::{INF_COST, PbqpAlternative, PbqpMatrix, PbqpProblem, solve};

    #[test]
    fn r1_selects_cheapest_compatible_alternatives() {
        let mut problem = PbqpProblem::new();
        let a = problem.add_node(vec![2, 0]);
        let b = problem.add_node(vec![0, 0]);
        problem.add_edge(a, b, PbqpMatrix::new(2, 2, vec![0, INF_COST, INF_COST, 3]));

        let solution = solve(&problem).expect("PBQP should be solvable");
        assert_eq!(solution.choices, vec![0, 0]);
        assert_eq!(solution.total_cost, 2);
    }

    #[test]
    fn r2_folds_chain_costs_into_neighbor_matrix() {
        let mut problem = PbqpProblem::new();
        let a = problem.add_node(vec![0, 2]);
        let b = problem.add_node(vec![1, 0]);
        let c = problem.add_node(vec![0, 0]);
        problem.add_edge(a, b, PbqpMatrix::new(2, 2, vec![0, 4, 3, 0]));
        problem.add_edge(b, c, PbqpMatrix::new(2, 2, vec![0, 5, 7, 0]));

        let solution = solve(&problem).expect("PBQP should be solvable");
        assert_eq!(solution.choices, vec![0, 0, 0]);
        assert_eq!(solution.total_cost, 1);
    }

    #[test]
    fn rn_accounts_for_neighbor_instruction_costs() {
        let mut problem = PbqpProblem::new();
        let root = problem.add_node(vec![1, 2]);
        for _ in 0..3 {
            let operand = problem.add_node(vec![0, 10]);
            problem.add_edge(
                root,
                operand,
                PbqpMatrix::new(2, 2, vec![INF_COST, 0, 0, INF_COST]),
            );
        }

        let solution = solve(&problem).expect("PBQP should be solvable");
        assert_eq!(solution.choices[0], 1);
        assert_eq!(solution.total_cost, 2);
    }

    #[test]
    fn coherence_set_propagates_impossible_pattern_fragments() {
        let mut problem = PbqpProblem::new();
        let root = problem.add_node(vec![INF_COST, 0]);
        let child = problem.add_node(vec![5, 0]);
        problem.add_coherence_set(vec![
            PbqpAlternative {
                node: root,
                alternative: 0,
            },
            PbqpAlternative {
                node: child,
                alternative: 1,
            },
        ]);

        let solution = solve(&problem).expect("PBQP should be solvable");
        assert_eq!(solution.choices, vec![1, 0]);
        assert_eq!(solution.total_cost, 5);
    }

    #[test]
    fn rn_keeps_high_degree_instances_solvable() {
        let mut problem = PbqpProblem::new();
        let center = problem.add_node(vec![4, 1]);
        let a = problem.add_node(vec![0, 0]);
        let b = problem.add_node(vec![0, 0]);
        let c = problem.add_node(vec![0, 0]);
        let prefer_alt_one = PbqpMatrix::new(2, 2, vec![2, 2, 0, 0]);
        problem.add_edge(center, a, prefer_alt_one.clone());
        problem.add_edge(center, b, prefer_alt_one.clone());
        problem.add_edge(center, c, prefer_alt_one);

        let solution = solve(&problem).expect("PBQP should be solvable");
        assert_eq!(solution.choices[center.index()], 1);
        assert_eq!(solution.total_cost, 1);
    }

    #[test]
    fn exact_reductions_precede_rn_decisions() {
        let mut problem = PbqpProblem::new();
        let center = problem.add_node(vec![0, 1]);
        let same_choice = PbqpMatrix::new(2, 2, vec![0, INF_COST, INF_COST, 0]);

        for _ in 0..3 {
            let middle = problem.add_node(vec![0, 0]);
            let leaf = problem.add_node(vec![10, 0]);
            problem.add_edge(center, middle, same_choice.clone());
            problem.add_edge(middle, leaf, same_choice.clone());
        }

        let solution = solve(&problem).expect("PBQP should be solvable");
        assert_eq!(solution.choices[center.index()], 1);
        assert_eq!(solution.total_cost, 1);
    }

    #[test]
    fn rn_backtracks_when_local_choice_is_globally_infeasible() {
        let mut problem = PbqpProblem::new();
        let center = problem.add_node(vec![0, 1]);
        let left = problem.add_node(vec![0, 0]);
        let right = problem.add_node(vec![0, 0]);
        let third = problem.add_node(vec![0, 0]);
        let same_choice = PbqpMatrix::new(2, 2, vec![0, INF_COST, INF_COST, 0]);
        problem.add_edge(center, left, same_choice.clone());
        problem.add_edge(center, right, same_choice.clone());
        problem.add_edge(center, third, same_choice);
        problem.add_edge(left, right, PbqpMatrix::new(2, 2, vec![INF_COST, 0, 0, 0]));

        let solution = solve(&problem).expect("PBQP should try the feasible Rn alternative");
        assert_eq!(solution.choices[center.index()], 1);
        assert_eq!(solution.total_cost, 1);
    }

    #[test]
    fn rn_propagates_branch_infinities_through_coherence_sets() {
        let mut problem = PbqpProblem::new();
        let center = problem.add_node(vec![0, 1]);
        let a = problem.add_node(vec![0, 0]);
        let b = problem.add_node(vec![0, INF_COST]);
        let c = problem.add_node(vec![0, 0]);
        let same_choice = PbqpMatrix::new(2, 2, vec![0, INF_COST, INF_COST, 0]);
        let penalty = PbqpMatrix::new(2, 2, vec![0, 0, 0, 1]);

        problem.add_edge(center, a, same_choice);
        for (lhs, rhs) in [(center, b), (center, c), (a, b), (a, c), (b, c)] {
            problem.add_edge(lhs, rhs, penalty.clone());
        }
        problem.add_coherence_set(vec![
            PbqpAlternative {
                node: a,
                alternative: 1,
            },
            PbqpAlternative {
                node: b,
                alternative: 0,
            },
        ]);

        let solution = solve(&problem).expect("PBQP should try the coherent Rn alternative");
        assert_eq!(solution.choices[center.index()], 1);
    }

    #[test]
    fn rn_restores_reductions_before_trying_the_next_alternative() {
        let mut problem = PbqpProblem::new();
        let center = problem.add_node(vec![0, 1]);
        let a = problem.add_node(vec![0, 0]);
        let b = problem.add_node(vec![0, 0]);
        let c = problem.add_node(vec![0, 0]);
        let same_choice = PbqpMatrix::new(2, 2, vec![0, INF_COST, INF_COST, 0]);
        let penalty = PbqpMatrix::new(2, 2, vec![0, 0, 0, 1]);

        problem.add_edge(center, a, same_choice.clone());
        problem.add_edge(center, b, penalty.clone());
        problem.add_edge(center, c, penalty);
        problem.add_edge(a, b, same_choice.clone());
        problem.add_edge(b, c, same_choice);
        problem.add_edge(a, c, PbqpMatrix::new(2, 2, vec![INF_COST, 0, 0, 0]));

        let solution = solve(&problem).expect("PBQP should restore the failed Rn branch");
        assert_eq!(solution.choices[center.index()], 1);
        assert_eq!(solution.total_cost, 3);
    }

    /// Equal-cost optima must not be decided by hash iteration order: the same
    /// problem built with its edges added in a different order must solve the
    /// same way, or the compiler's output depends on the process's hash seed.
    #[test]
    fn solution_is_independent_of_edge_insertion_order() {
        let ring = |reversed: bool| {
            let mut problem = PbqpProblem::new();
            let nodes: Vec<_> = (0..8).map(|_| problem.add_node(vec![0, 0])).collect();
            let differ = PbqpMatrix::new(2, 2, vec![1, 0, 0, 1]);
            let mut edges: Vec<_> = (0..nodes.len())
                .map(|i| (nodes[i], nodes[(i + 1) % nodes.len()]))
                .collect();
            if reversed {
                edges.reverse();
            }
            for (lhs, rhs) in edges {
                problem.add_edge(lhs, rhs, differ.clone());
            }
            solve(&problem).expect("PBQP should be solvable")
        };

        assert_eq!(ring(false), ring(true));
    }
}
