use super::*;
use tir_symbolic::lang::{SymKind, SymPayload};
use tir_symbolic::sem::{SemGraph, copy_subgraph};

fn transfer_proof_effects(
    candidate: &TransferCandidate<'_>,
    actual: &sem_expr_state::BehaviorGraph,
) -> Option<(Vec<tir_adt::NodeId>, Vec<tir_adt::NodeId>)> {
    use sem_expr_state::{Destination, EffectPayload};
    let handlers: HashSet<_> = actual
        .effect_nodes()
        .filter(|node| *actual.graph.get_node(*node) == SymKind::StateHandler)
        .flat_map(|node| actual.graph.preorder(node))
        .collect();
    let mut writes = Vec::new();
    let mut memory = Vec::new();
    for node in actual
        .effect_nodes()
        .filter(|node| !handlers.contains(node))
    {
        let kind = *actual.graph.get_node(node);
        if matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory) {
            memory.push(node);
        }
        let Some(effect) = actual.effect_payload(node) else {
            continue;
        };
        match (kind, effect) {
            (
                SymKind::StateBlock | SymKind::StateTry | SymKind::StateStore,
                EffectPayload::None,
            )
            | (SymKind::StateAssign, EffectPayload::Bind { .. }) => {}
            (
                SymKind::StateAssign,
                EffectPayload::Assign {
                    destination: Destination::Ident(name),
                },
            ) if candidate.destination == Some(name.as_str()) => {
                writes.push(actual.graph.children(node).next()?);
            }
            _ => return None,
        }
    }
    if writes.len() != usize::from(candidate.destination.is_some())
        || memory.len() != usize::from(matches!(candidate.kind, "Spill" | "Reload"))
    {
        return None;
    }
    Some((writes, memory))
}

struct TransferProofBindings {
    source_id: u32,
    base_id: u32,
    memory_id: u32,
    offset_id: u32,
    bindings: HashMap<u32, u32>,
}

fn transfer_proof_bindings(
    candidate: &TransferCandidate<'_>,
    actual: &sem_expr_state::BehaviorGraph,
    widths: &mut Vec<InstructionProofSymbol>,
) -> Option<TransferProofBindings> {
    let source_id = u32::try_from(widths.len()).ok()?;
    let source_type = if let Some(source) = candidate.source.first() {
        *widths.get(*actual.variable_symbols.get(*source)? as usize)?
    } else {
        InstructionProofSymbol::integer(candidate.width)
    };
    for source in &candidate.source {
        if *widths.get(*actual.variable_symbols.get(*source)? as usize)? != source_type {
            return None;
        }
    }
    widths.push(source_type);
    let base_id = u32::try_from(widths.len()).ok()?;
    let base_type = if let Some(address) = &candidate.address {
        *widths.get(*actual.variable_symbols.get(address.base)? as usize)?
    } else {
        InstructionProofSymbol::integer(1)
    };
    if base_type.kind != InstructionValueKind::Integer {
        return None;
    }
    widths.push(base_type);
    let memory_id = u32::try_from(widths.len()).ok()?;
    widths.push(InstructionProofSymbol {
        width: candidate.width,
        kind: if candidate.width > 64 {
            InstructionValueKind::RawBits
        } else {
            InstructionValueKind::Integer
        },
    });
    let offset_id = u32::try_from(widths.len()).ok()?;
    widths.push(InstructionProofSymbol::integer(
        candidate
            .address
            .as_ref()
            .and_then(|address| address.offset)
            .map_or(1, |(_, bits, _, _)| u32::from(bits)),
    ));
    let mut bindings = HashMap::new();
    for (name, symbol) in &actual.variable_symbols {
        let binding = if candidate.source.contains(&name.as_str()) {
            Some(source_id)
        } else if candidate
            .address
            .as_ref()
            .is_some_and(|address| address.base == name)
        {
            Some(base_id)
        } else if candidate
            .address
            .as_ref()
            .and_then(|address| address.offset)
            .is_some_and(|(field, _, _, _)| field == name)
        {
            Some(offset_id)
        } else {
            None
        };
        if let Some(binding) = binding {
            bindings.insert(*symbol, binding);
        }
    }
    Some(TransferProofBindings {
        source_id,
        base_id,
        memory_id,
        offset_id,
        bindings,
    })
}

pub(super) fn transfer_candidate_proves(
    candidate: &TransferCandidate<'_>,
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    actual: &sem_expr_state::BehaviorGraph,
    parameters: &HashMap<String, i64>,
) -> bool {
    transfer_candidate_proof(candidate, tables, ctx, actual, parameters).unwrap_or(false)
}

