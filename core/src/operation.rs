use crate::run::{AttrRunId, RunId};
use crate::{
    BlockId, Context, ContextIterator, Error, GetFromContext,
    context::ContextRef,
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
        ContextIterator::new(handle.context.upgrade(), handle.regions().to_vec())
    }

    fn operands(&self) -> ValueIds {
        self.handle().operands()
    }

    /// The operands that carry a value; see [`OpHandle::value_operands`].
    fn value_operands(&self) -> ValueIds {
        self.handle().value_operands()
    }

    /// The trailing operands that are dependencies; see [`OpHandle::dep_operands`].
    fn dep_operands(&self) -> ValueIds {
        self.handle().dep_operands()
    }

    /// The results that carry a value; see [`OpHandle::value_results`].
    fn value_results(&self) -> ValueIds {
        self.handle().value_results()
    }

    /// The trailing results that are dependencies; see [`OpHandle::dep_results`].
    fn dep_results(&self) -> ValueIds {
        self.handle().dep_results()
    }

    fn attributes(&self) -> Vec<crate::attributes::NamedAttribute> {
        self.handle().attributes()
    }

    /// The value of the attribute called `name`; see [`OpHandle::attr`].
    fn attr(&self, name: &str) -> Option<crate::attributes::AttributeValue> {
        self.handle().attr(name)
    }

    fn semantic_expr(&self, _g: &mut crate::sem::SemGraph) -> Option<crate::graph::NodeId> {
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
    /// 1. Operands verification - all operands must exist and value types must match.
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
    verify_op_tree_ops(context, op_id)?;
    verify_state_forks(context, op_id)
}

/// Checks the fork/join discipline memory state follows.
///
/// A dependency names the memory at one point in the program, so at most one
/// operation may *change* it: a second would describe two futures for one memory.
/// Everything else naming it observes it — a read, which leaves memory as it found
/// it, or a [`crate::state::JoinOp`], which names the memory its inputs merge into
/// — and any number of those may, unordered against each other. One operation
/// naming a state twice observes it once: joining a memory with itself is that
/// memory.
///
/// The check is structural, so it cannot see chains: that every access of a chain
/// is in the cone of the state its next write takes is what the `shuffle-state`
/// fuzzer variant is for, not this.
///
/// State crossing a region boundary does so as a carried argument, which is a
/// fresh value, so a single walk of the whole tree suffices.
pub(crate) fn verify_state_forks(context: &Context, op_id: OpId) -> Result<(), Error> {
    let mut consumers: std::collections::HashMap<crate::ValueId, Vec<(OpId, bool)>> =
        std::collections::HashMap::new();
    let mut worklist = vec![op_id];
    while let Some(op_id) = worklist.pop() {
        let instance = context.get_op(op_id);
        let observes = observes_only(&instance);
        // A terminator's dependency is the state an edge is entered on, or
        // handed back: a rename across the edge, like a region result, and the
        // paths it forks a state along are exclusive.
        let carried = instance.has_interface::<dyn crate::Terminator>();
        for operand in instance.dep_operands().iter().copied().filter(|_| !carried) {
            let taken = consumers.entry(operand).or_default();
            if !taken.iter().any(|(taker, _)| *taker == op_id) {
                taken.push((op_id, observes));
            }
        }
        for region_id in instance.regions().iter().rev() {
            worklist.extend(context.get_region(*region_id).op_ids().iter().rev());
        }
    }
    for (value, taken) in &consumers {
        if taken.len() > 1 && !taken.iter().all(|(_, observes)| *observes) {
            return Err(Error::VerificationError(format!(
                "dependency %{} is both observed and changed",
                value.number()
            )));
        }
    }
    Ok(())
}

/// Whether `op` leaves the memory it names as it found it. An operation declaring
/// both memory interfaces writes the extent it reads, so it is no observer.
///
/// A machine instruction states what it does to memory in its
/// [`InstrInfo`](crate::backend::InstrInfo) effects; the memory interfaces are
/// what a mid-end operation has instead. One rule, read off whichever the
/// operation carries, so the discipline is the same on both sides of selection.
pub(crate) fn observes_only(op: &OpHandle) -> bool {
    if op.is::<crate::state::JoinOp>() {
        return true;
    }
    if let Some(machine) = op
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>()
    {
        // Anything that does not write cannot have changed the memory a state
        // names: a load leaves it as it found it, and a branch only carries it
        // along the edge it takes.
        return !machine.info().effects.writes;
    }
    op.has_interface::<dyn crate::MemoryRead>() && !op.has_interface::<dyn crate::MemoryWrite>()
}

