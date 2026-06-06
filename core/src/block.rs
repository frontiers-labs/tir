use std::sync::Arc;

use parking_lot::RwLock;

use crate::{Context, ContextIterator, GetFromContext, OpId, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(u32);

#[derive(Debug)]
pub struct Block {
    id: BlockId,
    arguments: Vec<Value>,
    operations: RwLock<Vec<OpId>>,
    successors: RwLock<Vec<BlockId>>,
    predecessors: RwLock<Vec<BlockId>>,
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
}

impl Block {
    pub(crate) fn new(id: BlockId, arguments: Vec<Value>) -> Self {
        Self {
            id,
            arguments,
            operations: RwLock::new(vec![]),
            successors: RwLock::new(vec![]),
            predecessors: RwLock::new(vec![]),
        }
    }

    pub fn id(&self) -> BlockId {
        self.id
    }

    pub fn arguments(&self) -> &[Value] {
        &self.arguments
    }

    pub fn len(&self) -> usize {
        self.operations.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.operations.read().is_empty()
    }

    pub fn successors(&self) -> Vec<BlockId> {
        self.successors.read().clone()
    }

    pub fn predecessors(&self) -> Vec<BlockId> {
        self.predecessors.read().clone()
    }

    pub(crate) fn insert(&self, index: usize, id: OpId) {
        self.operations.write().insert(index, id);
    }

    pub fn op_ids(&self) -> Vec<OpId> {
        self.operations.read().clone()
    }

    pub fn replace_op(&self, old: OpId, new: OpId) -> bool {
        let mut ops = self.operations.write();
        if let Some(position) = ops.iter().position(|id| *id == old) {
            ops[position] = new;
            true
        } else {
            false
        }
    }

    pub fn remove_op(&self, id: OpId) -> bool {
        let mut ops = self.operations.write();
        if let Some(position) = ops.iter().position(|op_id| *op_id == id) {
            ops.remove(position);
            true
        } else {
            false
        }
    }

    pub fn iter(&self, context: Context) -> ContextIterator<OpId> {
        ContextIterator::new(context, self.operations.read().clone())
    }
}

impl GetFromContext for BlockId {
    type Item = Arc<Block>;

    fn get_from_context(&self, context: &crate::Context) -> Self::Item {
        context.get_block(*self)
    }
}