fn transfer_candidate_proof(
    candidate: &TransferCandidate<'_>,
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    actual: &sem_expr_state::BehaviorGraph,
    parameters: &HashMap<String, i64>,
) -> Option<bool> {
    use tir_symbolic::sem::{CopyAction, con, copy_subgraph_with, op, sym};
    let (writes, memory) = transfer_proof_effects(candidate, actual)?;
    let mut widths = instruction_symbol_widths(tables, ctx, actual, parameters)?;
    let TransferProofBindings {
        source_id,
        base_id,
        memory_id,
        offset_id,
        bindings,
    } = transfer_proof_bindings(candidate, actual, &mut widths)?;
    let value_graph = |root, replace_load: bool| -> Option<(SemGraph, tir_adt::NodeId)> {
        let (source, root) = actual.value_graph(root)?;
        let (source, root) =
            instruction_value_graph(&source, root, &actual.variable_symbols, ctx, parameters);
        let mut graph = SemGraph::<()>::new();
        let root = copy_subgraph_with(
            &mut graph,
            &source,
            root,
            &mut HashMap::new(),
            &mut |graph, node| {
                if replace_load && *source.get_node(node) == SymKind::LoadMemory {
                    return CopyAction::Replace(sym(graph, memory_id));
                }
                if let Some(SymPayload::SymbolId(symbol)) = source.get_leaf_data(node) {
                    if let Some(binding) = bindings.get(symbol) {
                        return CopyAction::Replace(sym(graph, *binding));
                    }
                    if candidate.kind == "Copy"
                        && let Some((name, _)) =
                            actual.variable_symbols.iter().find(|(_, id)| *id == symbol)
                    {
                        let bits = match ctx.ops_map.get(name) {
                            Some(Type::Bits(bits)) => Some(u32::from(*bits)),
                            Some(Type::Integer) => Some(64),
                            _ => None,
                        };
                        if let Some(bits) = bits {
                            return CopyAction::Replace(con(graph, 0, bits));
                        }
                    }
                }
                CopyAction::Keep
            },
        );
        Some((graph, root))
    };
    let address_graph = || -> Option<(SemGraph, tir_adt::NodeId)> {
        let address = candidate.address.as_ref()?;
        let mut graph = SemGraph::new();
        let base = sym(&mut graph, base_id);
        let value = if let Some((_, _, signed, factor)) = address.offset {
            let offset = sym(&mut graph, offset_id);
            let bits = con(&mut graph, u64::from(address.bits), 64);
            let offset = op(
                &mut graph,
                if signed { SymKind::SExt } else { SymKind::ZExt },
                &[offset, bits],
            );
            op(
                &mut graph,
                if factor < 0 {
                    SymKind::Sub
                } else {
                    SymKind::Add
                },
                &[base, offset],
            )
        } else {
            base
        };
        Some((graph, value))
    };
    if let Some(memory) = memory.first() {
        let kind = *actual.graph.get_node(*memory);
        if kind
            != if candidate.kind == "Spill" {
                SymKind::StoreMemory
            } else {
                SymKind::LoadMemory
            }
        {
            return None;
        }
        let children: Vec<_> = actual.graph.children(*memory).collect();
        let (size_graph, size) = value_graph(*children.get(1)?, false)?;
        if instruction_constant(&size_graph, size)? != u64::from(candidate.width / 8) {
            return None;
        }
        let metadata = children.get(if candidate.kind == "Spill" { 3 } else { 2 })?;
        let (meta_graph, metadata) = value_graph(*metadata, false)?;
        if instruction_constant(&meta_graph, metadata)? > u64::from(candidate.kind == "Reload") {
            return None;
        }
        let (mut graph, address) = value_graph(*children.first()?, false)?;
        let (expected, root) = address_graph()?;
        let root = copy_subgraph(&mut graph, &expected, root, &mut HashMap::new());
        if !instruction_value_proves(
            &graph,
            address,
            root,
            candidate.address.as_ref()?.bits,
            &widths,
        ) {
            return None;
        }
    }
    let value = if candidate.kind == "Spill" {
        actual.graph.children(*memory.first()?).nth(2)?
    } else {
        *writes.first()?
    };
    let (mut graph, value) = value_graph(value, candidate.kind == "Reload")?;
    let expected = if candidate.kind == "AddressAdd" {
        let (expected, root) = address_graph()?;
        copy_subgraph(&mut graph, &expected, root, &mut HashMap::new())
    } else {
        sym(
            &mut graph,
            if candidate.kind == "Reload" {
                memory_id
            } else {
                source_id
            },
        )
    };
    Some(instruction_value_proves(
        &graph,
        value,
        expected,
        candidate.width,
        &widths,
    ))
}
