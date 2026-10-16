//! Layouts: the size, alignment and field offsets of a type under the spec in
//! scope at an operation.
//!
//! A spec is a `#layout.spec<..>` attribute (see [`LayoutSpec`]) under the
//! [`LAYOUT_SPEC`] key of a scope op; a nested scope's spec overrides its
//! enclosing one entry by entry. Every size and alignment is in **bits**.
//!
//! A type's layout comes from, in order: the most specific spec entry that
//! matches it; for a named type, its `type.def`'s own layout region or else
//! its body's layout (the body passed `packed = true` under `layout {packed}`),
//! raised to the definition's `align`; for an aggregate constructor, or a
//! name the module does not define such as `!array<N, T>`, its core
//! definition (`dialects/layout/types.rs`). Regions run in the interpreter.
//! A type none of these covers has no layout.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub use crate::dialects::layout::*;

use crate::attributes::AttributeValue;
use crate::builtin::{AggregateType, FloatType, NamedType, TypeDefOp};
use crate::interp::{ExecutionState, Value};
use crate::scoped_attr::invalid_spec;
use crate::ty::TypeArg;
use crate::utils::APInt;
use crate::{Context, OpId, RegionId, SymbolTable, TypeId};

/// The attribute a layout spec is carried in.
pub const LAYOUT_SPEC: &str = "layout.spec";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
}

/// A type's layout, in bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: u64,
    pub align: u64,
    pub preferred: u64,
    /// The offset of each field, for an aggregate.
    pub fields: Vec<u64>,
}

/// The layout in scope at an operation, and the module whose `type.def`s
/// name types.
pub struct DataLayout {
    spec: Arc<LayoutSpec>,
    env: Rc<LayoutEnv>,
}

// The layouts of one module under one resolved spec, each computed once
// whichever query asked. The context keeps the one of each spec scope until
// the module's own partition is edited.
pub(crate) struct ModuleLayouts {
    spec: Arc<LayoutSpec>,
    definitions: Arc<HashMap<String, OpId>>,
    memo: Mutex<HashMap<TypeId, Option<Layout>>>,
}

impl ModuleLayouts {
    fn new(context: &Context, module: Option<OpId>, spec: Arc<LayoutSpec>) -> Self {
        Self {
            spec,
            definitions: module.map_or_else(Arc::default, |module| {
                context.module_type_definitions(module)
            }),
            memo: Mutex::default(),
        }
    }
}

pub(crate) struct LayoutEnv {
    context: Context,
    laid: Arc<ModuleLayouts>,
    // The types whose layouts are being computed. One asked for again holds
    // itself by value, which the verifier rejects, and has no layout.
    in_progress: RefCell<HashSet<TypeId>>,
}

pub(crate) struct LayoutRun {
    env: Rc<LayoutEnv>,
    fields: Vec<u64>,
}

impl DataLayout {
    fn new(context: &Context, laid: Arc<ModuleLayouts>) -> Self {
        Self {
            spec: laid.spec.clone(),
            env: Rc::new(LayoutEnv {
                context: context.clone(),
                laid,
                in_progress: RefCell::default(),
            }),
        }
    }

    // A layout no scope's cache holds: a spec the IR does not carry.
    fn unscoped(context: &Context, spec: Arc<LayoutSpec>, module: Option<OpId>) -> Self {
        Self::new(context, Arc::new(ModuleLayouts::new(context, module, spec)))
    }

    /// The layout in scope at `op`, or `None` when no enclosing scope declares one.
    pub fn for_op(context: &Context, op: OpId) -> Option<Self> {
        let scopes = enclosing_scopes(context, op)?;
        let scope = scopes.spec?;
        let laid = context.scope_layouts(scope, || {
            ModuleLayouts::new(
                context,
                scopes.module,
                Arc::new(scoped_spec(context, scope)),
            )
        });
        Some(Self::new(context, laid))
    }

    /// The layout in scope at `instance`: the scope chain of its id, falling back
    /// to the spec the instance carries itself. A detached instance — an op type
    /// probed with no IR home — belongs to no scope, so the layout resolved where
    /// the code came from rides along as its own attribute.
    pub fn for_instance(context: &Context, instance: &crate::OpHandle) -> Option<Self> {
        Self::for_op(context, instance.id).or_else(|| {
            let spec = spec_of(instance.attr(LAYOUT_SPEC).as_ref()?)?;
            Some(Self::unscoped(context, spec, None))
        })
    }

