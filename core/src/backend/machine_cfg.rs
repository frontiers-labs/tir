use std::collections::HashMap;

use tir::BlockId;

/// The dominator tree of a machine function's blocks, by index into the
/// block list; the first block is the entry.
pub(crate) struct Dominance {
    /// The reachable blocks in reverse postorder.
    order: Vec<usize>,
    rank: Vec<usize>,
    idom: Vec<Option<usize>>,
    predecessors: Vec<Vec<usize>>,
    seen: Vec<bool>,
}

pub(super) fn dominance(
    blocks: &[BlockId],
    successors: &HashMap<BlockId, Vec<BlockId>>,
) -> Dominance {
    let indices: HashMap<_, _> = blocks.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    let mut edges = vec![Vec::new(); blocks.len()];
    let mut predecessors = vec![Vec::new(); blocks.len()];
    for (i, block) in blocks.iter().enumerate() {
        for successor in successors.get(block).into_iter().flatten() {
            let next = indices[successor];
            edges[i].push(next);
            predecessors[next].push(i);
        }
    }

    let mut order = Vec::new();
    let mut seen = vec![false; blocks.len()];
    let mut dfs = vec![(0, false)];
    while let Some((block, finish)) = dfs.pop() {
        if finish {
            order.push(block);
        } else if !seen[block] {
            seen[block] = true;
            dfs.push((block, true));
            dfs.extend(edges[block].iter().rev().map(|&next| (next, false)));
        }
    }
    order.reverse();
    let mut rank = vec![usize::MAX; blocks.len()];
    for (i, &block) in order.iter().enumerate() {
        rank[block] = i;
    }

    let mut idom = vec![None; blocks.len()];
    idom[0] = Some(0);
    let mut changed = true;
    while changed {
        changed = false;
        for &block in &order[1..] {
            let mut incoming = predecessors[block]
                .iter()
                .copied()
                .filter(|&p| idom[p].is_some());
            let mut parent = incoming.next().expect("reachable block has a predecessor");
            for mut predecessor in incoming {
                while parent != predecessor {
                    if rank[parent] > rank[predecessor] {
                        parent = idom[parent].expect("known dominator");
                    } else {
                        predecessor = idom[predecessor].expect("known dominator");
                    }
                }
            }
            if idom[block] != Some(parent) {
                idom[block] = Some(parent);
                changed = true;
            }
        }
    }

    Dominance {
        order,
        rank,
        idom,
        predecessors,
        seen,
    }
}

impl Dominance {
    pub(super) fn idom(&self, block: usize) -> Option<usize> {
        self.idom[block].filter(|&parent| parent != block)
    }

    pub(super) fn preorder(&self) -> Vec<usize> {
        let mut children = vec![Vec::new(); self.idom.len()];
        for &block in self.order.iter().skip(1) {
            if let Some(parent) = self.idom[block] {
                children[parent].push(block);
            }
        }
        let mut walk = Vec::with_capacity(self.idom.len());
        let mut stack: Vec<usize> = self.order.first().copied().into_iter().collect();
        while let Some(block) = stack.pop() {
            walk.push(block);
            stack.extend(children[block].iter().rev());
        }
        walk.extend((0..self.idom.len()).filter(|&block| !self.seen[block]));
        walk
    }
}

/// The natural loops of a function, nested by header.
pub(crate) struct Loops {
    /// Per block, the header of the innermost loop holding it; a header
    /// names itself.
    innermost: Vec<Option<usize>>,
    /// Per header, the header of the loop around its own.
    enclosing: Vec<Option<usize>>,
}

impl Loops {
    pub(super) fn header(&self, block: usize) -> Option<usize> {
        self.innermost[block]
    }

    pub(super) fn is_header(&self, block: usize) -> bool {
        self.innermost[block] == Some(block)
    }

    pub(super) fn headers(&self, block: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.innermost[block], |&header| self.enclosing[header])
    }

    pub(super) fn contains(&self, header: usize, block: usize) -> bool {
        self.headers(block).any(|outer| outer == header)
    }
}

impl Dominance {
    pub(super) fn reverse_postorder(&self) -> Vec<usize> {
        let unreachable = (0..self.idom.len()).filter(|&block| !self.seen[block]);
        self.order.iter().copied().chain(unreachable).collect()
    }

    pub(super) fn loops(&self) -> Loops {
        let mut innermost = vec![None; self.idom.len()];
        let mut enclosing = vec![None; self.idom.len()];
        // Outer headers come first in reverse postorder, so an inner loop
        // marks its blocks last.
        for &header in &self.order {
            let mut work: Vec<usize> = self.predecessors[header]
                .iter()
                .copied()
                .filter(|&latch| self.seen[latch] && self.dominates(header, latch))
                .collect();
            if work.is_empty() {
                continue;
            }
            enclosing[header] = innermost[header];
            innermost[header] = Some(header);
            while let Some(block) = work.pop() {
                if innermost[block] == Some(header) {
                    continue;
                }
                innermost[block] = Some(header);
                work.extend(self.predecessors[block].iter().filter(|&&p| self.seen[p]));
            }
        }
        Loops {
            innermost,
            enclosing,
        }
    }

    fn dominates(&self, parent: usize, mut block: usize) -> bool {
        while self.rank[block] > self.rank[parent] {
            block = self.idom[block].expect("reachable block has a dominator");
        }
        block == parent
    }
}

pub(super) fn loop_depths(blocks: &[BlockId], dominance: &Dominance) -> HashMap<BlockId, u32> {
    let Dominance {
        order,
        rank,
        idom,
        predecessors,
        seen,
    } = dominance;
    let mut depths = vec![0; blocks.len()];
    let mut visited = vec![usize::MAX; blocks.len()];
    let mut work = Vec::new();
    for &header in order {
        for &latch in &predecessors[header] {
            if !seen[latch] {
                continue;
            }
            let mut ancestor = latch;
            while rank[ancestor] > rank[header] {
                ancestor = idom[ancestor].expect("reachable block has a dominator");
            }
            if ancestor == header {
                work.push(latch);
            }
        }
        if work.is_empty() {
            continue;
        }
        // Mark the header first so self-loops cannot walk into the preheader.
        visited[header] = header;
        depths[header] += 1;
        while let Some(block) = work.pop() {
            if visited[block] == header {
                continue;
            }
            visited[block] = header;
            depths[block] += 1;
            work.extend(predecessors[block].iter().copied().filter(|&p| seen[p]));
        }
    }
    blocks.iter().copied().zip(depths).collect()
}
