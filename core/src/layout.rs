//! Layouts: the size, alignment and field offsets of a type under the spec in
//! scope at an operation.
//!
//! A spec is a `#layout.spec<..>` attribute (see [`LayoutSpec`]) under the
//! [`LAYOUT_SPEC`] key of a scope op; a nested scope's spec overrides its
//! enclosing one entry by entry. Every size and alignment is in **bits**.
//!
//! The most specific matching spec entry gives a type's layout. A named type
//! can also use its `type.def` body's layout, with `packed` applied to the
//! body and the layout raised to the definition's `align`.

use std::collections::HashSet;
use std::sync::Arc;

pub use crate::dialects::layout::*;

use crate::attributes::AttributeValue;
use crate::builtin::{AggregateType, FloatType, NamedType, TypeDefOp};
use crate::{Context, OpId, SymbolTable, TypeId};

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
    context: Context,
    spec: Arc<LayoutSpec>,
    symbols: Option<SymbolTable>,
}

impl DataLayout {
    fn new(context: &Context, spec: Arc<LayoutSpec>, module: Option<OpId>) -> Self {
        Self {
            context: context.clone(),
            spec,
            symbols: module.map(|module| SymbolTable::build(context, module)),
        }
    }

    /// The layout in scope at `op`, or `None` when no enclosing scope declares one.
    pub fn for_op(context: &Context, op: OpId) -> Option<Self> {
        let scopes = enclosing_scopes(context, op)?;
        let scope = scopes.spec?;
        let spec = Arc::new(scoped_spec(context, scope));
        Some(Self::new(context, spec, scopes.module))
    }

    /// The layout in scope at `instance`: the scope chain of its id, falling back
    /// to the spec the instance carries itself. A detached instance — an op type
    /// probed with no IR home — belongs to no scope, so the layout resolved where
    /// the code came from rides along as its own attribute.
    pub fn for_instance(context: &Context, instance: &crate::OpHandle) -> Option<Self> {
        Self::for_op(context, instance.id).or_else(|| {
            let spec = spec_of(instance.attr(LAYOUT_SPEC).as_ref()?)?;
            Some(Self::new(context, spec, None))
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
        Some(Self::new(context, Arc::new(spec), module))
    }

    /// A layout read from a spec that is not attached to the IR — a target's
    /// default, which applies where the IR itself declares nothing. It names
    /// no `type.def`s; its types are interned in `context`.
    pub fn from_value(context: &Context, value: &AttributeValue) -> Option<Self> {
        Some(Self::new(context, spec_of(value)?, None))
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
        let terms = self.context.terms();
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
    pub fn layout_of(&self, mut ty: TypeId) -> Option<Layout> {
        let terms = self.context.terms();
        let mut seen = HashSet::new();
        let mut alignments = Vec::new();
        let (mut size, mut align, mut preferred) = loop {
            if let Some(layout) = self.spec.entry_layout(terms, ty) {
                break layout;
            }
            if !seen.insert(ty) {
                return None;
            }
            let named = NamedType::view(terms, ty)?;
            let definition = self.definition(named.name())?;
            if let Some(align) = definition.align() {
                alignments.push(align);
            }
            ty = definition.expand(named.args())?;
            if definition.packed() {
                ty = AggregateType::packed(terms, ty)?;
            }
        };
        for requested in alignments.into_iter().rev() {
            align = align.max(requested);
            preferred = preferred.max(requested);
            size = align_to(size, align)?;
        }
        Some(Layout {
            size,
            align,
            preferred,
            fields: vec![],
        })
    }

    /// Size of `ty` in bits.
    pub fn size_in_bits(&self, ty: TypeId) -> Option<u32> {
        u32::try_from(self.layout_of(ty)?.size).ok()
    }

    /// Alignment `ty` must be given to satisfy the ABI, in bits.
    pub fn abi_alignment(&self, ty: TypeId) -> Option<u32> {
        u32::try_from(self.layout_of(ty)?.align).ok()
    }

    /// Alignment `ty` is given when nothing constrains it, in bits. Defaults to
    /// the ABI alignment.
    pub fn preferred_alignment(&self, ty: TypeId) -> Option<u32> {
        u32::try_from(self.layout_of(ty)?.preferred).ok()
    }

    /// The offset in bits of field `field` of `ty`, a named or aggregate type.
    pub fn offset_in_bits(&self, ty: TypeId, field: &str) -> Option<u64> {
        let terms = self.context.terms();
        let aggregate = AggregateType::view(terms, ty).or_else(|| {
            let named = NamedType::view(terms, ty)?;
            let definition = self.definition(named.name())?;
            AggregateType::view(terms, definition.expand(named.args())?)
        })?;
        let index = aggregate.field_index(field)?;
        self.layout_of(ty)?.fields.get(index).copied()
    }

    /// A spec option outside the predefined set, for metadata a dialect defines
    /// itself (e.g. a GPU address-space mapping).
    pub fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.spec.option(key)
    }

    fn definition(&self, name: &str) -> Option<TypeDefOp> {
        self.symbols.as_ref()?.type_definition(&self.context, name)
    }
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