    /// The layout in scope at `op` over `default`: the target's own spec acts as
    /// the outermost scope, so IR entries override it key by key while entries
    /// the IR never mentions keep the target's value.
    pub fn for_op_with_default(
        context: &Context,
        op: OpId,
        default: Option<&AttributeValue>,
    ) -> Option<Self> {
        let Some(default) = default.and_then(spec_of) else {
            return Self::for_op(context, op);
        };
        let mut spec = (*default).clone();
        let scopes = enclosing_scopes(context, op);
        let module = scopes.as_ref().and_then(|scopes| scopes.module);
        if let Some(scope) = scopes.and_then(|scopes| scopes.spec) {
            spec.merge(&scoped_spec(context, scope));
        }
        Some(Self::unscoped(context, Arc::new(spec), module))
    }

    /// A layout read from a spec that is not attached to the IR — a target's
    /// default, which applies where the IR itself declares nothing. It names
    /// no `type.def`s; its types are interned in `context`.
    pub fn from_value(context: &Context, value: &AttributeValue) -> Option<Self> {
        Some(Self::unscoped(context, spec_of(value)?, None))
    }

    pub fn endianness(&self) -> Option<Endianness> {
        match self.spec.option("endianness")? {
            AttributeValue::Str(value) if &**value == "little" => Some(Endianness::Little),
            AttributeValue::Str(value) if &**value == "big" => Some(Endianness::Big),
            _ => None,
        }
    }

    /// Alignment the stack pointer is kept at, in bits.
    pub fn stack_alignment(&self) -> Option<u32> {
        self.number("stack_align")
    }

    /// Bytes a cache line holds, which is what a locality model counts in.
    pub fn cache_line(&self) -> Option<u32> {
        self.number("cache_line")
    }

    fn number(&self, key: &str) -> Option<u32> {
        u32::try_from(self.spec.option(key)?.as_int()?).ok()
    }

    /// The layout as the attribute an operation carries it in.
    pub fn into_value(self) -> AttributeValue {
        AttributeValue::LayoutSpec(self.spec)
    }

    /// The spec, in the form a [`LAYOUT_SPEC`] attribute carries it.
    pub fn spec(&self) -> AttributeValue {
        AttributeValue::LayoutSpec(self.spec.clone())
    }

    /// Width of a pointer, in bits.
    pub fn pointer_size(&self) -> Option<u32> {
        let terms = self.env.context.terms();
        let (size, _, _) = self
            .spec
            .entry_layout(terms, crate::ptr::opaque_pointer(terms))?;
        u32::try_from(size).ok()
    }

    /// Width of an index, in bits: the `index` option, else the pointer's.
    pub fn index_width(&self) -> Option<u32> {
        self.number("index").or_else(|| self.pointer_size())
    }

    /// `ty`'s layout.
    pub fn layout_of(&self, ty: TypeId) -> Option<Layout> {
        // Each part laid out before what holds it finds its own parts known,
        // so the recursion through layout regions stays shallow however
        // deeply types nest.
        for part in parts_innermost_first(&self.env, ty) {
            compute(&self.env, &self.spec, part);
        }
        compute(&self.env, &self.spec, ty)
    }

    pub(crate) fn measure(&self, ty: TypeId, measure: Measure) -> Option<u64> {
        let layout = self.layout_of(ty)?;
        Some(match measure {
            Measure::Size => layout.size,
            Measure::Align => layout.align,
        })
    }

    /// Size of `ty` in bits.
    pub fn size_in_bits(&self, ty: TypeId) -> Option<u32> {
        u32::try_from(self.measure(ty, Measure::Size)?).ok()
    }

    /// Alignment `ty` must be given to satisfy the ABI, in bits.
    pub fn abi_alignment(&self, ty: TypeId) -> Option<u32> {
        u32::try_from(self.measure(ty, Measure::Align)?).ok()
    }

    /// Alignment `ty` is given when nothing constrains it, in bits. Defaults to
    /// the ABI alignment.
    pub fn preferred_alignment(&self, ty: TypeId) -> Option<u32> {
        u32::try_from(self.layout_of(ty)?.preferred).ok()
    }

