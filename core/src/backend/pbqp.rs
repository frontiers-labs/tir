//! PBQP solver for SSA-based register assignment, after Buchwald, Zwinkau and
//! Bersch, "SSA-based Register Allocation with PBQP" (CC 2011).
//!
//! A node is a value, its alternatives the registers it may take; an edge
//! carries a cost matrix over the alternatives of its two ends (infinite where
//! two interfering values would share a register, positive where two
//! copy-related values would not). Spilling happens before the solver runs, so
//! every node has a register to take and no alternative models a spill.
//!
//! The solver never backtracks. It applies the optimal reductions — RE
//! (independent edges), R0, R1 and R2 — while any apply, then decides one node
//! heuristically (RN) with early decision: the node takes its locally cheapest
//! register, which turns its edges independent. RN visits nodes in the order
//! the caller supplies, which for SSA form is the definition order along the
//! dominance tree: the reverse of a perfect elimination order of the chordal
//! interference graph. Deciding in that order finds a register for every node
//! whenever register pressure fits, and the optimal reductions preserve the
//! order's validity (Theorems 1 and 2 of the paper). Before RN decides a node,
//! RM merges into it each neighbor whose alternative its own choice implies, so
//! the decision sees the neighbor's affinities too.
//!
//! Every reduction touches a node or edge a bounded number of times, so for a
//! fixed register count the solve is linear in the size of the graph.

use std::collections::HashMap;
use std::io::{self, Write};
use std::rc::Rc;

/// The cost of an alternative that must not be chosen.
pub const INF_COST: u64 = u64::MAX / 4;

fn add(lhs: u64, rhs: u64) -> u64 {
    lhs.saturating_add(rhs).min(INF_COST)
}

/// A dense cost matrix; rows index the alternatives of an edge's first node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matrix {
    rows: usize,
    cols: usize,
    costs: Vec<u64>,
}

impl Matrix {
    pub fn zero(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            costs: vec![0; rows * cols],
        }
    }

    pub fn get(&self, row: usize, col: usize) -> u64 {
        self.costs[row * self.cols + col]
    }

    pub fn set(&mut self, row: usize, col: usize, cost: u64) {
        self.costs[row * self.cols + col] = cost;
    }

    fn transposed(&self) -> Self {
        let mut result = Self::zero(self.cols, self.rows);
        for row in 0..self.rows {
            for col in 0..self.cols {
                result.set(col, row, self.get(row, col));
            }
        }
        result
    }
}

/// A register-assignment task: cost vectors and the edges between them.
/// Matrices are shared, so the equal interference matrices of one pair of
/// register classes are stored once.
#[derive(Default)]
pub struct Problem {
    costs: Vec<Vec<u64>>,
    edges: HashMap<(usize, usize), Rc<Matrix>>,
}

impl Problem {
    pub fn add_node(&mut self, costs: Vec<u64>) -> usize {
        assert!(!costs.is_empty(), "PBQP node must have alternatives");
        self.costs.push(costs);
        self.costs.len() - 1
    }

    /// Charge `matrix` (rows indexing `lhs`) on the edge between `lhs` and
    /// `rhs`, summing with whatever the edge already costs.
    pub fn add_edge(&mut self, lhs: usize, rhs: usize, matrix: Rc<Matrix>) {
        assert_ne!(lhs, rhs, "PBQP self-edges are not supported");
        let (key, matrix) = if lhs < rhs {
            ((lhs, rhs), matrix)
        } else {
            ((rhs, lhs), Rc::new(matrix.transposed()))
        };
        debug_assert_eq!(matrix.rows, self.costs[key.0].len());
        debug_assert_eq!(matrix.cols, self.costs[key.1].len());
        match self.edges.get_mut(&key) {
            Some(existing) => {
                let existing = Rc::make_mut(existing);
                for (cost, added) in existing.costs.iter_mut().zip(&matrix.costs) {
                    *cost = add(*cost, *added);
                }
            }
            None => {
                self.edges.insert(key, matrix);
            }
        }
    }

