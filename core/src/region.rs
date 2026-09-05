use crate::{
    BlockId, Context, ContextIterator, GetFromContext, OpId, Terminator, context::ContextRef,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RegionId(u32);

/// A region's storage record, living densely in the context's region slab and
/// edited in place through [`Context`] under its write lock. Reads go through
/// [`RegionHandle`].
#[derive(Debug)]
pub struct Region {
    blocks: Vec<BlockId>,
    parent_op: OpId,
}

impl Region {
    pub(crate) fn new() -> Region {
        Region {
            blocks: vec![],
            parent_op: OpId::invalid(),
        }
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.blocks.capacity() * std::mem::size_of::<BlockId>()
    }

    pub(crate) fn set_parent_op(&mut self, op: OpId) {
        self.parent_op = op;
    }

    /// The operation owning this region, if it has been attached to one.
    pub(crate) fn parent_op(&self) -> Option<OpId> {
        (self.parent_op != OpId::invalid()).then_some(self.parent_op)
    }

    pub(crate) fn blocks(&self) -> &[BlockId] {
        &self.blocks
    }

    pub(crate) fn blocks_mut(&mut self) -> &mut Vec<BlockId> {
        &mut self.blocks
    }
}

/// A reference to a region: the context that owns it, and its id. Reads answer
/// with the region as it stands now; see [`crate::OpHandle`].
#[derive(Clone)]
pub struct RegionHandle {
    pub context: ContextRef,
    pub(crate) generation: u32,
    pub id: RegionId,
}

impl std::fmt::Debug for RegionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RegionHandle").field(&self.id).finish()
    }
}

impl RegionHandle {
    /// The owning context, after checking this handle still names its own region.
    fn context(&self) -> Context {
        let context = self.context.upgrade();
        #[cfg(debug_assertions)]
        context.assert_region_generation(self.id, self.generation);
        context
    }

    /// Whether this handle still names the region it was minted for; see
    /// [`crate::OpHandle::is_live`].
    pub fn is_live(&self) -> bool {
        self.context.upgrade().region_generation(self.id) == self.generation
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    /// The operation owning this region, if it has been attached to one.
    pub fn parent_op(&self) -> Option<OpId> {
        self.context().region_parent_op(self.id)
    }

    pub fn add_block(&self, id: BlockId) {
        self.context().add_block_to_region(self.id, id);
    }

    pub fn remove_block(&self, id: BlockId) -> bool {
        self.context().remove_block_from_region(self.id, id)
    }

    pub fn block_ids(&self) -> Vec<BlockId> {
        self.context().region_block_ids(self.id)
    }

    /// Replace the whole block list at once. Only [`Context::replace_region_contents`]
    /// uses this: it owns the parent bookkeeping and the single version bump the
    /// swap is allowed to make, which the per-block mutators above would each repeat.
    pub(crate) fn set_blocks(&self, blocks: Vec<BlockId>) {
        self.context().set_region_blocks(self.id, blocks);
    }

    pub fn iter(&self, context: Context) -> ContextIterator<BlockId> {
        ContextIterator::new(context, self.block_ids())
    }

    pub fn verify(&self, context: &Context) -> Result<(), crate::Error> {
        for block_id in self.block_ids() {
            let block = context.get_block(block_id);
            let ops = block.op_ids();
            if ops.is_empty() {
                return Err(crate::Error::VerificationError(
                    "basic blocks must have at least one operation".to_string(),
                ));
            }

            let op = ops.last().unwrap().get_from_context(context);
            if op.as_interface::<dyn Terminator>().is_none() {
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

    /// The hive handle backing this id.
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

impl GetFromContext for RegionId {
    type Item = RegionHandle;

    fn get_from_context(&self, context: &Context) -> Self::Item {
        context.get_region(*self)
    }
}
