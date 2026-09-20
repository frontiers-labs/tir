use std::collections::{HashMap, HashSet};

use crate::builtin::IntegerType;
use crate::region::values_read;
use crate::{
    Binding, BlockId, ConstantLike, Context, Gamma, OpHandle, OpId, OperationRef, PassError,
    RegionId, Theta, TypeId, ValueId,
};

use super::{Edge, Edges, Test, decline, gamma, is_structured, theta, values_then_states};

/// Stable identity of one semantic control request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ControlId(pub u32);

/// Stable identity of a region or a lazy-Theta demand domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DemandDomainId(pub u32);

/// A recovery failure that either requires a bounded control demotion and
/// reselection, or is terminal for the current source program.
#[derive(Debug)]
pub(crate) enum RecoveryError {
    Placement(Vec<ControlId>),
    Invalid(PassError),
}

impl From<PassError> for RecoveryError {
    fn from(error: PassError) -> Self {
        Self::Invalid(error)
    }
}

impl From<RecoveryError> for PassError {
    fn from(error: RecoveryError) -> Self {
        match error {
            RecoveryError::Invalid(error) => error,
            RecoveryError::Placement(controls) => PassError::InvalidRuleSet(format!(
                "control recovery placement still rejects {controls:?} after all requested demotions"
            )),
        }
    }
}

/// The semantic part of a region in which an operation may execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DemandDomainKind {
    Region,
    ThetaHead,
    ThetaContinue,
    ThetaExit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SequenceId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Point {
    sequence: SequenceId,
    index: usize,
}

#[derive(Clone, Debug)]
enum SequenceEnd {
    Root {
        values: Vec<ValueId>,
        deps: Vec<ValueId>,
    },
    GammaArm {
        gamma: OpId,
        arm: usize,
        parent: Point,
    },
    ThetaHead {
        theta: OpId,
    },
    ThetaContinue {
        theta: OpId,
    },
    ThetaExit {
        theta: OpId,
        parent: Point,
    },
}

#[derive(Clone, Debug)]
struct Sequence {
    domain: DemandDomainId,
    entry: Vec<ValueId>,
    ops: Vec<OpId>,
    end: SequenceEnd,
}

#[derive(Clone, Debug)]
struct GammaArmPlan {
    sequence: SequenceId,
    ports: Vec<ValueId>,
    results: Vec<ValueId>,
}

#[derive(Clone, Debug)]
struct GammaPlan {
    predicate: ValueId,
    inputs: Vec<ValueId>,
    outputs: Vec<ValueId>,
    arms: Vec<GammaArmPlan>,
}

#[derive(Clone, Debug)]
struct ThetaPlan {
    predicate: ValueId,
    inits: Vec<ValueId>,
    ports: Vec<ValueId>,
    continue_values: Vec<ValueId>,
    exit_values: Vec<ValueId>,
    outputs: Vec<ValueId>,
    body: RegionId,
    head: SequenceId,
    continue_: SequenceId,
    exit: SequenceId,
}

