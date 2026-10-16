//! The simplifier over unordered regions: the shared driver's e-graph, seeded
//! with the declared loops and gates, committed without an order to place
//! into; see [`crate::rewrite`] for how a value's readers are rewired.
//!
//! The commit erases nothing on the way. The sweep after it erases every
//! operation the region's results do not demand. Demand runs through
//! dependencies as through values, so a loop whose body changes memory is
//! demanded by the state it leaves however unread its values are; a loop in
//! no cone at all never ran under the demand semantics, and goes.

use crate::{IdMap, IdSet};

use tir_egraph::{ClassId as Id, Engine, Ref, Vocabulary};

use super::rules::{Shared, builtin_ruleset};
use crate::analysis::AnalysisManager;
use crate::analysis::effects::{observed_state, produced_state};
use crate::binding::{StateChain, forwards_state, state_chains};
use crate::func::FuncOp;
use crate::rewrite::{Driver, Kinds, Rewrite, RuleSet, is_dead_read, published, seed};
use crate::sem::SemNode as Node;
use crate::sem::node::cost;
use crate::{
    Context, MemoryRead, MemoryWrite, OpId, OperationRef, Pass, PassError, PassTarget,
    PromotableAllocation, RegionId, RegionKind, Theta, ValueId,
};

#[derive(Clone, Default)]
pub struct InstCombineNodesPass {
    shared: Shared,
    vocabulary: Option<Vocabulary<Node>>,
    kinds: Kinds,
    trusted: bool,
}

impl InstCombineNodesPass {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pass applying the rules stated `proof trusted` too, which no
    /// solver checked: a pipeline opts into them, as `instcombine-nodes<trusted>`.
    pub fn trusting() -> Self {
        Self {
            trusted: true,
            ..Self::default()
        }
    }

    fn parse(args: &str) -> Result<Self, String> {
        match args {
            "" => Ok(Self::new()),
            "trusted" => Ok(Self::trusting()),
            other => Err(format!("instcombine-nodes takes `trusted`, not `{other}`")),
        }
    }
}

crate::register_pass!(
    InstCombineNodesPass,
    "instcombine-nodes",
    InstCombineNodesPass::parse
);

impl Pass for InstCombineNodesPass {
    fn name(&self) -> &'static str {
        "instcombine-nodes"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn settles(&self) -> bool {
        true
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let root = op.op().id;
        let vocabulary = self.vocabulary.take();
        let vocabulary = vocabulary.unwrap_or_else(|| Vocabulary::new(context.names()));
        let mut seeded = seed(context, root, vocabulary, &mut self.kinds, false);
        let loop_ports = std::mem::take(&mut seeded.loop_ports);
        let rewrite = InstCombine {
            ruleset: builtin_ruleset(context, &seeded, &mut self.shared, self.trusted),
        };
        let mut driver = Driver::new(context, seeded, &rewrite, self.name());
        driver.saturate();
        driver.hypothesize(loop_ports);
        driver.index_values();
        let extraction = driver.extract();
        let body = context.get_op(root).regions()[0];
        let committed = driver.commit(body, &extraction, &mut Default::default());
        self.vocabulary = Some(driver.eg.release());
        committed?;
        forget_write_only_slots(context, body);
        drop_untouched_chains(context, body);
        let result = sweep(context, body);
        tir_relational::report_saturation("instcombine-nodes");
        result
    }
}

struct InstCombine {
    ruleset: RuleSet,
}

impl Rewrite for InstCombine {
    fn rules(&self) -> &RuleSet {
        &self.ruleset
    }

    fn cost(&self, eg: &Engine<Node>, _: Id, node: &Node, operands: &[Ref], _: u64) -> u64 {
        spelling_cost(eg, node, operands)
    }

    fn arm_facts(&self) -> bool {
        true
    }
}

/// What spelling `node` over `operands` costs: the op, and one add for each
/// operand at an offset that is not a literal.
fn spelling_cost(eg: &Engine<Node>, node: &Node, operands: &[Ref]) -> u64 {
    cost(node)
        + operands
            .iter()
            .filter(|operand| operand.offset != 0 && eg.int_const(**operand).is_none())
            .count() as u64
}

