//! Generalized hypertree decompositions of a join's run: the run's atoms are
//! hyperedges over the variables it binds, grouped into bags that form a tree.
//! A bag's atoms are joined at once by intersection; a bag and its parent pass
//! each other the cells of the variables they share, their separator.

use smallvec::SmallVec;

/// A bag of a decomposition. Masks are over the atoms' positions in the run
/// and over the attribute order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Bag {
    pub(super) atoms: u64,
    pub(super) vars: u64,
    /// An earlier bag; the root's parent is itself.
    pub(super) parent: usize,
    /// The variables the bag shares with its parent.
    pub(super) separator: u64,
}

/// Runs with more atoms than this are joined as one bag rather than searched
/// for a decomposition.
const EXHAUSTIVE: usize = 8;

pub(super) fn decompose(edges: &[u64]) -> Vec<Bag> {
    let n = edges.len();
    let covers =
        |j: usize, i: usize| j != i && edges[i] & !edges[j] == 0 && (edges[i] != edges[j] || j < i);
    let tops: SmallVec<[usize; 8]> = (0..n).filter(|&i| !(0..n).any(|j| covers(j, i))).collect();
    let top_edges: SmallVec<[u64; 8]> = tops.iter().map(|&i| edges[i]).collect();
    let groups: SmallVec<[u64; 8]> = if join_tree(&top_edges).is_some() {
        (0..tops.len()).map(|top| 1 << top).collect()
    } else if tops.len() <= EXHAUSTIVE {
        partition(&top_edges)
    } else {
        SmallVec::from_elem(u64::MAX >> (64 - tops.len()), 1)
    };
    let group_of = |i: usize| {
        let top = tops
            .iter()
            .position(|&top| top == i || edges[i] & !edges[top] == 0)
            .expect("every atom lies within a maximal one");
        groups
            .iter()
            .position(|group| group >> top & 1 == 1)
            .expect("a partition")
    };
    let mut atoms: SmallVec<[u64; 8]> = SmallVec::from_elem(0, groups.len());
    let mut vars: SmallVec<[u64; 8]> = SmallVec::from_elem(0, groups.len());
    for (i, &edge) in edges.iter().enumerate() {
        let group = group_of(i);
        atoms[group] |= 1 << i;
        vars[group] |= edge;
    }
    // The groups' join tree, rooted at the first atom's group: each group
    // after its parent, a group the tree does not reach hung off the root.
    let tree = join_tree(&vars).expect("the groups form a tree");
    let root = group_of(0);
    let mut order: SmallVec<[(usize, usize); 8]> = SmallVec::new();
    let mut placed = vec![false; groups.len()];
    for start in std::iter::once(root).chain(0..groups.len()) {
        if std::mem::replace(&mut placed[start], true) {
            continue;
        }
        order.push((start, root));
        let mut at = order.len() - 1;
        while at < order.len() {
            let group = order[at].0;
            for next in 0..groups.len() {
                let adjacent = tree[next] == Some(group) || tree[group] == Some(next);
                if adjacent && !std::mem::replace(&mut placed[next], true) {
                    order.push((next, group));
                }
            }
            at += 1;
        }
    }
    order
        .iter()
        .map(|&(group, parent)| Bag {
            atoms: atoms[group],
            vars: vars[group],
            parent: order
                .iter()
                .position(|&(other, _)| other == parent)
                .unwrap(),
            separator: match group == parent {
                true => 0,
                false => vars[group] & vars[parent],
            },
        })
        .collect()
}

