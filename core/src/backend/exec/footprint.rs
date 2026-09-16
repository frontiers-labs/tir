use std::collections::{HashMap, HashSet};

use crate::graph::{Dag, NodeId};
use crate::sem::{ExtendSemBytes, SemGraph, SymKind, SymPayload, Value, execute_pure};

use super::{Effect, ExecEnv};

/// A RAM range that must be valid before a multi-access instruction starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRange {
    pub address: u64,
    pub size: usize,
    pub is_write: bool,
}

pub(super) fn preflight(
    env: &ExecEnv,
    effects: &[Effect],
    symbols: &[Value],
) -> Result<Vec<MemoryRange>, &'static str> {
    if effect_count(env, effects) <= 1 {
        return Ok(Vec::new());
    }
    let mut symbols = symbols.to_vec();
    let mut available = vec![true; symbols.len()];
    let mut ranges = Vec::new();
    collect_effects(env, effects, &mut symbols, &mut available, &mut ranges)?;
    if ranges.len() <= 1 {
        ranges.clear();
    }
    Ok(ranges)
}

fn graph(env: &ExecEnv, offset: u32) -> SemGraph {
    let mut graph = SemGraph::new();
    graph.extend_sem_bytes(env.kinds, env.blob, offset);
    graph
}

fn term_count(graph: &SemGraph) -> usize {
    let mut seen = HashSet::new();
    graph
        .preorder(graph.root().unwrap())
        .filter(|node| seen.insert(*node))
        .fold(0_usize, |count, node| {
            let kind = *graph.get_kind(node);
            if matches!(kind, SymKind::Map | SymKind::Reduce) {
                let has_effect = graph
                    .preorder(node)
                    .any(|child| is_memory(*graph.get_kind(child)));
                if has_effect {
                    return usize::MAX;
                }
            }
            count.saturating_add(usize::from(is_memory(kind)))
        })
}

fn is_memory(kind: SymKind) -> bool {
    matches!(
        kind,
        SymKind::LoadMemory
            | SymKind::StoreMemory
            | SymKind::LoadReserved
            | SymKind::StoreConditional
            | SymKind::AtomicRmw
            | SymKind::Fence
    )
}

fn effect_count(env: &ExecEnv, effects: &[Effect]) -> usize {
    effects.iter().fold(0_usize, |count, effect| {
        count.saturating_add(match effect {
            Effect::Assign { offset, .. } | Effect::Bind { offset, .. } => {
                term_count(&graph(env, *offset))
            }
            Effect::Trap { offset } => term_count(&graph(env, *offset)).saturating_add(1),
            Effect::If { cond, then, els } => term_count(&graph(env, *cond))
                .saturating_add(effect_count(env, then).max(effect_count(env, els))),
        })
    })
}

fn collect_effects(
    env: &ExecEnv,
    effects: &[Effect],
    symbols: &mut [Value],
    available: &mut [bool],
    ranges: &mut Vec<MemoryRange>,
) -> Result<(), &'static str> {
    for effect in effects {
        match effect {
            Effect::Assign { offset, .. } | Effect::Bind { offset, .. } => {
                let graph = graph(env, *offset);
                collect_term(
                    &graph,
                    graph.root().unwrap(),
                    symbols,
                    available,
                    &mut HashSet::new(),
                    ranges,
                )?;
                if let Effect::Bind { sym, .. } = effect {
                    let value = pure_at(&graph, graph.root().unwrap(), symbols, available);
                    available[*sym] = value.is_some();
                    if let Some(value) = value {
                        symbols[*sym] = Value::Int(value);
                    }
                }
            }
            Effect::If { cond, then, els } => {
                let graph = graph(env, *cond);
                let condition = pure_at(&graph, graph.root().unwrap(), symbols, available)
                    .ok_or("multi-access branch depends on an external effect")?;
                collect_effects(
                    env,
                    if condition.is_zero() { els } else { then },
                    symbols,
                    available,
                    ranges,
                )?;
            }
            Effect::Trap { .. } => {
                return Err("multi-access exception continuation is not supported");
            }
        }
    }
    Ok(())
}

fn collect_term(
    graph: &SemGraph,
    node: NodeId,
    symbols: &[Value],
    available: &[bool],
    seen: &mut HashSet<NodeId>,
    ranges: &mut Vec<MemoryRange>,
) -> Result<(), &'static str> {
    if !seen.insert(node) {
        return Ok(());
    }
    let kind = *graph.get_kind(node);
    let children: Vec<_> = graph.children(node).collect();
    if matches!(kind, SymKind::If | SymKind::Switch) {
        let condition = pure_at(graph, children[0], symbols, available)
            .ok_or("multi-access condition depends on an external effect")?;
        let index = if kind == SymKind::If {
            if condition.is_zero() { 2 } else { 1 }
        } else {
            condition
                .to_u64()
                .saturating_add(1)
                .min((children.len() - 1) as u64) as usize
        };
        return collect_term(graph, children[index], symbols, available, seen, ranges);
    }
    if matches!(kind, SymKind::Map | SymKind::Reduce) {
        if graph
            .preorder(node)
            .any(|child| is_memory(*graph.get_kind(child)))
        {
            return Err("effectful vector iteration requires an explicit restart policy");
        }
        return Ok(());
    }
    if matches!(
        kind,
        SymKind::LoadReserved | SymKind::StoreConditional | SymKind::AtomicRmw | SymKind::Fence
    ) {
        return Err("multiple atomic or ordering effects require an explicit restart policy");
    }
    for &child in &children {
        collect_term(graph, child, symbols, available, seen, ranges)?;
    }
    if matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory) {
        let address = pure_at(graph, children[0], symbols, available)
            .ok_or("multi-access address depends on an external effect")?
            .to_u64();
        let size = pure_at(graph, children[1], symbols, available)
            .and_then(|value| usize::try_from(value.to_u64()).ok())
            .ok_or("multi-access size is not available before execution")?;
        if !ranges.is_empty()
            && !matches!(kind, SymKind::StoreMemory)
            && ranges.iter().any(|range| range.is_write)
        {
            return Err("a load after an uncommitted store requires an explicit restart policy");
        }
        ranges.push(MemoryRange {
            address,
            size,
            is_write: kind == SymKind::StoreMemory,
        });
    }
    Ok(())
}

fn pure_at(
    graph: &SemGraph,
    root: NodeId,
    symbols: &[Value],
    available: &[bool],
) -> Option<crate::utils::APInt> {
    if graph.preorder(root).any(|node| {
        matches!(graph.get_leaf_data(node), Some(SymPayload::SymbolId(id)) if !available.get(*id as usize).copied().unwrap_or(false))
    }) {
        return None;
    }
    let mut copy = SemGraph::new();
    tir_symbolic::sem::copy_subgraph(&mut copy, graph, root, &mut HashMap::new());
    execute_pure(&copy, symbols)
}
