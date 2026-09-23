use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use super::{
    BlockId, Context, ControlDefinition, ControlId, ControlOutcome, ControlPortId, Edge, Edges,
    GammaPlan, OpId, OperationRef, PassError, Point, Prepared, RecoveryError, RecoveryPlan,
    RegionId, SequenceEnd, Test, ThetaPlan, ValueBinding, ValueId, values_read, values_then_states,
};

/// Bindings name values at the originating fragment, not at an intermediate
/// region boundary. Each boundary composes one parallel assignment into them.
#[derive(Clone, Debug)]
struct Route {
    point: Point,
    bindings: Vec<ValueBinding>,
    facts: HashMap<ValueId, ControlOutcome>,
    control_facts: HashMap<ControlPortId, ControlOutcome>,
    skip_control: Option<ControlId>,
    /// Set only while routing the current transfer across a Theta repeat.
    loops_back: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Node(usize);

#[derive(Clone, Debug)]
struct Transfer {
    target: Node,
    bindings: Vec<ValueBinding>,
    loops_back: bool,
}

#[derive(Clone, Debug)]
enum Term {
    Pending,
    Jump(Transfer),
    Branch {
        control: ControlId,
        outcome: usize,
        predicate: ValueId,
        bindings: Vec<ValueBinding>,
        taken: Transfer,
        other: Transfer,
    },
    Leave {
        values: Vec<ValueId>,
        states: Vec<ValueId>,
    },
}

impl Term {
    fn successors(&self) -> Vec<&Transfer> {
        match self {
            Self::Jump(edge) => vec![edge],
            Self::Branch { taken, other, .. } => vec![taken, other],
            Self::Pending | Self::Leave { .. } => Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct Fragment {
    ops: Vec<OpId>,
    term: Term,
}

enum Destination {
    Computation(usize),
    Producer(ControlId),
    Gamma(OpId),
    Repeat(OpId),
    Return(Vec<ValueId>, Vec<ValueId>),
    Cycle(
        Vec<(ValueId, ControlOutcome)>,
        Vec<(ControlPortId, ControlOutcome)>,
    ),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CycleKey {
    point: Point,
    values: Vec<(ValueId, ControlOutcome)>,
    controls: Vec<(ControlPortId, ControlOutcome)>,
}

/// FINISH produces a graph of transfers before creating IR blocks. In
/// particular, a fragment's parameters cannot depend on which edge found it
/// first. SSA construction sees every predecessor and every binding.
struct Finish<'a> {
    context: &'a Context,
    edges: &'a dyn Edges,
    recovery: &'a RecoveryPlan,
    prepared: Prepared,
    fragments: Vec<Fragment>,
    computations: HashMap<usize, Node>,
    controls: HashMap<OpId, Node>,
    definitions: HashMap<ControlId, Node>,
    cycles: HashMap<CycleKey, Node>,
    constants: HashMap<ValueId, ControlOutcome>,
    ambiguous_facts: HashSet<ValueId>,
    literals: HashSet<OpId>,
    theta_regions: HashMap<RegionId, HashSet<RegionId>>,
    lost_direct: HashSet<ControlId>,
}

pub(crate) fn emit_recovery(
    context: &Context,
    region: RegionId,
    edges: &dyn Edges,
    recovery: &RecoveryPlan,
) -> Result<(), RecoveryError> {
    let mut prepared = Prepared::analyze(context, region, edges, recovery)?;
    let mut constants = HashMap::new();
    let mut ambiguous_facts = HashSet::new();
    let mut projected_types = HashMap::new();
    for (&value, &ty) in &recovery.control_types {
        let mapped = edges.value(value);
        if projected_types
            .insert(mapped, ty)
            .is_some_and(|old| old != ty)
        {
            ambiguous_facts.insert(mapped);
        }
    }
    for (&value, &bits) in &recovery.constants {
        let value = edges.value(value);
        if constants
            .insert(value, ControlOutcome::Exact(bits))
            .is_some_and(|old| old != ControlOutcome::Exact(bits))
        {
            ambiguous_facts.insert(value);
        }
    }
    constants.retain(|value, _| !ambiguous_facts.contains(value));
    // A scoped e-graph can replace an arm's literal with the predicate proved
    // equal to it only in that arm. Such a remap is not a global constant fact.
    let unprojected: HashSet<_> = recovery
        .constants
        .keys()
        .copied()
        .filter(|&value| edges.value(value) == value)
        .collect();
    let proven: HashSet<_> = constants
        .keys()
        .copied()
        .filter(|&value| {
            unprojected.contains(&value)
                || context.get_value(value).defining_op().is_some_and(|op| {
                    context.has_operation(op) && movable_literal(context, edges, &constants, op)
                })
        })
        .collect();
    constants.retain(|value, _| proven.contains(value));

    let mut hoisted = Vec::new();
    for fragment in &mut prepared.fragments {
        fragment.ops.retain(|&op| {
            if movable_literal(context, edges, &constants, op) {
                hoisted.push(op);
                false
            } else {
                true
            }
        });
    }
    let theta_regions = prepared
        .thetas
        .values()
        .map(|theta| {
            (
                theta.body,
                context.nested_regions(theta.body).into_iter().collect(),
            )
        })
        .collect();
    let mut finish = Finish {
        context,
        edges,
        recovery,
        prepared,
        fragments: Vec::new(),
        computations: HashMap::new(),
        controls: HashMap::new(),
        definitions: HashMap::new(),
        cycles: HashMap::new(),
        constants,
        ambiguous_facts,
        literals: hoisted.iter().copied().collect(),
        theta_regions,
        lost_direct: HashSet::new(),
    };
    let entry = finish.new_fragment(hoisted);
    let point = Point {
        sequence: finish.prepared.root,
        index: 0,
    };
    finish.connect(entry, finish.canonical(point))?;
    if !finish.lost_direct.is_empty() {
        let mut controls: Vec<_> = finish.lost_direct.iter().copied().collect();
        controls.sort();
        return Err(RecoveryError::Placement(controls));
    }
    finish.emit(region, entry).map_err(RecoveryError::Invalid)
}

fn movable_literal(
    context: &Context,
    edges: &dyn Edges,
    constants: &HashMap<ValueId, ControlOutcome>,
    op: OpId,
) -> bool {
    let op = context.get_op(op);
    if op.results().is_empty()
        || !op.operands().is_empty()
        || !op.state_results().is_empty()
        || !op.regions().is_empty()
        || !edges.implicit_inputs(op.id).is_empty()
        || !op
            .results()
            .iter()
            .all(|value| constants.contains_key(value))
    {
        return false;
    }
    if let Some(machine) = op
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>()
    {
        !machine.info().effects.reads
            && !machine.info().effects.writes
            && matches!(
                machine.info().control_flow,
                crate::backend::ControlFlow::None
            )
            && crate::analysis::execution_regs(&op).phys_defs.is_empty()
            // Selected literals may read a hardwired zero register, as in
            // RISC-V addi rd, x0, imm. Their selection proves that read invariant.
            && (edges.is_literal(op.id)
                || crate::analysis::execution_regs(&op).phys_uses.is_empty())
    } else {
        op.has_interface::<dyn crate::Pure>() || op.has_interface::<dyn crate::ConstantLike>()
    }
}

impl Finish<'_> {
    fn value(&self, value: ValueId) -> ValueId {
        self.edges.value(value)
    }

    fn canonical(&self, point: Point) -> Route {
        let mut facts = HashMap::new();
        for entry in self
            .prepared
            .entry_facts
            .get(&point.sequence)
            .into_iter()
            .flatten()
        {
            let predicate = self.value(entry.predicate);
            if !self.ambiguous_facts.contains(&predicate)
                && !self.constants.contains_key(&predicate)
            {
                facts.insert(predicate, entry.fact);
            }
        }
        Route {
            point,
            bindings: Vec::new(),
            facts,
            control_facts: HashMap::new(),
            skip_control: None,
            // A new fragment starts a new transfer, regardless of its entries.
            loops_back: false,
        }
    }

    fn new_fragment(&mut self, ops: Vec<OpId>) -> Node {
        let node = Node(self.fragments.len());
        self.fragments.push(Fragment {
            ops,
            term: Term::Pending,
        });
        node
    }

    fn resolved(bindings: &[ValueBinding], value: ValueId) -> ValueId {
        bindings
            .iter()
            .find(|binding| binding.source == value)
            .map_or(value, |binding| binding.current)
    }

    fn read(&self, route: &Route, value: ValueId) -> ValueId {
        Self::resolved(&route.bindings, self.value(value))
    }

    fn fact(&self, route: &Route, value: ValueId) -> Option<ControlOutcome> {
        self.fact_with(route, value, &mut HashSet::new())
    }

    fn fact_with(
        &self,
        route: &Route,
        value: ValueId,
        seen: &mut HashSet<ValueId>,
    ) -> Option<ControlOutcome> {
        let value = self.value(value);
        if self.ambiguous_facts.contains(&value) || !seen.insert(value) {
            return None;
        }
        if let Some(&fact) = route.facts.get(&value) {
            return Some(fact);
        }
        // The right side of a parallel binding names an incoming value. A
        // later fact about the overwritten name cannot describe that value.
        if route.bindings.iter().any(|binding| binding.source == value) {
            return None;
        }
        if let Some(&fact) = self.constants.get(&value) {
            return Some(fact);
        }
        for alias in &self.recovery.control_aliases {
            let result = self.value(alias.value);
            let source = self.value(alias.source);
            let other = if result == value {
                source
            } else if source == value {
                result
            } else {
                continue;
            };
            let Some(fact) = self.fact_with(route, other, seen) else {
                continue;
            };
            if !alias.inverted {
                return Some(fact);
            }
            match fact {
                ControlOutcome::Exact(0) => return Some(ControlOutcome::Exact(1)),
                ControlOutcome::Exact(1) | ControlOutcome::DefaultFrom(1) => {
                    return Some(ControlOutcome::Exact(0));
                }
                _ => {}
            }
        }
        None
    }

    fn bind(
        &self,
        route: &mut Route,
        destinations: &[ValueId],
        sources: &[ValueId],
    ) -> Result<(), PassError> {
        if destinations.len() != sources.len() {
            return Err(PassError::InvalidRuleSet(
                "recovery boundary has mismatched binding arity".into(),
            ));
        }
        let incoming: Vec<_> = destinations
            .iter()
            .zip(sources)
            .map(|(&to, &from)| {
                (
                    self.value(to),
                    self.read(route, from),
                    self.fact(route, from),
                )
            })
            .collect();
        self.assign(route, incoming);
        Ok(())
    }

    fn assign(&self, route: &mut Route, incoming: Vec<(ValueId, ValueId, Option<ControlOutcome>)>) {
        for (to, from, fact) in incoming {
            route.bindings.retain(|binding| binding.source != to);
            if to != from {
                route.bindings.push(ValueBinding {
                    source: to,
                    current: from,
                });
            }
            route.facts.remove(&to);
            if let Some(fact) = fact.filter(|fact| self.constants.get(&to) != Some(fact)) {
                route.facts.insert(to, fact);
            }
        }
    }

    fn gamma_outcome(&self, route: &Route, gamma: &GammaPlan) -> Option<usize> {
        let last = gamma.arms.len().checked_sub(1)?;
        match self.fact(route, gamma.predicate)? {
            ControlOutcome::Exact(value) => {
                Some(usize::try_from(value).unwrap_or(usize::MAX).min(last))
            }
            ControlOutcome::DefaultFrom(first) if first >= last => Some(last),
            ControlOutcome::DefaultFrom(_) => None,
        }
    }

    fn repeat_outcome(&self, route: &Route, theta_id: OpId, theta: &ThetaPlan) -> Option<bool> {
        if let Some(repeated) = self.recovery.repeated_control(theta_id) {
            return match route.control_facts.get(&repeated.port)? {
                ControlOutcome::Exact(value) => Some(*value != 0),
                ControlOutcome::DefaultFrom(first) if *first > 0 => Some(true),
                ControlOutcome::DefaultFrom(_) => None,
            };
        }
        match self.fact(route, theta.predicate)? {
            ControlOutcome::Exact(value) => Some(value != 0),
            ControlOutcome::DefaultFrom(first) if first > 0 => Some(true),
            ControlOutcome::DefaultFrom(_) => None,
        }
    }

    fn select_arm(&self, mut route: Route, op: OpId, index: usize) -> Result<Route, PassError> {
        let gamma = &self.prepared.gammas[&op];
        let arm = &gamma.arms[index];
        let fact = if index + 1 == gamma.arms.len() {
            ControlOutcome::DefaultFrom(index)
        } else {
            ControlOutcome::Exact(index as u64)
        };
        let predicate = self.value(gamma.predicate);
        if !self.ambiguous_facts.contains(&predicate) && !self.constants.contains_key(&predicate) {
            route
                .facts
                .entry(predicate)
                .and_modify(|old| {
                    if !matches!(old, ControlOutcome::Exact(_)) {
                        *old = fact;
                    }
                })
                .or_insert(fact);
        }
        self.bind(&mut route, &arm.ports, &gamma.inputs)?;
        route.point = Point {
            sequence: arm.sequence,
            index: 0,
        };
        Ok(route)
    }

    fn forget_iteration(&self, route: &mut Route, theta: &ThetaPlan) {
        route.control_facts.retain(|port, _| {
            !self
                .recovery
                .repeated_controls
                .values()
                .any(|repeated| repeated.port == *port && repeated.scope == theta.body)
        });
        let regions = &self.theta_regions[&theta.body];
        route.facts.retain(|value, _| {
            let owner = self.context.region_of_port(*value).or_else(|| {
                self.context.get_value(*value).defining_op().and_then(|op| {
                    self.context.region_of_op(op).or_else(|| {
                        self.recovery.domain_of(op).and_then(|domain| {
                            self.recovery
                                .domains()
                                .get(domain.0 as usize)
                                .map(|domain| domain.region)
                        })
                    })
                })
            });
            owner.is_some_and(|region| region != theta.body && !regions.contains(&region))
        });
    }

    fn repeat(&self, route: &mut Route, theta: &ThetaPlan) -> Result<(), PassError> {
        // Capture the old iteration's values and selector facts before
        // invalidating predicates that the next iteration recomputes.
        let carried: Vec<_> = theta
            .continue_values
            .iter()
            .map(|&value| (self.read(route, value), self.fact(route, value)))
            .collect();
        self.forget_iteration(route, theta);
        let incoming = theta
            .ports
            .iter()
            .zip(carried)
            .map(|(&port, (value, fact))| (self.value(port), value, fact))
            .collect();
        self.assign(route, incoming);
        route.point = Point {
            sequence: theta.head,
            index: 0,
        };
        route.loops_back = true;
        Ok(())
    }

    fn dynamic_facts(&self, route: &Route) -> Vec<(ValueId, ControlOutcome)> {
        let mut facts: Vec<_> = route
            .facts
            .iter()
            .map(|(&value, &fact)| (value, fact))
            .collect();
        facts.sort();
        facts
    }

    fn dynamic_control_facts(&self, route: &Route) -> Vec<(ControlPortId, ControlOutcome)> {
        let mut facts: Vec<_> = route
            .control_facts
            .iter()
            .map(|(&port, &fact)| (port, fact))
            .collect();
        facts.sort();
        facts
    }

    fn advance(&self, route: &mut Route) -> Result<Destination, PassError> {
        let mut seen = HashSet::new();
        loop {
            if let Some(&control) = self.prepared.control_at.get(&route.point) {
                if route.skip_control == Some(control) {
                    route.skip_control = None;
                } else {
                    return Ok(Destination::Producer(control));
                }
            }
            let facts = self.dynamic_facts(route);
            let control_facts = self.dynamic_control_facts(route);
            if !seen.insert((route.point, facts.clone(), control_facts.clone())) {
                return Ok(Destination::Cycle(facts, control_facts));
            }
            if let Some(&fragment) = self.prepared.fragment_at.get(&route.point) {
                if self.prepared.fragments[fragment].ops.is_empty() {
                    route.point.index = self.prepared.fragments[fragment].end;
                    continue;
                }
                return Ok(Destination::Computation(fragment));
            }
            let sequence = &self.prepared.sequences[route.point.sequence.0];
            if let Some(&op) = sequence.ops.get(route.point.index) {
                if let Some(gamma) = self.prepared.gammas.get(&op) {
                    if let Some(index) = self.gamma_outcome(route, gamma) {
                        *route = self.select_arm(route.clone(), op, index)?;
                        continue;
                    }
                    return Ok(Destination::Gamma(op));
                }
                if let Some(theta) = self.prepared.thetas.get(&op) {
                    self.bind(route, &theta.ports, &theta.inits)?;
                    route.point = Point {
                        sequence: theta.head,
                        index: 0,
                    };
                    continue;
                }
                return Err(PassError::InvalidRuleSet(
                    "unprepared recovery computation".into(),
                ));
            }
            match &sequence.end {
                SequenceEnd::Root { values, deps } => {
                    return Ok(Destination::Return(values.clone(), deps.clone()));
                }
                SequenceEnd::GammaArm { gamma, arm, parent } => {
                    let gamma_id = *gamma;
                    let gamma = &self.prepared.gammas[&gamma_id];
                    self.bind(route, &gamma.outputs, &gamma.arms[*arm].results)?;
                    for repeated in self.recovery.repeated_controls.values() {
                        if repeated.gamma == gamma_id
                            && self.recovery.repeated_control(repeated.theta).is_some()
                        {
                            route
                                .control_facts
                                .insert(repeated.port, ControlOutcome::Exact(*arm as u64));
                        }
                    }
                    route.point = *parent;
                }
                SequenceEnd::ThetaHead { theta } => {
                    let plan = &self.prepared.thetas[theta];
                    let Some(repeat) = self.repeat_outcome(route, *theta, plan) else {
                        return Ok(Destination::Repeat(*theta));
                    };
                    route.point = Point {
                        sequence: if repeat { plan.continue_ } else { plan.exit },
                        index: 0,
                    };
                }
                SequenceEnd::ThetaContinue { theta } => {
                    self.repeat(route, &self.prepared.thetas[theta])?;
                }
                SequenceEnd::ThetaExit { theta, parent } => {
                    let theta = &self.prepared.thetas[theta];
                    self.bind(route, &theta.outputs, &theta.exit_values)?;
                    route.point = *parent;
                }
            }
        }
    }

    fn transfer(target: Node, route: Route) -> Transfer {
        Transfer {
            target,
            bindings: route.bindings,
            loops_back: route.loops_back,
        }
    }

    fn computation(&mut self, id: usize, incoming: Route) -> Result<Transfer, PassError> {
        if let Some(&node) = self.computations.get(&id) {
            return Ok(Self::transfer(node, incoming));
        }
        let fragment = self.prepared.fragments[id].clone();
        let sequence = &self.prepared.sequences[fragment.sequence.0];
        for &op in &fragment.ops {
            if let Some(actual) = self.edges.execution_domain(op)
                && actual != sequence.domain
            {
                return Err(PassError::InvalidRuleSet(format!(
                    "selected operation {op:?} {}.{} crosses recovery demand domains: {:?} -> {:?}",
                    self.context.get_op(op).dialect(),
                    self.context.get_op(op).name(),
                    self.recovery.domains()[actual.0 as usize],
                    self.recovery.domains()[sequence.domain.0 as usize]
                )));
            }
        }
        let node = self.new_fragment(fragment.ops);
        self.computations.insert(id, node);
        // Joining at executable code forgets incoming path facts. Selectors
        // needed beyond it are ordinary live values and become parameters.
        let next = self.canonical(Point {
            sequence: fragment.sequence,
            index: fragment.end,
        });
        self.connect(node, next)?;
        Ok(Self::transfer(node, incoming))
    }

    fn target(&mut self, mut route: Route) -> Result<Transfer, PassError> {
        match self.advance(&mut route)? {
            Destination::Computation(id) => self.computation(id, route),
            Destination::Producer(control) => {
                if let Some(&node) = self.definitions.get(&control) {
                    return Ok(Self::transfer(node, route));
                }
                let node = self.new_fragment(Vec::new());
                self.definitions.insert(control, node);
                // All entries, including loop feedback, supply this point's
                // bindings through SSA parameters. A first incoming route is
                // not the environment of subsequent invocations.
                self.producer_branch(node, self.canonical(route.point), control)?;
                Ok(Self::transfer(node, route))
            }
            Destination::Gamma(op) | Destination::Repeat(op) => {
                if let Some(&node) = self.controls.get(&op) {
                    return Ok(Self::transfer(node, route));
                }
                let node = self.new_fragment(Vec::new());
                self.controls.insert(op, node);
                let canonical = self.canonical(route.point);
                self.connect(node, canonical)?;
                Ok(Self::transfer(node, route))
            }
            Destination::Return(values, states) => {
                let node = self.new_fragment(Vec::new());
                self.fragments[node.0].term = Term::Leave {
                    values: values.into_iter().map(|v| self.value(v)).collect(),
                    states: states.into_iter().map(|v| self.value(v)).collect(),
                };
                Ok(Self::transfer(node, route))
            }
            Destination::Cycle(facts, control_facts) => {
                let key = CycleKey {
                    point: route.point,
                    values: facts.clone(),
                    controls: control_facts.clone(),
                };
                if let Some(&node) = self.cycles.get(&key) {
                    return Ok(Self::transfer(node, route));
                }
                let node = self.new_fragment(Vec::new());
                self.cycles.insert(key, node);
                let mut canonical = self.canonical(route.point);
                canonical.facts.extend(facts);
                canonical.control_facts.extend(control_facts);
                self.connect(node, canonical)?;
                Ok(Self::transfer(node, route))
            }
        }
    }

    fn connect(&mut self, node: Node, mut route: Route) -> Result<(), PassError> {
        match self.advance(&mut route)? {
            Destination::Gamma(op) => {
                let predicate = self.read(&route, self.prepared.gammas[&op].predicate);
                if self.owns_control(node, op, predicate) {
                    self.gamma_branch(node, route, op)
                } else {
                    let edge = self.target(route)?;
                    self.fragments[node.0].term = Term::Jump(edge);
                    Ok(())
                }
            }
            Destination::Repeat(op) => {
                let predicate = self.read(&route, self.prepared.thetas[&op].predicate);
                if self.owns_control(node, op, predicate) {
                    self.repeat_branch(node, route, op)
                } else {
                    let edge = self.target(route)?;
                    self.fragments[node.0].term = Term::Jump(edge);
                    Ok(())
                }
            }
            Destination::Return(values, states) => {
                self.fragments[node.0].term = Term::Leave {
                    values: values
                        .into_iter()
                        .map(|value| self.read(&route, value))
                        .collect(),
                    states: states
                        .into_iter()
                        .map(|value| self.read(&route, value))
                        .collect(),
                };
                Ok(())
            }
            Destination::Computation(id) => {
                let edge = self.computation(id, route)?;
                self.fragments[node.0].term = Term::Jump(edge);
                Ok(())
            }
            Destination::Producer(_) => {
                let edge = self.target(route)?;
                self.fragments[node.0].term = Term::Jump(edge);
                Ok(())
            }
            Destination::Cycle(_, _) => {
                let edge = self.target(route)?;
                self.fragments[node.0].term = Term::Jump(edge);
                Ok(())
            }
        }
    }

    fn producer_branch(
        &mut self,
        node: Node,
        route: Route,
        control: ControlId,
    ) -> Result<(), PassError> {
        let definition = self.request(control)?.clone();
        let mut reachable = Vec::new();
        for (index, &outcome) in definition.outcomes.iter().enumerate() {
            let decision = self.edges.decided_control(&definition, index);
            if decision != Some(false) {
                reachable.push((index, outcome));
            }
            if decision == Some(true) {
                break;
            }
        }
        let Some((_, last)) = reachable.pop() else {
            return Err(PassError::InvalidRuleSet(
                "control definition has no reachable outcomes".into(),
            ));
        };
        let mut taken_routes = Vec::with_capacity(reachable.len());
        for (outcome, fact) in reachable {
            let mut selected = route.clone();
            let predicate = self.value(definition.source_predicate);
            if !self.constants.contains_key(&predicate) {
                selected.facts.insert(predicate, fact);
            }
            selected.skip_control = Some(control);
            taken_routes.push((outcome, self.target(selected)?));
        }
        let mut last_route = route.clone();
        let predicate = self.value(definition.source_predicate);
        if !self.constants.contains_key(&predicate) {
            last_route.facts.insert(predicate, last);
        }
        last_route.skip_control = Some(control);
        let mut other = self.target(last_route)?;
        for (position, (outcome, taken)) in taken_routes.into_iter().enumerate().rev() {
            let here = if position == 0 {
                node
            } else {
                self.new_fragment(Vec::new())
            };
            self.fragments[here.0].term = Term::Branch {
                control,
                outcome,
                predicate: self.read(&route, definition.source_predicate),
                bindings: route.bindings.clone(),
                taken,
                other,
            };
            other = Transfer {
                target: here,
                bindings: Vec::new(),
                loops_back: false,
            };
        }
        if matches!(self.fragments[node.0].term, Term::Pending) {
            self.fragments[node.0].term = Term::Jump(other);
        }
        Ok(())
    }

    fn owns_control(&self, node: Node, consumer: OpId, predicate: ValueId) -> bool {
        self.controls.get(&consumer) == Some(&node)
            || self
                .context
                .get_value(predicate)
                .defining_op()
                .is_some_and(|producer| self.fragments[node.0].ops.contains(&producer))
    }

    fn gamma_branch(&mut self, node: Node, route: Route, op: OpId) -> Result<(), PassError> {
        let gamma = self.prepared.gammas[&op].clone();
        if self.recovery.control(op, Test::Arm(0)).is_none() {
            self.lost_direct.extend(
                self.recovery
                    .covering_controls(op, Test::Arm(0))
                    .iter()
                    .copied(),
            );
            let edge = self.target(self.select_arm(route, op, 0)?)?;
            self.fragments[node.0].term = Term::Jump(edge);
            return Ok(());
        }
        let mut arms = Vec::new();
        for index in 0..gamma.arms.len() {
            let decision = self
                .recovery
                .control(op, Test::Arm(index))
                .and_then(|(control, route)| self.edges.decided_control(control, route.outcome));
            if decision != Some(false) {
                arms.push(index);
            }
            if decision == Some(true) {
                break;
            }
        }
        let mut destinations = Vec::new();
        for &arm in &arms {
            destinations.push(self.target(self.select_arm(route.clone(), op, arm)?)?);
        }
        let Some(last) = destinations.pop() else {
            return Err(PassError::InvalidRuleSet(
                "recovery Gamma has no reachable arm".into(),
            ));
        };
        if destinations.is_empty() {
            self.fragments[node.0].term = Term::Jump(last);
            return Ok(());
        }
        let mut other = last;
        for (position, taken) in destinations.into_iter().enumerate().rev() {
            let here = if position == 0 {
                node
            } else {
                self.new_fragment(Vec::new())
            };
            let (control, control_route) = self
                .recovery
                .control(op, Test::Arm(arms[position]))
                .ok_or_else(|| {
                    PassError::InvalidRuleSet("recovery Gamma lost a control request".into())
                })?;
            self.fragments[here.0].term = Term::Branch {
                control: control.id,
                outcome: control_route.outcome,
                predicate: self.read(&route, gamma.predicate),
                bindings: route.bindings.clone(),
                taken,
                other,
            };
            other = Transfer {
                target: here,
                bindings: Vec::new(),
                loops_back: false,
            };
        }
        Ok(())
    }

    fn repeat_branch(&mut self, node: Node, route: Route, op: OpId) -> Result<(), PassError> {
        let theta = &self.prepared.thetas[&op];
        let routed = self.recovery.control(op, Test::Repeat);
        let (control_id, outcome) = if let Some((control, route)) = routed {
            (control.id, route.outcome)
        } else if let Some(repeated) = self.recovery.repeated_control(op) {
            self.lost_direct
                .extend(repeated.source_controls.iter().copied());
            (repeated.fallback_control, 0)
        } else {
            self.lost_direct.extend(
                self.recovery
                    .covering_controls(op, Test::Repeat)
                    .iter()
                    .copied(),
            );
            let edge = self.target(route)?;
            self.fragments[node.0].term = Term::Jump(edge);
            return Ok(());
        };
        let predicate = self.read(&route, theta.predicate);
        let mut repeat = route.clone();
        let logical = self.value(theta.predicate);
        if !self.ambiguous_facts.contains(&logical) && !self.constants.contains_key(&logical) {
            repeat.facts.insert(logical, ControlOutcome::DefaultFrom(1));
        }
        repeat.point = Point {
            sequence: theta.continue_,
            index: 0,
        };
        let mut exit = route.clone();
        if !self.ambiguous_facts.contains(&logical) && !self.constants.contains_key(&logical) {
            exit.facts.insert(logical, ControlOutcome::Exact(0));
        }
        exit.point = Point {
            sequence: theta.exit,
            index: 0,
        };
        let taken = self.target(repeat)?;
        let other = self.target(exit)?;
        self.fragments[node.0].term = Term::Branch {
            control: control_id,
            outcome,
            predicate,
            bindings: route.bindings,
            taken,
            other,
        };
        Ok(())
    }

    fn request(&self, id: ControlId) -> Result<&ControlDefinition, PassError> {
        self.recovery.definition(id).ok_or_else(|| {
            PassError::InvalidRuleSet("recovery branch has no semantic control".into())
        })
    }
}

/// Symbolic parameters permit eliminating trivial phis before allocating IR
/// values. Source definitions retain their own identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SsaValue {
    Definition(ValueId),
    Parameter(Node, ValueId),
}

struct Ssa {
    live: Vec<BTreeSet<ValueId>>,
    demands: Vec<BTreeSet<ValueId>>,
    defs: Vec<HashSet<ValueId>>,
    incoming: Vec<Vec<(Node, Transfer)>>,
    aliases: HashMap<SsaValue, SsaValue>,
    parameters: HashMap<SsaValue, ValueId>,
    blocks: Vec<BlockId>,
    /// External values and unchanged SSA definitions proved available on
    /// every path to their uses by the live-in fixed point.
    available: HashSet<ValueId>,
}

impl Ssa {
    fn canonical(&self, mut value: SsaValue) -> SsaValue {
        while let Some(&next) = self.aliases.get(&value) {
            value = next;
        }
        value
    }