// GYO reduction: remove an ear, an edge whose variables that any other edge
// reads all lie in one other edge, until no edge is left. Per edge, the edge
// it was an ear of (`None` for the last of its component); `None` if the edges
// are cyclic.
fn join_tree(edges: &[u64]) -> Option<SmallVec<[Option<usize>; 8]>> {
    let mut alive: SmallVec<[usize; 8]> = (0..edges.len()).collect();
    let mut tree = SmallVec::from_elem(None, edges.len());
    while !alive.is_empty() {
        let (at, witness) = alive.iter().enumerate().find_map(|(at, &edge)| {
            let others = alive
                .iter()
                .filter(|&&other| other != edge)
                .fold(0, |union, &other| union | edges[other]);
            let shared = edges[edge] & others;
            if shared == 0 {
                return Some((at, None));
            }
            alive
                .iter()
                .find(|&&other| other != edge && shared & !edges[other] == 0)
                .map(|&other| (at, Some(other)))
        })?;
        tree[alive.remove(at)] = witness;
    }
    Some(tree)
}

// The best partition of cyclic `edges` into bags, as masks over them, by the
// rule `decompose` gives.
fn partition(edges: &[u64]) -> SmallVec<[u64; 8]> {
    fn assign(
        edges: &[u64],
        at: usize,
        blocks: &mut SmallVec<[u64; 8]>,
        best: &mut (u32, usize, SmallVec<[u64; 8]>),
    ) {
        if at == edges.len() {
            let cost = (
                blocks.iter().map(|block| block.count_ones()).max().unwrap(),
                blocks.len(),
            );
            if (best.0, best.1) <= cost {
                return;
            }
            let reads = |block: u64| {
                (0..edges.len())
                    .filter(|&i| block >> i & 1 == 1)
                    .map(|i| edges[i])
                    .collect::<SmallVec<[u64; 8]>>()
            };
            let vars: SmallVec<[u64; 8]> = blocks
                .iter()
                .map(|&block| reads(block).iter().fold(0, |union, edge| union | edge))
                .collect();
            let cyclic = blocks
                .iter()
                .all(|&block| block.count_ones() == 1 || join_tree(&reads(block)).is_none());
            let nested =
                (0..vars.len()).any(|a| (0..vars.len()).any(|b| a != b && vars[a] & !vars[b] == 0));
            if cyclic && !nested && join_tree(&vars).is_some() {
                *best = (cost.0, cost.1, blocks.clone());
            }
            return;
        }
        for block in 0..blocks.len() {
            blocks[block] |= 1 << at;
            assign(edges, at + 1, blocks, best);
            blocks[block] &= !(1 << at);
        }
        blocks.push(1 << at);
        assign(edges, at + 1, blocks, best);
        blocks.pop();
    }
    // One bag of every atom always qualifies.
    let mut best = (u32::MAX, usize::MAX, SmallVec::new());
    assign(edges, 0, &mut SmallVec::new(), &mut best);
    best.2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(vars: &[u32]) -> u64 {
        vars.iter().fold(0, |mask, var| mask | 1 << var)
    }

    #[test]
    fn gyo_tells_a_triangle_from_a_chain() {
        let triangle = [mask(&[0, 1]), mask(&[1, 2]), mask(&[0, 2])];
        let chain = [mask(&[0, 1]), mask(&[1, 2]), mask(&[2, 3])];
        assert!(join_tree(&triangle).is_none());
        assert!(join_tree(&chain).is_some());
    }

    #[test]
    fn two_triangles_and_a_bridge_are_three_bags() {
        let (a, b, x, c, d, e) = (0, 1, 2, 3, 4, 5);
        let edges = [
            mask(&[a, b]),
            mask(&[a, x]),
            mask(&[b, x]),
            mask(&[x, c]),
            mask(&[c, d]),
            mask(&[d, e]),
            mask(&[c, e]),
        ];
        let bag = |atoms: &[u32], vars: &[u32], parent, separator: &[u32]| Bag {
            atoms: mask(atoms),
            vars: mask(vars),
            parent,
            separator: mask(separator),
        };
        assert_eq!(
            decompose(&edges),
            vec![
                bag(&[0, 1, 2], &[a, b, x], 0, &[]),
                bag(&[3], &[x, c], 0, &[x]),
                bag(&[4, 5, 6], &[c, d, e], 1, &[c]),
            ]
        );
    }
}
