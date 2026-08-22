//! Sparse conditional constant facts: what every value is, and which blocks
//! control reaches, solved together.
//!
//! The two halves feed each other. A value is constant when every executed
//! definition of it computes the same integer, so a block nothing reaches
//! contributes nothing to the values it defines; a branch on a constant reaches
//! only the successor that constant selects, so a folded guard prunes blocks and
//! γ arms. Both are stated as one flat lattice over two kinds of node — values
//! and blocks — and run to fixpoint by the generic [`solver`](super::solver).
//!
//! An op contributes a constant through the interfaces it already declares:
//! [`ConstantLike`] reads a literal, [`ConstantFold`] evaluates an op over
//! constant operands. Anything else is overdefined, which is always sound.
//!
//! Facts are block-scoped where the CFG says so: a value a dominating guarded
//! edge pins to a boolean reads as that boolean inside the blocks the edge
//! dominates ([`DominatingEdgeFacts`]), so a re-tested condition decides its own
//! branch.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tir_adt::APInt;

use crate::analysis::scopes::{carried_operands, port_edges, region_exit, tested_ports};
use crate::analysis::solver::{FactDomain, Facts, Lattice, solve};
use crate::analysis::{Analysis, AnalysisManager, DefUse, DominatingEdgeFacts};
use crate::sem::Value as SemValue;
use crate::{
    BlockId, BranchGuard, BranchTerminator, Conditional, ConstantFold, ConstantLike, Context,
    LoopLike, OpHandle, OpId, RegionId, Terminator, TokenScope, ValueId,
};

/// What the solver knows about one node. Values take [`Fact::Const`] and
/// [`Fact::Overdefined`], blocks take [`Fact::Reached`]; the two never meet, so
/// one flat lattice states both.
#[derive(Clone, Debug)]
pub enum Fact {
    /// Nothing has been proven yet: no executed path computes this value, or
    /// none reaches this block.
    Unknown,
    /// Every execution computes this integer.
    Const(APInt),
    /// Control reaches this block.
    Reached,
    /// The node takes more than one value, or one this analysis cannot name.
    Overdefined,
}

/// Two constants are the same fact when they are the same bits at the same
/// width, however each was signed on its way here.
impl PartialEq for Fact {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Fact::Const(a), Fact::Const(b)) => a.width() == b.width() && a.to_u64() == b.to_u64(),
            (Fact::Unknown, Fact::Unknown)
            | (Fact::Reached, Fact::Reached)
            | (Fact::Overdefined, Fact::Overdefined) => true,
            _ => false,
        }
    }
}

impl Lattice for Fact {
    fn bottom() -> Self {
        Fact::Unknown
    }

    fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (Fact::Unknown, fact) | (fact, Fact::Unknown) => fact.clone(),
            (Fact::Const(a), _) if self == other => Fact::Const(a.clone()),
            (Fact::Reached, Fact::Reached) => Fact::Reached,
            _ => Fact::Overdefined,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Node {
    Value(ValueId),
    Block(BlockId),
}

/// Per-value constants and per-block executability for the IR under a function.
/// Build through [`AnalysisManager`].
pub struct ConstantFacts {
    facts: Facts<Node, Fact>,
}

impl ConstantFacts {
    /// What is known about `value`, for a caller reading the lattice directly.
    pub fn fact(&self, value: ValueId) -> Fact {
        self.facts.get(Node::Value(value))
    }

    /// The integer `value` holds on every execution that computes it.
    pub fn constant(&self, value: ValueId) -> Option<APInt> {
        match self.fact(value) {
            Fact::Const(value) => Some(value),
            _ => None,
        }
    }

    /// Whether any execution reaches `block`.
    pub fn is_executable(&self, block: BlockId) -> bool {
        self.facts.get(Node::Block(block)) == Fact::Reached
    }
}

impl Analysis for ConstantFacts {
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
        let propagation = Propagation {
            context,
            root: op,
            defuse: analyses.get::<DefUse>(context, op),
            edges: analyses.get::<DominatingEdgeFacts>(context, op),
            back_edges: RefCell::new(HashMap::new()),
        };
        Self {
            facts: solve(&propagation),
        }
    }
}

type State = Facts<Node, Fact>;