    pub fn node_count(&self) -> usize {
        self.costs.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Bytes held by the distinct cost matrices.
    pub fn matrix_bytes(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        self.edges
            .values()
            .filter(|matrix| seen.insert(Rc::as_ptr(matrix)))
            .map(|matrix| matrix.costs.len() * std::mem::size_of::<u64>())
            .sum()
    }

    /// Write the task in the version 1 `tir-pbqp` JSON dump format.
    pub fn write_json(&self, writer: &mut dyn Write, kind: &str) -> io::Result<()> {
        fn list(writer: &mut dyn Write, costs: &[u64]) -> io::Result<()> {
            write!(writer, "[")?;
            for (index, cost) in costs.iter().enumerate() {
                let separator = if index == 0 { "" } else { "," };
                write!(writer, "{separator}{cost}")?;
            }
            write!(writer, "]")
        }

        let mut edges: Vec<_> = self.edges.iter().collect();
        edges.sort_unstable_by_key(|(key, _)| **key);
        let mut matrices: Vec<&Rc<Matrix>> = Vec::new();
        let mut ids = HashMap::new();
        let edge_matrices: Vec<usize> = edges
            .iter()
            .map(|(_, matrix)| {
                *ids.entry(Rc::as_ptr(matrix)).or_insert_with(|| {
                    matrices.push(matrix);
                    matrices.len() - 1
                })
            })
            .collect();

        write!(
            writer,
            "{{\"format\":\"tir-pbqp\",\"version\":1,\"kind\":\"{kind}\",\"inf_cost\":{INF_COST},\"node_costs\":["
        )?;
        for (index, costs) in self.costs.iter().enumerate() {
            write!(writer, "{}", if index == 0 { "" } else { "," })?;
            list(writer, costs)?;
        }
        write!(writer, "],\"edges\":[")?;
        for (index, ((lhs, rhs), _)) in edges.iter().enumerate() {
            let separator = if index == 0 { "" } else { "," };
            write!(
                writer,
                "{separator}{{\"lhs\":{lhs},\"rhs\":{rhs},\"matrix\":{}}}",
                edge_matrices[index]
            )?;
        }
        write!(writer, "],\"matrices\":[")?;
        for (index, matrix) in matrices.iter().enumerate() {
            let separator = if index == 0 { "" } else { "," };
            write!(
                writer,
                "{separator}{{\"rows\":{},\"cols\":{},\"costs\":",
                matrix.rows, matrix.cols
            )?;
            list(writer, &matrix.costs)?;
            write!(writer, "}}")?;
        }
        write!(writer, "]}}")
    }

    /// Choose an alternative for every node. `order` ranks the nodes for
    /// heuristic decisions, earliest first; nodes it omits come after it in
    /// index order. Should no finite choice be found, every node is still given
    /// one, and [`Solution::infeasible`] names the nodes at fault: each whose
    /// own choice costs infinity, and the later-ranked end of each edge whose
    /// two choices do.
    pub fn solve(self, order: &[usize]) -> Solution {
        let mut ranked = order.to_vec();
        let mut listed = vec![false; self.costs.len()];
        for &node in order {
            listed[node] = true;
        }
        ranked.extend((0..self.costs.len()).filter(|&node| !listed[node]));
        let mut rank = vec![0; ranked.len()];
        for (position, &node) in ranked.iter().enumerate() {
            rank[node] = position;
        }

        let costs = self.costs.clone();
        let edges: Vec<_> = self
            .edges
            .iter()
            .map(|(&key, matrix)| (key, matrix.clone()))
            .collect();
        let choices = Solver::new(self).run(&ranked);

        // A node whose own choice is infinite answers for its edges too: once
        // it had nothing finite left, what its edges forbid no longer steered
        // its neighbors.
        let stuck: Vec<bool> = choices
            .iter()
            .enumerate()
            .map(|(node, &choice)| costs[node][choice] >= INF_COST)
            .collect();
        let mut at_fault = stuck.clone();
        for ((lhs, rhs), matrix) in edges {
            if !stuck[lhs] && !stuck[rhs] && matrix.get(choices[lhs], choices[rhs]) >= INF_COST {
                at_fault[if rank[lhs] > rank[rhs] { lhs } else { rhs }] = true;
            }
        }
        let mut infeasible: Vec<usize> = (0..costs.len()).filter(|&node| at_fault[node]).collect();
        infeasible.sort_by_key(|&node| rank[node]);
        Solution {
            choices,
            infeasible,
        }
    }
}

/// The alternative chosen for each node, and the nodes whose choice costs
/// infinity: those the caller must relieve (by spilling) and solve again.
#[derive(Debug)]
pub struct Solution {
    pub choices: Vec<usize>,
    pub infeasible: Vec<usize>,
}

struct Edge {
    lhs: usize,
    rhs: usize,
    matrix: Rc<Matrix>,
    live: bool,
}

/// A node taken out of the graph by an optimal reduction, solved once the
/// neighbors its edges name are.
enum Reduced {
    R0(usize),
    R1(usize, usize),
    R2(usize, usize, usize),
}

struct Solver {
    costs: Vec<Vec<u64>>,
    edges: Vec<Edge>,
    /// Live edges of each live node.
    adjacency: Vec<Vec<usize>>,
    between: HashMap<(usize, usize), usize>,
    alive: Vec<bool>,
    /// Live nodes whose degree fell to two or less since they were last looked at.
    pending: Vec<usize>,
    reduced: Vec<Reduced>,
    choices: Vec<usize>,
}

impl Solver {
    fn new(problem: Problem) -> Self {
        let count = problem.costs.len();
        let mut solver = Self {
            costs: problem.costs,
            edges: Vec::with_capacity(problem.edges.len()),
            adjacency: vec![Vec::new(); count],
            between: HashMap::with_capacity(problem.edges.len()),
            alive: vec![true; count],
            pending: (0..count).rev().collect(),
            reduced: Vec::new(),
            choices: vec![0; count],
        };
        let mut edges: Vec<_> = problem.edges.into_iter().collect();
        edges.sort_unstable_by_key(|(key, _)| *key);
        for ((lhs, rhs), matrix) in edges {
            solver.link(lhs, rhs, matrix);
        }
        solver
    }

