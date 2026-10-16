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
///         scheme: "() -> ()",
///     }
/// }
/// ```
///
/// This macro will generate a BarOp structure, as well as BarOpBuilder for constructing
/// custom operation.
///
/// Because all operations implement this trait, generic IR passes can inspect,
/// transform, or analyze any construct in the IR using the same programming model.
pub trait Operation: 'static + Any + Verifiable + OpDefVerifiable {
    fn name() -> &'static str
    where
        Self: Sized;
    fn dialect() -> &'static str
    where
        Self: Sized;

    fn id(&self) -> OpId {
        self.handle().id
    }

    /// The handle naming this op's storage. The `operation!` macro wires this to
    /// the newtype field; most trait methods delegate to it by default so
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

    fn semantic_expr(&self, _g: &mut crate::sem::SemGraph) -> Option<crate::NodeId> {
        None
    }

    /// Where the op yields poison, a one-bit expression over its operands as
    /// [`Operation::semantic_expr`] reads them: the `poison:` of its
    /// definition. Poison is deferred undefined behavior; only a use of the
    /// result is undefined.
    fn poison_expr(&self, _g: &mut crate::sem::SemGraph) -> Option<crate::NodeId> {
        None
    }

    /// What must hold for the op to run, a one-bit expression over its
    /// operands: the `requires:` of its definition. Running it where it fails
    /// is undefined behavior at once.
    fn requires_expr(&self, _g: &mut crate::sem::SemGraph) -> Option<crate::NodeId> {
        None
    }

    fn register_interfaces(_context: &Context)
    where
        Self: Sized,
    {
    }

    fn parent_block(&self) -> Option<BlockId> {
        self.handle().parent_block()
    }

    /// Verifies that operation is valid.
    ///
    /// Order of verification:
    /// 1. Operands verification - all operands must exist and fill the declared ports.
    /// 2. Attributes verification - all DSL-defined attributes must have a value and types must match.
    /// 3. Region verification - all basic blocks must end with a terminator, there must be at least one basic block.
    /// 4. Custom verification - additional constraints imposed by dialect authors.
    fn verify(&self, context: &Context) -> Result<(), crate::Error> {
        self.verify_operands(context)?;
        self.verify_attributes(context)?;

        for r in self.regions() {
            r.verify(context)?;
        }

        self.verify_impl(context)?;

        Ok(())
    }
}

pub fn verify_op_tree(context: &Context, op_id: OpId) -> Result<(), Error> {
    verify_op_trees(context, &[op_id])
}

pub(crate) fn verify_op_trees(context: &Context, roots: &[OpId]) -> Result<(), Error> {
    let mut ops = Vec::new();
    for &root in roots {
        verify_op_tree_ops(context, root, &mut ops)?;
    }
    crate::check_types(context, &ops)?;
    crate::sem::refine::check_refinements(context, &ops)?;
    roots
        .iter()
        .try_for_each(|&root| verify_state_forks(context, root))
}

pub(crate) fn verify_state_forks(context: &Context, op_id: OpId) -> Result<(), Error> {
    crate::state_verification::verify_state_forks(context, op_id)
}

fn verify_op_tree_ops(context: &Context, op_id: OpId, ops: &mut Vec<OpId>) -> Result<(), Error> {
    if !context.has_operation(op_id) {
        return Err(Error::VerificationError(format!(
            "operation {op_id:?} does not exist"
        )));
    }

    let instance = context.get_op(op_id);
    verify_scoped_metadata(&instance)?;
    instance.clone().as_dyn_op().verify(context)?;
    ops.push(op_id);

    for region_id in instance.regions().to_vec() {
        for child in context.get_region(region_id).op_ids() {
            verify_op_tree_ops(context, child, ops)?;
        }
    }

    Ok(())
}

/// Checks the metadata specs this op contributes to its nested scopes.
fn verify_scoped_metadata(instance: &OpHandle) -> Result<(), Error> {
    if let Some(value) = instance.attr(crate::LAYOUT_SPEC) {
        crate::layout::verify_spec(&value)?;
    }
    if let Some(value) = instance.attr(crate::TARGET_ENV) {
        crate::target_env::verify_spec(&value)?;
    }
    Ok(())
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
}

/// Static operand/result specification for [`OpDefVerifiable`], emitted per op by
/// the `operation!` macro. The verification engine lives in core so a generated
/// op carries only this table instead of an inlined copy of the engine. Types
/// are not checked here: the op's scheme is, by [`crate::check_types`].
pub struct OpDefSpec {
    pub schema: &'static crate::OpSchema,
    /// What each declared region's body may be, positionally. Checked against
    /// what the op actually holds, so an op declaring blocks cannot be handed
    /// an unordered region.
    pub region_kinds: &'static [crate::RegionKind],
}