    fn at(&self, node: Node, value: ValueId) -> Result<SsaValue, PassError> {
        if self.defs[node.0].contains(&value) || self.available.contains(&value) {
            return Ok(SsaValue::Definition(value));
        }
        if self.live[node.0].contains(&value) {
            return Ok(self.canonical(SsaValue::Parameter(node, value)));
        }
        Err(PassError::InvalidRuleSet(format!(
            "recovery fragment {} reads unbound value {}",
            node.0,
            value.number()
        )))
    }

    fn value(&self, node: Node, value: ValueId) -> Result<ValueId, PassError> {
        self.concrete(self.at(node, value)?)
    }

    fn concrete(&self, value: SsaValue) -> Result<ValueId, PassError> {
        match self.canonical(value) {
            SsaValue::Definition(value) => Ok(value),
            parameter => self.parameters.get(&parameter).copied().ok_or_else(|| {
                PassError::InvalidRuleSet("recovery parameter was not allocated".into())
            }),
        }
    }
}

impl Finish<'_> {
    fn branch_reads(&self, term: &Term) -> Result<Vec<ValueId>, PassError> {
        let Term::Branch {
            control,
            outcome,
            bindings,
            ..
        } = term
        else {
            return Ok(Vec::new());
        };
        let request = self.request(*control)?;
        Ok(self
            .edges
            .control_reads(request, *outcome)
            .into_iter()
            .map(|value| Self::resolved(bindings, self.value(value)))
            .collect())
    }