/// Checks that every port list keeps values and dependencies apart: a dependency
/// sits only in a dependency partition, and a value only in a value one. The
/// partitions are counts, so this is what keeps the counts honest against the
/// values they name — across the op's own ports and its regions' arguments.
fn verify_dep_partitions(context: &Context, instance: &OpHandle) -> Result<(), Error> {
    let check = |values: &[crate::ValueId], dependencies: bool, kind: &str| {
        for value in values {
            if !context.has_value(*value)
                || context.get_value(*value).is_dependency() == dependencies
            {
                continue;
            }
            return Err(Error::VerificationError(if dependencies {
                format!("value %{} sits in a dependency {kind} slot", value.number())
            } else {
                format!("dependency %{} sits in a value {kind} slot", value.number())
            }));
        }
        Ok(())
    };
    check(&instance.value_operands(), false, "operand")?;
    check(&instance.dep_operands(), true, "operand")?;
    check(&instance.value_results(), false, "result")?;
    check(&instance.dep_results(), true, "result")?;
    let ids = |values: Vec<crate::Value>| values.iter().map(crate::Value::id).collect::<Vec<_>>();
    for region in instance.regions() {
        let region = context.get_region(region);
        check(&ids(region.value_arguments()), false, "argument")?;
        check(&ids(region.dep_arguments()), true, "argument")?;
        check(&region.value_results(), false, "result")?;
        check(&region.dep_results(), true, "result")?;
        for block in region.block_ids() {
            let block = context.get_block(block);
            check(&ids(block.value_arguments()), false, "argument")?;
            check(&ids(block.dep_arguments()), true, "argument")?;
        }
    }
    Ok(())
}

fn verify_op_tree_ops(context: &Context, op_id: OpId) -> Result<(), Error> {
    if !context.has_operation(op_id) {
        return Err(Error::VerificationError(format!(
            "operation {op_id:?} does not exist"
        )));
    }

    let instance = context.get_op(op_id);
    verify_token_region_arguments(context, &instance)?;
    verify_scoped_metadata(&instance)?;
    verify_dep_partitions(context, &instance)?;
    instance.clone().as_dyn_op().verify(context)?;

    for region_id in instance.regions().to_vec() {
        for child in context.get_region(region_id).op_ids() {
            verify_op_tree_ops(context, child)?;
        }
    }

    Ok(())
}

/// Checks the metadata specs this op contributes to its nested scopes.
fn verify_scoped_metadata(instance: &OpHandle) -> Result<(), Error> {
    if let Some(value) = instance.attr(crate::DATA_LAYOUT) {
        crate::layout::verify_spec(&value)?;
    }
    if let Some(value) = instance.attr(crate::TARGET_ENV) {
        crate::target_env::verify_spec(&value)?;
    }
    Ok(())
}