    /// The offset in bits of field `field` of `ty`, a named or aggregate type.
    pub fn offset_in_bits(&self, ty: TypeId, field: &str) -> Option<u64> {
        let aggregate = aggregate_of(&self.env, ty)?;
        let index = aggregate.field_index(field)?;
        self.layout_of(ty)?.fields.get(index).copied()
    }

    /// A spec option outside the predefined set, for metadata a dialect defines
    /// itself (e.g. a GPU address-space mapping).
    pub fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.spec.option(key)
    }
}

fn aggregate_of(env: &LayoutEnv, ty: TypeId) -> Option<AggregateType> {
    let terms = env.context.terms();
    if let Some(aggregate) = AggregateType::view(terms, ty) {
        return Some(aggregate);
    }
    let named = NamedType::view(terms, ty)?;
    let definition = env.definition(named.name())?;
    AggregateType::view(terms, definition.expand(named.args())?)
}

impl LayoutEnv {
    fn definition(&self, name: &str) -> Option<TypeDefOp> {
        let op = *self.laid.definitions.get(name)?;
        self.context.get_op(op).as_op::<TypeDefOp>()
    }

    // The core definition of `name`.
    fn core_definition(&self, name: &str) -> Option<(Context, TypeDefOp)> {
        let (context, definitions) = self.context.constructors()?;
        let definition = context.get_op(*definitions.get(name)?).as_op()?;
        Some((context, definition))
    }
}

pub(crate) fn definitions_in(context: &Context, module: OpId) -> HashMap<String, OpId> {
    let table = SymbolTable::build(context, module);
    table
        .names()
        .filter_map(|name| {
            let entry = table.lookup(name).first()?;
            Some((name.strip_prefix('!')?.to_string(), entry.op))
        })
        .collect()
}

// The types `ty` holds by value, itself included, whose layouts are not
// known yet, each after the ones it holds.
fn parts_innermost_first(env: &LayoutEnv, ty: TypeId) -> Vec<TypeId> {
    let known = env.laid.memo.lock().unwrap_or_else(|e| e.into_inner());
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = vec![(ty, false)];
    while let Some((ty, ready)) = pending.pop() {
        if ready {
            order.push(ty);
        } else if !known.contains_key(&ty) && seen.insert(ty) {
            pending.push((ty, true));
            pending.extend(parts(env, ty).into_iter().map(|part| (part, false)));
        }
    }
    order
}

// The types `ty` holds by value: a named type's body, else the types among
// its arguments.
fn parts(env: &LayoutEnv, ty: TypeId) -> Vec<TypeId> {
    let terms = env.context.terms();
    if crate::ptr::PtrType::view(terms, ty).is_some() {
        return Vec::new();
    }
    let body = NamedType::view(terms, ty).and_then(|named| match env.definition(named.name()) {
        Some(definition) => definition.expand(named.args()),
        None => env.core_definition(named.name())?.1.expand(named.args()),
    });
    if let Some(body) = body {
        return vec![body];
    }
    let term = terms.term(ty);
    let mut args: Vec<&TypeArg> = term.args().iter().collect();
    let mut parts = Vec::new();
    while let Some(arg) = args.pop() {
        match arg {
            TypeArg::Type(part) => parts.push(*part),
            TypeArg::List(items) => args.extend(items.iter()),
            _ => {}
        }
    }
    parts
}

fn compute(env: &Rc<LayoutEnv>, spec: &Arc<LayoutSpec>, ty: TypeId) -> Option<Layout> {
    let memo = || env.laid.memo.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(known) = memo().get(&ty) {
        return known.clone();
    }
    if !env.in_progress.borrow_mut().insert(ty) {
        return None;
    }
    let layout = compute_uncached(env, spec, ty);
    env.in_progress.borrow_mut().remove(&ty);
    memo().insert(ty, layout.clone());
    layout
}