    fn liveness(&self, root: Node, ports: &[ValueId]) -> Result<Ssa, PassError> {
        let all_ops: HashSet<_> = self
            .prepared
            .sequences
            .iter()
            .flat_map(|sequence| sequence.ops.iter().copied())
            .collect();
        let mut defs = Vec::new();
        let mut uses = Vec::new();
        // Selection may erase an earlier function's symbol-producing op.
        // Its source-level external identity remains valid for named calls.
        let mut globals: HashSet<_> = self
            .recovery
            .external_values
            .iter()
            .map(|&value| self.value(value))
            .collect();
        let mut incoming = vec![Vec::new(); self.fragments.len()];
        for (index, fragment) in self.fragments.iter().enumerate() {
            let mut defined: HashSet<ValueId> = if index == root.0 {
                ports.iter().copied().collect()
            } else {
                HashSet::new()
            };
            let mut read = BTreeSet::new();
            for &op in &fragment.ops {
                let instance = self.context.get_op(op);
                let mut inputs = values_read(self.context, op);
                for input in self.edges.implicit_inputs(op) {
                    inputs.extend(self.context.get_op(input).results());
                }
                read.extend(inputs.into_iter().filter(|value| !defined.contains(value)));
                defined.extend(instance.results());
            }
            read.extend(
                self.branch_reads(&fragment.term)?
                    .into_iter()
                    .filter(|v| !defined.contains(v)),
            );
            if let Term::Leave { values, states } = &fragment.term {
                read.extend(
                    values
                        .iter()
                        .chain(states)
                        .filter(|v| !defined.contains(v))
                        .copied(),
                );
            }
            if matches!(fragment.term, Term::Pending) {
                return Err(PassError::InvalidRuleSet(
                    "recovery left an unresolved fragment".into(),
                ));
            }
            for edge in fragment.term.successors() {
                incoming[edge.target.0].push((Node(index), edge.clone()));
            }
            let routed = fragment
                .term
                .successors()
                .into_iter()
                .flat_map(|edge| edge.bindings.iter().map(|binding| binding.current));
            for value in read.iter().copied().chain(routed) {
                if self
                    .context
                    .get_value(value)
                    .defining_op()
                    .is_some_and(|op| self.context.has_operation(op) && !all_ops.contains(&op))
                {
                    globals.insert(value);
                }
            }
            defs.push(defined);
            uses.push(read);
        }
        let mut live = uses;
        let mut queue: VecDeque<_> = (0..self.fragments.len()).collect();
        let mut queued = vec![true; self.fragments.len()];
        while let Some(index) = queue.pop_front() {
            queued[index] = false;
            let needed = live[index].clone();
            for (source, edge) in &incoming[index] {
                let before = live[source.0].len();
                for &value in &needed {
                    let value = Self::resolved(&edge.bindings, value);
                    if !defs[source.0].contains(&value) && !globals.contains(&value) {
                        live[source.0].insert(value);
                    }
                }
                if before != live[source.0].len() && !queued[source.0] {
                    queued[source.0] = true;
                    queue.push_back(source.0);
                }
            }
        }
        for values in &mut live {
            values.retain(|value| !globals.contains(value));
        }
        if !live[root.0].is_empty() {
            return Err(PassError::InvalidRuleSet(format!(
                "recovery has unbound function-entry values: {:?}",
                live[root.0]
            )));
        }
        // A name never assigned by a boundary still has its one SSA
        // definition. If any path bypassed that definition, the fixed point
        // above would have carried the name to the function entry. Keep such
        // dominating values directly instead of inventing cyclic phis for
        // every loop-invariant capture.
        let demands = live.clone();
        let assigned: HashSet<_> = self
            .fragments
            .iter()
            .flat_map(|fragment| {
                fragment
                    .term
                    .successors()
                    .into_iter()
                    .flat_map(|edge| edge.bindings.iter().map(|binding| binding.source))
            })
            .collect();
        let mut available = globals;
        available.extend(
            defs.iter()
                .flat_map(|defined| defined.iter().copied())
                .filter(|value| {
                    !assigned.contains(value)
                        && !self
                            .context
                            .is_state_type(self.context.get_value(*value).ty())
                }),
        );
        for values in &mut live {
            values.retain(|value| !available.contains(value));
        }
        Ok(Ssa {
            live,
            demands,
            defs,
            incoming,
            aliases: HashMap::new(),
            parameters: HashMap::new(),
            blocks: Vec::new(),
            available,
        })
    }