fn verify_token_region_arguments(context: &Context, instance: &OpHandle) -> Result<(), Error> {
    let token = crate::builtin::TokenType::new(context);
    let scope_regions = instance
        .clone()
        .as_interface::<dyn crate::TokenScope>()
        .map(|scope| scope.token_scope_regions())
        .unwrap_or_default();
    for region_id in instance.regions().iter() {
        let region = context.get_region(*region_id);
        let entry = scope_regions.contains(region_id);
        let arguments = region.ports().into_iter().map(|port| (entry, port)).chain(
            region
                .block_ids()
                .into_iter()
                .skip(1)
                .flat_map(|block| context.get_block(block).arguments())
                .map(|argument| (false, argument)),
        );
        for (allowed, argument) in arguments {
            if argument.ty() == token && !allowed {
                return Err(Error::VerificationError(
                    "token values are only allowed as loop body entry arguments".to_string(),
                ));
            }
        }
    }
    if instance.is::<crate::func::FuncOp>()
        && matches!(
            instance.attr("ret_type"),
            Some(crate::attributes::AttributeValue::Type(ty)) if ty == token
        )
    {
        return Err(Error::VerificationError(
            "token values are not allowed in function signatures".to_string(),
        ));
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
/// op carries only this table instead of an inlined copy of the engine.
pub struct OpDefSpec {
    pub schema: &'static crate::OpSchema,
    pub operand_checkers: &'static [fn(&dyn crate::Type) -> bool],
    pub result_checkers: &'static [fn(&dyn crate::Type) -> bool],
    /// What each declared region's body may be, positionally. Checked against
    /// what the op actually holds, so an op declaring blocks cannot be handed
    /// an unordered region.
    pub region_kinds: &'static [crate::RegionKind],
    /// The op implements `SameOperandAndResultType`: every operand and value
    /// result carries one type.
    pub same_type: bool,
}

/// The constraint a field's declared type names, with the optional (`?`) and
/// variadic (`*`) markers stripped: what verification errors report.
fn constraint_name(ty: &str) -> &str {
    ty.strip_prefix('?')
        .or_else(|| ty.strip_prefix('*'))
        .unwrap_or(ty)
}

fn verify_def_value(
    context: &Context,
    op_name: &str,
    kind: &str,
    value_name: &str,
    value_id: ValueId,
    checker: fn(&dyn crate::Type) -> bool,
    constraint_name: &str,
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

    let actual_ty = value.ty();
    let actual_ty_data = context.get_type_data(actual_ty);
    if !checker(actual_ty_data.as_ref()) {
        return Err(crate::Error::VerificationError(format!(
            "{op_name} {kind} '{value_name}' expected constraint {constraint_name}, got {}",
            context.type_to_string(actual_ty)
        )));
    }
    Ok(())
}

/// [`OpDefVerifiable::verify_operands`] engine, driven by an [`OpDefSpec`].
pub fn verify_opdef_operands(
    context: &Context,
    instance: &OpHandle,
    op_name: &str,
    spec: &OpDefSpec,
) -> Result<(), crate::Error> {
    verify_region_kinds(context, instance, op_name, spec)?;
    let operands = &instance.value_operands();
    let results = instance.value_results();
    let operand_fields = spec.schema.operands;
    let result_fields = spec.schema.results;

    if operand_fields.iter().any(|field| field.variadic) {
        // Variadic ops recover their operand grouping from the segment sizes
        // recorded at build time, then validate each declared operand's segment.
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

        if segment_sizes.len() != operand_fields.len() {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} expects {} operand segments, got {}",
                operand_fields.len(),
                segment_sizes.len()
            )));
        }

        let total: usize = segment_sizes.iter().sum();
        if total != operands.len() {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} operand segment sizes sum to {total}, but it has {} operands",
                operands.len()
            )));
        }

        let mut cursor = 0usize;
        for (idx, field) in operand_fields.iter().enumerate() {
            if !field.variadic && segment_sizes[idx] != 1 {
                return Err(crate::Error::VerificationError(format!(
                    "{op_name} operand '{}' takes one value, got {}",
                    field.name, segment_sizes[idx]
                )));
            }
            for k in 0..segment_sizes[idx] {
                verify_def_value(
                    context,
                    op_name,
                    "operand",
                    field.name,
                    operands[cursor + k],
                    spec.operand_checkers[idx],
                    constraint_name(field.ty),
                )?;
            }
            cursor += segment_sizes[idx];
        }
    } else {
        if operands.len() > operand_fields.len() {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} expects at most {} operands, got {}",
                operand_fields.len(),
                operands.len()
            )));
        }

        for (idx, field) in operand_fields.iter().enumerate() {
            let Some(value_id) = operands.get(idx).copied() else {
                if field.ty.starts_with('?') {
                    continue;
                }
                return Err(crate::Error::VerificationError(format!(
                    "{op_name} missing required operand '{name}'",
                    name = field.name
                )));
            };

            verify_def_value(
                context,
                op_name,
                "operand",
                field.name,
                value_id,
                spec.operand_checkers[idx],
                constraint_name(field.ty),
            )?;
        }
    }

    let value_results_len = results.len();

    let variadic_result = result_fields.iter().any(|field| field.variadic);
    if variadic_result {
        // A variadic result declares one spec covering every result value.
    } else if result_fields.iter().any(|field| field.ty.starts_with('?')) {
        if value_results_len > result_fields.len() {
            return Err(crate::Error::VerificationError(format!(
                "{op_name} expects at most {} results, got {}",
                result_fields.len(),
                value_results_len
            )));
        }
    } else if value_results_len != result_fields.len() {
        return Err(crate::Error::VerificationError(format!(
            "{op_name} expects {} results, got {}",
            result_fields.len(),
            value_results_len
        )));
    }

    for result_index in 0..value_results_len {
        let idx = if variadic_result { 0 } else { result_index };
        let field = &result_fields[idx];
        verify_def_value(
            context,
            op_name,
            "result",
            field.name,
            results[result_index],
            spec.result_checkers[idx],
            constraint_name(field.ty),
        )?;
    }

    if spec.same_type {
        let mut types = operands
            .iter()
            .chain(results.iter())
            .map(|&value| context.get_value(value).ty());
        if let Some(first) = types.next()
            && types.any(|ty| ty != first)
        {
            return Err(crate::Error::VerificationError(
                "operand and result types must be the same".to_string(),
            ));
        }
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
        "Int" => matches!(value, V::Int(_)),
        "UInt" => matches!(value, V::UInt(_)),
        "F32" => matches!(value, V::F32(_)),
        "F64" => matches!(value, V::F64(_)),
        "Bool" => matches!(value, V::Bool(_)),
        "Array" => matches!(value, V::Array(_)),
        "Dict" => matches!(value, V::Dict(_)),
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
        let Some(value) = instance.attr(attr_name) else {
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
/// attributes, and the dependencies the op observes and leaves. One field of
/// one type, so every builder drops exactly one thing it did not declare.
#[derive(Default)]
pub struct NewOpParts {
    pub attributes: Vec<crate::attributes::NamedAttribute>,
    pub dep_operands: Vec<ValueId>,
    pub dep_results: usize,
}

/// An operation as it is described before the context stores it: the parts a
/// builder assembles, handed to [`crate::Context::add_operation`] in one go.
pub struct NewOp {
    pub(crate) name: OpNameId,
    /// Operands, then results: the values first, then the trailing
    /// dependencies the counts below say how many of each list are.
    pub(crate) operands: Vec<ValueId>,
    pub(crate) results: Vec<ValueId>,
    pub(crate) dep_operands: u16,
    pub(crate) dep_results: u16,
    pub(crate) regions: Vec<RegionId>,
    pub(crate) attributes: Vec<crate::attributes::NamedAttribute>,
}

impl NewOp {
    /// The op a builder assembled: its value ports, and in `parts` the
    /// attributes and the dependencies trailing those ports.
    pub fn new<T: Operation>(
        context: ContextRef,
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
        context: ContextRef,
        mut operands: Vec<ValueId>,
        mut results: Vec<ValueId>,
        regions: Vec<RegionId>,
        parts: NewOpParts,
    ) -> Self {
        let context = context.upgrade();
        let name = context.intern_op_name(identity.0, identity.1);
        let dep_operands = parts.dep_operands.len() as u16;
        operands.extend(parts.dep_operands);
        results.extend((0..parts.dep_results).map(|_| context.create_dependency()));
        NewOp {
            name,
            operands,
            results,
            dep_operands,
            dep_results: parts.dep_results as u16,
            regions,
            attributes: parts.attributes,
        }
    }

    /// Describes an operation selected from textual input at a parser boundary.
    pub fn new_dynamic(
        identity: (&'static str, &'static str),
        context: ContextRef,
        operands: Vec<ValueId>,
        results: Vec<ValueId>,
        regions: Vec<RegionId>,
        attributes: Vec<crate::attributes::NamedAttribute>,
    ) -> Self {
        let (dialect, name) = identity;
        let name = context.upgrade().intern_op_name(dialect, name);
        NewOp {
            name,
            operands,
            results,
            dep_operands: 0,
            dep_results: 0,
            regions,
            attributes,
        }
    }

    /// Mark the trailing `operands` operands and `results` results already in
    /// the lists as dependencies: what a copy of an op does with values it has
    /// already minted.
    pub(crate) fn with_dependency_counts(mut self, operands: usize, results: usize) -> Self {
        self.dep_operands = operands as u16;
        self.dep_results = results as u16;
        self
    }
}

pub type ValueIds = smallvec::SmallVec<[ValueId; 4]>;
pub type RegionIds = smallvec::SmallVec<[RegionId; 2]>;

/// An operation's stored state: its identity, the cell holding its ports, and
/// the cell holding its attributes.
///
/// Everything an op holds beyond these thirty-two bytes lives in the context's
/// hives, so storing an operation allocates nothing; reading a port is one
/// index into its run.
#[derive(Debug, Clone)]
pub struct OpInstance {
    pub id: OpId,
    pub(crate) name: OpNameId,
    pub(crate) run: RunId,
    /// Every operand, the trailing `dep_operand_count` of which are
    /// dependencies; likewise the results.
    pub(crate) operand_count: u16,
    pub(crate) result_count: u16,
    pub(crate) dep_operand_count: u16,
    pub(crate) dep_result_count: u16,
    pub(crate) region_count: u16,
    pub(crate) attrs: AttrRunId,
    pub(crate) attr_count: u16,
    /// Structural version, bumped along the spine by every tree edit; see
    /// [`crate::Context::op_version`]. Never reset, so an id reused after an
    /// erase cannot match a cached analysis of the op that held it.
    pub(crate) version: u32,
}

impl OpInstance {
    pub(crate) fn name_id(&self) -> OpNameId {
        self.name
    }

    /// How many entries of its run the op is using.
    pub(crate) fn port_count(&self) -> usize {
        (self.operand_count + self.result_count + self.region_count) as usize
    }
}

/// A reference to an operation: the context that owns it, and its id.
///
/// Reads go to the context's storage as they are asked for, so a handle always
/// answers with the operation as it stands now. Ids *are* reused once the
/// context is told to [`recycle`](crate::Context::recycle), so a handle also
/// records the generation its id carried when it was minted:
/// [`OpHandle::is_live`] answers whether it still names its own op, and debug
/// builds additionally panic on any read through a handle that does not.
#[derive(Clone)]
pub struct OpHandle {
    pub context: ContextRef,
    pub id: OpId,
    pub(crate) generation: u32,
}

impl std::fmt::Debug for OpHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("OpHandle").field(&self.id).finish()
    }
}