struct Propagation<'a> {
    context: &'a Context,
    root: OpId,
    defuse: Rc<DefUse>,
    edges: Rc<DominatingEdgeFacts>,
    /// The edges back into each loop's carried ports. Finding them walks the
    /// loop's whole body tree, and a loop is transferred once per fact its body
    /// raises, so they are found once.
    back_edges: RefCell<HashMap<OpId, Vec<OpId>>>,
}

impl FactDomain for Propagation<'_> {
    type Node = Node;
    type Fact = Fact;

    fn seed(&self, facts: &mut State) {
        for region in self.context.get_op(self.root).regions() {
            self.enter_region(region, &HashMap::new(), facts);
        }
    }

    fn transfer(&self, node: Node, facts: &mut State) {
        match node {
            Node::Block(block) => {
                for op in self.context.get_block(block).op_ids() {
                    self.visit_op(op, facts);
                }
            }
            Node::Value(value) => {
                for &user in self.defuse.users_of(value.number()) {
                    self.visit_op(user, facts);
                    // Only a terminator carries a value out of the region it
                    // sits in. A yield hands it to the op holding that region,
                    // but a `break`/`continue` hands it to a loop any number of
                    // regions further out, so every enclosing op reads it again.
                    if self
                        .context
                        .get_op(user)
                        .as_interface::<dyn Terminator>()
                        .is_none()
                    {
                        continue;
                    }
                    let mut enclosing = self.context.parent_op(user);
                    while let Some(op) = enclosing.filter(|&op| op != self.root) {
                        self.visit_op(op, facts);
                        enclosing = self.context.parent_op(op);
                    }
                }
            }
        }
    }
}