#[derive(Clone, Debug)]
struct Fragment {
    sequence: SequenceId,
    end: usize,
    ops: Vec<OpId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum PreparedFact {
    Exact(u64),
    DefaultFrom(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct EntryFact {
    pub predicate: ValueId,
    pub fact: PreparedFact,
}

/// PREPARE's private graph. Continuations stay symbolic until FINISH has an
/// outcome fact and a simultaneous binding environment with which to route
/// them.
struct Prepared {
    root: SequenceId,
    sequences: Vec<Sequence>,
    gammas: HashMap<OpId, GammaPlan>,
    thetas: HashMap<OpId, ThetaPlan>,
    fragments: Vec<Fragment>,
    fragment_at: HashMap<Point, usize>,
    structured: Vec<OpId>,
    pub(super) entry_facts: HashMap<SequenceId, Vec<EntryFact>>,
}

struct Prepare<'a> {
    context: &'a Context,
    edges: &'a dyn Edges,
    recovery: &'a RecoveryPlan,
    prepared: Prepared,
}

impl Prepared {
    fn analyze(
        context: &Context,
        region: RegionId,
        edges: &dyn Edges,
        recovery: &RecoveryPlan,
    ) -> Result<Self, RecoveryError> {
        let root_domain = recovery.region_domain(region).ok_or_else(|| {
            PassError::InvalidRuleSet("root region has no recovery demand domain".into())
        })?;
        let handle = context.get_region(region);
        let end = SequenceEnd::Root {
            values: handle.value_results(),
            deps: handle.state_results(),
        };
        let mut prepare = Prepare {
            context,
            edges,
            recovery,
            prepared: Self {
                root: SequenceId(0),
                sequences: Vec::new(),
                gammas: HashMap::new(),
                thetas: HashMap::new(),
                fragments: Vec::new(),
                fragment_at: HashMap::new(),
                structured: Vec::new(),
                entry_facts: HashMap::new(),
            },
        };
        let root = prepare.normal_region(region, root_domain, end)?;
        prepare.prepared.root = root;
        prepare.propagate_entry_facts();
        prepare.make_fragments();
        Ok(prepare.prepared)
    }
}

/// Stable Kahn order for contracted groups and the operations inside them.
fn stable_group_order(
    outgoing: &[HashSet<usize>],
    pending: &mut [usize],
    ranks: &[usize],
) -> Vec<usize> {
    let mut ready: std::collections::BTreeSet<_> = pending
        .iter()
        .enumerate()
        .filter_map(|(node, &count)| (count == 0).then_some((ranks[node], node)))
        .collect();
    let mut order = Vec::with_capacity(pending.len());
    while let Some((_, node)) = ready.pop_first() {
        order.push(node);
        for &reader in &outgoing[node] {
            pending[reader] -= 1;
            if pending[reader] == 0 {
                ready.insert((ranks[reader], reader));
            }
        }
    }
    order
}

impl Prepare<'_> {
    fn control_reads(&self, op: &OpHandle, test: Test) -> Vec<ValueId> {
        match self.recovery.control(op.id, test) {
            Some(control) => self.edges.control_reads(control, op),
            None => self.edges.test_reads(op, test),
        }
    }

    fn test_reads_under(&self, op: OpId, read: &mut Vec<ValueId>) {
        let instance = self.context.get_op(op);
        if let Some(gamma) = instance.clone().as_interface::<dyn Gamma>() {
            for index in 0..gamma.arms().len().saturating_sub(1) {
                read.extend(self.control_reads(&instance, Test::Arm(index)));
            }
        } else if instance.clone().as_interface::<dyn Theta>().is_some() {
            read.extend(self.control_reads(&instance, Test::Repeat));
        }
        for region in instance.regions() {
            for child in self.context.get_region(region).op_ids() {
                self.test_reads_under(child, read);
            }
        }
    }

    fn inputs(&self, op: OpId) -> Vec<OpId> {
        let mut read = values_read(self.context, op);
        self.test_reads_under(op, &mut read);
        let mut inputs: Vec<OpId> = read
            .into_iter()
            .filter_map(|value| self.context.get_value(value).defining_op())
            .collect();
        inputs.extend(self.edges.implicit_inputs(op));
        inputs
    }

    fn order(&self, region: RegionId) -> Result<Vec<OpId>, RecoveryError> {
        let ops = self.context.get_region(region).op_ids();
        let held: HashSet<OpId> = ops.iter().copied().collect();
        let mut pending: HashMap<OpId, usize> = HashMap::new();
        let mut readers: HashMap<OpId, Vec<OpId>> = HashMap::new();
        for &op in &ops {
            let inputs: HashSet<OpId> = self
                .inputs(op)
                .into_iter()
                .filter(|input| held.contains(input))
                .collect();
            for &input in &inputs {
                readers.entry(input).or_default().push(op);
            }
            pending.insert(op, inputs.len());
        }
        let mut order = Vec::with_capacity(ops.len());
        let mut ready: Vec<OpId> = ops
            .iter()
            .rev()
            .copied()
            .filter(|op| pending[op] == 0)
            .collect();
        while let Some(op) = ready.pop() {
            order.push(op);
            for &reader in readers.get(&op).into_iter().flatten() {
                let count = pending.get_mut(&reader).expect("reader belongs to region");
                *count -= 1;
                if *count == 0 {
                    ready.push(reader);
                    ready.sort_by_key(|op| {
                        std::cmp::Reverse(ops.iter().position(|held| held == op))
                    });
                }
            }
        }
        if order.len() != ops.len() {
            return Err(PassError::InvalidRuleSet(
                "an unordered recovery region contains a dependency cycle".into(),
            )
            .into());
        }
        self.abut_direct_controls(region, &mut order)?;
        super::abut_implicit_inputs(self.edges, &mut order);
        Ok(order)
    }

    /// Pick the PCF evaluation order promised by normalization. Contract each
    /// direct predicate producer, its transparent aliases, and its consumer
    /// before choosing a stable dependency order. A contraction cycle means
    /// the saved direct request is stale and recovery must reject it.
    fn abut_direct_controls(
        &self,
        region: RegionId,
        order: &mut Vec<OpId>,
    ) -> Result<(), RecoveryError> {
        fn root(parents: &mut [usize], mut index: usize) -> usize {
            let mut root = index;
            while parents[root] != root {
                root = parents[root];
            }
            while parents[index] != index {
                let next = parents[index];
                parents[index] = root;
                index = next;
            }
            root
        }

        fn join(parents: &mut [usize], lhs: usize, rhs: usize) {
            let lhs = root(parents, lhs);
            let rhs = root(parents, rhs);
            if lhs != rhs {
                parents[rhs] = lhs;
            }
        }

        fn add_edge(
            outgoing: &mut [HashSet<usize>],
            pending: &mut [usize],
            from: usize,
            to: usize,
        ) {
            if from != to && outgoing[from].insert(to) {
                pending[to] += 1;
            }
        }

        let original = order.clone();
        let indices: HashMap<OpId, usize> = original
            .iter()
            .enumerate()
            .map(|(index, &op)| (op, index))
            .collect();
        let mut parents: Vec<usize> = (0..original.len()).collect();
        let mut constraints = Vec::new();
        let mut terminals = Vec::new();
        let mut anchors = Vec::new();
        let mut moved = HashSet::new();
        for control in self
            .recovery
            .requirements()
            .iter()
            .filter(|control| control.direct && control.consumer_scope == region)
        {
            let consumer = self.context.get_op(control.consumer);
            let predicate = match control.test {
                Test::Arm(_) => gamma(&consumer)?.predicate(),
                Test::Repeat => theta(&consumer)?.predicate(),
            };
            let Some(producer) = self.context.get_value(predicate).defining_op() else {
                continue;
            };
            // A fused branch retains the source predicate's identity after
            // its value-producing operation has been erased. Its selected
            // inputs, rather than that retired operation, determine order.
            if !indices.contains_key(&producer) {
                continue;
            }
            if !moved.insert((producer, control.consumer)) {
                continue;
            }
            let group = self.direct_control_group(producer, region, control.domain);
            let group: Vec<usize> = original
                .iter()
                .enumerate()
                .filter_map(|(index, op)| group.contains(op).then_some(index))
                .collect();
            let Some(&anchor) = group.first() else {
                continue;
            };
            for &member in &group[1..] {
                join(&mut parents, anchor, member);
            }
            for pair in group.windows(2) {
                constraints.push((pair[0], pair[1], control.id));
            }
            if let Some(&consumer) = indices.get(&control.consumer) {
                join(&mut parents, anchor, consumer);
                constraints.push((
                    *group.last().expect("a direct control group has a producer"),
                    consumer,
                    control.id,
                ));
            } else if control.test == Test::Repeat {
                terminals.push((anchor, control.id));
            }
            anchors.push((anchor, control.id));
        }
        if anchors.is_empty() {
            return Ok(());
        }

        let mut root_to_component = HashMap::new();
        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut component_of = Vec::with_capacity(original.len());
        for index in 0..original.len() {
            let root = root(&mut parents, index);
            let component = match root_to_component.get(&root) {
                Some(&component) => component,
                None => {
                    let component = components.len();
                    root_to_component.insert(root, component);
                    components.push(Vec::new());
                    component
                }
            };
            components[component].push(index);
            component_of.push(component);
        }

        let mut component_controls = vec![None; components.len()];
        for &(anchor, control) in &anchors {
            let component = component_of[anchor];
            component_controls[component] = Some(
                component_controls[component].map_or(control, |held: ControlId| held.min(control)),
            );
        }
        let mut outgoing = vec![HashSet::new(); components.len()];
        let mut pending = vec![0; components.len()];
        for (to, &op) in original.iter().enumerate() {
            for input in self.inputs(op) {
                let Some(&from) = indices.get(&input) else {
                    continue;
                };
                add_edge(
                    &mut outgoing,
                    &mut pending,
                    component_of[from],
                    component_of[to],
                );
            }
        }
        for &(anchor, _) in &terminals {
            let terminal = component_of[anchor];
            for component in 0..components.len() {
                add_edge(&mut outgoing, &mut pending, component, terminal);
            }
        }

        let ranks: Vec<usize> = components.iter().map(|members| members[0]).collect();
        let component_order = stable_group_order(&outgoing, &mut pending, &ranks);
        if component_order.len() != components.len() {
            let control = pending
                .iter()
                .enumerate()
                .filter(|&(_, &count)| count != 0)
                .filter_map(|(component, _)| component_controls[component])
                .min()
                .unwrap_or(anchors[0].1);
            return Err(RecoveryError::Placement(vec![control]));
        }

        let mut scheduled = Vec::with_capacity(original.len());
        for component in component_order {
            let members = &components[component];
            if members.len() == 1 {
                scheduled.push(original[members[0]]);
                continue;
            }
            let local_indices: HashMap<usize, usize> = members
                .iter()
                .enumerate()
                .map(|(local, &global)| (global, local))
                .collect();
            let mut local_outgoing = vec![HashSet::new(); members.len()];
            let mut local_pending = vec![0; members.len()];
            for (to, &global) in members.iter().enumerate() {
                for input in self.inputs(original[global]) {
                    let Some(&input) = indices.get(&input) else {
                        continue;
                    };
                    let Some(&from) = local_indices.get(&input) else {
                        continue;
                    };
                    add_edge(&mut local_outgoing, &mut local_pending, from, to);
                }
            }
            for &(from, to, _) in &constraints {
                let (Some(&from), Some(&to)) = (local_indices.get(&from), local_indices.get(&to))
                else {
                    continue;
                };
                add_edge(&mut local_outgoing, &mut local_pending, from, to);
            }
            let start = scheduled.len();
            let local_order = stable_group_order(&local_outgoing, &mut local_pending, members);
            scheduled.extend(
                local_order
                    .into_iter()
                    .map(|local| original[members[local]]),
            );
            if scheduled.len() - start != members.len() {
                let control = constraints
                    .iter()
                    .filter(|(from, _, _)| component_of[*from] == component)
                    .map(|&(_, _, control)| control)
                    .min()
                    .or(component_controls[component])
                    .unwrap_or(anchors[0].1);
                return Err(RecoveryError::Placement(vec![control]));
            }
        }
        *order = scheduled;
        Ok(())
    }

    fn direct_control_group(
        &self,
        producer: OpId,
        region: RegionId,
        domain: DemandDomainId,
    ) -> HashSet<OpId> {
        let mut group = HashSet::from([producer]);
        let mut values: HashSet<ValueId> = self
            .context
            .get_op(producer)
            .results()
            .into_iter()
            .collect();
        loop {
            let mut changed = false;
            for alias in &self.recovery.control_aliases {
                let source = self.edges.value(alias.source);
                if !values.contains(&source) || !self.recovery.control_only(alias.value) {
                    continue;
                }
                let value = self.edges.value(alias.value);
                let Some(alias_op) = self.context.get_value(value).defining_op() else {
                    continue;
                };
                if self.context.region_of_op(alias_op) != Some(region)
                    || self
                        .edges
                        .execution_domain(alias_op)
                        .or_else(|| self.recovery.domain_of(alias_op))
                        != Some(domain)
                {
                    continue;
                }
                changed |= group.insert(alias_op);
                changed |= values.insert(value);
            }
            if !changed {
                return group;
            }
        }
    }

    fn cone(&self, region: RegionId, roots: &[ValueId]) -> HashSet<OpId> {
        let mut cone = HashSet::new();
        let mut pending: Vec<OpId> = roots
            .iter()
            .filter_map(|&value| self.context.get_value(value).defining_op())
            .collect();
        while let Some(op) = pending.pop() {
            if self.context.parent_nodes_region(op) != Some(region) || !cone.insert(op) {
                continue;
            }
            pending.extend(self.inputs(op));
        }
        cone
    }

    fn require_effects(&self, order: &[OpId], placed: &HashSet<OpId>) -> Result<(), PassError> {
        for &op in order {
            if placed.contains(&op) || self.context.get_op(op).state_results().is_empty() {
                continue;
            }
            let instance = self.context.get_op(op);
            return Err(PassError::InvalidRuleSet(format!(
                "{}.{} leaves a dependency no result demands",
                instance.dialect(),
                instance.name()
            )));
        }
        Ok(())
    }

    fn reserve_sequence(
        &mut self,
        _region: RegionId,
        domain: DemandDomainId,
        entry: Vec<ValueId>,
        ops: Vec<OpId>,
        end: SequenceEnd,
    ) -> SequenceId {
        let id = SequenceId(self.prepared.sequences.len());
        self.prepared.sequences.push(Sequence {
            domain,
            entry,
            ops,
            end,
        });
        id
    }

    fn normal_region(
        &mut self,
        region: RegionId,
        domain: DemandDomainId,
        end: SequenceEnd,
    ) -> Result<SequenceId, RecoveryError> {
        let handle = self.context.get_region(region);
        let order = self.order(region)?;
        let demanded = self.cone(region, &handle.results());
        self.require_effects(&order, &demanded)?;
        let ops = order
            .into_iter()
            .filter(|op| demanded.contains(op))
            .collect();
        let entry = handle.ports().iter().map(crate::Value::id).collect();
        let sequence = self.reserve_sequence(region, domain, entry, ops, end);
        self.prepare_structured(sequence)?;
        Ok(sequence)
    }

    fn prepare_structured(&mut self, sequence: SequenceId) -> Result<(), RecoveryError> {
        let ops = self.prepared.sequences[sequence.0].ops.clone();
        for (index, op_id) in ops.into_iter().enumerate() {
            let op = self.context.get_op(op_id);
            if op.clone().as_interface::<dyn Gamma>().is_some() {
                self.prepare_gamma(
                    op_id,
                    Point {
                        sequence,
                        index: index + 1,
                    },
                )?;
            } else if op.clone().as_interface::<dyn Theta>().is_some() {
                self.prepare_theta(
                    op_id,
                    Point {
                        sequence,
                        index: index + 1,
                    },
                )?;
            }
        }
        Ok(())
    }

    fn prepare_gamma(&mut self, op_id: OpId, parent: Point) -> Result<(), RecoveryError> {
        if self.prepared.gammas.contains_key(&op_id) {
            return Ok(());
        }
        let op = self.context.get_op(op_id);
        let gamma = gamma(&op)?;
        let binding = gamma.binding();
        let inputs = op.operands()[binding.operands].to_vec();
        let predicate = gamma.predicate();
        let outputs = op.results().to_vec();
        let mut arms = Vec::new();
        let arm_regions = gamma.arms();
        let last = arm_regions.len().saturating_sub(1);
        let semantic_control = (arm_regions.len() > 1)
            .then(|| self.recovery.control(op_id, Test::Arm(0)))
            .flatten()
            .map(|control| (control.source_predicate, control.predicate_type));
        self.prepared.structured.push(op_id);
        for (index, region) in arm_regions.into_iter().enumerate() {
            let domain = self
                .recovery
                .region_domain(region)
                .ok_or_else(|| decline(&op, "has an arm without a recovery demand domain"))?;
            let handle = self.context.get_region(region);
            let ports = handle.ports().iter().map(crate::Value::id).collect();
            let results = handle.results();
            let end = SequenceEnd::GammaArm {
                gamma: op_id,
                arm: index,
                parent,
            };
            let sequence = self.normal_region(region, domain, end)?;
            if let Some((source_predicate, predicate_type)) = semantic_control {
                let boolean = predicate_type == IntegerType::new(self.context, 1);
                let fact = if index < last || (boolean && last == 1) {
                    PreparedFact::Exact(index as u64)
                } else {
                    PreparedFact::DefaultFrom(index)
                };
                self.prepared
                    .entry_facts
                    .entry(sequence)
                    .or_default()
                    .push(EntryFact {
                        predicate: source_predicate,
                        fact,
                    });
            }
            arms.push(GammaArmPlan {
                sequence,
                ports,
                results,
            });
        }
        self.prepared.gammas.insert(
            op_id,
            GammaPlan {
                predicate,
                inputs,
                outputs,
                arms,
            },
        );
        Ok(())
    }

    fn prepare_theta(&mut self, op_id: OpId, parent: Point) -> Result<(), RecoveryError> {
        if self.prepared.thetas.contains_key(&op_id) {
            return Ok(());
        }
        let op = self.context.get_op(op_id);
        let theta = theta(&op)?;
        let binding = theta.binding();
        let body = theta.body();
        let handle = self.context.get_region(body);
        let results = handle.results();
        let ports: Vec<ValueId> = handle.ports().iter().map(crate::Value::id).collect();
        let inits = op.operands()[binding.operands].to_vec();
        let continue_values = results[binding.continue_].to_vec();
        let exit_values = results[binding.exit].to_vec();
        let predicate_value = theta.predicate();
        let mut tested = vec![predicate_value];
        tested.extend(self.control_reads(&op, Test::Repeat));
        let predicate = self.cone(body, &tested);
        let continue_cone = self.cone(body, &continue_values);
        let exit_cone = self.cone(body, &exit_values);
        let head_ops: HashSet<OpId> = predicate
            .iter()
            .copied()
            .chain(continue_cone.intersection(&exit_cone).copied())
            .collect();
        let continue_only: HashSet<OpId> = continue_cone.difference(&head_ops).copied().collect();
        let exit_only: HashSet<OpId> = exit_cone.difference(&head_ops).copied().collect();
        let order = self.order(body)?;
        let placed: HashSet<OpId> = head_ops
            .iter()
            .chain(&continue_only)
            .chain(&exit_only)
            .copied()
            .collect();
        self.require_effects(&order, &placed)?;
        let head_domain = self
            .recovery
            .theta_domain(op_id, DemandDomainKind::ThetaHead)
            .ok_or_else(|| decline(&op, "has no head demand domain"))?;
        let continue_domain = self
            .recovery
            .theta_domain(op_id, DemandDomainKind::ThetaContinue)
            .ok_or_else(|| decline(&op, "has no continue demand domain"))?;
        let exit_domain = self
            .recovery
            .theta_domain(op_id, DemandDomainKind::ThetaExit)
            .ok_or_else(|| decline(&op, "has no exit demand domain"))?;
        // Selection can remove a use and narrow an operation's computed
        // cone. Retain its original execution domain rather than moving it
        // across the lazy repeat decision.
        let selected_domain = |op: OpId| {
            self.edges
                .execution_domain(op)
                .filter(|domain| [head_domain, continue_domain, exit_domain].contains(domain))
        };
        let filter = |held: &HashSet<OpId>, domain| {
            order
                .iter()
                .copied()
                .filter(|&op| {
                    placed.contains(&op)
                        && selected_domain(op)
                            .map_or_else(|| held.contains(&op), |actual| actual == domain)
                })
                .collect::<Vec<_>>()
        };
        let head_ops = filter(&head_ops, head_domain);
        let continue_ops = filter(&continue_only, continue_domain);
        let exit_ops = filter(&exit_only, exit_domain);
        let head = self.reserve_sequence(
            body,
            head_domain,
            ports.clone(),
            head_ops,
            SequenceEnd::ThetaHead { theta: op_id },
        );
        let continue_ = self.reserve_sequence(
            body,
            continue_domain,
            Vec::new(),
            continue_ops,
            SequenceEnd::ThetaContinue { theta: op_id },
        );
        let exit = self.reserve_sequence(
            body,
            exit_domain,
            Vec::new(),
            exit_ops,
            SequenceEnd::ThetaExit {
                theta: op_id,
                parent,
            },
        );
        self.prepared.structured.push(op_id);
        self.prepared.thetas.insert(
            op_id,
            ThetaPlan {
                predicate: predicate_value,
                inits,
                ports,
                continue_values,
                exit_values,
                outputs: op.results().to_vec(),
                body,
                head,
                continue_,
                exit,
            },
        );
        self.prepare_structured(head)?;
        self.prepare_structured(continue_)?;
        self.prepare_structured(exit)?;
        Ok(())
    }

    fn propagate_entry_facts(&mut self) {
        loop {
            let mut changed = false;
            for (index, sequence) in self.prepared.sequences.iter().enumerate() {
                let parent = SequenceId(index);
                let inherited = self
                    .prepared
                    .entry_facts
                    .get(&parent)
                    .cloned()
                    .unwrap_or_default();
                if inherited.is_empty() {
                    continue;
                }
                let mut children = Vec::new();
                for op in &sequence.ops {
                    if let Some(gamma) = self.prepared.gammas.get(op) {
                        children.extend(gamma.arms.iter().map(|arm| arm.sequence));
                    }
                    if let Some(theta) = self.prepared.thetas.get(op) {
                        children.extend([theta.head, theta.continue_, theta.exit]);
                    }
                }
                for child in children {
                    let held = self.prepared.entry_facts.entry(child).or_default();
                    for fact in &inherited {
                        if !held.contains(fact) {
                            held.push(*fact);
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn make_fragments(&mut self) {
        for (sequence_index, sequence) in self.prepared.sequences.iter().enumerate() {
            let sequence_id = SequenceId(sequence_index);
            let mut start = 0;
            while start < sequence.ops.len() {
                if is_structured(&self.context.get_op(sequence.ops[start])) {
                    start += 1;
                    continue;
                }
                let mut end = start + 1;
                while end < sequence.ops.len()
                    && !is_structured(&self.context.get_op(sequence.ops[end]))
                {
                    end += 1;
                }
                let index = self.prepared.fragments.len();
                self.prepared.fragment_at.insert(
                    Point {
                        sequence: sequence_id,
                        index: start,
                    },
                    index,
                );
                self.prepared.fragments.push(Fragment {
                    sequence: sequence_id,
                    end,
                    ops: sequence.ops[start..end].to_vec(),
                });
                start = end;
            }
        }
    }
}
mod finish;
pub(crate) use finish::emit_recovery;

/// One execution domain exposed to instruction selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemandDomain {
    pub id: DemandDomainId,
    pub region: RegionId,
    pub kind: DemandDomainKind,
    pub owner: Option<OpId>,
}

/// One simultaneous substitution applied while FINISH crosses a boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ValueBinding {
    pub source: ValueId,
    pub current: ValueId,
}

/// A selector that instruction selection must preserve or implement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub id: ControlId,
    pub consumer: OpId,
    pub test: Test,
    pub source_predicate: ValueId,
    pub predicate_type: TypeId,
    pub source_producer: Option<OpId>,
    pub origin_scope: RegionId,
    pub consumer_scope: RegionId,
    pub domain: DemandDomainId,
    /// Whether the predicate producer satisfies the conservative PCF check.
    /// False requests must retain ordinary selector data until their local
    /// conversion at the consumer.
    pub direct: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BoundarySnapshot {
    Gamma {
        arms: Vec<RegionId>,
        binding: Binding,
        result_types: Vec<TypeId>,
    },
    Theta {
        body: RegionId,
        binding: Binding,
        result_types: Vec<TypeId>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ControlAlias {
    pub value: ValueId,
    pub source: ValueId,
    pub inverted: bool,
}

/// Verified semantic control and demand ownership captured before selection.
///
/// The plan deliberately does not retain handles. Selection may replace the
/// source computations, while structured consumers and their regions remain
/// the stable boundaries against which emission checks the plan.
#[derive(Clone, Debug)]
pub struct RecoveryPlan {
    root: RegionId,
    external_values: HashSet<ValueId>,
    controls: Vec<ControlRequest>,
    control_by_test: HashMap<(OpId, Test), usize>,
    domains: Vec<DemandDomain>,
    op_domains: HashMap<OpId, DemandDomainId>,
    region_domains: HashMap<RegionId, DemandDomainId>,
    theta_domains: HashMap<(OpId, DemandDomainKind), DemandDomainId>,
    constants: HashMap<ValueId, u64>,
    boundaries: HashMap<OpId, BoundarySnapshot>,
    control_values: HashSet<ValueId>,
    control_only_values: HashSet<ValueId>,
    pub(super) control_types: HashMap<ValueId, TypeId>,
    pub(super) control_aliases: Vec<ControlAlias>,
}

impl RecoveryPlan {
    /// Normalize the structured semantic region into private control requests
    /// and lazy-Theta execution domains. This does not mutate IR.
    pub fn analyze(context: &Context, root: RegionId) -> Result<Self, PassError> {
        if !context.has_region(root) {
            return Err(PassError::InvalidRuleSet(
                "cannot recover a missing region".into(),
            ));
        }
        if !context.get_region(root).is_nodes() {
            return Err(PassError::InvalidRuleSet(
                "RecoveryPlan::analyze requires an unordered semantic region; use recover_cfg for an ordered function body"
                    .into(),
            ));
        }
        let mut local_values = HashSet::new();
        let mut reads = Vec::new();
        for region in std::iter::once(root).chain(context.nested_regions(root)) {
            let handle = context.get_region(region);
            local_values.extend(handle.ports().iter().map(crate::Value::id));
            reads.extend(handle.results());
            for op in handle.op_ids() {
                local_values.extend(context.get_op(op).results());
                reads.extend(values_read(context, op));
            }
        }
        let external_values = reads
            .into_iter()
            .filter(|value| !local_values.contains(value))
            .collect();
        let (control_values, control_only_values, control_aliases) =
            discover_control_roles(context, root);
        let control_types = control_values
            .iter()
            .map(|&value| (value, context.get_value(value).ty()))
            .collect();
        let mut builder = PlanBuilder {
            context,
            plan: Self {
                root,
                external_values,
                controls: Vec::new(),
                control_by_test: HashMap::new(),
                domains: Vec::new(),
                op_domains: HashMap::new(),
                region_domains: HashMap::new(),
                theta_domains: HashMap::new(),
                constants: HashMap::new(),
                boundaries: HashMap::new(),
                control_values,
                control_only_values,
                control_types,
                control_aliases,
            },
        };
        let root_domain = builder.domain(root, DemandDomainKind::Region, None);
        builder.walk_region(root, root_domain)?;
        builder.verify()?;
        Ok(builder.plan)
    }

    pub fn requirements(&self) -> &[ControlRequest] {
        &self.controls
    }

    /// Permanently turn one direct control definition into a consumer-local
    /// data test. Returns false when the request does not exist or was already
    /// demoted, so a retry loop can prove that each iteration makes progress.
    pub fn demote(&mut self, id: ControlId) -> bool {
        let Some(index) = self.controls.iter().position(|control| control.id == id) else {
            return false;
        };
        if !self.controls[index].direct {
            return false;
        }
        let consumer = self.controls[index].consumer;
        let test = self.controls[index].test;
        let local = match test {
            Test::Arm(_) => self.op_domains.get(&consumer).copied(),
            Test::Repeat => self
                .theta_domains
                .get(&(consumer, DemandDomainKind::ThetaHead))
                .copied(),
        };
        let Some(domain) = local else {
            return false;
        };
        let control = &mut self.controls[index];
        control.direct = false;
        control.domain = domain;
        true
    }

    pub fn domains(&self) -> &[DemandDomain] {
        &self.domains
    }

    pub fn domain_of(&self, source_op: OpId) -> Option<DemandDomainId> {
        self.op_domains.get(&source_op).copied()
    }

    /// Whether every source use of `value` is a control consumer, transparent
    /// inversion, or structured forwarding lane.
    pub fn control_only(&self, value: ValueId) -> bool {
        self.control_only_values.contains(&value)
    }

    /// The source value whose boolean fact `value` inverts.
    pub fn inverse_of(&self, value: ValueId) -> Option<ValueId> {
        self.control_aliases
            .iter()
            .find(|alias| alias.value == value && alias.inverted)
            .map(|alias| alias.source)
    }

    pub(crate) fn region_domain(&self, region: RegionId) -> Option<DemandDomainId> {
        self.region_domains.get(&region).copied()
    }

    pub(crate) fn theta_domain(
        &self,
        owner: OpId,
        kind: DemandDomainKind,
    ) -> Option<DemandDomainId> {
        self.theta_domains.get(&(owner, kind)).copied()
    }

    pub(crate) fn control(&self, consumer: OpId, test: Test) -> Option<&ControlRequest> {
        self.control_by_test
            .get(&(consumer, test))
            .map(|&index| &self.controls[index])
    }

    pub(crate) fn check_live(
        &self,
        context: &Context,
        region: RegionId,
        edges: &dyn Edges,
    ) -> Result<(), PassError> {
        if region != self.root {
            return Err(PassError::InvalidRuleSet(format!(
                "recovery plan belongs to region {}, not {}",
                self.root.number(),
                region.number()
            )));
        }
        for control in &self.controls {
            if !context.has_operation(control.consumer) {
                return Err(PassError::InvalidRuleSet(format!(
                    "recovery control {:?} lost structured consumer {}",
                    control.id,
                    control.consumer.number()
                )));
            }
            let op = context.get_op(control.consumer);
            let current = match control.test {
                Test::Repeat => theta(&op)?.predicate(),
                Test::Arm(_) => gamma(&op)?.predicate(),
            };
            let mapped = edges.value(control.source_predicate);
            if current != mapped || !context.has_value(mapped) {
                return Err(decline(&op, "lost its recovery predicate"));
            }
        }
        for (&op_id, expected) in &self.boundaries {
            if !context.has_operation(op_id) {
                return Err(PassError::InvalidRuleSet(format!(
                    "recovery plan lost structured boundary {}",
                    op_id.number()
                )));
            }
            let op = context.get_op(op_id);
            let mut result_types = op
                .results()
                .iter()
                .map(|&result| context.get_value(result).ty())
                .collect::<Vec<_>>();
            let original_types = match expected {
                BoundarySnapshot::Gamma { result_types, .. }
                | BoundarySnapshot::Theta { result_types, .. } => result_types,
            };
            if original_types.len() == result_types.len()
                && original_types
                    .iter()
                    .zip(&result_types)
                    .all(|(&source, &selected)| edges.compatible_type(source, selected))
            {
                result_types.clone_from(original_types);
            }
            let current = if let Some(gamma) = op.clone().as_interface::<dyn Gamma>() {
                BoundarySnapshot::Gamma {
                    arms: gamma.arms(),
                    binding: gamma.binding(),
                    result_types,
                }
            } else if let Some(theta) = op.clone().as_interface::<dyn Theta>() {
                BoundarySnapshot::Theta {
                    body: theta.body(),
                    binding: theta.binding(),
                    result_types,
                }
            } else {
                return Err(decline(&op, "lost its structured recovery interface"));
            };
            if &current != expected {
                return Err(decline(
                    &op,
                    &format!(
                        "changed after recovery analysis: expected {expected:?}, observed {current:?}"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Recover generic CFG IR, demoting each typed placement failure before
    /// retrying PREPARE. Placement errors occur before mutation and every retry
    /// must newly demote at least one request, so this loop is finite.
    pub fn emit(
        mut self,
        context: &Context,
        region: RegionId,
        edges: &dyn Edges,
    ) -> Result<(), PassError> {
        loop {
            match self.emit_selected(context, region, edges) {
                Ok(()) => return Ok(()),
                Err(RecoveryError::Invalid(error)) => return Err(error),
                Err(RecoveryError::Placement(controls)) => {
                    let mut changed = false;
                    for control in &controls {
                        changed |= self.demote(*control);
                    }
                    if !changed {
                        return Err(RecoveryError::Placement(controls).into());
                    }
                }
            }
        }
    }

    /// Recover one already-selected candidate without retrying. Machine
    /// selection must discard its staged context, demote the returned controls,
    /// and reselect before calling this again.
    pub(crate) fn emit_selected(
        &self,
        context: &Context,
        region: RegionId,
        edges: &dyn Edges,
    ) -> Result<(), RecoveryError> {
        self.check_live(context, region, edges)?;
        super::recover_with_plan(context, region, edges, self)
    }
}

/// Find selector values through structural forwarding and the inversion used
/// by CFG restructuring. These are roles in the private recovery graph. An
/// `i1` can still have an ordinary data use, which `direct_control` checks
/// separately before granting predicate-definition ownership.
fn discover_control_roles(
    context: &Context,
    root: RegionId,
) -> (HashSet<ValueId>, HashSet<ValueId>, Vec<ControlAlias>) {
    let mut seeds = Vec::new();
    let mut predecessors: HashMap<ValueId, Vec<ValueId>> = HashMap::new();
    let mut aliases = Vec::new();
    for region in context.nested_regions(root) {
        for op_id in context.get_region(region).op_ids() {
            let op = context.get_op(op_id);
            if let Some(gamma) = op.clone().as_interface::<dyn Gamma>() {
                seeds.push(gamma.predicate());
                let binding = gamma.binding();
                let inputs = &op.operands()[binding.operands];
                for arm in gamma.arms() {
                    let handle = context.get_region(arm);
                    let ports: Vec<ValueId> = handle.ports().iter().map(crate::Value::id).collect();
                    for (&port, &input) in ports[binding.ports.clone()].iter().zip(inputs) {
                        predecessors.entry(port).or_default().push(input);
                    }
                    for (&output, &result) in op.results().iter().zip(handle.results().iter()) {
                        predecessors.entry(output).or_default().push(result);
                    }
                }
            } else if let Some(theta) = op.clone().as_interface::<dyn Theta>() {
                seeds.push(theta.predicate());
                let binding = theta.binding();
                let handle = context.get_region(theta.body());
                let ports: Vec<ValueId> = handle.ports().iter().map(crate::Value::id).collect();
                let results = handle.results();
                let inits = &op.operands()[binding.operands];
                let carried_ports = &ports[binding.ports];
                let continue_values = &results[binding.continue_];
                for ((&port, &init), &continue_value) in
                    carried_ports.iter().zip(inits).zip(continue_values)
                {
                    predecessors.entry(port).or_default().push(init);
                    predecessors.entry(port).or_default().push(continue_value);
                }
                for (&output, &exit) in op.results().iter().zip(results[binding.exit].iter()) {
                    predecessors.entry(output).or_default().push(exit);
                }
            }
            if let Some(&result) = op.results().first()
                && let Some(input) = super::unnegate(context, result)
            {
                aliases.push(ControlAlias {
                    value: result,
                    source: input,
                    inverted: true,
                });
                predecessors.entry(result).or_default().push(input);
            }
        }
    }
    let mut roles = HashSet::new();
    let mut pending = seeds;
    while let Some(value) = pending.pop() {
        if !roles.insert(value) {
            continue;
        }
        pending.extend(predecessors.get(&value).into_iter().flatten().copied());
    }
    aliases.retain(|alias| roles.contains(&alias.value));
    let control_only = roles
        .iter()
        .copied()
        .filter(|&value| control_value_has_no_data_use(context, root, value, &roles, &aliases))
        .collect();
    (roles, control_only, aliases)
}

fn control_value_has_no_data_use(
    context: &Context,
    root: RegionId,
    value: ValueId,
    roles: &HashSet<ValueId>,
    aliases: &[ControlAlias],
) -> bool {
    for r#use in context.uses_of(value) {
        let user = context.get_op(r#use.op);
        if aliases.iter().any(|alias| {
            alias.source == value && context.get_value(alias.value).defining_op() == Some(r#use.op)
        }) {
            continue;
        }
        if let Some(gamma) = user.clone().as_interface::<dyn Gamma>() {
            let binding = gamma.binding();
            if gamma.predicate() == value && !binding.operands.contains(&r#use.index) {
                continue;
            }
            if binding.operands.contains(&r#use.index) {
                let lane = r#use.index - binding.operands.start;
                let forwarded_control = gamma.arms().into_iter().all(|arm| {
                    let ports = context.get_region(arm).ports();
                    ports
                        .get(binding.ports.start + lane)
                        .is_some_and(|port| roles.contains(&port.id()))
                });
                if forwarded_control {
                    continue;
                }
            }
        }
        if let Some(theta) = user.clone().as_interface::<dyn Theta>() {
            let binding = theta.binding();
            if binding.operands.contains(&r#use.index) {
                let lane = r#use.index - binding.operands.start;
                let ports = context.get_region(theta.body()).ports();
                if ports
                    .get(binding.ports.start + lane)
                    .is_some_and(|port| roles.contains(&port.id()))
                {
                    continue;
                }
            }
        }
        return false;
    }

    for region in context.nested_regions(root) {
        let handle = context.get_region(region);
        for (index, result) in handle.results().iter().copied().enumerate() {
            if result != value {
                continue;
            }
            let Some(owner) = handle.parent_op() else {
                return false;
            };
            let owner = context.get_op(owner);
            if let Some(gamma) = owner.clone().as_interface::<dyn Gamma>()
                && gamma.arms().contains(&region)
                && owner
                    .results()
                    .get(index)
                    .is_some_and(|value| roles.contains(value))
            {
                continue;
            }
            if let Some(theta) = owner.clone().as_interface::<dyn Theta>() {
                let binding = theta.binding();
                if theta.body() == region {
                    if index < binding.continue_.start && theta.predicate() == value {
                        continue;
                    }
                    if binding.continue_.contains(&index) {
                        let lane = index - binding.continue_.start;
                        let ports = handle.ports();
                        if ports
                            .get(binding.ports.start + lane)
                            .is_some_and(|port| roles.contains(&port.id()))
                        {
                            continue;
                        }
                    }
                    if binding.exit.contains(&index) {
                        let lane = index - binding.exit.start;
                        if owner
                            .results()
                            .get(binding.results.start + lane)
                            .is_some_and(|value| roles.contains(value))
                        {
                            continue;
                        }
                    }
                }
            }
            return false;
        }
    }
    true
}

struct PlanBuilder<'a> {
    context: &'a Context,
    plan: RecoveryPlan,
}

impl PlanBuilder<'_> {
    fn verify(&self) -> Result<(), PassError> {
        for (&op, &domain) in &self.plan.op_domains {
            let descriptor = self
                .plan
                .domains
                .get(domain.0 as usize)
                .filter(|descriptor| descriptor.id == domain)
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!(
                        "operation {} names an unknown recovery demand domain",
                        op.number()
                    ))
                })?;
            if self.context.region_of_op(op) != Some(descriptor.region) {
                return Err(PassError::InvalidRuleSet(format!(
                    "operation {} is outside recovery demand domain {:?}",
                    op.number(),
                    descriptor.id
                )));
            }
        }
        let mut direct_owner: HashMap<OpId, OpId> = HashMap::new();
        for control in &self.plan.controls {
            let op = self.context.get_op(control.consumer);
            let predicate = match control.test {
                Test::Arm(_) => gamma(&op)?.predicate(),
                Test::Repeat => theta(&op)?.predicate(),
            };
            if predicate != control.source_predicate {
                return Err(decline(&op, "changed predicate during recovery analysis"));
            }
            if self.context.get_value(predicate).ty() != control.predicate_type {
                return Err(decline(&op, "has an inconsistent predicate type"));
            }
            if !control.direct {
                continue;
            }
            let producer = control
                .source_producer
                .ok_or_else(|| decline(&op, "marks an argument control as a direct definition"))?;
            if self.plan.domain_of(producer) != Some(control.domain) {
                return Err(decline(
                    &op,
                    "separates a direct control from its demand domain",
                ));
            }
            if !self.direct_control(
                producer,
                predicate,
                control.consumer,
                control.consumer_scope,
                control.test,
            ) {
                return Err(decline(&op, "does not satisfy predicate continuation form"));
            }
            if let Some(previous) = direct_owner.insert(producer, control.consumer)
                && previous != control.consumer
            {
                return Err(decline(
                    &op,
                    "shares one direct predicate definition across consumers",
                ));
            }
        }
        Ok(())
    }

    fn domain(
        &mut self,
        region: RegionId,
        kind: DemandDomainKind,
        owner: Option<OpId>,
    ) -> DemandDomainId {
        let id = DemandDomainId(self.plan.domains.len() as u32);
        self.plan.domains.push(DemandDomain {
            id,
            region,
            kind,
            owner,
        });
        if kind == DemandDomainKind::Region {
            self.plan.region_domains.insert(region, id);
        }
        if let Some(owner) = owner {
            self.plan.theta_domains.insert((owner, kind), id);
        }
        id
    }

    fn walk_region(&mut self, region: RegionId, fallback: DemandDomainId) -> Result<(), PassError> {
        let ops = self.context.get_region(region).op_ids();
        for &op in &ops {
            self.plan.op_domains.entry(op).or_insert(fallback);
            self.record_constants(op);
        }
        for op_id in ops {
            let op = self.context.get_op(op_id);
            if let Some(gate) = op.clone().as_interface::<dyn Gamma>() {
                self.plan.boundaries.insert(
                    op_id,
                    BoundarySnapshot::Gamma {
                        arms: gate.arms(),
                        binding: gate.binding(),
                        result_types: op
                            .results()
                            .iter()
                            .map(|&result| self.context.get_value(result).ty())
                            .collect(),
                    },
                );
                self.add_gamma_controls(region, &op, gate.as_ref())?;
                for arm in gate.arms() {
                    let domain = self.domain(arm, DemandDomainKind::Region, None);
                    self.walk_region(arm, domain)?;
                }
                continue;
            }
            let Some(loop_) = op.clone().as_interface::<dyn Theta>() else {
                continue;
            };
            let body = loop_.body();
            self.plan.boundaries.insert(
                op_id,
                BoundarySnapshot::Theta {
                    body,
                    binding: loop_.binding(),
                    result_types: op
                        .results()
                        .iter()
                        .map(|&result| self.context.get_value(result).ty())
                        .collect(),
                },
            );
            let head = self.domain(body, DemandDomainKind::ThetaHead, Some(op_id));
            let continue_ = self.domain(body, DemandDomainKind::ThetaContinue, Some(op_id));
            let exit = self.domain(body, DemandDomainKind::ThetaExit, Some(op_id));
            self.assign_theta_domains(loop_.as_ref(), head, continue_, exit);
            self.add_control(body, &op, Test::Repeat, loop_.predicate())?;
            self.walk_region(body, head)?;
        }
        Ok(())
    }

    fn record_constants(&mut self, op: OpId) {
        let instance = self.context.get_op(op);
        let Some(constant) = instance.clone().as_interface::<dyn ConstantLike>() else {
            return;
        };
        if let Some(&result) = instance.results().first() {
            self.plan
                .constants
                .insert(result, constant.constant_value().to_u64());
        }
    }

    fn add_gamma_controls(
        &mut self,
        region: RegionId,
        op: &crate::OpHandle,
        gate: &dyn Gamma,
    ) -> Result<(), PassError> {
        if gate.arms().is_empty() {
            return Err(decline(op, "has no arm"));
        }
        for index in 0..gate.arms().len().saturating_sub(1) {
            self.add_control(region, op, Test::Arm(index), gate.predicate())?;
        }
        Ok(())
    }

    fn add_control(
        &mut self,
        consumer_scope: RegionId,
        consumer: &crate::OpHandle,
        test: Test,
        predicate: ValueId,
    ) -> Result<(), PassError> {
        let producer = self.context.get_value(predicate).defining_op();
        let origin_scope = producer
            .and_then(|op| self.context.region_of_op(op))
            .unwrap_or(consumer_scope);
        let direct = producer.is_some_and(|producer| {
            self.direct_control(producer, predicate, consumer.id, consumer_scope, test)
        });
        let local_domain = match test {
            Test::Arm(_) => self.plan.op_domains.get(&consumer.id).copied(),
            Test::Repeat => self
                .plan
                .theta_domains
                .get(&(consumer.id, DemandDomainKind::ThetaHead))
                .copied(),
        };
        let domain = if direct {
            producer.and_then(|op| self.plan.op_domains.get(&op).copied())
        } else {
            local_domain
        }
        .ok_or_else(|| decline(consumer, "has no recovery demand domain"))?;
        let request = ControlRequest {
            id: ControlId(self.plan.controls.len() as u32),
            consumer: consumer.id,
            test,
            source_predicate: predicate,
            predicate_type: self.context.get_value(predicate).ty(),
            source_producer: producer,
            origin_scope,
            consumer_scope,
            domain,
            direct,
        };
        self.plan
            .control_by_test
            .insert((consumer.id, test), self.plan.controls.len());
        self.plan.controls.push(request);
        Ok(())
    }

    fn direct_control(
        &self,
        producer: OpId,
        predicate: ValueId,
        consumer: OpId,
        consumer_scope: RegionId,
        test: Test,
    ) -> bool {
        if self.context.region_of_op(producer) != Some(consumer_scope) {
            return false;
        }
        if !self.plan.control_only(predicate) {
            return false;
        }
        let op = self.context.get_op(producer);
        match test {
            Test::Arm(_) => {
                let boundary = self.context.get_region(consumer_scope).results();
                self.context
                    .get_op(consumer)
                    .operands()
                    .iter()
                    .filter(|&&operand| operand == predicate)
                    .count()
                    == 1
                    && op.results().iter().all(|&result| {
                        let uses = self.context.users_of(result);
                        !uses.is_empty()
                            && uses.iter().all(|&user| {
                                user == consumer || self.is_control_alias_use(result, user)
                            })
                            && (!boundary.contains(&result)
                                || self.plan.control_values.contains(&result))
                    })
            }
            Test::Repeat => {
                let boundary = self.context.get_region(consumer_scope).results();
                op.results().iter().all(|&result| {
                    self.context.use_count(result) == 0 && boundary.contains(&result)
                }) && boundary.iter().filter(|&&value| value == predicate).count() == 1
            }
        }
    }

    fn is_control_alias_use(&self, input: ValueId, user: OpId) -> bool {
        self.plan.control_aliases.iter().any(|alias| {
            alias.source == input
                && self.context.get_value(alias.value).defining_op() == Some(user)
                && self.plan.control_only(alias.value)
        })
    }

    fn assign_theta_domains(
        &mut self,
        loop_: &dyn Theta,
        head: DemandDomainId,
        continue_: DemandDomainId,
        exit: DemandDomainId,
    ) {
        let body = loop_.body();
        let binding = loop_.binding();
        let results = self.context.get_region(body).results();
        let predicate = self.cone(body, &[loop_.predicate()]);
        let continue_cone = self.cone(body, &results[binding.continue_]);
        let exit_cone = self.cone(body, &results[binding.exit]);
        let shared: HashSet<OpId> = predicate
            .into_iter()
            .chain(continue_cone.intersection(&exit_cone).copied())
            .collect();
        for op in self.context.get_region(body).op_ids() {
            let domain = if shared.contains(&op) {
                head
            } else if continue_cone.contains(&op) {
                continue_
            } else if exit_cone.contains(&op) {
                exit
            } else {
                head
            };
            self.plan.op_domains.insert(op, domain);
        }
    }

    fn cone(&self, region: RegionId, roots: &[ValueId]) -> HashSet<OpId> {
        let mut cone = HashSet::new();
        let mut pending: Vec<OpId> = roots
            .iter()
            .filter_map(|&value| self.context.get_value(value).defining_op())
            .collect();
        while let Some(op) = pending.pop() {
            if self.context.parent_nodes_region(op) != Some(region) || !cone.insert(op) {
                continue;
            }
            pending.extend(
                values_read(self.context, op)
                    .into_iter()
                    .filter_map(|value| self.context.get_value(value).defining_op()),
            );
        }
        cone
    }
}
