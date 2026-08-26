//! Dead store elimination: a write nothing can read back before another
//! overwrites it never happened.
//!
//! The reasoning is the state chain. A write publishes the memory it left, so
//! whatever names that state is what may tell the write happened, and the walk is
//! forward along those edges: a write covering at least the extent leaves nothing
//! of it, an access the facts place elsewhere in the same memory is walked past,
//! and anything else — an access that may be of those bytes, a call, a port
//! carrying the chain out of a region, the export handing it to the caller — can
//! observe it.
//!
//! Nothing here is a barrier rule over the order operations are written in: two
//! objects the facts tell apart are two chains, so the walk never meets the other
//! object at all, and an operation whose effect on memory nothing models names a
//! chain the walk stops at. [`AliasFacts`] is asked only what the addresses of two
//! accesses are.

use std::collections::HashSet;

use crate::analysis::{AliasFacts, AliasResult, DefUse};
use crate::func::FuncOp;
use crate::state::JoinOp;
use crate::{
    AnalysisManager, Context, DataLayout, MemoryRead, MemoryWrite, OpHandle, OpId, OperationRef,
    Pass, PassError, PassTarget, Rewriter, ValueId,
};

#[derive(Default)]
pub struct DeadStoreEliminationPass;

impl DeadStoreEliminationPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(DeadStoreEliminationPass, "dse");

impl Pass for DeadStoreEliminationPass {
    fn name(&self) -> &'static str {
        "dse"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }
        let root = op.op().id;
        let layout = DataLayout::for_op(context, root);
        let walk = Walk {
            context,
            facts: analyses.get::<AliasFacts>(context, root),
            uses: analyses.get::<DefUse>(context, root),
            layout: layout.as_ref(),
        };

        let mut dead = Vec::new();
        for &op_id in walk.uses.ops() {
            if walk.overwritten(op_id) {
                dead.push(op_id);
            }
        }

        for op_id in dead {
            let instance = context.get_op(op_id);
            // The write published the state its readers name; erasing it hands
            // them the state it observed instead.
            if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>()
                && let (Some(published), Some(observed)) =
                    (write.state_result(), write.state_operand())
            {
                context.replace_value_uses(published, observed);
            }
            let block = context.parent_block(op_id).map(|id| context.get_block(id));
            rewriter.erase_op(&OperationRef::new(instance, block, None))?;
        }
        Ok(())
    }
}

/// One write, as far as the alias facts can spell it.
struct Write {
    location: ValueId,
    /// The bytes it covers, absent where the IR spells an extent the written
    /// value's type does not describe — a `memset`'s size operand.
    extent: Option<u64>,
}

/// What one operation naming a state does to the write that published it.
enum Step {
    /// Leaves nothing of the write to read back.
    Overwrites,
    /// Cannot be of those bytes, so the walk carries on down the chain.
    Elsewhere(ValueId),
    /// May tell the write happened.
    Observes,
}

struct Walk<'a> {
    context: &'a Context,
    facts: std::rc::Rc<AliasFacts>,
    uses: std::rc::Rc<DefUse>,
    layout: Option<&'a DataLayout>,
}

impl Walk<'_> {
    /// Whether the memory `op` left is overwritten before anything can read it
    /// back. A write off the chain publishes no state, and one whose extent the
    /// IR does not spell covers bytes the walk cannot compare.
    fn overwritten(&self, op: OpId) -> bool {
        let instance = self.context.get_op(op);
        let Some(published) = instance
            .clone()
            .as_interface::<dyn MemoryWrite>()
            .and_then(|write| write.state_result())
        else {
            return false;
        };
        let store = self.write(&instance).expect("the op is a write");
        if store.extent.is_none() {
            return false;
        }

        let mut pending = vec![published];
        let mut seen = HashSet::new();
        while let Some(state) = pending.pop() {
            if !seen.insert(state) {
                continue;
            }
            let naming = self.uses.users_of(state.number());
            // A state nothing names ends the chain the walk was following, which
            // says nothing about what the memory it leaves is read back as.
            if naming.is_empty() {
                return false;
            }
            for &op in naming {
                match self.step(&store, op) {
                    Step::Overwrites => {}
                    Step::Elsewhere(next) => pending.push(next),
                    Step::Observes => return false,
                }
            }
        }
        true
    }

    fn step(&self, store: &Write, op: OpId) -> Step {
        let instance = self.context.get_op(op);
        // A join names the memory after every read of one fork, so what may
        // observe the write is what names the join.
        if instance.is::<JoinOp>() {
            return Step::Elsewhere(instance.results()[0]);
        }
        if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
            let other = self.write(&instance).expect("the op is a write");
            let alias =
                self.facts
                    .alias(store.location, store.extent, other.location, other.extent);
            let covers = other
                .extent
                .is_some_and(|covered| store.extent.is_some_and(|written| covered >= written));
            return match alias {
                AliasResult::MustAlias if covers => Step::Overwrites,
                AliasResult::NoAlias => match write.state_result() {
                    Some(next) => Step::Elsewhere(next),
                    None => Step::Observes,
                },
                _ => Step::Observes,
            };
        }
        if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
            let alias = self
                .facts
                .alias(store.location, store.extent, read.read_location(), None);
            return match (alias, read.state_result()) {
                (AliasResult::NoAlias, Some(next)) => Step::Elsewhere(next),
                _ => Step::Observes,
            };
        }
        Step::Observes
    }

    fn write(&self, instance: &OpHandle) -> Option<Write> {
        let write = instance.clone().as_interface::<dyn MemoryWrite>()?;
        Some(Write {
            location: write.write_location(),
            extent: self.extent(instance, write.as_ref()),
        })
    }

    /// The bytes a write covers: the size of the value it writes, where every
    /// operand it names is that value, the location, or the state it observes.
    /// An operand beyond those — `memset`'s size — spells an extent the written
    /// type does not, and so does a width that is not whole bytes: the pass
    /// counts bytes, and cannot say what of a partly written one survives.
    fn extent(&self, instance: &OpHandle, write: &dyn MemoryWrite) -> Option<u64> {
        let spelled = [
            Some(write.write_location()),
            Some(write.written_value()),
            write.state_operand(),
        ];
        if instance
            .operands()
            .iter()
            .any(|operand| !spelled.contains(&Some(*operand)))
        {
            return None;
        }
        let ty = self.context.get_value(write.written_value()).ty();
        let bits = self.layout?.size_in_bits(self.context, ty)?;
        (bits % 8 == 0).then(|| u64::from(bits / 8))
    }
}
