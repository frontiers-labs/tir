//! Pure, formally verified TEGG v15 core algorithms.
//!
//! Extracted to Lean 4 via Hax and proved against `CdslV3Spec.TeggSpec`.
//! Directly executed at runtime by `tegg.rs` when emitting TEGG v15 bytecode.

pub struct VmNode<T> {
    pub payload: T,
    pub children: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    New(usize),
    Same(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColReq {
    Fresh,
    Known(usize),
}

/// Appends one parent step onto `nodes` with child `child_id` and returns the new node id.
pub fn push_parent_node<T>(nodes: &mut Vec<VmNode<T>>, child_id: usize, step: T) -> usize {
    let parent_id = nodes.len();
    let mut children = Vec::new();
    children.push(child_id);
    nodes.push(VmNode {
        payload: step,
        children,
    });
    parent_id
}

/// Appends parent chain steps bottom-up so every node satisfies `child_id < parent_id`.
pub fn build_chain_aux<T>(
    mut nodes: Vec<VmNode<T>>,
    mut child_id: usize,
    mut steps: Vec<T>,
) -> (Vec<VmNode<T>>, usize) {
    // Reverse then pop so we process steps in original order without needing `IntoIter`.
    steps.reverse();
    while let Some(st) = steps.pop() {
        child_id = push_parent_node(&mut nodes, child_id, st);
    }
    (nodes, child_id)
}

/// Builds a bottom-up linear chain starting with `leaf_step` and wrapping with `parent_steps`.
pub fn build_chain<T>(
    mut nodes: Vec<VmNode<T>>,
    leaf_step: T,
    parent_steps: Vec<T>,
) -> (Vec<VmNode<T>>, usize) {
    let leaf_id = nodes.len();
    let leaf_node = VmNode {
        payload: leaf_step,
        children: Vec::new(),
    };
    nodes.push(leaf_node);
    build_chain_aux(nodes, leaf_id, parent_steps)
}

/// Allocates sequential scan column bindings starting at register watermark `w`.
pub fn alloc_binds(mut w: usize, reqs: &[ColReq]) -> (Vec<Bind>, usize) {
    let mut binds = Vec::new();
    let mut i = 0;
    while i < reqs.len() {
        let req = reqs[i];
        match req {
            ColReq::Fresh => {
                binds.push(Bind::New(w));
                w += 1;
            }
            ColReq::Known(r) => {
                binds.push(Bind::Same(r));
            }
        }
        i += 1;
    }
    (binds, w)
}

/// 4-byte padding needed after a string of length `len`.
#[inline]
pub fn pad4(len: usize) -> usize {
    (4 - (len % 4)) % 4
}

/// Byte size of one interned string entry: 4-byte length prefix + bytes + 4-byte padding.
#[inline]
pub fn string_entry_bytes(len: usize) -> usize {
    4 + len + pad4(len)
}

/// Total byte size of the TEGG string table payload.
pub fn sum_string_entry_bytes(lens: &[usize]) -> usize {
    let mut total = 0usize;
    let mut i = 0usize;
    while i < lens.len() {
        total += string_entry_bytes(lens[i]);
        i += 1;
    }
    total
}

/// Total byte size of a TEGG v15 binary file (12-byte header + string table + u32 words).
#[inline]
pub fn tegg_file_bytes(str_lens: &[usize], word_count: usize) -> usize {
    12 + sum_string_entry_bytes(str_lens) + 4 * word_count
}