    fn run(mut self, order: &[usize]) -> Vec<usize> {
        let mut next = 0;
        loop {
            self.reduce_optimally();
            while next < order.len() && !self.alive[order[next]] {
                next += 1;
            }
            let Some(&node) = order.get(next) else {
                break;
            };
            self.merge_implied(node);
            if self.alive[node] && self.adjacency[node].len() > 2 {
                self.decide(node);
            }
        }
        self.back_propagate();
        self.choices
    }

    /// The cost of `edge` when `node` takes `own` and the other end `other`.
    fn cost(&self, edge: usize, node: usize, own: usize, other: usize) -> u64 {
        let edge = &self.edges[edge];
        if edge.lhs == node {
            edge.matrix.get(own, other)
        } else {
            edge.matrix.get(other, own)
        }
    }

    fn other(&self, edge: usize, node: usize) -> usize {
        let edge = &self.edges[edge];
        if edge.lhs == node { edge.rhs } else { edge.lhs }
    }

    fn finite(&self, node: usize) -> usize {
        self.costs[node]
            .iter()
            .filter(|&&cost| cost < INF_COST)
            .count()
    }

    fn link(&mut self, lhs: usize, rhs: usize, matrix: Rc<Matrix>) {
        let id = self.edges.len();
        self.edges.push(Edge {
            lhs,
            rhs,
            matrix,
            live: true,
        });
        self.adjacency[lhs].push(id);
        self.adjacency[rhs].push(id);
        self.between.insert((lhs.min(rhs), lhs.max(rhs)), id);
        self.normalize(id);
    }

