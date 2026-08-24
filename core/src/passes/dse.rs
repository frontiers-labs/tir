//! Dead store elimination: a write nothing can read back before another
//! overwrites it never happened.
//!
//! The reasoning is [`AliasFacts`]: two writes kill each other when they start
//! at the same address of the same object and the later one covers at least the
//! extent of the earlier. Between them nothing may read that extent, and nothing
//! whose effect on memory the facts do not model may run at all — a call, an
//! operation holding regions the scan does not enter. The facts are
//! flow-insensitive and say nothing about what a call does, so that barrier rule
//! carries the whole soundness argument.
//!
//! The scan is block-local: crossing a block boundary needs a post-dominance
//! view over regions, which no consumer asks for yet.

use crate::analysis::{AliasFacts, AliasResult};
use crate::func::FuncOp;
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
        let facts = analyses.get::<AliasFacts>(context, root);
        let layout = DataLayout::for_op(context, root);
        let scan = Scan {
            context,
            facts: &facts,
            layout: layout.as_ref(),
        };

        let mut dead = Vec::new();
        for region in super::regions_under(context, root) {
            for block in context.get_region(region).block_ids() {
                let op_ids = context.get_block(block).op_ids();
                for index in 0..op_ids.len() {
                    if scan.overwritten(&op_ids, index) {
                        dead.push(op_ids[index]);
                    }
                }
            }
        }

        for op_id in dead {
            let instance = context.get_op(op_id);
            // A write still on a threaded chain publishes the state its readers
            // name; erasing it hands them the state it observed instead.
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

struct Scan<'a> {
    context: &'a Context,
    facts: &'a AliasFacts,
    layout: Option<&'a DataLayout>,
}

impl Scan<'_> {
    /// Whether a later write in the block covers everything the operation at
    /// `index` wrote, with nothing in between able to observe it.
    fn overwritten(&self, op_ids: &[OpId], index: usize) -> bool {
        let instance = self.context.get_op(op_ids[index]);
        let Some(store) = self.write(&instance) else {
            return false;
        };
        let Some(extent) = store.extent else {
            return false;
        };
        for &later in &op_ids[index + 1..] {
            let instance = self.context.get_op(later);
            // Both interfaces are asked: an operation declaring the two reads
            // the extent it also writes.
            let read = instance.clone().as_interface::<dyn MemoryRead>();
            let write = self.write(&instance);
            if let Some(read) = &read
                && self
                    .facts
                    .alias(store.location, Some(extent), read.read_location(), None)
                    != AliasResult::NoAlias
            {
                return false;
            }
            if let Some(other) = &write {
                let alias =
                    self.facts
                        .alias(store.location, Some(extent), other.location, other.extent);
                if alias == AliasResult::MustAlias
                    && other.extent.is_some_and(|covered| covered >= extent)
                {
                    return true;
                }
                if alias != AliasResult::NoAlias {
                    return false;
                }
            }
            // An op touching no memory it declares still may through a region the
            // scan does not enter, or through effects nothing here models.
            if read.is_none()
                && write.is_none()
                && (!instance.regions().is_empty() || !super::is_pure_value(&instance))
            {
                return false;
            }
        }
        false
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