impl OpHandle {
    /// The owning context, after checking this handle still names its own op.
    fn context(&self) -> crate::Context {
        let context = self.context.upgrade();
        #[cfg(debug_assertions)]
        context.assert_op_generation(self.id, self.generation);
        context
    }

    /// Whether this handle still names the operation it was minted for. False
    /// once the op is erased, including when another op has taken its id.
    pub fn is_live(&self) -> bool {
        self.context.upgrade().op_generation(self.id) == self.generation
    }

    /// Every operand: the values, then the trailing dependencies.
    pub fn operands(&self) -> ValueIds {
        self.context().op_operands(self.id)
    }

    /// Every result: the values, then the trailing dependencies.
    pub fn results(&self) -> ValueIds {
        self.context().op_results(self.id)
    }

    /// The operands that carry a value.
    pub fn value_operands(&self) -> ValueIds {
        let mut operands = self.operands();
        operands.truncate(operands.len() - self.context().op_dep_counts(self.id).0);
        operands
    }

    /// The trailing operands that are dependencies: the chains this op observes.
    pub fn dep_operands(&self) -> ValueIds {
        let operands = self.operands();
        operands[operands.len() - self.context().op_dep_counts(self.id).0..].into()
    }

    /// The results that carry a value.
    pub fn value_results(&self) -> ValueIds {
        let mut results = self.results();
        results.truncate(results.len() - self.context().op_dep_counts(self.id).1);
        results
    }

    /// The trailing results that are dependencies: the chains this op leaves.
    pub fn dep_results(&self) -> ValueIds {
        let results = self.results();
        results[results.len() - self.context().op_dep_counts(self.id).1..].into()
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
        self.context().op_attr(self.id, name)
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
        context.find_op_interface::<I>(self.identity()).is_some()
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

    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }

    /// The hive handle backing this id.
    pub(crate) fn raw(self) -> u32 {
        self.0
    }

    /// Raw integer id, for stable identification across an FFI boundary.
    pub fn number(self) -> u32 {
        self.0
    }

    /// Reconstruct an id from its raw integer, the inverse of [`OpId::number`].
    pub fn from_number(id: u32) -> Self {
        Self(id)
    }
}

impl GetFromContext for OpId {
    type Item = OpHandle;

    fn get_from_context(&self, context: &crate::Context) -> Self::Item {
        context.get_op(*self)
    }
}