/// A loop or a gate carrying a chain its body never names carries nothing: the
/// port hands back the memory the operation was entered on, so it goes with the
/// chain. What leaves such a port behind is promotion, whose slot values move
/// onto the value ports and leave the chain empty.
fn drop_untouched_chains(context: &Context, region: RegionId) {
    for op in context.get_region(region).op_ids() {
        for sub in context.get_op(op).regions() {
            drop_untouched_chains(context, sub);
        }
        let instance = context.get_op(op);
        // A split names one memory per chain crossing it, so where one chain is
        // left it names that memory and nothing else: its reader takes the
        // state the split was handed.
        if instance.is::<crate::state::SplitOp>() {
            let read: Vec<ValueId> = instance
                .state_results()
                .into_iter()
                .filter(|&state| {
                    !context.users_of(state).is_empty() || published(context, region, state)
                })
                .collect();
            if let [kept] = read[..] {
                let taken = instance.state_operands()[0];
                context.replace_value_uses(kept, taken);
                context.rename_region_results(region, kept, taken, &[]);
            }
            continue;
        }
        // Highest chain first, so the ports that stay keep their positions.
        for ordinal in (0..state_chains(context, &instance).len()).rev() {
            let Some(chain) = carries_nothing(context, op, ordinal) else {
                continue;
            };
            context.replace_value_uses(chain.left, chain.entered);
            context.rename_region_results(region, chain.left, chain.entered, &[]);
            context.drop_state(op, ordinal);
        }
    }
}

/// The `ordinal`-th chain `op` carries, if nothing under the op names it:
/// every region hands the port straight back, and nothing else reads it.
fn carries_nothing(context: &Context, op: OpId, ordinal: usize) -> Option<StateChain> {
    let handle = context.get_op(op);
    let chain = state_chains(context, &handle).swap_remove(ordinal);
    if !forwards_state(&chain) {
        return None;
    }
    let groups = if handle.has_interface::<dyn Theta>() {
        2
    } else {
        1
    };
    let unnamed = handle
        .regions()
        .iter()
        .zip(&chain.ports)
        .all(|(&region, &port)| {
            let named = context
                .result_uses(port)
                .into_iter()
                .filter(|&(naming, _)| context.region_under(naming, region))
                .count();
            named == groups && context.users_of(port).is_empty()
        });
    unnamed.then_some(chain)
}

/// A slot whose address reaches only writes is a memory nothing observes: each
/// write leaves the state it was handed, and the sweep takes the writes and
/// the allocation with nothing left demanding them.
fn forget_write_only_slots(context: &Context, body: RegionId) {
    let mut renames: IdMap<ValueId, ValueId> = IdMap::new();
    for op in context.get_region(body).op_ids() {
        let instance = context.get_op(op);
        let Some(allocation) = instance.clone().as_interface::<dyn PromotableAllocation>() else {
            continue;
        };
        let mut writes = Vec::new();
        if !only_written(context, body, allocation.promoted_location(), &mut writes) {
            continue;
        }
        for write in writes {
            let instance = context.get_op(write);
            if !instance.has_interface::<dyn MemoryWrite>() {
                continue;
            }
            let (Some(taken), Some(published)) =
                (observed_state(&instance), produced_state(&instance))
            else {
                continue;
            };
            context.replace_value_uses(published, taken);
            renames.insert(published, taken);
        }
    }
    context.rename_region_results_batch(body, &renames);
}

/// Whether every use of `address`, through pointer arithmetic, is as the
/// location of a write or of a read nothing demands; the writes are collected.
fn only_written(
    context: &Context,
    scope: RegionId,
    address: ValueId,
    writes: &mut Vec<OpId>,
) -> bool {
    context.users_of(address).into_iter().all(|user| {
        if is_dead_read(context, scope, user) {
            return true;
        }
        let instance = context.get_op(user);
        if instance.is::<crate::ptr::PtrAddOp>() {
            return instance
                .results()
                .iter()
                .all(|&derived| only_written(context, scope, derived, writes));
        }
        match instance.clone().as_interface::<dyn MemoryWrite>() {
            // The address is the write's location and nothing else: a write
            // storing the address itself lets it out.
            Some(write)
                if write.write_location() == address
                    && !instance.has_interface::<dyn MemoryRead>()
                    && instance
                        .operands()
                        .iter()
                        .filter(|&&v| v == address)
                        .count()
                        == 1 =>
            {
                writes.push(user);
                true
            }
            _ => false,
        }
    }) && !published(context, scope, address)
}

/// Erase every operation under `body` its region's results do not demand.
/// Demand runs from the callable's results through operands and nested
/// regions' results, dependencies included: what an effect leaves behind is
/// what demands it.
pub(crate) fn sweep(context: &Context, body: RegionId) -> Result<(), PassError> {
    let demanded = crate::passes::demanded_ops(context, &[body]);
    sweep_demanded(context, body, &demanded)
}

pub(crate) fn sweep_demanded(
    context: &Context,
    body: RegionId,
    demanded: &IdSet<OpId>,
) -> Result<(), PassError> {
    // An operation is listed ahead of what its regions hold, which goes
    // with it.
    for op in context.subtree_ops(&context.get_region(body).op_ids()) {
        if context.has_operation(op) && !demanded.contains(&op) {
            context.erase_op(&OperationRef::new(context.get_op(op)))?;
        }
    }
    Ok(())
}
