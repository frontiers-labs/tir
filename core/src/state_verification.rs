use crate::{Context, Error, OpHandle, OpId, RegionId};

/// Checks the fork/join discipline resource state follows.
///
/// Memory permits unordered reads because each leaves memory as it found it, but
/// at most one unordered operation may change it. Other resources, including the
/// floating-point environment, describe one executed continuation and do not
/// permit unordered reads. Mutually exclusive branch arms may consume the same
/// state. An operation naming a state twice consumes it once, except for
/// exports through region results or return and yield terminators.
///
/// Theta consumers are exclusive when their demand paths are disjoint. This
/// check does not prove that every access is in the cone of the state its next
/// write takes; insertion-order shuffling and `--shuffle-seed` check that.
///
/// State crossing a region boundary does so as a carried argument, which is a
/// fresh value, so a single walk of the whole tree suffices.
pub(crate) fn verify_state_forks(context: &Context, op_id: OpId) -> Result<(), Error> {
    let mut consumers: tir_adt::IndexMap<crate::ValueId, Vec<(StateConsumer, bool)>> =
        tir_adt::IndexMap::new();
    let mut theta_paths = std::collections::HashMap::new();
    let mut worklist = vec![op_id];
    while let Some(op_id) = worklist.pop() {
        let instance = context.get_op(op_id);
        let exports =
            instance.is::<crate::func::ReturnOp>() || instance.is::<crate::scf::YieldOp>();
        for operand in instance.state_operands() {
            let resource = context
                .state_resource(context.get_value(operand).ty())
                .expect("state operand has a state type");
            let observes = resource_access(&instance, resource) != crate::ResourceAccess::Change;
            let taken = consumers.entry(operand).or_default();
            let consumer = StateConsumer::Op(op_id);
            if exports || !taken.iter().any(|(taker, _)| *taker == consumer) {
                taken.push((consumer, observes));
            }
        }
        for region_id in instance.regions().iter().rev() {
            let region = context.get_region(*region_id);
            let results = region.results();
            let paths = instance
                .clone()
                .as_interface::<dyn crate::Theta>()
                .filter(|theta| theta.body() == *region_id)
                .map(|theta| {
                    let binding = theta.binding();
                    let mut paths = std::collections::HashMap::new();
                    for (path, range) in [
                        (ExportPath::Continue, binding.continue_.clone()),
                        (ExportPath::Exit, binding.exit.clone()),
                    ] {
                        let mut roots = results[range].to_vec();
                        roots.push(theta.predicate());
                        while let Some(value) = roots.pop() {
                            let Some(op) = context.get_value(value).defining_op() else {
                                continue;
                            };
                            if context.parent_nodes_region(op) != Some(*region_id) {
                                continue;
                            }
                            let membership = paths.entry(op).or_insert(0);
                            if *membership & path.mask() == 0 {
                                *membership |= path.mask();
                                roots.extend(crate::region::values_read(context, op));
                            }
                        }
                    }
                    theta_paths.insert(*region_id, paths);
                    vec![
                        (ExportPath::Continue, binding.continue_),
                        (ExportPath::Exit, binding.exit),
                    ]
                })
                .unwrap_or_else(|| vec![(ExportPath::Only, 0..results.len())]);
            for (path, range) in paths {
                for &result in &results[range] {
                    if context
                        .state_resource(context.get_value(result).ty())
                        .is_some()
                    {
                        consumers.entry(result).or_default().push((
                            StateConsumer::Export {
                                region: *region_id,
                                path,
                            },
                            true,
                        ));
                    }
                }
            }
            worklist.extend(region.op_ids().iter().rev());
        }
    }
    for (value, taken) in &consumers {
        let permits_read_forks = context
            .state_resource(context.get_value(*value).ty())
            .is_some_and(|resource| resource == crate::builtin::StateResource::Memory);
        let conflicts = taken
            .iter()
            .enumerate()
            .any(|(index, &(left, left_reads))| {
                taken[index + 1..].iter().any(|&(right, right_reads)| {
                    !(permits_read_forks && left_reads && right_reads)
                        && !mutually_exclusive(context, *value, left, right, &theta_paths)
                })
            });
        if conflicts {
            return Err(Error::VerificationError(format!(
                "state %{} has incompatible concurrent uses",
                value.number()
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StateConsumer {
    Op(OpId),
    Export {
        region: crate::RegionId,
        path: ExportPath,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportPath {
    Only,
    Continue,
    Exit,
}

impl ExportPath {
    fn mask(self) -> u8 {
        match self {
            Self::Only => 3,
            Self::Continue => 1,
            Self::Exit => 2,
        }
    }
}

fn mutually_exclusive(
    context: &Context,
    value: crate::ValueId,
    left: StateConsumer,
    right: StateConsumer,
    theta_paths: &std::collections::HashMap<RegionId, std::collections::HashMap<OpId, u8>>,
) -> bool {
    if left == right {
        return false;
    }
    let ancestry = |mut consumer| {
        let mut ancestry = Vec::new();
        let mut region = match consumer {
            StateConsumer::Op(op) => context.region_of_op(op),
            StateConsumer::Export { region, .. } => Some(region),
        };
        while let Some(current) = region {
            let Some(owner) = context.get_region(current).parent_op() else {
                break;
            };
            let paths = match consumer {
                StateConsumer::Op(op) => theta_paths
                    .get(&current)
                    .map_or(3, |paths| paths.get(&op).copied().unwrap_or(0)),
                StateConsumer::Export { path, .. } => path.mask(),
            };
            ancestry.push((owner, current, paths));
            consumer = StateConsumer::Op(owner);
            region = context.region_of_op(owner);
        }
        ancestry
    };
    let left_ancestry = ancestry(left);
    let right_ancestry = ancestry(right);
    let exclusive_arms = left_ancestry
        .iter()
        .any(|&(owner, left_region, left_paths)| {
            right_ancestry
                .iter()
                .find(|(candidate, _, _)| *candidate == owner)
                .is_some_and(|&(_, right_region, right_paths)| {
                    if left_region == right_region {
                        return theta_paths.contains_key(&left_region)
                            && left_paths & right_paths == 0;
                    }
                    context
                        .get_op(owner)
                        .as_interface::<dyn crate::Gamma>()
                        .is_some_and(|gamma| {
                            let arms = gamma.arms();
                            arms.contains(&left_region) && arms.contains(&right_region)
                        })
                })
        });
    exclusive_arms
        || match (left, right) {
            (StateConsumer::Op(left), StateConsumer::Op(right)) => {
                mutually_exclusive_cfg(context, value, left, right)
            }
            _ => false,
        }
}

fn mutually_exclusive_cfg(
    context: &Context,
    value: crate::ValueId,
    left: OpId,
    right: OpId,
) -> bool {
    let Some(region) = context.region_of_op(left) else {
        return false;
    };
    if context.region_of_op(right) != Some(region) || context.get_region(region).is_nodes() {
        return false;
    }
    let (definition, definition_block) = match context.get_value(value).defining_op() {
        Some(op) if context.region_of_op(op) == Some(region) => (Some(op), None),
        None => match context.block_of_argument(value) {
            Some(block) if context.parent_region(block) == Some(region) => (None, Some(block)),
            _ => return false,
        },
        _ => return false,
    };
    if definition.is_some_and(|op| op == left || op == right) {
        return false;
    }

    // A new definition starts another dynamic state lifetime, so paths through it
    // do not make the two uses concurrent.
    !path_before_definition(context, region, left, right, definition, definition_block)
        .unwrap_or(true)
        && !path_before_definition(context, region, right, left, definition, definition_block)
            .unwrap_or(true)
}

fn path_before_definition(
    context: &Context,
    region: crate::RegionId,
    from: OpId,
    to: OpId,
    definition: Option<OpId>,
    definition_block: Option<crate::BlockId>,
) -> Option<bool> {
    let mut pending = vec![from];
    let mut seen = std::collections::HashSet::new();
    while let Some(op_id) = pending.pop() {
        if !seen.insert(op_id) {
            continue;
        }
        if op_id != from {
            if Some(op_id) == definition {
                continue;
            }
            if op_id == to {
                return Some(true);
            }
        }
        let block = context.parent_block(op_id)?;
        let ops = context.get_block(block).op_ids();
        let index = ops.iter().position(|&candidate| candidate == op_id)?;
        // Keep sequential flow even after a terminator. This may reject a valid
        // fork, but it cannot mistake two executable uses for exclusive ones.
        if let Some(&next) = ops.get(index + 1) {
            pending.push(next);
        }
        if let Some(terminator) = context
            .get_op(op_id)
            .as_interface::<dyn crate::Terminator>()
        {
            for successor in terminator.successors() {
                if context.parent_region(successor) != Some(region) {
                    return None;
                }
                if Some(successor) == definition_block {
                    continue;
                }
                let first = context.get_block(successor).op_ids().first().copied()?;
                pending.push(first);
            }
        }
    }
    Some(false)
}

fn resource_access(
    op: &OpHandle,
    resource: crate::builtin::StateResource,
) -> crate::ResourceAccess {
    op.clone()
        .as_interface::<dyn crate::ResourceEffects>()
        .and_then(|effects| {
            effects
                .resource_effects()
                .into_iter()
                .find(|effect| effect.resource == resource)
                .map(|effect| effect.access)
        })
        .unwrap_or(crate::ResourceAccess::Read)
}
