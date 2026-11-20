use crate::{
    BlockId, Context, ContextIterator, Error, GetFromContext,
    ir_formatter::IRFormatter,
    parse::{Span, text::Parser as IRParser},
    region::RegionId,
    value::ValueId,
};
use std::any::Any;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationName(&'static str);

impl OperationName {
    pub fn of<T: Operation>() -> Self {
        Self(T::name())
    }

    /// Returns the textual spelling used by IR parsers and printers.
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for OperationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::fmt::Debug for OperationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OperationName").field(&self.0).finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DialectName(&'static str);

impl DialectName {
    pub fn of<T: crate::Dialect>() -> Self {
        Self(T::name())
    }

    pub fn of_operation<T: Operation>() -> Self {
        Self(T::dialect())
    }

    /// Returns the textual spelling used by IR parsers and printers.
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for DialectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::fmt::Debug for DialectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DialectName").field(&self.0).finish()
    }
}

pub type ErasedOpInterface = Box<dyn Any>;
pub type OpInterfaceConverter = fn(OpHandle) -> ErasedOpInterface;

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

pub fn op_interface_converter<Op, I>(instance: OpHandle) -> ErasedOpInterface
where
    Op: ImplementsOpInterface<I>,
    I: ?Sized + 'static,
{
    let op = Box::new(Op::from_op_instance(instance));
    erase_op_interface(ImplementsOpInterface::<I>::into_interface(op))
}

id_newtype!(OpId);

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
/// Because all operations implement this trait, generic IR passes can inspect,
/// transform, or analyze any construct in the IR using the same programming model.
pub trait Operation: 'static + Any {
    fn name() -> &'static str
    where
        Self: Sized;
    fn dialect() -> &'static str
    where
        Self: Sized;

    fn id(&self) -> OpId {
        self.handle().id
    }

    /// The handle naming this op's storage. Most trait methods delegate to it so
    /// generated ops carry no per-op copies of the plumbing.
    #[doc(hidden)]
    fn handle(&self) -> &OpHandle;

    fn from_op_instance(instance: OpHandle) -> Self
    where
        Self: Sized;

    fn from_op_instance_dyn(instance: OpHandle) -> Box<dyn Operation>
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

    fn regions(&self) -> ContextIterator<RegionId> {
        let handle = self.handle();
        ContextIterator::new(handle.context.clone(), handle.regions().to_vec())
    }

    fn operands(&self) -> ValueIds {
        self.handle().operands()
    }

    /// The operands that are not memory states; see [`OpHandle::value_operands`].
    fn value_operands(&self) -> ValueIds {
        self.handle().value_operands()
    }

    /// The operands that are memory states; see [`OpHandle::state_operands`].
    fn state_operands(&self) -> ValueIds {
        self.handle().state_operands()
    }

    /// The results that are not memory states; see [`OpHandle::value_results`].
    fn value_results(&self) -> ValueIds {
        self.handle().value_results()
    }

    /// The results that are memory states; see [`OpHandle::state_results`].
    fn state_results(&self) -> ValueIds {
        self.handle().state_results()
    }

    fn attributes(&self) -> Vec<crate::attributes::NamedAttribute> {
        self.handle().attributes()
    }

    /// The value of the attribute called `name`; see [`OpHandle::attr`].
    fn attr(&self, name: &str) -> Option<crate::attributes::AttributeValue> {
        self.handle().attr(name)
    }

    fn register_interfaces(_context: &Context)
    where
        Self: Sized,
    {
    }

    fn parent_block(&self) -> Option<BlockId> {
        self.handle().parent_block()
    }
}

pub(crate) fn attr_type_matches(
    attr_type: &str,
    value: &crate::attributes::AttributeValue,
) -> bool {
    use crate::attributes::AttributeValue as V;
    match attr_type {
        "any" => true,
        "Str" => matches!(value, V::Str(_)),
        "Int" => matches!(value, V::Int(_) | V::Wide(_)),
        "UInt" => matches!(value, V::UInt(_)),
        "F32" => matches!(value, V::F32(_)),
        "F64" => matches!(value, V::F64(_)),
        // A flag is a fact the op's result may rely on: where it fails, the
        // result is poison. Dropping one only makes the op more defined.
        "Bool" | "Flag" => matches!(value, V::Bool(_)),
        "Array" => matches!(value, V::Array(_)),
        "Dict" => matches!(value, V::Dict(_)),
        "Register" => matches!(value, V::Register(_)),
        "Type" => matches!(value, V::Type(_)),
        "Block" => matches!(value, V::Block(_)),
        "Predicate" => matches!(value, V::Predicate(_)),
        _ => false,
    }
}

