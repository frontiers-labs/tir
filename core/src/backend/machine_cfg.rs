use std::collections::HashMap;

use tir::BlockId;

/// Count natural loops by header, ignoring blocks unreachable from entry.
pub(super) fn loop_depths(
    blocks: &[BlockId],
    successors: &HashMap<BlockId, Vec<BlockId>>,
) -> HashMap<BlockId, u32> {
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

    // Immediate dominators in reverse postorder keep storage linear in the CFG.
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

    let mut depths = vec![0; blocks.len()];
    let mut visited = vec![usize::MAX; blocks.len()];
    let mut work = Vec::new();
    for &header in &order {
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