impl Propagation<'_> {
    /// Run the op's transfer function, unless nothing reaches it.
    fn visit_op(&self, op: OpId, facts: &mut State) {
        let instance = self.context.get_op(op);
        let Some(block) = instance.parent_block() else {
            return;
        };
        if facts.get(Node::Block(block)) != Fact::Reached {
            return;
        }
        if !instance.regions().is_empty() {
            self.visit_region_op(&instance, block, facts);
        } else if let Some(terminator) = instance.clone().as_interface::<dyn Terminator>() {
            self.visit_terminator(&instance, terminator.successors(), block, facts);
        } else {
            self.visit_leaf(&instance, facts);
        }
    }

    /// A value-producing op: the constant its interfaces name over the constants
    /// its operands hold.
    fn visit_leaf(&self, instance: &OpHandle, facts: &mut State) {
        let results = instance.results().to_vec();
        let fact = match results.as_slice() {
            [result] => self.folded(instance, *result, facts),
            _ => Fact::Overdefined,
        };
        for result in results {
            facts.raise(Node::Value(result), fact.clone());
        }
    }

    fn folded(&self, instance: &OpHandle, result: ValueId, facts: &State) -> Fact {
        if !is_integer(self.context, result) {
            return Fact::Overdefined;
        }
        if let Some(literal) = instance.clone().as_interface::<dyn ConstantLike>() {
            return Fact::Const(literal.constant_value());
        }
        let Some(fold) = instance.clone().as_interface::<dyn ConstantFold>() else {
            return Fact::Overdefined;
        };
        let mut operands = Vec::new();
        for operand in instance.operands().to_vec() {
            match facts.get(Node::Value(operand)) {
                Fact::Const(value) => operands.push(SemValue::Int(value)),
                // Nothing computes the operand yet: the op waits rather than
                // giving up on a fact that is still coming.
                Fact::Unknown => return Fact::Unknown,
                _ => return Fact::Overdefined,
            }
        }
        match fold.fold(&operands) {
            Some(SemValue::Int(value)) => Fact::Const(value),
            _ => Fact::Overdefined,
        }
    }

    /// A branch: reach the successors its guard leaves possible, and carry the
    /// values it forwards into their block arguments. The two interfaces report
    /// one entry per outgoing edge in the same order, so a guard is read against
    /// the edge at its index.
    fn visit_terminator(
        &self,
        instance: &OpHandle,
        successors: Vec<BlockId>,
        block: BlockId,
        facts: &mut State,
    ) {
        if successors.is_empty() {
            return;
        }
        let edges = match instance.clone().as_interface::<dyn BranchTerminator>() {
            Some(branch) => branch.successor_operands(),
            None => successors
                .into_iter()
                .map(|dest| (dest, Vec::new()))
                .collect(),
        };
        let guards = instance
            .clone()
            .as_interface::<dyn BranchGuard>()
            .map(|guard| guard.guarded_successors())
            .unwrap_or_default();
        for (index, (dest, forwarded)) in edges.into_iter().enumerate() {
            if let Some(&(_, condition, holds)) =
                guards.get(index).filter(|&&(guarded, ..)| guarded == dest)
            {
                let taken = match self.decision(condition, block, facts) {
                    Fact::Const(value) => !value.is_zero() == holds,
                    // Nothing computes the condition yet; the edge waits.
                    Fact::Unknown => false,
                    _ => true,
                };
                if !taken {
                    continue;
                }
            }
            facts.raise(Node::Block(dest), Fact::Reached);
            for (argument, value) in self
                .context
                .get_block(dest)
                .arguments()
                .iter()
                .zip(forwarded)
            {
                let fact = facts.get(Node::Value(value));
                facts.raise(Node::Value(argument.id()), fact);
            }
        }
    }

    fn visit_region_op(&self, instance: &OpHandle, block: BlockId, facts: &mut State) {
        if let Some(conditional) = instance.clone().as_interface::<dyn Conditional>() {
            return self.visit_gamma(instance, conditional.as_ref(), block, facts);
        }
        if let Some(loop_like) = instance.clone().as_interface::<dyn LoopLike>() {
            return self.visit_theta(instance, loop_like.as_ref(), facts);
        }
        for region in instance.regions() {
            self.enter_region(region, &HashMap::new(), facts);
        }
        for result in instance.results().to_vec() {
            facts.raise(Node::Value(result), Fact::Overdefined);
        }
    }

    /// γ: only the arms its decision leaves possible run, and each result is the
    /// choice between what those arms yield.
    fn visit_gamma(
        &self,
        instance: &OpHandle,
        conditional: &dyn Conditional,
        block: BlockId,
        facts: &mut State,
    ) {
        let decision = self.decision(conditional.decision(), block, facts);
        let cases = conditional.case_values();
        for &(region, case) in &cases {
            if !arm_runs(&decision, case, &cases) {
                continue;
            }
            let bindings = self.arm_bindings(instance, region, facts);
            self.enter_region(region, &bindings, facts);
        }
        // An arm leaving the enclosing loop never reaches what follows the gate,
        // so what it would have yielded is never read.
        let yielding: Vec<RegionId> = cases
            .iter()
            .map(|&(region, _)| region)
            .filter(|&region| self.is_reached(region, facts))
            .filter(|&region| region_exit(self.context, region).is_none())
            .collect();
        for (index, &result) in instance.results().iter().enumerate() {
            let mut fact = Fact::Unknown;
            for &region in &yielding {
                fact = match conditional.region_yields(region).get(index) {
                    Some(&value) => fact.join(&facts.get(Node::Value(value))),
                    None => Fact::Overdefined,
                };
            }
            facts.raise(Node::Value(result), fact);
        }
    }

    /// θ: a carried port holds what the loop was entered with joined with what
    /// every edge back into it carries, and the loop's result is that port where
    /// the loop was left.
    fn visit_theta(&self, instance: &OpHandle, loop_like: &dyn LoopLike, facts: &mut State) {
        let inits = loop_like.inits();
        let ports = inits.len();
        let mut carried: Vec<Fact> = inits
            .iter()
            .map(|&init| facts.get(Node::Value(init)))
            .collect();
        for edge in self.back_edges(instance) {
            let values = carried_operands(&self.context.get_op(edge));
            if values.len() != ports {
                continue;
            }
            for (port, &value) in carried.iter_mut().zip(values.iter()) {
                *port = port.join(&facts.get(Node::Value(value)));
            }
        }

        // A loop that tests the carried values before each iteration reads them
        // in its test region and forwards what the test computed into the body,
        // which is also what it leaves the loop with.
        let tested = tested_ports(self.context, instance, ports);
        let heads = match &tested {
            Some((_, arguments, _)) => arguments.clone(),
            None => loop_like.carried_args(),
        };
        let mut bindings: HashMap<ValueId, Fact> =
            heads.iter().copied().zip(carried.iter().cloned()).collect();
        let mut finals = carried;
        if let Some((_, _, forwarded)) = &tested {
            let body_args = loop_like.carried_args();
            for (index, &value) in forwarded.iter().enumerate() {
                let fact = facts.get(Node::Value(value));
                if let Some(&argument) = body_args.get(index) {
                    bindings.insert(argument, fact.clone());
                }
                finals[index] = finals[index].join(&fact);
            }
        }

        for region in instance.regions() {
            self.enter_region(region, &bindings, facts);
        }
        for (&result, fact) in loop_like.finals().iter().zip(finals) {
            facts.raise(Node::Value(result), fact);
        }
    }

    /// Reach `region`'s entry block, binding its arguments to what the op holding
    /// the region feeds them; an argument nothing names is overdefined.
    fn enter_region(&self, region: RegionId, bindings: &HashMap<ValueId, Fact>, facts: &mut State) {
        let Some(entry) = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .next()
        else {
            return;
        };
        facts.raise(Node::Block(entry.id()), Fact::Reached);
        for argument in entry.arguments() {
            let fact = bindings
                .get(&argument.id())
                .cloned()
                .unwrap_or(Fact::Overdefined);
            facts.raise(Node::Value(argument.id()), fact);
        }
    }

    /// An arm's entry arguments are the inputs the gate forwards into it: the
    /// operation's trailing operands, one per argument.
    fn arm_bindings(
        &self,
        instance: &OpHandle,
        region: RegionId,
        facts: &State,
    ) -> HashMap<ValueId, Fact> {
        let Some(entry) = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .next()
        else {
            return HashMap::new();
        };
        let arguments = entry.arguments();
        let operands = instance.operands();
        let first = operands.len().saturating_sub(arguments.len());
        arguments
            .iter()
            .zip(&operands[first..])
            .map(|(argument, &input)| (argument.id(), facts.get(Node::Value(input))))
            .collect()
    }

    fn is_reached(&self, region: RegionId, facts: &State) -> bool {
        self.context
            .get_region(region)
            .iter(self.context.clone())
            .next()
            .is_some_and(|entry| facts.get(Node::Block(entry.id())) == Fact::Reached)
    }

    /// Every edge back into a loop's carried ports: the body's own latch and the
    /// `break`/`continue` exits leaving its scope.
    fn back_edges(&self, instance: &OpHandle) -> Vec<OpId> {
        if let Some(edges) = self.back_edges.borrow().get(&instance.id) {
            return edges.clone();
        }
        let bodies = match instance.clone().as_interface::<dyn TokenScope>() {
            Some(scope) => scope.token_scope_regions(),
            None => instance.regions().last().copied().into_iter().collect(),
        };
        let edges: Vec<OpId> = bodies
            .into_iter()
            .flat_map(|body| port_edges(self.context, body))
            .collect();
        self.back_edges
            .borrow_mut()
            .insert(instance.id, edges.clone());
        edges
    }

    /// What decides a branch or a gate in `block`: the value's own fact, or the
    /// boolean a dominating guarded edge pins it to.
    fn decision(&self, value: ValueId, block: BlockId, facts: &State) -> Fact {
        let fact = facts.get(Node::Value(value));
        if matches!(fact, Fact::Const(_)) {
            return fact;
        }
        match self
            .edges
            .facts(block)
            .iter()
            .find(|edge| edge.condition == value)
        {
            Some(edge) => Fact::Const(APInt::new(1, edge.holds as u64)),
            None => fact,
        }
    }
}

/// Whether the arm of `case` runs under `decision`. The arm without a case value
/// is the one that runs when no other case matches.
fn arm_runs(decision: &Fact, case: Option<i64>, cases: &[(RegionId, Option<i64>)]) -> bool {
    let Fact::Const(value) = decision else {
        return *decision != Fact::Unknown;
    };
    let matches = |case: i64| APInt::new(value.width(), case as u64).to_u64() == value.to_u64();
    match case {
        Some(case) => matches(case),
        None => !cases.iter().any(|&(_, case)| case.is_some_and(matches)),
    }
}

fn is_integer(context: &Context, value: ValueId) -> bool {
    let ty = context.get_type_data(context.get_value(value).ty());
    (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::builtin::IntegerType>()
        .is_some()
}
