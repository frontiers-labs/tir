use std::sync::Arc;

use parking_lot::RwLock;

use crate::{Context, ContextIterator, GetFromContext, OpId, Value, context::ContextRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(u32);

#[derive(Debug)]
pub struct Block {
    id: BlockId,
    arguments: Vec<Value>,
    operations: RwLock<Vec<OpId>>,
    successors: RwLock<Vec<BlockId>>,
    predecessors: RwLock<Vec<BlockId>>,
    /// Handle back to the owning context, used to keep its op-to-parent-block index
    /// in step with every membership change below. Never held across a context lock.
    context: ContextRef,
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
}

impl Block {
    pub(crate) fn new(id: BlockId, arguments: Vec<Value>, context: ContextRef) -> Self {
        Self {
            id,
            arguments,
            operations: RwLock::new(vec![]),
            successors: RwLock::new(vec![]),
            predecessors: RwLock::new(vec![]),
            context,
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
        self.context.upgrade().set_op_parent(id, self.id);
    }

    pub fn op_ids(&self) -> Vec<OpId> {
        self.operations.read().clone()
    }

    pub fn replace_op(&self, old: OpId, new: OpId) -> bool {
        let replaced = {
            let mut ops = self.operations.write();
            match ops.iter().position(|id| *id == old) {
                Some(position) => {
                    ops[position] = new;
                    true
                }
                None => false,
            }
        };
        if replaced {
            let context = self.context.upgrade();
            context.clear_op_parent(old);
            context.set_op_parent(new, self.id);
        }
        replaced
    }

    pub fn remove_op(&self, id: OpId) -> bool {
        let removed = {
            let mut ops = self.operations.write();
            match ops.iter().position(|op_id| *op_id == id) {
                Some(position) => {
                    ops.remove(position);
                    true
                }
                None => false,
            }
        };
        if removed {
            self.context.upgrade().clear_op_parent(id);
        }
        removed
    }

    /// Returns true if a comes before b in the block, false otherwise
    pub fn is_before(&self, a: OpId, b: OpId) -> bool {
        let ops = self.operations.read();
        let a_pos = ops.iter().position(|op_id| *op_id == a);
        let b_pos = ops.iter().position(|op_id| *op_id == b);

        if let (Some(a_pos), Some(b_pos)) = (a_pos, b_pos) {
            a_pos < b_pos
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