    fn unlink(&mut self, edge: usize) {
        let (lhs, rhs) = (self.edges[edge].lhs, self.edges[edge].rhs);
        self.edges[edge].live = false;
        for node in [lhs, rhs] {
            let list = &mut self.adjacency[node];
            let position = list.iter().position(|&e| e == edge).expect("live edge");
            list.swap_remove(position);
            if list.len() <= 2 {
                self.pending.push(node);
            }
        }
        self.between.remove(&(lhs.min(rhs), lhs.max(rhs)));
    }

    /// Add `delta` (rows indexing `lhs`) to the edge between `lhs` and `rhs`.
    fn charge(&mut self, lhs: usize, rhs: usize, delta: Matrix) {
        match self.between.get(&(lhs.min(rhs), lhs.max(rhs))).copied() {
            Some(edge) => {
                let delta = if self.edges[edge].lhs == lhs {
                    delta
                } else {
                    delta.transposed()
                };
                let matrix = Rc::make_mut(&mut self.edges[edge].matrix);
                for (cost, added) in matrix.costs.iter_mut().zip(delta.costs) {
                    *cost = add(*cost, added);
                }
                self.normalize(edge);
            }
            None => self.link(lhs, rhs, Rc::new(delta)),
        }
    }

    /// Add `costs` to `node`'s vector. A node left with one finite alternative
    /// is decided, so every edge it has turns independent and goes.
    fn charge_node(&mut self, node: usize, costs: impl IntoIterator<Item = u64>) {
        for (cost, added) in self.costs[node].iter_mut().zip(costs) {
            *cost = add(*cost, added);
        }
        if self.finite(node) <= 1 {
            for edge in self.adjacency[node].clone() {
                if self.edges[edge].live {
                    self.normalize(edge);
                }
            }
        }
        self.pending.push(node);
    }

    /// RE: move each row's minimum into the edge's first node and each
    /// column's into its second, then drop the edge if nothing is left in it.
    /// Rows and columns of alternatives already infinite constrain nothing and
    /// are cleared.
    fn normalize(&mut self, edge: usize) {
        let Edge { lhs, rhs, .. } = self.edges[edge];
        let (rows, cols) = (self.costs[lhs].len(), self.costs[rhs].len());
        let matrix = &self.edges[edge].matrix;
        let live_row = |row: usize| self.costs[lhs][row] < INF_COST;
        let live_col = |col: usize| self.costs[rhs][col] < INF_COST;
        let row_min = |row: usize| {
            (0..cols)
                .filter(|&col| live_col(col))
                .map(|col| matrix.get(row, col))
                .min()
                .unwrap_or(0)
        };
        let col_min = |col: usize| {
            (0..rows)
                .filter(|&row| live_row(row))
                .map(|row| matrix.get(row, col))
                .min()
                .unwrap_or(0)
        };
        let settled = (0..rows).all(|row| {
            if live_row(row) {
                row_min(row) == 0
            } else {
                (0..cols).all(|col| matrix.get(row, col) == 0)
            }
        }) && (0..cols).all(|col| {
            if live_col(col) {
                col_min(col) == 0
            } else {
                (0..rows).all(|row| matrix.get(row, col) == 0)
            }
        });
        if settled {
            if matrix.costs.iter().all(|&cost| cost == 0) {
                self.unlink(edge);
            }
            return;
        }

        let mut matrix = Rc::unwrap_or_clone(std::mem::replace(
            &mut self.edges[edge].matrix,
            Rc::new(Matrix::zero(0, 0)),
        ));
        for row in 0..rows {
            if self.costs[lhs][row] < INF_COST {
                let min = (0..cols)
                    .filter(|&col| self.costs[rhs][col] < INF_COST)
                    .map(|col| matrix.get(row, col))
                    .min()
                    .unwrap_or(0);
                if min != 0 {
                    for col in 0..cols {
                        let cost = matrix.get(row, col);
                        matrix.set(
                            row,
                            col,
                            if cost >= INF_COST {
                                INF_COST
                            } else {
                                cost.saturating_sub(min)
                            },
                        );
                    }
                    self.costs[lhs][row] = add(self.costs[lhs][row], min);
                }
            }
            if self.costs[lhs][row] >= INF_COST {
                for col in 0..cols {
                    matrix.set(row, col, 0);
                }
            }
        }
        for col in 0..cols {
            if self.costs[rhs][col] < INF_COST {
                let min = (0..rows)
                    .filter(|&row| self.costs[lhs][row] < INF_COST)
                    .map(|row| matrix.get(row, col))
                    .min()
                    .unwrap_or(0);
                if min != 0 {
                    for row in 0..rows {
                        let cost = matrix.get(row, col);
                        matrix.set(
                            row,
                            col,
                            if cost >= INF_COST {
                                INF_COST
                            } else {
                                cost.saturating_sub(min)
                            },
                        );
                    }
                    self.costs[rhs][col] = add(self.costs[rhs][col], min);
                }
            }
            if self.costs[rhs][col] >= INF_COST {
                for row in 0..rows {
                    matrix.set(row, col, 0);
                }
            }
        }
        let empty = matrix.costs.iter().all(|&cost| cost == 0);
        self.edges[edge].matrix = Rc::new(matrix);
        if empty {
            self.unlink(edge);
        }
    }