/// Check a builder's attributes before it allocates results or publishes an op.
pub fn check_opdef_attributes(
    context: &Context,
    attributes: &[crate::attributes::NamedAttribute],
    op_name: &str,
    attr_specs: &[crate::AttrSchema],
) -> Result<(), crate::Error> {
    check_attribute_specs(op_name, attr_specs, |name| {
        let symbol = context.sym(name);
        let mut matching = attributes.iter().filter(|attr| Some(attr.name) == symbol);
        let first = matching.next();
        if matching.next().is_some() {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} has duplicate attribute '{name}'"
            )));
        }
        Ok(first.map(|attr| attr.value.clone()))
    })
}

fn check_attribute_specs(
    op_name: &str,
    attr_specs: &[crate::AttrSchema],
    read: impl Fn(&str) -> Result<Option<crate::attributes::AttributeValue>, crate::Error>,
) -> Result<(), crate::Error> {
    for spec in attr_specs {
        let (attr_name, attr_type) = (spec.name, spec.ty);
        let optional = attr_type.strip_prefix('?');
        let attr_type = optional.unwrap_or(attr_type);
        let Some(value) = read(attr_name)? else {
            if optional.is_some() {
                continue;
            }
            return Err(crate::Error::VerificationError(format!(
                "{op_name} missing required attribute '{attr_name}'"
            )));
        };

        if !attr_type_matches(attr_type, &value) {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} attribute '{attr_name}' expected type '{attr_type}'"
            )));
        }

        if let crate::attributes::AttributeValue::Predicate(predicate) = value
            && !spec.vocabulary.is_empty()
            && !spec.vocabulary.contains(&predicate)
        {
            let expected = spec
                .vocabulary
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(crate::Error::VerificationError(format!(
                "{op_name} predicate '{}' is not in its vocabulary (expected {expected})",
                predicate.name()
            )));
        }
    }

    Ok(())
}

/// What a generated builder accumulates beside its declared ports: the
/// attributes. One field of one type, so every builder drops exactly one
/// thing it did not declare.
#[derive(Default)]
pub struct NewOpParts {
    pub attributes: Vec<crate::attributes::NamedAttribute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertionPoint {
    /// An operation with no enclosing block or region, such as a module.
    Root,
    /// An operation in an unordered graph region.
    Region(crate::RegionId),
    /// An operation immediately before the current position, or at the end
    /// when `position` equals the block's length.
    Block {
        block: crate::BlockId,
        position: usize,
    },
}

/// An operation as it is described before the context stores it: the parts a
/// builder assembles, handed to [`crate::Context::add_operation`] in one go.
pub struct NewOp {
    pub(crate) name: (&'static str, &'static str),
    pub(crate) operands: Vec<ValueId>,
    pub(crate) results: Vec<ValueId>,
    pub(crate) regions: Vec<RegionId>,
    pub(crate) attributes: Vec<crate::attributes::NamedAttribute>,
}

impl NewOp {
    /// The op a builder assembled: its ports, and in `parts` the attributes.
    pub fn new<T: Operation>(
        context: Context,
        operands: Vec<ValueId>,
        results: Vec<ValueId>,
        regions: Vec<RegionId>,
        parts: NewOpParts,
    ) -> Self {
        Self::assemble(
            (T::dialect(), T::name()),
            context,
            operands,
            results,
            regions,
            parts,
        )
    }

    fn assemble(
        identity: (&'static str, &'static str),
        context: Context,
        operands: Vec<ValueId>,
        results: Vec<ValueId>,
        regions: Vec<RegionId>,
        parts: NewOpParts,
    ) -> Self {
        context.register_op_name(identity.0, identity.1);
        NewOp {
            name: identity,
            operands,
            results,
            regions,
            attributes: parts.attributes,
        }
    }

