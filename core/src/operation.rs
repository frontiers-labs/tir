use crate::{
    BlockId, Context, ContextIterator, Error, GetFromContext,
    context::ContextRef,
    ir_formatter::IRFormatter,
    parse::{Span, text::Parser as IRParser},
    region::RegionId,
    value::ValueId,
};
use std::{any::Any, sync::Arc};

pub type ErasedOpInterface = Box<dyn Any>;
pub type OpInterfaceConverter = fn(Arc<OpInstance>) -> ErasedOpInterface;

struct InterfaceValue<I: ?Sized + 'static>(Box<I>);

pub trait ImplementsOpInterface<I: ?Sized + 'static>: Operation {
    fn into_interface(self: Box<Self>) -> Box<I>;
}

pub fn erase_op_interface<I: ?Sized + 'static>(value: Box<I>) -> ErasedOpInterface {
    Box::new(InterfaceValue::<I>(value))
}

pub fn downcast_op_interface<I: ?Sized + 'static>(erased: ErasedOpInterface) -> Option<Box<I>> {
    erased
        .downcast::<InterfaceValue<I>>()
        .ok()
        .map(|boxed| boxed.0)
}

pub fn op_interface_converter<Op, I>(instance: Arc<OpInstance>) -> ErasedOpInterface
where
    Op: ImplementsOpInterface<I>,
    I: ?Sized + 'static,
{
    let op = Box::new(Op::from_op_instance(instance));
    erase_op_interface(ImplementsOpInterface::<I>::into_interface(op))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(u32);

/// Core trait for all operations in TIR’s intermediate representation.
///
/// An `Operation` is the fundamental building block of the IR graph,
/// forming nodes that represent everything from high-level constructs to low-level code.
/// Each operation models a transformation or computation in the program,
/// and is used to describe program modules, functions, control flow, arithmetic, memory access,
/// and target-specific instructions.
///
/// All language constructs, from an entire module to a single arithmetic instruction,
/// are expressed as operations. This unified abstraction allows for powerful analyses,
/// transformations, and extension with new operation kinds through custom dialects.
///
/// # Example
///
/// Defining and using a custom operation:
/// ```rust
/// use tir_macros::operation;
///
/// operation! {
///     BarOp {
///         name: "bar",
///         dialect: "foo",
///     }
/// }
/// ```
///
/// This macro will generate a BarOp structure, as well as BarOpBuilder for constructing
/// custom operation.
///
/// Because all operations implement this trait, generic IR passes can inspect,
/// transform, or analyze any construct in the IR using the same programming model.
pub trait Operation: 'static + Send + Sync + Any + Verifiable + OpDefVerifiable {
    fn name() -> &'static str
    where
        Self: Sized;
    fn dialect() -> &'static str
    where
        Self: Sized;

    fn id(&self) -> OpId;

    fn from_op_instance(instance: Arc<OpInstance>) -> Self
    where
        Self: Sized;

    fn from_op_instance_dyn(instance: Arc<OpInstance>) -> Box<dyn Operation>
    where
        Self: Sized;

    fn into_any(self: Box<Self>) -> Box<dyn Any>;

    fn print<'a, 'b: 'a>(&'a self, fmt: &'a mut IRFormatter<'b>) -> Result<(), std::fmt::Error>;
    fn parse<'src>(
        parser: &mut IRParser<'src>,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)>
    where
        Self: Sized;

    fn regions(&self) -> ContextIterator<RegionId>;
    fn operands(&self) -> &[ValueId];
    fn attributes(&self) -> &[crate::attributes::NamedAttribute];

    fn operand_names(&self) -> &'static [&'static str] {
        &[]
    }

    fn semantic_expr(
        &self,
        _g: &mut crate::sem_expr::ExprPostGraph,
    ) -> Option<crate::graph::NodeId> {
        None
    }

    fn register_interfaces(_context: &Context)
    where
        Self: Sized,
    {
    }

    fn parent_block(&self) -> Option<BlockId>;

    /// Verifies that operation is valid.
    ///
    /// Order of verification:
    /// 1. Operands verification - all operands must exist and value types must match.
    /// 2. Attributes verification - all DSL-defined attributes must have a value and types must match.
    /// 3. Interface verification - same operand types, etc...
    /// 4. Region verification - all basic blocks must end with a terminator, there must be at least one basic block.
    /// 5. Custom verification - additional constraints imposed by dialect authors.
    fn verify(&self, context: &Context) -> Result<(), crate::Error> {
        self.verify_operands(context)?;
        self.verify_attributes(context)?;
        self.verify_interfaces(context)?;

        for r in self.regions() {
            r.verify(context)?;
        }

        self.verify_impl(context)?;

        Ok(())
    }
}