    /// Apply R0, R1 and R2 until every live node has degree three or more.
    fn reduce_optimally(&mut self) {
        while let Some(node) = self.pending.pop() {
            if !self.alive[node] {
                continue;
            }
            match self.adjacency[node].len() {
                0 => {
                    self.alive[node] = false;
                    self.reduced.push(Reduced::R0(node));
                }
                1 => self.reduce_one(node),
                2 => self.reduce_two(node),
                _ => {}
            }
        }
    }

    /// R1: fold a degree-one node's cheapest response to each alternative of
    /// its neighbor into the neighbor.
    fn reduce_one(&mut self, node: usize) {
        let edge = self.adjacency[node][0];
        let neighbor = self.other(edge, node);
        let delta: Vec<u64> = (0..self.costs[neighbor].len())
            .map(|other| self.best_response(node, &[(edge, other)]).1)
            .collect();
        self.alive[node] = false;
        self.unlink(edge);
        self.reduced.push(Reduced::R1(node, edge));
        self.charge_node(neighbor, delta);
    }

    /// R2: replace a degree-two node by an edge between its neighbors that
    /// prices its cheapest response to each pair of their alternatives.
    fn reduce_two(&mut self, node: usize) {
        let (first, second) = (self.adjacency[node][0], self.adjacency[node][1]);
        let (left, right) = (self.other(first, node), self.other(second, node));
        let mut delta = Matrix::zero(self.costs[left].len(), self.costs[right].len());
        for l in 0..delta.rows {
            for r in 0..delta.cols {
                delta.set(l, r, self.best_response(node, &[(first, l), (second, r)]).1);
            }
        }
        self.alive[node] = false;
        self.unlink(first);
        self.unlink(second);
        self.reduced.push(Reduced::R2(node, first, second));
        self.charge(left, right, delta);
    }

    /// The cheapest alternative of `node` and its cost, given the alternative
    /// chosen across each of `fixed`'s edges.
    fn best_response(&self, node: usize, fixed: &[(usize, usize)]) -> (usize, u64) {
        (0..self.costs[node].len())
            .map(|own| {
                let cost = fixed
                    .iter()
                    .fold(self.costs[node][own], |total, &(edge, other)| {
                        add(total, self.cost(edge, node, own, other))
                    });
                (own, cost)
            })
            .min_by_key(|&(_, cost)| cost)
            .unwrap_or((0, INF_COST))
    }