    /// Describes an operation selected from textual input at a parser boundary.
    pub fn new_dynamic(
        identity: (&'static str, &'static str),
        context: Context,
        operands: Vec<ValueId>,
        results: Vec<ValueId>,
        regions: Vec<RegionId>,
        attributes: Vec<crate::attributes::NamedAttribute>,
    ) -> Self {
        context.register_op_name(identity.0, identity.1);
        NewOp {
            name: identity,
            operands,
            results,
            regions,
            attributes,
        }
    }
}

pub type ValueIds = smallvec::SmallVec<[ValueId; 4]>;
pub type RegionIds = smallvec::SmallVec<[RegionId; 2]>;

/// A reference to an operation: the context that owns it, and its id.
///
/// Reads go to the context's storage as they are asked for, so a handle always
/// answers with the operation as it stands now. Ids are never reused, so
/// [`OpHandle::is_live`] answers whether the op was erased, and debug builds
/// additionally panic on any read through a handle to an erased op.
#[derive(Clone)]
pub struct OpHandle {
    pub context: Context,
    pub id: OpId,
}

impl std::fmt::Debug for OpHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OpHandle").field(&self.id).finish()
    }
}

impl OpHandle {
    fn context(&self) -> crate::Context {
        let context = self.context.clone();
        debug_assert!(
            context.has_operation(self.id),
            "handle to erased operation {:?}",
            self.id
        );
        context
    }

    /// Whether the operation this handle names still exists.
    pub fn is_live(&self) -> bool {
        self.context.has_operation(self.id)
    }

    pub fn operands(&self) -> ValueIds {
        self.context().op_operands(self.id)
    }

    pub fn results(&self) -> ValueIds {
        self.context().op_results(self.id)
    }

    /// The operands that are not memory states.
    pub fn value_operands(&self) -> ValueIds {
        self.context().values_among(&self.operands())
    }

    /// The operands that are memory states: the chains this op observes.
    pub fn state_operands(&self) -> ValueIds {
        self.context().states_among(&self.operands())
    }

    /// The results that are not memory states.
    pub fn value_results(&self) -> ValueIds {
        self.context().values_among(&self.results())
    }

    /// The results that are memory states: the chains this op leaves behind.
    pub fn state_results(&self) -> ValueIds {
        self.context().states_among(&self.results())
    }

    pub fn regions(&self) -> RegionIds {
        self.context().op_regions(self.id)
    }

    pub fn attributes(&self) -> Vec<crate::attributes::NamedAttribute> {
        self.context().op_attributes(self.id)
    }

    /// The value of the attribute called `name`, resolving the name through the
    /// owning context's interner.
    pub fn attr(&self, name: &str) -> Option<crate::attributes::AttributeValue> {
        self.context().with_attr(self.id, name, Clone::clone)
    }

    /// [`OpHandle::attr`] for a name already interned, which is the form a
    /// repeated lookup wants: the comparison is on `u32`s.
    pub fn attr_sym(&self, name: tir_adt::Sym) -> Option<crate::attributes::AttributeValue> {
        self.context().op_attr_sym(self.id, name)
    }

    /// Returns an opaque name for textual output, not operation identity.
    ///
    /// ```compile_fail
    /// fn is_function(op: &tir::OpHandle) -> bool {
    ///     op.name() == "func"
    /// }
    /// ```
    pub fn name(&self) -> OperationName {
        OperationName(self.identity().1)
    }

    pub fn dialect(&self) -> DialectName {
        DialectName(self.identity().0)
    }

    fn identity(&self) -> (&'static str, &'static str) {
        self.context().op_identity(self.id)
    }

    /// The block that holds this operation, or `None` if it is detached or the root.
    pub fn parent_block(&self) -> Option<crate::BlockId> {
        self.context().parent_block(self.id)
    }

    /// Returns whether this operation has type `T`.
    pub fn is<T: Operation>(&self) -> bool {
        self.identity() == (T::dialect(), T::name())
    }

    pub fn as_op<T: Operation + Sized>(self) -> Option<T> {
        if self.is::<T>() {
            Some(T::from_op_instance(self))
        } else {
            None
        }
    }

