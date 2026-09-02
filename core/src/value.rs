use crate::OpId;
use crate::TypeId;

pub use tir_symbolic::sem::ValueId;

#[derive(Debug, Clone)]
pub struct Value {
    id: ValueId,
    ty: TypeId,
    defining_op: Option<OpId>,
}

impl Value {
    pub fn new(id: ValueId, ty: TypeId, defining_op: Option<OpId>) -> Self {
        Self {
            id,
            ty,
            defining_op,
        }
    }

    pub fn id(&self) -> ValueId {
        self.id
    }

    pub fn ty(&self) -> TypeId {
        self.ty
    }

    pub fn defining_op(&self) -> Option<OpId> {
        self.defining_op
    }

    pub(crate) fn set_defining_op(&mut self, op: OpId) {
        self.defining_op = Some(op);
    }

    pub(crate) fn set_ty(&mut self, ty: TypeId) {
        self.ty = ty;
    }
}
