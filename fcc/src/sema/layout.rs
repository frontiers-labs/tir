//! Sizes, alignments and field offsets of C types. fcc computes none of them:
//! each complete record is a `type.def` of a module, and the core lays types
//! out under the target's layout spec.

use std::cell::RefCell;
use std::collections::HashSet;

use tir::attributes::AttributeValue;
use tir::builtin::{
    AggregateKind, AggregateType, FloatType, ModuleOp, NamedType, TypeDefOp, ops as b, type_def,
};
use tir::ptr::PtrType;
use tir::{Context, DataLayout, Operation, TypeArg, TypeId};

use super::{IntegerKind, TargetProfile, TypeInterner, TypeKind};
use crate::ast::RecordKind;

/// The record definitions of a translation unit and the target's layout spec
/// the core reads them under.
pub struct Layouts {
    context: Context,
    module: ModuleOp,
    defined: HashSet<String>,
    spec: AttributeValue,
    // A `DataLayout` reads the module's definitions once, so a new definition
    // drops it.
    layout: RefCell<Option<DataLayout>>,
}

impl Layouts {
    pub(super) fn new(context: &Context, spec: AttributeValue) -> Self {
        let module = b::module(context, None).build();
        Self {
            context: context.clone(),
            module,
            defined: HashSet::new(),
            spec,
            layout: RefCell::new(None),
        }
    }

    pub(super) fn define(&mut self, name: &str, kind: RecordKind, fields: &[(&str, TypeArg)]) {
        let kind = match kind {
            RecordKind::Struct => AggregateKind::Struct,
            RecordKind::Union => AggregateKind::Union,
        };
        let body = AggregateType::get(&self.context, kind, fields).expect("a record body");
        self.module
            .body()
            .append_op(type_def(&self.context, name, Some(body)).build());
        self.defined.insert(name.to_string());
        *self.layout.get_mut() = None;
    }

    /// The context the record types are interned in.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The module the unit is generated into. It holds a `type.def` for every
    /// record.
    pub fn module(&self) -> &ModuleOp {
        &self.module
    }

    // Records still without a definition get an incomplete one.
    pub(super) fn declare_incomplete<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        for name in names.filter(|name| !self.defined.contains(*name)) {
            self.module
                .body()
                .append_op(type_def(&self.context, name, None).build());
        }
    }

    // An identifier the IR does not spell a type of its own with.
    pub(super) fn is_free_name(&self, tag: &str) -> bool {
        is_identifier(tag) && !TypeDefOp::is_core_type_name(&self.context, tag)
    }

    /// Size and alignment of `ty`, in bytes.
    pub fn size_align(&self, ty: TypeId) -> Option<(u64, u64)> {
        let layout = self.with_layout(|layout| layout.layout_of(ty))?;
        Some((layout.size / 8, layout.align / 8))
    }

    /// The offset of each field of the record `ty`, in bytes.
    pub fn field_offsets(&self, ty: TypeId) -> Option<Vec<u64>> {
        let layout = self.with_layout(|layout| layout.layout_of(ty))?;
        Some(layout.fields.iter().map(|offset| offset / 8).collect())
    }

    fn with_layout<T>(&self, query: impl FnOnce(&DataLayout) -> T) -> T {
        let mut layout = self.layout.borrow_mut();
        let layout = layout.get_or_insert_with(|| {
            DataLayout::for_op_with_default(&self.context, self.module.id(), Some(&self.spec))
                .expect("the target's spec is a layout")
        });
        query(layout)
    }

    // The type the core lays `ty` out as; `record` names a record's type.
    pub(super) fn type_of(
        &self,
        types: &TypeInterner,
        target: TargetProfile,
        record: &dyn Fn(crate::ast::RecordId) -> Option<TypeId>,
        ty: super::QualType,
    ) -> Option<TypeId> {
        Some(match types.kind(ty) {
            TypeKind::Integer(kind) => {
                let terms = self.context.terms();
                terms.int(u64::from(target.integer_width(*kind)))
            }
            TypeKind::Enum(_) => {
                let terms = self.context.terms();
                terms.int(u64::from(target.integer_width(IntegerKind::Int)))
            }
            TypeKind::Float => FloatType::f32(&self.context),
            TypeKind::Double => FloatType::f64(&self.context),
            TypeKind::LongDouble => target.long_double(&self.context),
            TypeKind::Pointer(_) => PtrType::opaque(&self.context),
            TypeKind::Array(element, Some(length)) => {
                let element = self.type_of(types, target, record, *element)?;
                let array = vec![TypeArg::Nat(*length), TypeArg::Type(element)];
                NamedType::get(&self.context, "array", array).ok()?
            }
            TypeKind::Record(id) => record(*id)?,
            TypeKind::Array(_, None)
            | TypeKind::Void
            | TypeKind::Function { .. }
            | TypeKind::Error => {
                return None;
            }
        })
    }

    // The type of the record called `name`.
    pub(super) fn record_type(&self, name: &str) -> TypeId {
        NamedType::get(&self.context, name, vec![]).expect("a record type")
    }
}

fn is_identifier(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
