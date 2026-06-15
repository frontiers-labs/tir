use crate::OpId;
use crate::TypeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(u32);

impl ValueId {
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

#[derive(Debug, Clone)]
pub struct Value {
    id: ValueId,
    ty: TypeId,
    defining_op: Option<OpId>,
    uses: Vec<Use>,
}

impl Value {
    pub fn new(id: ValueId, ty: TypeId, defining_op: Option<OpId>) -> Self {
        Self {
            id,
            ty,
            defining_op,
            uses: vec![],
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

    pub fn with_defining_op(mut self, op: OpId) -> Self {
        self.set_defining_op(op);
        self
    }

    pub(crate) fn set_defining_op(&mut self, op: OpId) {
        self.defining_op = Some(op);
    }

    /// The operations that reference this value, with where the reference sits (see
    /// [`UseSite`]).
    ///
    /// Maintained by the [`Context`](crate::Context): an entry is added when an
    /// operation is added to the context and removed when it is erased or replaced.
    /// Both SSA `operands` and machine-IR register operands carried in attributes
    /// (`RegisterAttr::Virtual` tagged `Use`/`ReadWrite`) are tracked; physical
    /// registers have no value id and so never appear here.
    pub fn uses(&self) -> &[Use] {
        &self.uses
    }

    /// Whether any operation references this value. See [`Value::uses`].
    pub fn is_used(&self) -> bool {
        !self.uses.is_empty()
    }

    pub(crate) fn add_use(&mut self, op: OpId, site: UseSite) {
        self.uses.push(Use { op, site });
    }

    /// Drop every use contributed by `op` (an op may use a value at several sites).
    pub(crate) fn remove_uses_of(&mut self, op: OpId) {
        self.uses.retain(|u| u.op != op);
    }

    pub(crate) fn remove_use(&mut self, op: OpId, site: UseSite) {
        if let Some(index) = self.uses.iter().position(|u| u.op == op && u.site == site) {
            self.uses.remove(index);
        }
    }
}

/// Where a [`Use`] sits within the referencing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseSite {
    /// The operand at this index in the op's SSA `operands`.
    Operand(usize),
    /// A register operand carried in this named attribute (machine ops).
    Attribute(&'static str),
}

/// A reference to a value from an operation: the using `op` and the [`UseSite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Use {
    op: OpId,
    site: UseSite,
}

impl Use {
    pub fn op(&self) -> OpId {
        self.op
    }

    pub fn site(&self) -> UseSite {
        self.site
    }

    /// The operand index, if this use is an SSA operand (not a register attribute).
    pub fn operand_index(&self) -> Option<usize> {
        match self.site {
            UseSite::Operand(i) => Some(i),
            UseSite::Attribute(_) => None,
        }
    }
}
