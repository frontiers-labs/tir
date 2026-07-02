use std::sync::Arc;

use parking_lot::RwLock;

use crate::{
    BlockId, Context, ContextIterator, GetFromContext, OpId, Terminator, context::ContextRef,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(u32);

#[derive(Debug)]
pub struct Region {
    id: RegionId,
    blocks: RwLock<Vec<BlockId>>,
    parent_op: RwLock<OpId>,
    /// Handle back to the owning context, used to keep its block-to-parent-region
    /// index in step with `add_block`. Never held across a context lock.
    context: ContextRef,
}

impl Region {
    pub fn id(&self) -> RegionId {
        self.id
    }

    pub(crate) fn new(id: RegionId, context: ContextRef) -> Region {
        Region {
            id,
            blocks: RwLock::new(vec![]),
            parent_op: RwLock::new(OpId::invalid()),
            context,
        }
    }

    pub(crate) fn set_parent_op(&self, op: OpId) {
        *self.parent_op.write() = op;
    }

    /// The operation owning this region, if it has been attached to one.
    pub fn parent_op(&self) -> Option<OpId> {
        let op = *self.parent_op.read();
        (op != OpId::invalid()).then_some(op)
    }

    pub fn add_block(&self, id: BlockId) {
        self.blocks.write().push(id);
        self.context.upgrade().set_block_parent(id, self.id);
    }

    pub fn iter(&self, context: Context) -> ContextIterator<BlockId> {
        ContextIterator::new(context, self.blocks.read().clone())
    }

    pub fn verify(&self, context: &Context) -> Result<(), crate::Error> {
        let blocks = self.blocks.read();

        for block_id in &*blocks {
            let block = context.get_block(*block_id);
            if block.op_ids().is_empty() {
                return Err(crate::Error::VerificationError(
                    "basic blocks must have at least one operation".to_string(),
                ));
            }

            let last_op = *block.op_ids().last().unwrap();

            let op = last_op.get_from_context(context);
            let terminator = op.as_interface::<dyn Terminator>();
            if terminator.is_none() {
                return Err(crate::Error::VerificationError(
                    "basic blocks must end with a terminator".to_string(),
                ));
            }
        }

        Ok(())
    }
}

impl RegionId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn number(self) -> u32 {
        self.0
    }

    pub fn from_number(n: u32) -> Self {
        Self(n)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

impl GetFromContext for RegionId {
    type Item = Arc<Region>;

    fn get_from_context(&self, context: &Context) -> Self::Item {
        context.get_region(*self)
    }
}