    fn simplify_parameters(&self, ssa: &mut Ssa) -> Result<(), PassError> {
        loop {
            let mut changed = false;
            for index in 0..self.fragments.len() {
                for &raw in &ssa.live[index] {
                    // Resource chains enter each block explicitly.
                    if self.context.is_state_type(self.context.get_value(raw).ty()) {
                        continue;
                    }
                    let parameter = SsaValue::Parameter(Node(index), raw);
                    if ssa.aliases.contains_key(&parameter) {
                        continue;
                    }
                    let mut same = None;
                    let mut conflict = false;
                    for (source, edge) in &ssa.incoming[index] {
                        let value =
                            ssa.canonical(ssa.at(*source, Self::resolved(&edge.bindings, raw))?);
                        if value == parameter {
                            continue;
                        }
                        if same.is_some_and(|previous| previous != value) {
                            conflict = true;
                            break;
                        }
                        same = Some(value);
                    }
                    if !conflict && let Some(value) = same {
                        ssa.aliases.insert(parameter, value);
                        changed = true;
                    }
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    fn discard_unused_literals(&mut self, ssa: &Ssa, root: Node) -> Result<(), PassError> {
        let mut used = HashSet::new();
        for fragment in &self.fragments {
            for &op in &fragment.ops {
                if !self.literals.contains(&op) {
                    used.extend(values_read(self.context, op));
                }
            }
            used.extend(self.branch_reads(&fragment.term)?);
            if let Term::Leave { values, states } = &fragment.term {
                used.extend(values.iter().chain(states).copied());
            }
            for edge in fragment.term.successors() {
                for &value in &ssa.live[edge.target.0] {
                    used.insert(Self::resolved(&edge.bindings, value));
                }
            }
        }
        self.fragments[root.0].ops.retain(|&op| {
            !self.literals.contains(&op)
                || self
                    .context
                    .get_op(op)
                    .results()
                    .iter()
                    .any(|value| used.contains(value))
        });
        Ok(())
    }

    /// Literal definitions do not stop continuation tracing. Place a used
    /// literal down a single-entry path while only one successor needs it.
    /// Stop at a join, including every loop header, to avoid rematerializing
    /// an invariant on each iteration.
    fn place_literals(&mut self, ssa: &Ssa, root: Node) -> Result<(), PassError> {
        let mut local_reads = Vec::new();
        for (index, fragment) in self.fragments.iter().enumerate() {
            let mut read = Vec::new();
            for &op in &fragment.ops {
                read.extend(values_read(self.context, op));
            }
            read.extend(self.branch_reads(&fragment.term)?);
            if let Term::Leave { values, states } = &fragment.term {
                read.extend(values.iter().chain(states).copied());
            }
            for edge in fragment.term.successors() {
                for &raw in &ssa.live[edge.target.0] {
                    if !ssa
                        .aliases
                        .contains_key(&SsaValue::Parameter(edge.target, raw))
                    {
                        read.push(Self::resolved(&edge.bindings, raw));
                    }
                }
            }
            let read = read
                .into_iter()
                .map(|raw| ssa.at(Node(index), raw).map(|value| ssa.canonical(value)))
                .collect::<Result<HashSet<_>, _>>()?;
            local_reads.push(read);
        }
        let literals: Vec<_> = self.fragments[root.0]
            .ops
            .iter()
            .copied()
            .filter(|op| self.literals.contains(op))
            .collect();
        let mut placements = vec![Vec::new(); self.fragments.len()];
        for op in literals {
            let results = self.context.get_op(op).results();
            let uses: Vec<_> = local_reads
                .iter()
                .enumerate()
                .filter(|(_, read)| {
                    results
                        .iter()
                        .any(|&value| read.contains(&SsaValue::Definition(value)))
                })
                .map(|(index, _)| Node(index))
                .collect();
            // A single returning fragment is entered at most once. Constants
            // used only there need not cross preceding acyclic joins.
            let terminal =
                uses.len() == 1 && matches!(self.fragments[uses[0].0].term, Term::Leave { .. });
            let mut node = if terminal { uses[0] } else { root };
            if !terminal {
                loop {
                    if results
                        .iter()
                        .any(|value| local_reads[node.0].contains(&SsaValue::Definition(*value)))
                    {
                        break;
                    }
                    let mut needed = Vec::new();
                    for edge in self.fragments[node.0].term.successors() {
                        let mut reads = false;
                        for &raw in &ssa.demands[edge.target.0] {
                            let value = ssa.canonical(ssa.at(edge.target, raw)?);
                            reads |= results
                                .iter()
                                .any(|&result| value == SsaValue::Definition(result));
                        }
                        if reads {
                            needed.push(edge);
                        }
                    }
                    if needed.len() != 1 || ssa.incoming[needed[0].target.0].len() != 1 {
                        break;
                    }
                    let target = needed[0].target;
                    if target == root || target == node {
                        break;
                    }
                    node = target;
                }
            }
            let local = results
                .iter()
                .any(|value| local_reads[node.0].contains(&SsaValue::Definition(*value)));
            let mut position = self.fragments[node.0].ops.len();
            for (index, &held) in self.fragments[node.0].ops.iter().enumerate() {
                let reads = values_read(self.context, held);
                if reads.into_iter().any(|raw| {
                    ssa.at(node, raw).is_ok_and(|value| {
                        let value = ssa.canonical(value);
                        results
                            .iter()
                            .any(|&result| value == SsaValue::Definition(result))
                    })
                }) {
                    position = index;
                    break;
                }
            }
            placements[node.0].push((position, local, op));
        }
        for (fragment, mut placed) in self.fragments.iter_mut().zip(placements) {
            // At the terminator, keep successor-only literals before the
            // current test's literals. Equal reader positions retain source
            // order without extending live ranges across earlier operations.
            placed.sort_by_key(|&(position, local, _)| (position, local));
            let mut placed = placed.into_iter().peekable();
            let original = std::mem::take(&mut fragment.ops);
            for (index, op) in original.into_iter().enumerate() {
                while placed
                    .peek()
                    .is_some_and(|&(position, _, _)| position == index)
                {
                    fragment.ops.push(placed.next().unwrap().2);
                }
                if !self.literals.contains(&op) {
                    fragment.ops.push(op);
                }
            }
            fragment.ops.extend(placed.map(|(_, _, op)| op));
        }
        Ok(())
    }

    fn block_groups(&self, ssa: &mut Ssa, root: Node) -> Result<Vec<Vec<Node>>, PassError> {
        let mut next = vec![None; self.fragments.len()];
        let mut previous = vec![None; self.fragments.len()];
        for (index, fragment) in self.fragments.iter().enumerate() {
            let Term::Jump(edge) = &fragment.term else {
                continue;
            };
            if edge.target == root
                || edge.target == Node(index)
                || ssa.incoming[edge.target.0].len() != 1
            {
                continue;
            }
            next[index] = Some(edge.target);
            previous[edge.target.0] = Some(Node(index));
            for &raw in &ssa.live[edge.target.0] {
                let incoming = ssa.at(Node(index), Self::resolved(&edge.bindings, raw))?;
                ssa.aliases
                    .insert(SsaValue::Parameter(edge.target, raw), incoming);
            }
        }
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        for (index, predecessor) in previous.iter().enumerate() {
            if predecessor.is_some() {
                continue;
            }
            let mut group = Vec::new();
            let mut node = Node(index);
            loop {
                if !seen.insert(node) {
                    return Err(PassError::InvalidRuleSet(
                        "cyclic linear recovery fragment chain".into(),
                    ));
                }
                group.push(node);
                let Some(following) = next[node.0] else {
                    break;
                };
                node = following;
            }
            groups.push(group);
        }
        if seen.len() != self.fragments.len() {
            return Err(PassError::InvalidRuleSet(
                "recovery contains an unentered linear cycle".into(),
            ));
        }
        Ok(groups)
    }

    fn allocate(&self, ssa: &mut Ssa, root: Node, ports: &[ValueId], groups: &[Vec<Node>]) {
        ssa.blocks
            .resize(self.fragments.len(), BlockId::PLACEHOLDER);
        for group in groups {
            let first = group[0];
            let block = self.context.create_block(Vec::new()).id();
            for node in group {
                ssa.blocks[node.0] = block;
            }
            if first == root {
                for &port in ports {
                    self.context.adopt_block_argument(block, port);
                }
                continue;
            }
            let params: Vec<_> = ssa.live[first.0]
                .iter()
                .copied()
                .filter(|&raw| !ssa.aliases.contains_key(&SsaValue::Parameter(first, raw)))
                .collect();
            for raw in values_then_states(self.context, &params) {
                let value = self
                    .context
                    .append_block_argument(block, self.context.get_value(raw).ty())
                    .id();
                ssa.parameters
                    .insert(SsaValue::Parameter(first, raw), value);
            }
        }
    }

    fn emitted_edge(
        &self,
        ssa: &Ssa,
        source: Node,
        transfer: &Transfer,
    ) -> Result<Edge, PassError> {
        let raw: Vec<_> = ssa.live[transfer.target.0]
            .iter()
            .copied()
            .filter(|&raw| {
                ssa.parameters
                    .contains_key(&SsaValue::Parameter(transfer.target, raw))
            })
            .collect();
        let args = values_then_states(self.context, &raw)
            .into_iter()
            .map(|raw| ssa.value(source, Self::resolved(&transfer.bindings, raw)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut edge = Edge::with(ssa.blocks[transfer.target.0], &args);
        edge.loops_back = transfer.loops_back;
        Ok(edge)
    }

    fn emit_branch(
        &self,
        ssa: &Ssa,
        source: Node,
        term: &Term,
        extra_blocks: &mut Vec<BlockId>,
    ) -> Result<(), PassError> {
        let Term::Branch {
            control,
            outcome,
            predicate,
            bindings,
            taken,
            other,
        } = term
        else {
            unreachable!("called for branch terminator");
        };
        let request = self.request(*control)?;
        let mut substitutions = Vec::new();
        for value in self.edges.control_reads(request, *outcome) {
            let raw = Self::resolved(bindings, self.value(value));
            substitutions.push(ValueBinding {
                source: value,
                current: ssa.value(source, raw)?,
            });
        }
        // A fused machine branch does not read the source comparison result.
        let predicate = ssa.value(source, *predicate).unwrap_or(*predicate);
        substitutions.push(ValueBinding {
            source: request.source_predicate,
            current: predicate,
        });
        let taken = self.emitted_edge(ssa, source, taken)?;
        let other = self.emitted_edge(ssa, source, other)?;
        self.edges.branch_control(
            request,
            *outcome,
            predicate,
            &substitutions,
            ssa.blocks[source.0],
            &taken,
            &other,
            &mut || {
                let block = self.context.create_block(Vec::new()).id();
                extra_blocks.push(block);
                block
            },
        )
    }

    fn emit(mut self, region: RegionId, root: Node) -> Result<(), PassError> {
        let ports: Vec<_> = self.prepared.sequences[self.prepared.root.0]
            .entry
            .iter()
            .map(|&value| self.value(value))
            .collect();
        let ports = values_then_states(self.context, &ports);
        let mut ssa = self.liveness(root, &ports)?;
        self.simplify_parameters(&mut ssa)?;
        self.discard_unused_literals(&ssa, root)?;
        self.place_literals(&ssa, root)?;
        let groups = self.block_groups(&mut ssa, root)?;
        self.allocate(&mut ssa, root, &ports, &groups);
        let mut extra_blocks = vec![Vec::new(); self.fragments.len()];
        for node in groups.iter().flatten() {
            let index = node.0;
            let fragment = &self.fragments[index];
            let block = ssa.blocks[index];
            for &op in &fragment.ops {
                let instance = self.context.get_op(op);
                let operands = instance
                    .operands()
                    .iter()
                    .map(|&value| ssa.value(Node(index), value))
                    .collect::<Result<Vec<_>, _>>()?;
                let renames: Vec<_> = instance
                    .operands()
                    .iter()
                    .copied()
                    .zip(operands)
                    .filter(|(source, current)| source != current)
                    .collect();
                super::super::rename_within(self.context, op, &renames);
                if let Some(region) = self.context.parent_nodes_region(op) {
                    self.context.remove_from_region(region, op);
                }
                self.context.get_block(block).append(op);
            }
            match &fragment.term {
                Term::Jump(edge)
                    if block == ssa.blocks[edge.target.0]
                        && edge.target != Node(index)
                        && ssa.incoming[edge.target.0].len() == 1 => {}
                Term::Jump(edge) => self
                    .edges
                    .jump(block, &self.emitted_edge(&ssa, Node(index), edge)?),
                Term::Branch { .. } => {
                    self.emit_branch(&ssa, Node(index), &fragment.term, &mut extra_blocks[index])?
                }
                Term::Leave { values, states } => {
                    let values = values
                        .iter()
                        .map(|&v| ssa.value(Node(index), v))
                        .collect::<Result<Vec<_>, _>>()?;
                    let states = states
                        .iter()
                        .map(|&v| ssa.value(Node(index), v))
                        .collect::<Result<Vec<_>, _>>()?;
                    self.edges.leave(block, &values, &states)?;
                }
                Term::Pending => unreachable!("liveness rejects pending terminators"),
            }
        }
        for &op in self.prepared.structured.iter().rev() {
            if self.context.has_operation(op) {
                self.context
                    .erase_op(&OperationRef::new(self.context.get_op(op)))?;
            }
        }
        let mut blocks = Vec::new();
        for group in &groups {
            blocks.push(ssa.blocks[group[0].0]);
            // Keep edge-transfer blocks with the branch that creates them.
            // Appending them after unrelated code biases equal-affinity
            // layout choices against short loop backedges.
            for node in group {
                blocks.append(&mut extra_blocks[node.0]);
            }
        }
        self.context.replace_region_with_blocks(region, blocks);
        self.discard_unused_controls()?;
        Ok(())
    }

    /// Retire control aliases replaced by routing, and their dead literals.
    /// Other computations remain: an unused demanded load can still trap.
    fn discard_unused_controls(&self) -> Result<(), PassError> {
        let mut candidates = self.literals.clone();
        for alias in &self.recovery.control_aliases {
            let value = self.value(alias.value);
            if self.context.has_value(value)
                && let Some(op) = self.context.get_value(value).defining_op()
                && self.context.has_operation(op)
                && self.context.get_op(op).is::<crate::builtin::ops::XOrIOp>()
            {
                candidates.insert(op);
            }
        }
        let mut pending: Vec<_> = candidates.iter().copied().collect();
        pending.sort_by_key(|op| op.number());
        while let Some(op) = pending.pop() {
            if !self.context.has_operation(op) || self.context.parent_block(op).is_none() {
                continue;
            }
            let instance = self.context.get_op(op);
            if instance
                .results()
                .iter()
                .any(|&value| self.context.use_count(value) != 0)
            {
                continue;
            }
            let inputs: Vec<_> = instance
                .operands()
                .iter()
                .filter_map(|&value| self.context.get_value(value).defining_op())
                .filter(|producer| candidates.contains(producer))
                .collect();
            self.context.erase_op(&OperationRef::new(instance))?;
            pending.extend(inputs);
        }
        Ok(())
    }
}