fn compute_uncached(env: &Rc<LayoutEnv>, spec: &Arc<LayoutSpec>, ty: TypeId) -> Option<Layout> {
    let terms = env.context.terms();
    if let Some((size, align, preferred)) = spec.entry_layout(terms, ty) {
        return Some(Layout {
            size,
            align,
            preferred,
            fields: vec![],
        });
    }
    let named = NamedType::view(terms, ty);
    if let Some(named) = &named
        && let Some(definition) = env.definition(named.name())
    {
        let mut layout = match definition.layout_region() {
            Some(region) => {
                let arguments = region_arguments(spec, &definition, named.args())?;
                run_region(env, &env.context, region, arguments)?
            }
            None => {
                let mut body = definition.expand(named.args())?;
                if definition.packed() {
                    body = AggregateType::packed(terms, body)?;
                }
                compute(env, spec, body)?
            }
        };
        if let Some(align) = definition.align() {
            layout.align = layout.align.max(align);
            layout.preferred = layout.preferred.max(align);
            layout.size = align_to(layout.size, layout.align)?;
        }
        return Some(layout);
    }
    // A name the module does not define, as `!array<4, !i32>`, or an
    // aggregate constructor: the core type definitions define it.
    let term = terms.term(ty);
    let ctor = terms.ctor_info(term.ctor());
    let (name, args) = match &named {
        Some(named) => (named.name(), named.args()),
        None if ctor.dialect == "type" => (ctor.name, term.args()),
        None => return None,
    };
    let (context, definition) = env.core_definition(name)?;
    if let Some(body) = definition.expand(args) {
        return compute(env, spec, body);
    }
    let region = definition.layout_region()?;
    let arguments = region_arguments(spec, &definition, args)?;
    run_region(env, &context, region, arguments)
}

// What a layout region of `definition` is handed: the spec, then each
// parameter as `args` binds it or its default.
fn region_arguments(
    spec: &Arc<LayoutSpec>,
    definition: &TypeDefOp,
    args: &[TypeArg],
) -> Option<Vec<Value>> {
    let mut arguments = vec![Value::Layout(spec.clone())];
    for (index, (_, kind)) in definition.params().iter().enumerate() {
        let arg = args
            .get(index)
            .cloned()
            .or_else(|| crate::builtin::type_arg_of(&definition.param_default(index)?));
        arguments.push(region_argument(kind, arg?)?);
    }
    Some(arguments)
}

fn region_argument(kind: &str, arg: TypeArg) -> Option<Value> {
    match (kind, arg) {
        ("fields", TypeArg::List(fields)) => fields
            .iter()
            .map(|field| match field {
                TypeArg::List(pair) => pair.get(1)?.as_type().map(Value::Type),
                _ => None,
            })
            .collect::<Option<_>>()
            .map(Value::Tuple),
        ("data", TypeArg::Type(ty)) => Some(Value::Type(ty)),
        ("nat", TypeArg::Nat(n)) => Some(Value::Int(APInt::new(64, n))),
        ("bool", TypeArg::Nat(n)) => Some(Value::Int(APInt::new(1, n))),
        _ => None,
    }
}

fn run_region(
    env: &Rc<LayoutEnv>,
    context: &Context,
    region: RegionId,
    arguments: Vec<Value>,
) -> Option<Layout> {
    let mut state = ExecutionState {
        layout: Some(LayoutRun {
            env: env.clone(),
            fields: vec![],
        }),
        ..ExecutionState::default()
    };
    let results = crate::interp::run_region(context, region, arguments, &mut state).ok()?;
    let [Value::Int(size), Value::Int(align)] = results.as_slice() else {
        return None;
    };
    Some(Layout {
        size: size.to_u64(),
        align: align.to_u64(),
        preferred: align.to_u64(),
        fields: state.layout?.fields,
    })
}

pub(crate) fn measure_in(
    state: &ExecutionState,
    spec: &Arc<LayoutSpec>,
    ty: TypeId,
    measure: Measure,
) -> Option<u64> {
    let run = state.layout.as_ref()?;
    let layout = compute(&run.env, spec, ty)?;
    Some(match measure {
        Measure::Size => layout.size,
        Measure::Align => layout.align,
    })
}

pub(crate) fn record_field(state: &mut ExecutionState, index: u64, offset: u64) -> Option<()> {
    let fields = &mut state.layout.as_mut()?.fields;
    let index = usize::try_from(index).ok()?;
    if fields.len() <= index {
        fields.resize(index + 1, 0);
    }
    fields[index] = offset;
    Some(())
}

