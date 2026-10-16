use crate::{
    Context, ContextIterator, GetFromContext, OpId, Value,
    attributes::{AttributeValue, NamedAttribute},
};

id_newtype!(BlockId);

impl BlockId {
    /// A block no context holds: what a branch is bound to before the block it
    /// will reach exists.
    pub const PLACEHOLDER: BlockId = BlockId(u32::MAX);
}

/// A reference to a basic block: the context that owns it, and its id.
///
/// Like [`crate::OpHandle`], reads go to the context's storage as they are asked
/// for, so a handle always answers with the block as it stands now. A handle to
/// an erased block reads as a panic, never as some other block: ids are never
/// reused.
#[derive(Clone)]
pub struct BlockHandle {
    pub context: Context,
    pub id: BlockId,
}

impl std::fmt::Debug for BlockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BlockHandle").field(&self.id).finish()
    }
}

impl BlockHandle {
    fn context(&self) -> Context {
        let context = self.context.clone();
        debug_assert!(
            context.has_block(self.id),
            "handle to erased block {:?}",
            self.id
        );
        context
    }

    /// Whether this handle still names the block it was minted for; see
    /// [`crate::OpHandle::is_live`].
    pub fn is_live(&self) -> bool {
        self.context.has_block(self.id)
    }

    pub fn id(&self) -> BlockId {
        self.id
    }

    pub fn arguments(&self) -> Vec<Value> {
        self.context().block_arguments(self.id)
    }

    /// The arguments that are not resource states.
    pub fn value_arguments(&self) -> Vec<Value> {
        let context = self.context();
        self.arguments()
            .into_iter()
            .filter(|argument| !context.is_state_type(argument.ty()))
            .collect()
    }

    /// The resource states that carry execution dependencies into the block.
    pub fn state_arguments(&self) -> Vec<Value> {
        let context = self.context();
        self.arguments()
            .into_iter()
            .filter(|argument| context.is_state_type(argument.ty()))
            .collect()
    }

    pub fn attributes(&self) -> Vec<NamedAttribute> {
        self.context().block_attributes(self.id)
    }

    /// The value of the attribute called `name`, resolving the name through the
    /// owning context's interner.
    pub fn attr(&self, name: &str) -> Option<AttributeValue> {
        self.context().block_attr(self.id, name)
    }

    /// Set (or replace) a named attribute on this block.
    pub fn set_attr(&self, name: &str, value: AttributeValue) {
        self.context().set_block_attr(self.id, name, value);
    }

    pub fn len(&self) -> usize {
        self.op_ids().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn op_ids(&self) -> Vec<OpId> {
        self.context().block_ops(self.id)
    }

    pub fn insert(&self, index: usize, id: OpId) {
        self.context().insert_op(self.id, index, id);
    }

    /// Add `id` after every operation the block currently holds.
    pub fn append(&self, id: OpId) {
        self.context().append_op(self.id, id);
    }

    /// Append `op` and hand it back, for building a block in sequence.
    pub fn append_op<T: crate::Operation>(&self, op: T) -> T {
        self.append(op.id());
        op
    }

    pub fn replace_op(&self, old: OpId, new: OpId) -> bool {
        self.context().replace_op_in_block(self.id, old, new)
    }

    /// Choose another order for the operations the block already holds; `ops`
    /// must be a permutation of [`op_ids`](Self::op_ids). Order inside a machine
    /// block is a linearization of its dependence graph, and this is how one is
    /// installed.
    pub fn set_ops(&self, ops: Vec<OpId>) {
        self.context().set_block_ops(self.id, ops);
    }

    pub fn remove_op(&self, id: OpId) -> bool {
        self.context().remove_op_from_block(self.id, id)
    }

    pub fn iter(&self, context: Context) -> ContextIterator<OpId> {
        ContextIterator::new(context, self.op_ids())
    }
}

impl GetFromContext for BlockId {
    type Item = BlockHandle;

    fn get_from_context(&self, context: &crate::Context) -> Self::Item {
        context.get_block(*self)
    }
}