    /// For each finite alternative of `node`, the one alternative of the
    /// neighbor across `edge` it leaves finite — or `None` if some alternative
    /// leaves several, so a choice at `node` does not decide the neighbor.
    fn implied(&self, node: usize, edge: usize) -> Option<Vec<usize>> {
        let neighbor = self.other(edge, node);
        let mut implied = Vec::with_capacity(self.costs[node].len());
        for own in 0..self.costs[node].len() {
            if self.costs[node][own] >= INF_COST {
                implied.push(0);
                continue;
            }
            let mut finite = (0..self.costs[neighbor].len()).filter(|&other| {
                add(
                    self.cost(edge, node, own, other),
                    self.costs[neighbor][other],
                ) < INF_COST
            });
            let first = finite.next().unwrap_or(0);
            if finite.next().is_some() {
                return None;
            }
            implied.push(first);
        }
        Some(implied)
    }

    /// RM: merge into `node` every neighbor its own choice decides. The
    /// neighbor's other edges move onto `node`, priced through the implied
    /// choice, which leaves the neighbor of degree one for R1.
    fn merge_implied(&mut self, node: usize) {
        let mut index = 0;
        while self.alive[node] && index < self.adjacency[node].len() {
            let edge = self.adjacency[node][index];
            let neighbor = self.other(edge, node);
            let implied = if self.adjacency[neighbor].len() > 1 {
                self.implied(node, edge)
            } else {
                None
            };
            let Some(implied) = implied else {
                index += 1;
                continue;
            };
            for far in self.adjacency[neighbor].clone() {
                if far == edge {
                    continue;
                }
                let target = self.other(far, neighbor);
                let mut delta = Matrix::zero(self.costs[node].len(), self.costs[target].len());
                for (own, &chosen) in implied.iter().enumerate() {
                    if self.costs[node][own] < INF_COST {
                        for other in 0..delta.cols {
                            delta.set(own, other, self.cost(far, neighbor, chosen, other));
                        }
                    }
                }
                self.unlink(far);
                self.charge(node, target, delta);
            }
            if self.alive[neighbor] && self.adjacency[neighbor].len() == 1 {
                self.reduce_one(neighbor);
            }
            index = 0;
        }
    }

    /// RN with early decision: take the alternative that is cheapest counting
    /// each neighbor's best response, then charge that choice to the neighbors.
    fn decide(&mut self, node: usize) {
        let edges = self.adjacency[node].clone();
        let (choice, _) = (0..self.costs[node].len())
            .map(|own| {
                let cost = edges.iter().fold(self.costs[node][own], |total, &edge| {
                    let neighbor = self.other(edge, node);
                    let best = (0..self.costs[neighbor].len())
                        .map(|other| {
                            add(
                                self.cost(edge, node, own, other),
                                self.costs[neighbor][other],
                            )
                        })
                        .min()
                        .unwrap_or(INF_COST);
                    add(total, best)
                });
                (own, cost)
            })
            .min_by_key(|&(_, cost)| cost)
            .unwrap_or((0, INF_COST));
        self.choices[node] = choice;
        self.alive[node] = false;
        for edge in edges {
            let neighbor = self.other(edge, node);
            let delta: Vec<u64> = (0..self.costs[neighbor].len())
                .map(|other| self.cost(edge, node, choice, other))
                .collect();
            self.unlink(edge);
            self.charge_node(neighbor, delta);
        }
    }

    /// Solve the reduced nodes last-reduced first: each one's neighbors are
    /// decided by then.
    fn back_propagate(&mut self) {
        for reduction in std::mem::take(&mut self.reduced).into_iter().rev() {
            let (node, fixed) = match reduction {
                Reduced::R0(node) => (node, Vec::new()),
                Reduced::R1(node, edge) => (node, vec![edge]),
                Reduced::R2(node, first, second) => (node, vec![first, second]),
            };
            let fixed: Vec<(usize, usize)> = fixed
                .into_iter()
                .map(|edge| (edge, self.choices[self.other(edge, node)]))
                .collect();
            self.choices[node] = self.best_response(node, &fixed).0;
        }
    }
}