// Where a layout query at an op is answered: the module whose `type.def`s
// name types, and the innermost op carrying a spec.
struct Scopes {
    module: Option<OpId>,
    spec: Option<OpId>,
}

// A detached instance — an op type probed with no IR home to ask what it is
// — belongs to no scope, so it reads no metadata.
fn enclosing_scopes(context: &Context, op: OpId) -> Option<Scopes> {
    if !context.has_operation(op) {
        return None;
    }
    let mut scopes = Scopes {
        module: None,
        spec: None,
    };
    let mut current = Some(op);
    while let Some(id) = current {
        let handle = context.get_op(id);
        if scopes.spec.is_none() && handle.attr(LAYOUT_SPEC).is_some() {
            scopes.spec = Some(id);
        }
        if scopes.module.is_none() && handle.is::<crate::builtin::ModuleOp>() {
            scopes.module = Some(id);
        }
        if scopes.module.is_some() && scopes.spec.is_some() {
            break;
        }
        current = context.parent_op(id);
    }
    Some(scopes)
}

fn spec_of(value: &AttributeValue) -> Option<Arc<LayoutSpec>> {
    match value {
        AttributeValue::LayoutSpec(spec) => Some(spec.clone()),
        _ => None,
    }
}

// The spec in force at `scope`: every enclosing spec, inner over outer.
fn scoped_spec(context: &Context, scope: OpId) -> LayoutSpec {
    let mut scopes = vec![];
    let mut current = Some(scope);
    while let Some(id) = current {
        if let Some(AttributeValue::LayoutSpec(spec)) = context.get_op(id).attr(LAYOUT_SPEC) {
            scopes.push(spec);
        }
        current = context.parent_op(id);
    }
    let mut resolved = LayoutSpec::new();
    while let Some(inner) = scopes.pop() {
        resolved.merge(&inner);
    }
    resolved
}

fn class_type(context: &Context, class: &str) -> Option<TypeId> {
    if class == "p" {
        return Some(crate::ptr::opaque_pointer(context.terms()));
    }
    if let Some(width) = class.strip_prefix('i').and_then(|width| width.parse().ok()) {
        return Some(context.terms().int(width));
    }
    FloatType::named(context, class)
}

/// Builds a [`LAYOUT_SPEC`] spec, the form a target describes its own ABI in.
/// `types` gives each scalar (`i32`, `f64`, `p` for pointers) its size and ABI
/// alignment in bits.
pub fn data_layout_spec(
    context: &Context,
    endianness: Endianness,
    stack_alignment: u32,
    types: &[(&str, u32, u32)],
) -> AttributeValue {
    let order = match endianness {
        Endianness::Little => "little",
        Endianness::Big => "big",
    };
    let mut spec = LayoutSpec::new()
        .with_option("endianness", AttributeValue::from(order))
        .with_option(
            "stack_align",
            AttributeValue::UInt(u64::from(stack_alignment)),
        )
        .with_option("cache_line", AttributeValue::UInt(64));
    for (class, size, abi) in types {
        let ty =
            class_type(context, class).unwrap_or_else(|| panic!("unknown scalar class '{class}'"));
        spec = spec.with_type(ty, u64::from(*size), u64::from(*abi));
    }
    spec.into()
}

// The spec's parser already checked its entries.
pub(crate) fn verify_spec(value: &AttributeValue) -> Result<(), crate::Error> {
    let AttributeValue::LayoutSpec(spec) = value else {
        return Err(invalid_spec("layout.spec must be a #layout.spec"));
    };
    match spec.option("endianness") {
        None => {}
        Some(AttributeValue::Str(order)) if &**order == "little" || &**order == "big" => {}
        Some(_) => {
            return Err(invalid_spec(
                "layout.spec 'endianness' must be \"little\" or \"big\"",
            ));
        }
    }
    for key in ["stack_align", "native", "index", "cache_line"] {
        if spec
            .option(key)
            .is_some_and(|value| value.as_int().is_none_or(|bits| bits < 0))
        {
            return Err(invalid_spec(format!(
                "layout.spec '{key}' must be a bit count"
            )));
        }
    }
    Ok(())
}
