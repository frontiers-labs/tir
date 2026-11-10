//! Checked ordinary scalar forwarding. This service checks supplied operation IDs;
//! it neither discovers candidates nor publishes rewrites. Functional cell and
//! range interpretation and scalar SAT encoding are trusted. Arbitrary mutable
//! memory callbacks are outside the ordinary resource contract.

use super::{ProofOutcome, SemGraph, SmtOracle, SymKind, SymPayload, con, op};
use crate::ptr::semantics::Body;
use crate::ptr::{LOAD, PtrType, STORE};
use crate::{Context, NodeId, OpId, TypeId, ValueId};
use std::collections::HashMap;
use tir_symbolic::sem::{CopyAction, copy_subgraph, copy_subgraph_with, sym};

type GraphSnapshot = Vec<(
    SymKind,
    Option<SymPayload<tir_symbolic::sem::ValueId>>,
    Vec<NodeId>,
    Option<super::NodeMeta>,
)>;

#[derive(Clone, Debug, PartialEq)]
struct Source {
    id: OpId,
    kind: (String, String),
    parent: Option<OpId>,
    region: Option<crate::RegionId>,
    block: Option<crate::BlockId>,
    placement: crate::InsertionPoint,
    operands: Vec<ValueId>,
    results: Vec<ValueId>,
    types: Vec<TypeId>,
    attributes: Vec<crate::attributes::NamedAttribute>,
}

