//! The `ptr` dialect: pointers plus the loads and stores that read and write
//! through them. It is deliberately tiny — just enough to express the
//! memory-based (non-SSA) lowering a simple C frontend produces, where every
//! local lives in a stack slot. Pointer arithmetic is byte-based and loads/stores
//! carry no offset. Every integer added to or taken from a pointer is exactly as
//! wide as the pointer, the width the data layout gives `p`, so a constant added
//! to an offset moves to the address unchanged and no width change is implicit.

mod intrinsics;

use tir_symbolic::term::{no_carrier, valid};

use crate::ty::{Ctor, Kind, Param, Terms, TypeArg, TypeCtor, TypeSpelling};
use crate::{
    Context, Error, MemoryRead, MemoryWrite, Operation, PromotableAllocation, ResourceAccess,
    ResourceEffect, ResourceEffects, TypeId, dialect,
};

use crate as tir;

pub mod ops {
    pub use super::{
        AllocaOp, CmpOp, CopyOp, DisjointOp, FieldOp, LoadOp, MemcpyOp, MemsetOp, NullOp, PtrAddOp,
        PtrDiffOp, StoreOp, alloca, cmp, copy, disjoint, field, load, memcpy, memset, null, ptradd,
        ptrdiff, store,
    };
}

dialect! {
    PtrDialect {
        name: "ptr",
        operations: [
            AllocaOp,
            CmpOp,
            NullOp,
            PtrAddOp,
            PtrDiffOp,
            DisjointOp,
            LoadOp,
            StoreOp,
            MemcpyOp,
            MemsetOp,
            FieldOp,
            CopyOp,
        ],
        types: [PtrType],
    }
}

static PTR: Ctor = Ctor::new(TypeCtor {
    dialect: "ptr",
    name: "p",
    params: &[("pointee", Param::Opt(Kind::Data))],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

/// A pointer type, written `!ptr.p` (opaque) or `!ptr.p<!i32>` (typed). A typed
/// pointer remembers its pointee so loads can recover their result type; an
/// opaque pointer carries no pointee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtrType {
    pointee: Option<TypeId>,
}

pub(crate) fn opaque_pointer(terms: &Terms) -> TypeId {
    terms
        .intern(PTR.id(terms), vec![])
        .expect("the opaque pointer")
}

impl PtrType {
    /// An opaque pointer: `!ptr.p`.
    pub fn opaque(context: &Context) -> TypeId {
        opaque_pointer(context.terms())
    }

    /// A typed pointer to `pointee`: `!ptr.p<!pointee>`.
    pub fn typed(context: &Context, pointee: TypeId) -> TypeId {
        context
            .terms()
            .intern(PTR.id(context), vec![TypeArg::Type(pointee)])
            .expect("a pointer to a data type")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        PTR.is(terms, ty).then(|| Self {
            pointee: terms.child(ty, 0),
        })
    }

    /// The pointee type id, or `None` for an opaque pointer.
    pub fn pointee(&self) -> Option<TypeId> {
        self.pointee
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: PTR.id(context),
            keys: &["p"],
            parse: |_, parser, context| {
                use tir::parse::common::Cursor;
                if !parser.parse_token("<") {
                    return Ok(Self::opaque(context));
                }
                let pointee = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                Ok(Self::typed(context, pointee))
            },
            print: |spellings, ty, fmt| {
                fmt.write("ptr.p")?;
                if let Some(pointee) = spellings.terms().child(ty, 0) {
                    fmt.write("<")?;
                    spellings.print(pointee, fmt)?;
                    fmt.write(">")?;
                }
                Ok(())
            },
            variadic_tail: false,
        }
    }
}

__cdsl_ptr_AllocaOp!();

__cdsl_ptr_NullOp!();

impl crate::ConstantLike for NullOp {
    fn constant_value(&self) -> tir::utils::APInt {
        let context = self.0.context.clone();
        let layout = crate::DataLayout::for_instance(&context, &self.0);
        let width = layout.and_then(|layout| layout.pointer_size());
        tir::utils::APInt::new(u64::from(width.unwrap_or(64)), 0)
    }
}

__cdsl_ptr_CmpOp!();