    pub fn as_dyn_op(self) -> Box<dyn Operation> {
        let context = self.context();
        context.get_dyn_op(self)
    }

    pub fn as_interface<I: ?Sized + 'static>(self) -> Option<Box<I>> {
        let context = self.context();
        context.get_op_interface::<I>(self)
    }

    pub fn has_interface<I: ?Sized + 'static>(&self) -> bool {
        let context = self.context();
        context
            .operation_kind(self.id)
            .is_some_and(|kind| context.kind_has_interface::<I>(&kind))
    }
}

impl Default for OpId {
    fn default() -> Self {
        Self(u32::MAX)
    }
}

impl OpId {
    /// The def site of a value no operation defines: a block or region
    /// argument. See [`crate::Value::defining_op`].
    pub const ARGUMENT: OpId = OpId(u32::MAX);

    pub fn invalid() -> Self {
        Self::default()
    }
}

impl GetFromContext for OpId {
    type Item = OpHandle;

    fn get_from_context(&self, context: &crate::Context) -> Self::Item {
        context.get_op(*self)
    }
}

pub(crate) fn operand_fields(
    instance: &OpHandle,
    op_name: &str,
    fields: &[crate::FieldSchema],
) -> Result<Vec<usize>, crate::Error> {
    let operands = instance.operands().len();
    if !fields.iter().any(|field| field.variadic) {
        if operands > fields.len() {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} expects at most {} operands, got {operands}",
                fields.len(),
            )));
        }
        if let Some(missing) = fields[operands..]
            .iter()
            .find(|field| !field.ty.starts_with('?'))
        {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} missing required operand '{name}'",
                name = missing.name
            )));
        }
        return Ok((0..operands).collect());
    }

    // Variadic ops recover their operand grouping from the segment sizes
    // recorded at build time.
    let segment_sizes: Vec<usize> = match instance.attr("operand_segment_sizes") {
        Some(crate::attributes::AttributeValue::Array(items)) => items
            .iter()
            .map(|v| match v {
                crate::attributes::AttributeValue::UInt(n) => *n as usize,
                crate::attributes::AttributeValue::Int(n) if *n >= 0 => *n as usize,
                _ => 0usize,
            })
            .collect(),
        _ => {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} missing operand_segment_sizes attribute"
            )));
        }
    };

    if segment_sizes.len() != fields.len() {
        return Err(crate::Error::VerificationError(format!(
            "{op_name} expects {} operand segments, got {}",
            fields.len(),
            segment_sizes.len()
        )));
    }

    let total: usize = segment_sizes.iter().sum();
    if total != operands {
        return Err(crate::Error::VerificationError(format!(
            "{op_name} operand segment sizes sum to {total}, but it has {operands} operands"
        )));
    }

    let mut filled = Vec::with_capacity(operands);
    for (idx, field) in fields.iter().enumerate() {
        let optional = field.ty.starts_with('?');
        let size = segment_sizes[idx];
        if !field.variadic && size != 1 && !(optional && size == 0) {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} operand '{}' takes one value, got {size}",
                field.name
            )));
        }
        filled.extend(std::iter::repeat_n(idx, size));
    }
    Ok(filled)
}

pub(crate) fn result_fields(
    instance: &OpHandle,
    op_name: &str,
    fields: &[crate::FieldSchema],
) -> Result<Vec<usize>, crate::Error> {
    let results = instance.results().len();
    let mut filled = Vec::with_capacity(results);
    for (index, field) in fields.iter().enumerate() {
        let required_after = fields[index + 1..]
            .iter()
            .filter(|field| !field.variadic && !field.ty.starts_with('?'))
            .count();
        let available = results.saturating_sub(filled.len() + required_after);
        let count = if field.variadic {
            available
        } else if field.ty.starts_with('?') {
            usize::from(available != 0)
        } else {
            1
        };
        if filled.len() + count > results {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} missing required result '{}'",
                field.name
            )));
        }
        filled.extend(std::iter::repeat_n(index, count));
    }
    if filled.len() != results {
        return Err(crate::Error::VerificationError(format!(
            "{op_name} expects {} results, got {results}",
            filled.len()
        )));
    }
    Ok(filled)
}
