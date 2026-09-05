use crate::OpId;
use crate::TypeId;

pub use tir_symbolic::sem::ValueId;

/// An SSA value: its identity, its type, and where it is defined.
///
/// Twelve bytes, so a hive chunk holds several thousand. The def site is an
/// [`OpId`] rather than an `Option<OpId>`: a value no operation defines is a
/// block or region argument, which [`OpId::ARGUMENT`] says in the same four
/// bytes the id would need anyway.
#[derive(Debug, Clone)]
pub struct Value {
    id: ValueId,
    ty: TypeId,
    defining_op: OpId,
}

impl Value {
    pub fn new(id: ValueId, ty: TypeId, defining_op: Option<OpId>) -> Self {
        Self {
            id,
            ty,
            defining_op: defining_op.unwrap_or(OpId::ARGUMENT),
        }
    }

    pub fn id(&self) -> ValueId {
        self.id
    }

    pub fn ty(&self) -> TypeId {
        self.ty
    }

    /// The operation defining this value, or `None` for a block or region
    /// argument.
    pub fn defining_op(&self) -> Option<OpId> {
        (self.defining_op != OpId::ARGUMENT).then_some(self.defining_op)
    }

    pub(crate) fn set_defining_op(&mut self, op: OpId) {
        self.defining_op = op;
    }

    /// Drop the def-site: the value becomes a block or region argument.
    pub(crate) fn clear_defining_op(&mut self) {
        self.defining_op = OpId::ARGUMENT;
    }

    pub(crate) fn set_ty(&mut self, ty: TypeId) {
        self.ty = ty;
    }
}

/// One operand slot naming a value: the reading operation and the slot's index
/// among its operands. An op reading a value twice owns two uses of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    pub op: OpId,
    pub index: usize,
}

impl Use {
    pub fn new(op: OpId, index: usize) -> Self {
        Self { op, index }
    }
}