// `base + offset` modulo 2^W, W the pointer width. The offset is exactly W bits
// wide: a narrower or wider index is sign- or zero-extended or truncated by an
// op of its own, so the IR spells which one the source language means.
__cdsl_ptr_PtrAddOp!();

impl crate::Additive for PtrAddOp {}

// `lhs - rhs` modulo 2^W, as a W-bit integer, W the pointer width. A narrower
// or wider result is a truncation or extension of its own.
__cdsl_ptr_PtrDiffOp!();

impl crate::Subtractive for PtrDiffOp {}

/// The integer type of a pointer offset at `op`: as wide as the pointer the
/// layout in scope declares. With no layout nothing fixes the width or checks
/// it, so it is 64 bits, the width every byte count in this dialect has.
pub fn offset_type(context: &Context, op: crate::OpId) -> TypeId {
    let width = crate::DataLayout::for_op(context, op)
        .and_then(|layout| layout.pointer_size())
        .unwrap_or(64);
    crate::builtin::IntegerType::new(context, u64::from(width))
}

// `lhs + lhs_size <= rhs || rhs + rhs_size <= lhs`, the addresses unsigned: the
// two ranges share no byte, provided neither wraps past the end of the address
// space, which is what the producer of the op has to know. The sizes are as
// wide as a pointer, since they are added to one. Where a range is
// empty the answer is the formula's, not emptiness's — an empty range inside
// another is not reported disjoint.
//
// The no-alias fact in IR form: whoever proves it — a `restrict` qualifier, a
// runtime range check guarding a copy of a loop — emits this op, and the alias
// analysis reads it by name rather than pattern-matching a tree of compares.
// It reads addresses, not memory, so it is pure and takes no state.
__cdsl_ptr_DisjointOp!();

__cdsl_ptr_LoadOp!();

__cdsl_ptr_StoreOp!();

// Copy exactly `size` bytes between non-overlapping ranges. A zero-sized copy
// accesses neither pointer. This operation has no volatile or atomic semantics.
__cdsl_ptr_MemcpyOp!();

__cdsl_ptr_MemsetOp!();

// The address of field `field` of the `ty` that `base` points at: a field by
// name, never a byte offset, which the layout in scope gives.
__cdsl_ptr_FieldOp!();

impl crate::Speculatable for FieldOp {}

// Copy one `ty` from `source` to `destination`: a struct assignment, as many
// bytes as the layout in scope gives `ty`.
__cdsl_ptr_CopyOp!();

impl crate::Congruent for CopyOp {}

impl ResourceEffects for CopyOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        memory_effect(&self.0, ResourceAccess::Change)
    }
}

impl PromotableAllocation for AllocaOp {
    fn promoted_location(&self) -> tir::ValueId {
        self.result()
    }
}

impl MemoryRead for LoadOp {
    fn read_location(&self) -> tir::ValueId {
        self.operands()[0]
    }

    fn read_value(&self) -> tir::ValueId {
        self.result()
    }
}

impl MemoryWrite for StoreOp {
    fn write_location(&self) -> tir::ValueId {
        self.operands()[1]
    }

    fn written_value(&self) -> tir::ValueId {
        self.operands()[0]
    }
}

impl MemoryWrite for MemsetOp {
    fn write_location(&self) -> tir::ValueId {
        self.operands()[0]
    }

    fn written_value(&self) -> tir::ValueId {
        self.operands()[1]
    }
}

fn memory_effect(op: &tir::OpHandle, access: ResourceAccess) -> Vec<ResourceEffect> {
    vec![ResourceEffect {
        resource: tir::builtin::StateResource::Memory,
        access,
        observed: op.state_operands().to_vec(),
        produced: op.state_results().to_vec(),
    }]
}

impl ResourceEffects for LoadOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        memory_effect(&self.0, ResourceAccess::Read)
    }
}

impl ResourceEffects for StoreOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        memory_effect(&self.0, ResourceAccess::Change)
    }
}

impl ResourceEffects for MemcpyOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        memory_effect(&self.0, ResourceAccess::Change)
    }
}

impl ResourceEffects for MemsetOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        memory_effect(&self.0, ResourceAccess::Change)
    }
}