fn verify_def_value(
    context: &Context,
    op_name: &str,
    kind: &str,
    value_name: &str,
    value_id: ValueId,
) -> Result<(), crate::Error> {
    if !context.has_value(value_id) {
        return Err(crate::Error::VerificationError(format!(
            "{op_name} {kind} '{value_name}' references unknown value %{id}",
            id = value_id.number()
        )));
    }

    let value = context.get_value(value_id);
    match value.defining_op() {
        Some(def_op) => {
            if !context.has_operation(def_op) {
                return Err(crate::Error::VerificationError(format!(
                    "{op_name} {kind} '{value_name}' value %{id} references missing defining op",
                    id = value_id.number()
                )));
            }
        }
        None => {
            let detail = if kind == "operand" {
                "has no defining op and is not an argument"
            } else {
                "has no defining op"
            };
            let argument = context.is_block_argument(value_id) || context.is_region_port(value_id);
            if kind != "operand" || !argument {
                return Err(crate::Error::VerificationError(format!(
                    "{op_name} {kind} '{value_name}' value %{id} {detail}",
                    id = value_id.number()
                )));
            }
        }
    }
    Ok(())
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

/// [`OpDefVerifiable::verify_operands`] engine, driven by an [`OpDefSpec`].
pub fn verify_opdef_operands(
    context: &Context,
    instance: &OpHandle,
    op_name: &str,
    spec: &OpDefSpec,
) -> Result<(), crate::Error> {
    verify_region_kinds(context, instance, op_name, spec)?;
    let operands = instance.operands();
    let results = instance.results();
    let operand_fields_of = operand_fields(instance, op_name, spec.schema.operands)?;
    for (&value, &field) in operands.iter().zip(&operand_fields_of) {
        let name = spec.schema.operands[field].name;
        verify_def_value(context, op_name, "operand", name, value)?;
    }
    let result_fields_of = result_fields(instance, op_name, spec.schema.results)?;
    for (&value, &field) in results.iter().zip(&result_fields_of) {
        let name = spec.schema.results[field].name;
        verify_def_value(context, op_name, "result", name, value)?;
    }
    Ok(())
}

/// Checks each region an op holds against the kind its declaration allows.
fn verify_region_kinds(
    context: &Context,
    instance: &OpHandle,
    op_name: &str,
    spec: &OpDefSpec,
) -> Result<(), crate::Error> {
    for (index, region) in instance.regions().iter().enumerate() {
        // A variadic group is declared once and holds every region from its
        // position on, so the last declaration covers whatever follows it.
        let Some(kind) = spec
            .region_kinds
            .get(index)
            .or_else(|| spec.region_kinds.last())
        else {
            break;
        };
        let nodes = context.get_region(*region).is_nodes();
        let allowed = match kind {
            crate::RegionKind::Blocks => !nodes,
            crate::RegionKind::Nodes => nodes,
            crate::RegionKind::Any => true,
        };
        if !allowed {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} declares a {kind:?} region, but holds {}",
                if nodes { "an unordered one" } else { "blocks" }
            )));
        }
    }
    Ok(())
}

fn attr_type_matches(attr_type: &str, value: &crate::attributes::AttributeValue) -> bool {
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
        "FpSemantics" => matches!(value, V::FpSemantics(_)),
        "EvaluationContract" => matches!(value, V::EvaluationContract(_)),
        "Register" => matches!(value, V::Register(_)),
        "Type" => matches!(value, V::Type(_)),
        "Block" => matches!(value, V::Block(_)),
        "Predicate" => matches!(value, V::Predicate(_)),
        _ => false,
    }
}

/// [`OpDefVerifiable::verify_attributes`] engine, driven by the op's schema.
pub fn verify_opdef_attributes(
    context: &Context,
    instance: &OpHandle,
    op_name: &str,
    attr_specs: &[crate::AttrSchema],
) -> Result<(), crate::Error> {
    let _ = context;
    for spec in attr_specs {
        let (attr_name, attr_type) = (spec.name, spec.ty);
        let optional = attr_type.strip_prefix('?');
        let attr_type = optional.unwrap_or(attr_type);
        let Some(value) = instance.attr(attr_name) else {
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

/// A dense id for an op's `(dialect, name)` pair, minted by the context that
/// owns the op. Meaningless outside that context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpNameId(u32);

impl OpNameId {
    pub(crate) fn new(index: u32) -> Self {
        Self(index)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a generated builder accumulates beside its declared ports: the
/// attributes. One field of one type, so every builder drops exactly one
/// thing it did not declare.
#[derive(Default)]
pub struct NewOpParts {
    pub attributes: Vec<crate::attributes::NamedAttribute>,
}

/// An operation as it is described before the context stores it: the parts a
/// builder assembles, handed to [`crate::Context::add_operation`] in one go.
pub struct NewOp {
    pub(crate) name: OpNameId,
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

    /// [`NewOp::new`] off an identity, so the one body serves every op type.
    fn assemble(
        identity: (&'static str, &'static str),
        context: Context,
        operands: Vec<ValueId>,
        results: Vec<ValueId>,
        regions: Vec<RegionId>,
        parts: NewOpParts,
    ) -> Self {
        let name = context.intern_op_name(identity.0, identity.1);
        NewOp {
            name,
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
        let (dialect, name) = identity;
        let name = context.intern_op_name(dialect, name);
        NewOp {
            name,
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
    /// The owning context, after checking this handle still names its own op.
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

    pub(crate) fn is_name(&self, dialect: &str, name: &str) -> bool {
        self.identity() == (dialect, name)
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
            .find_op_interface::<I>(context.op_name_id(self.id))
            .is_some()
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
