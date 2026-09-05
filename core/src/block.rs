use crate::{
    Context, ContextIterator, GetFromContext, OpId, Value,
    attributes::{AttributeValue, NamedAttribute},
    context::ContextRef,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(u32);

/// A basic block's storage record, living densely in the context's block slab
/// and edited in place through [`Context`] under its write lock. Reads go
/// through [`BlockHandle`]; nothing outside the context lock holds one of these.
#[derive(Debug, Clone)]
pub struct Block {
    /// The block's arguments: values first, then its trailing dependencies.
    arguments: Vec<Value>,
    dep_arguments: u32,
    operations: Vec<OpId>,
    /// Discardable metadata scoped to this block (e.g. `fpmath`), printed in the
    /// block label.
    attributes: Vec<NamedAttribute>,
}

impl BlockId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn number(&self) -> u32 {
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

impl Block {
    pub(crate) fn new(arguments: Vec<Value>) -> Self {
        Self {
            arguments,
            dep_arguments: 0,
            operations: vec![],
            attributes: vec![],
        }
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.arguments.capacity() * std::mem::size_of::<Value>()
            + self.operations.capacity() * std::mem::size_of::<OpId>()
            + self.attributes.capacity() * std::mem::size_of::<NamedAttribute>()
    }

    pub(crate) fn operations(&self) -> &[OpId] {
        &self.operations
    }

    pub(crate) fn arguments(&self) -> &[Value] {
        &self.arguments
    }

    /// How many trailing arguments are dependencies.
    pub(crate) fn dep_argument_count(&self) -> usize {
        self.dep_arguments as usize
    }

    pub(crate) fn set_dep_argument_count(&mut self, count: usize) {
        self.dep_arguments = count as u32;
    }

    pub(crate) fn attributes(&self) -> &[NamedAttribute] {
        &self.attributes
    }

    pub(crate) fn operations_mut(&mut self) -> &mut Vec<OpId> {
        &mut self.operations
    }

    pub(crate) fn arguments_mut(&mut self) -> &mut Vec<Value> {
        &mut self.arguments
    }

    pub(crate) fn attributes_mut(&mut self) -> &mut Vec<NamedAttribute> {
        &mut self.attributes
    }
}

/// A reference to a basic block: the context that owns it, and its id.
///
/// Like [`crate::OpHandle`], reads go to the context's storage as they are asked
/// for, so a handle always answers with the block as it stands now. A handle to
/// an erased block reads as a panic, never as some other block: ids are never
/// reused.
#[derive(Clone)]
pub struct BlockHandle {
    pub context: ContextRef,
    pub(crate) generation: u32,
    pub id: BlockId,
}

impl std::fmt::Debug for BlockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BlockHandle").field(&self.id).finish()
    }
}

impl BlockHandle {
    /// The owning context, after checking this handle still names its own block.
    fn context(&self) -> Context {
        let context = self.context.upgrade();
        #[cfg(debug_assertions)]
        context.assert_block_generation(self.id, self.generation);
        context
    }

    /// Whether this handle still names the block it was minted for; see
    /// [`crate::OpHandle::is_live`].
    pub fn is_live(&self) -> bool {
        self.context.upgrade().block_generation(self.id) == self.generation
    }

    pub fn id(&self) -> BlockId {
        self.id
    }

    /// Every argument: the values, then the trailing dependencies.
    pub fn arguments(&self) -> Vec<Value> {
        self.context()
            .with_block(self.id, |block| block.arguments().to_vec())
    }

    /// The arguments that carry a value.
    pub fn value_arguments(&self) -> Vec<Value> {
        self.context().with_block(self.id, |block| {
            let values = block.arguments().len() - block.dep_argument_count();
            block.arguments()[..values].to_vec()
        })
    }

    /// The arguments that are dependencies: the chains the block is entered on.
    pub fn dep_arguments(&self) -> Vec<Value> {
        self.context().with_block(self.id, |block| {
            let values = block.arguments().len() - block.dep_argument_count();
            block.arguments()[values..].to_vec()
        })
    }

    pub fn attributes(&self) -> Vec<NamedAttribute> {
        self.context()
            .with_block(self.id, |block| block.attributes().to_vec())
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
        self.context()
            .with_block(self.id, |block| block.operations().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn op_ids(&self) -> Vec<OpId> {
        self.context()
            .with_block(self.id, |block| block.operations().to_vec())
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

    /// Returns true if a comes before b in the block, false otherwise
    pub fn is_before(&self, a: OpId, b: OpId) -> bool {
        self.context().with_block(self.id, |block| {
            let operations = block.operations();
            let a_pos = operations.iter().position(|op_id| *op_id == a);
            let b_pos = operations.iter().position(|op_id| *op_id == b);
            match (a_pos, b_pos) {
                (Some(a_pos), Some(b_pos)) => a_pos < b_pos,
                _ => false,
            }
        })
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