impl Source {
    fn capture(c: &Context, id: OpId) -> Result<Self, MemoryProofError> {
        if !c.has_operation(id) {
            return Err(MemoryProofError::Stale);
        }
        let h = c.get_op(id);
        if !h.is_live() {
            return Err(MemoryProofError::Stale);
        }
        let operands = h.operands().to_vec();
        let results = h.results().to_vec();
        if operands.iter().chain(&results).any(|v| !c.has_value(*v)) {
            return Err(MemoryProofError::Stale);
        }
        let types = operands
            .iter()
            .chain(&results)
            .map(|v| c.get_value(*v).ty())
            .collect();
        Ok(Self {
            id,
            kind: (h.dialect().to_string(), h.name().to_string()),
            parent: c.parent_op(id),
            region: c.region_of_op(id),
            block: c.parent_block(id),
            placement: c.insertion_point(id),
            operands,
            results,
            types,
            attributes: h.attributes(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Declaration {
    graph: GraphSnapshot,
    root: NodeId,
    value_results: Vec<NodeId>,
    state_results: Vec<NodeId>,
    raised_flags: Option<NodeId>,
    normal: NodeId,
    address_poison: NodeId,
    value_poison: NodeId,
}
fn declaration(b: &Body) -> Declaration {
    let g = &b.resources.graph;
    Declaration {
        graph: (0..g.len())
            .map(|i| {
                let n = NodeId::from_index(i);
                (
                    *g.get_node(n),
                    g.get_leaf_data(n).cloned(),
                    g.children(n).collect(),
                    g.get_annotation(n).copied(),
                )
            })
            .collect(),
        root: b.resources.root,
        value_results: b.resources.value_results.clone(),
        state_results: b.resources.state_results.clone(),
        raised_flags: b.resources.raised_flags,
        normal: b.normal,
        address_poison: b.address_poison,
        value_poison: b.value_poison,
    }
}

/// A rejected domain, unsupported residual or stale source never grants a law.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryProofError {
    Stale,
    Domain,
    Unsupported,
    NotProven,
}

/// Complete coupled equality for a retained store. Construction is private.
#[derive(Clone, Debug, PartialEq)]
pub struct ForwardWitness {
    scope: OpId,
    store: OpId,
    load: OpId,
    source: Vec<Source>,
    bodies: [Declaration; 2],
    layouts: Vec<Option<crate::attributes::AttributeValue>>,
    replacements: Vec<(ValueId, ValueId)>,
}
impl ForwardWitness {
    /// All actual load results must be replaced together.
    pub fn replacements(&self) -> &[(ValueId, ValueId)] {
        &self.replacements
    }
    /// The store and its incoming prerequisites remain in the computation.
    pub fn retained_store(&self) -> OpId {
        self.store
    }
    /// Re-read real declarations, layout, types, attributes and prerequisites.
    pub fn validate(&self, c: &Context) -> Result<(), MemoryProofError> {
        let current = check_forward(c, self.scope, self.store, self.load)?;
        if current == *self {
            Ok(())
        } else {
            Err(MemoryProofError::Stale)
        }
    }
}

pub(crate) fn ordinary_layout(
    layout: Option<&crate::layout::DataLayout>,
    pointer_value: bool,
) -> bool {
    match layout {
        None => !pointer_value,
        Some(layout) => {
            crate::layout::verify_spec(&layout.spec()).is_ok()
                && layout.pointer_size() == Some(64)
                && layout
                    .endianness()
                    .unwrap_or(crate::layout::Endianness::Little)
                    == crate::layout::Endianness::Little
        }
    }
}

/// Check the direct ordinary scalar store/load law at supplied live IDs. Requires
/// the ordinary shared LE64 access and range contract. Integer accesses default
/// to that contract; pointer values require an explicit 64-bit layout. Explicit
/// layouts must agree. No callback or publication is certified.
pub fn check_forward(
    c: &Context,
    scope: OpId,
    store: OpId,
    load: OpId,
) -> Result<ForwardWitness, MemoryProofError> {
    let mut source = vec![
        Source::capture(c, scope)?,
        Source::capture(c, store)?,
        Source::capture(c, load)?,
    ];
    if source[1].parent != Some(scope) || source[2].parent != Some(scope) || store == load {
        return Err(MemoryProofError::Domain);
    }
    if source[1].region != source[2].region || source[1].block != source[2].block {
        return Err(MemoryProofError::Domain);
    }
    let s = c.get_op(store);
    let l = c.get_op(load);
    let sop = s
        .clone()
        .as_operation(&STORE)
        .ok_or(MemoryProofError::Domain)?;
    let lop = l
        .clone()
        .as_operation(&LOAD)
        .ok_or(MemoryProofError::Domain)?;
    if s.value_operands().len() != 2
        || l.value_operands().len() != 1
        || l.value_results().len() != 1
        || s.state_operands().len() != 1
        || s.state_results().len() != 1
        || l.state_operands() != s.state_results()
        || l.state_results().len() > 1
    {
        return Err(MemoryProofError::Domain);
    }
    let stored = s.value_operands()[0];
    let pointer = s.value_operands()[1];
    let ty = c.get_value(stored).ty();
    if l.value_operands()[0] != pointer
        || c.get_value(l.value_results()[0]).ty() != ty
        || PtrType::view(c, c.get_value(pointer).ty()).and_then(|t| t.pointee()) != Some(ty)
        || s.state_operands()
            .iter()
            .chain(s.state_results().iter())
            .chain(l.state_results().iter())
            .any(|v| c.get_value(*v).ty() != TypeId::STATE)
    {
        return Err(MemoryProofError::Domain);
    }
    let width = crate::ptr::semantics::ordinary_width(c, ty).ok_or(MemoryProofError::Domain)?;
    let pointer_value = PtrType::view(c, ty).is_some();
    let mut layouts = Vec::new();
    for id in [scope, store, load] {
        let layout = crate::layout::DataLayout::for_op(c, id);
        if !ordinary_layout(layout.as_ref(), pointer_value) {
            return Err(MemoryProofError::Domain);
        }
        layouts.push(layout.map(|layout| layout.spec()));
    }
    let sb = crate::ptr::semantics::store_memory_semantics(&sop, c)
        .ok_or(MemoryProofError::Unsupported)?;
    let lb = crate::ptr::semantics::load_memory_semantics(&lop, c)
        .ok_or(MemoryProofError::Unsupported)?;
    check_body(c, &sb, true, width)?;
    check_body(c, &lb, false, width)?;
    prove(&sb, &lb, width)?;
    let prerequisites: Vec<_> = s
        .operands()
        .iter()
        .filter_map(|v| c.get_value(*v).defining_op())
        .collect();
    for id in prerequisites {
        if !source.iter().any(|s| s.id == id) {
            source.push(Source::capture(c, id)?);
        }
    }
    let mut replacements = vec![(l.value_results()[0], stored)];
    if let Some(&state) = l.state_results().first() {
        replacements.push((state, s.state_results()[0]));
    }
    Ok(ForwardWitness {
        scope,
        store,
        load,
        source,
        bodies: [declaration(&sb), declaration(&lb)],
        layouts,
        replacements,
    })
}

fn check_body(c: &Context, body: &Body, writing: bool, width: u32) -> Result<(), MemoryProofError> {
    let symbol_types = [
        TypeId::STATE,
        c.terms().int(64),
        c.terms().int(1),
        c.terms().int(u64::from(width)),
        c.terms().int(1),
    ];
    let symbol_types = &symbol_types[..if writing { 5 } else { 3 }];
    let graph = &body.resources.graph;
    for index in 0..graph.len() {
        let node = NodeId::from_index(index);
        let kind = *graph.get_node(node);
        if !matches!(
            kind,
            SymKind::Symbol
                | SymKind::Constant
                | SymKind::Add
                | SymKind::And
                | SymKind::Not
                | SymKind::Concat
                | SymKind::Extract
                | SymKind::If
                | SymKind::Poison
                | SymKind::MemoryByteRead
                | SymKind::MemoryByteWrite
                | SymKind::MemoryRangeValid
        ) || !kind.accepts_arity(graph.children(node).count())
        {
            return Err(MemoryProofError::Unsupported);
        }
        if kind == SymKind::Constant
            && !matches!(graph.get_leaf_data(node), Some(SymPayload::Int(_)))
        {
            return Err(MemoryProofError::Unsupported);
        }
        if kind == SymKind::Symbol {
            let Some(SymPayload::SymbolId(id)) = graph.get_leaf_data(node) else {
                return Err(MemoryProofError::Unsupported);
            };
            if symbol_types.get(*id as usize).is_none()
                || symbol_types.get(*id as usize).copied()
                    != graph.get_annotation(node).and_then(|m| m.actual_type)
            {
                return Err(MemoryProofError::Unsupported);
            }
        }
    }
    super::infer_types(c.terms(), graph, |node| {
        if let Some(SymPayload::Int(value)) = graph.get_leaf_data(node) {
            Some(c.terms().int(value.width()))
        } else {
            graph.get_annotation(node).and_then(|m| m.actual_type)
        }
    })
    .map_err(|_| MemoryProofError::Unsupported)?;
    let symbol_widths = [0, 64, 1, width, 1, 1];
    let widths = super::infer_widths(graph, |node| match graph.get_leaf_data(node) {
        Some(SymPayload::SymbolId(id)) if *id != 0 => symbol_widths.get(*id as usize).copied(),
        _ => None,
    });
    let width = |node: NodeId| widths.get(node.index()).copied().flatten();
    if [body.normal, body.address_poison, body.value_poison]
        .into_iter()
        .any(|node| width(node) != Some(1))
        || body
            .resources
            .value_results
            .iter()
            .any(|&node| width(node) != Some(symbol_widths[3]))
        || std::iter::once(&body.resources.root)
            .chain(&body.resources.state_results)
            .any(|node| !widths.get(node.index()).is_some_and(Option::is_none))
    {
        return Err(MemoryProofError::Unsupported);
    }
    Ok(())
}

fn scalar_copy(g: &SemGraph, root: NodeId) -> SemGraph {
    let mut out = SemGraph::new();
    copy_subgraph(&mut out, g, root, &mut HashMap::new());
    out
}
fn scalar_equal(lhs: &SemGraph, rhs: &SemGraph, widths: &[u32]) -> bool {
    // Unsized states and functional cells must never enter scalar bitblasting.
    let residual = |g: &SemGraph| {
        (0..g.len()).any(|index| {
            let n = NodeId::from_index(index);
            matches!(
                g.get_node(n),
                SymKind::MemoryByteRead | SymKind::MemoryByteWrite | SymKind::MemoryRangeValid
            ) || matches!(g.get_leaf_data(n), Some(SymPayload::SymbolId(0)))
        })
    };
    !residual(lhs)
        && !residual(rhs)
        && SmtOracle.equivalent_outcome(lhs, rhs, widths) == ProofOutcome::Proven
}
fn equal(g: &SemGraph, a: NodeId, b: NodeId, widths: &[u32]) -> bool {
    scalar_equal(&scalar_copy(g, a), &scalar_copy(g, b), widths)
}
fn constant(g: &SemGraph, n: NodeId) -> Option<u64> {
    match g.get_leaf_data(n) {
        Some(SymPayload::Int(v)) => Some(v.to_u64()),
        _ => None,
    }
}

fn written_cell(
    src: &SemGraph,
    mut state: NodeId,
    address: NodeId,
    widths: &[u32],
) -> Option<NodeId> {
    while *src.get_node(state) == SymKind::MemoryByteWrite {
        let update: Vec<_> = src.children(state).collect();
        if equal(src, address, update[1], widths) {
            return Some(update[2]);
        }
        let mut query = scalar_copy(src, address);
        let a = NodeId::from_index(query.len() - 1);
        let b = copy_subgraph(&mut query, src, update[1], &mut HashMap::new());
        let eq = op(&mut query, SymKind::Eq, &[a, b]);
        let zero = con(&mut query, 0, 1);
        if !equal(&query, eq, zero, widths) {
            return None;
        }
        state = update[0];
    }
    None
}

fn reduce_node(
    dst: &mut SemGraph,
    src: &SemGraph,
    n: NodeId,
    payload: bool,
    failed: &mut bool,
    widths: &[u32],
) -> NodeId {
    copy_subgraph_with(dst, src, n, &mut HashMap::new(), &mut |dst, n| {
        let ch: Vec<_> = src.children(n).collect();
        let kind = *src.get_node(n);
        if kind == SymKind::Symbol
            && !matches!(src.get_leaf_data(n), Some(SymPayload::SymbolId(1..=5)))
        {
            *failed = true;
        }
        if payload && matches!(src.get_leaf_data(n), Some(SymPayload::SymbolId(4))) {
            return CopyAction::Replace(con(dst, 0, 1));
        }
        if kind == SymKind::If && payload {
            let cond = reduce_node(dst, src, ch[0], payload, failed, widths);
            let zero = con(dst, 0, 1);
            let one = con(dst, 1, 1);
            let branch = if equal(dst, cond, zero, widths) {
                Some(ch[2])
            } else if equal(dst, cond, one, widths) {
                Some(ch[1])
            } else {
                None
            };
            if let Some(b) = branch {
                return CopyAction::Replace(reduce_node(dst, src, b, payload, failed, widths));
            }
        }
        if kind == SymKind::Extract {
            let mut cell = ch[0];
            if *src.get_node(cell) == SymKind::MemoryByteRead {
                let read: Vec<_> = src.children(cell).collect();
                if let Some(found) = written_cell(src, read[0], read[1], widths) {
                    cell = found;
                }
            }
            if *src.get_node(cell) == SymKind::Concat {
                let pair: Vec<_> = src.children(cell).collect();
                let inferred = super::infer_widths(src, |n| match src.get_leaf_data(n) {
                    Some(SymPayload::SymbolId(i)) if *i != 0 => widths.get(*i as usize).copied(),
                    _ => None,
                });
                if let (Some(hi), Some(lo), Some(w)) = (
                    constant(src, ch[1]),
                    constant(src, ch[2]),
                    inferred[pair[1].index()],
                ) {
                    let chosen = if lo == 0 && hi + 1 == u64::from(w) {
                        Some(pair[1])
                    } else if lo == u64::from(w)
                        && inferred[pair[0].index()].is_some_and(|v| hi + 1 == u64::from(w + v))
                    {
                        Some(pair[0])
                    } else {
                        None
                    };
                    if let Some(v) = chosen {
                        return CopyAction::Replace(reduce_node(
                            dst, src, v, payload, failed, widths,
                        ));
                    }
                }
            }
        }
        if kind == SymKind::MemoryRangeValid {
            let mut state = ch[0];
            while *src.get_node(state) == SymKind::MemoryByteWrite {
                state = src.children(state).next().unwrap_or(state);
            }
            if !matches!(src.get_leaf_data(state), Some(SymPayload::SymbolId(0))) {
                *failed = true;
            }
            let p = scalar_copy(src, ch[1]);
            let size = scalar_copy(src, ch[2]);
            let mut want = SemGraph::new();
            sym(&mut want, 1);
            let mut extent = SemGraph::new();
            con(&mut extent, u64::from(widths[3] / 8), 64);
            if !scalar_equal(&p, &want, widths) || !scalar_equal(&size, &extent, widths) {
                *failed = true;
            }
            return CopyAction::Replace(sym(dst, 5));
        }
        if kind == SymKind::MemoryByteRead {
            if let Some(cell) = written_cell(src, ch[0], ch[1], widths) {
                return CopyAction::Replace(reduce_node(dst, src, cell, payload, failed, widths));
            }
            *failed = true;
            return CopyAction::Replace(con(dst, 0, 9));
        }
        if !matches!(
            kind,
            SymKind::Symbol
                | SymKind::Constant
                | SymKind::Add
                | SymKind::And
                | SymKind::Not
                | SymKind::Concat
                | SymKind::Extract
                | SymKind::If
                | SymKind::Poison
        ) {
            *failed = true;
        }
        CopyAction::Keep
    })
}

// Unknown addresses and base reads reject; only cell updates preserve domains.
fn reduce(
    src: &SemGraph,
    root: NodeId,
    payload: bool,
    widths: &[u32],
) -> Result<SemGraph, MemoryProofError> {
    let mut g = SemGraph::new();
    let mut failed = false;
    reduce_node(&mut g, src, root, payload, &mut failed, widths);
    if failed {
        Err(MemoryProofError::Unsupported)
    } else {
        Ok(g)
    }
}

fn prove(s: &Body, l: &Body, width: u32) -> Result<(), MemoryProofError> {
    if s.resources.state_results.len() != 1
        || !s.resources.value_results.is_empty()
        || l.resources.state_results.len() != 1
        || l.resources.value_results.len() != 1
        || s.resources.raised_flags.is_some()
        || l.resources.raised_flags.is_some()
    {
        return Err(MemoryProofError::Unsupported);
    }
    let widths = &[0, 64, 1, width, 1, 1];
    let mut g = SemGraph::new();
    let mut memo = HashMap::new();
    let state = copy_subgraph(
        &mut g,
        &s.resources.graph,
        s.resources.state_results[0],
        &mut memo,
    );
    let sn = copy_subgraph(&mut g, &s.resources.graph, s.normal, &mut memo);
    let sa = copy_subgraph(&mut g, &s.resources.graph, s.address_poison, &mut memo);
    let sp = copy_subgraph(&mut g, &s.resources.graph, s.value_poison, &mut memo);
    let mut memo = HashMap::new();
    let mut compose = |root| {
        copy_subgraph_with(&mut g, &l.resources.graph, root, &mut memo, &mut |_, n| {
            if matches!(
                l.resources.graph.get_leaf_data(n),
                Some(SymPayload::SymbolId(0))
            ) {
                CopyAction::Replace(state)
            } else {
                CopyAction::Keep
            }
        })
    };
    let outgoing = compose(l.resources.state_results[0]);
    let ln = compose(l.normal);
    let la = compose(l.address_poison);
    let lp = compose(l.value_poison);
    let value = compose(l.resources.value_results[0]);
    // State equality is structural, never a finite scalar encoding.
    if s.resources.root != s.resources.state_results[0]
        || l.resources.root != l.resources.state_results[0]
        || outgoing != state
    {
        return Err(MemoryProofError::NotProven);
    }
    for (a, b, payload) in [(sn, ln, false), (sa, la, false), (sp, lp, false)] {
        if !scalar_equal(
            &reduce(&g, a, payload, widths)?,
            &reduce(&g, b, payload, widths)?,
            widths,
        ) {
            return Err(MemoryProofError::NotProven);
        }
    }
    let children: Vec<_> = g.children(value).collect();
    if *g.get_node(value) != SymKind::Poison || children.len() != 2 || children[0] != lp {
        return Err(MemoryProofError::Unsupported);
    }
    let actual = reduce(&g, children[1], true, widths)?;
    let mut expected = SemGraph::new();
    sym(&mut expected, 3);
    if !scalar_equal(&actual, &expected, widths) {
        return Err(MemoryProofError::NotProven);
    }
    Ok(())
}