pub trait Verifiable {
    fn verify_impl(&self, _context: &Context) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Common functions for verifying basic operation properties, that can be inferred
/// from DSL operation definition. Users are discouraged from implemnting this trait
/// manually. Prefer auto-generated DSL implementation instead.
pub trait OpDefVerifiable {
    /// Verify operands are correct. That is, values exist in the context and have the
    /// same type as described in DSL.
    fn verify_operands(&self, context: &Context) -> Result<(), crate::Error>;
    /// Verify attributes are correct. For each attribute that is defined in the DSL
    /// a value must exist and its types must match.
    fn verify_attributes(&self, context: &Context) -> Result<(), crate::Error>;
    /// Verify interfaces. For each implemented interface a verify_interface
    /// function is called.
    fn verify_interfaces(&self, _context: &Context) -> Result<(), crate::Error> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct OpInstance {
    pub id: OpId,
    pub name: &'static str,
    pub dialect: &'static str,
    pub context: ContextRef,
    pub operands: Vec<ValueId>,
    pub results: Vec<ValueId>,
    pub regions: Vec<RegionId>,
    pub attributes: Vec<crate::attributes::NamedAttribute>,
    /// Def/use role of each named register attribute, threaded from the op's
    /// generated `attribute_roles()` table. Lets the context maintain a def-use
    /// chain over machine-IR register operands (which live in attributes, not
    /// `operands`). Empty for ops without roles (e.g. builtin SSA ops).
    pub attribute_roles: &'static [(&'static str, crate::attributes::AttributeRole)],
}

impl OpInstance {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn dialect(&self) -> &'static str {
        self.dialect
    }

    /// The block that holds this operation, or `None` if it is detached or the root.
    pub fn parent_block(&self) -> Option<crate::BlockId> {
        self.context.upgrade().parent_block(self.id)
    }

    pub fn as_op<T: Operation + Sized>(self: Arc<Self>) -> Option<T> {
        if self.name == T::name() {
            Some(T::from_op_instance(self))
        } else {
            None
        }
    }

    pub fn as_dyn_op(self: Arc<Self>) -> Box<dyn Operation> {
        let context = self.context.upgrade();
        context.get_dyn_op(self.clone())
    }

    pub fn as_interface<I: ?Sized + 'static>(self: Arc<Self>) -> Option<Box<I>> {
        let context = self.context.upgrade();
        context.get_op_interface::<I>(self.clone())
    }
}

impl Default for OpId {
    fn default() -> Self {
        Self(u32::MAX)
    }
}

impl OpId {
    pub fn invalid() -> Self {
        Self::default()
    }

    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }

    /// Raw integer id, for stable identification across an FFI boundary.
    pub fn as_raw(self) -> u32 {
        self.0
    }

    /// Reconstruct an id from its raw integer, the inverse of [`OpId::as_raw`].
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

impl GetFromContext for OpId {
    type Item = Arc<OpInstance>;

    fn get_from_context(&self, context: &crate::Context) -> Self::Item {
        context.get_op(*self)
    }
}
