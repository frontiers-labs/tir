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
    ResourceEffect, ResourceEffects, TypeId,
    attributes::{AttributeValue, Predicate},
    dialect, operation,
};

use crate as tir;

pub mod ops {
    pub use super::{
        AllocaOp, CmpOp, CmpOpBuilder, CopyOp, DisjointOp, FieldOp, LoadOp, MemcpyOp, MemsetOp,
        NullOp, PtrAddOp, PtrDiffOp, StoreOp, alloca, cmp, copy, disjoint, field, load, memcpy,
        memset, null, ptradd, ptrdiff, store,
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

operation! {
    AllocaOp {
        name: "alloca",
        dialect: "ptr",
        scheme: "() -> ptr.p<_>",
        attributes: A {
            size: "UInt",
            align: "UInt",
        },
        results: R { result },
        interfaces: [PromotableAllocation, crate::interp::Interp],
        state: "out",
    }
}

operation! {
    NullOp {
        name: "null",
        dialect: "ptr",
        scheme: "() -> ptr.p<_>",
        results: R { result },
        interfaces: [crate::ConstantLike],
        sem: "(set result $null_address)",
    }
}

impl crate::ConstantLike for NullOp {
    fn constant_value(&self) -> tir::utils::APInt {
        let context = self.0.context.clone();
        let layout = crate::DataLayout::for_instance(&context, &self.0);
        let width = layout.and_then(|layout| layout.pointer_size());
        tir::utils::APInt::new(u64::from(width.unwrap_or(64)), 0)
    }
}

impl NullOp {
    fn null_address(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        let context = self.0.context.clone();
        let width = crate::DataLayout::for_instance(&context, &self.0)?.pointer_size()?;
        let node = g.add_node(tir::sem::SymKind::Constant);
        g.set_leaf_data(
            node,
            tir::sem::SymPayload::Int(tir::utils::APInt::new(u64::from(width), 0)),
        );
        Some(node)
    }
}

operation! {
    CmpOp {
        name: "cmp",
        dialect: "ptr",
        scheme: "(ptr.p<_>, ptr.p<_>) -> int<1>",
        attributes: A {
            predicate: "Predicate in POINTER",
        },
        operands: O { lhs, rhs },
        results: R { result },
        sem: "(set result $cmp_expr)",
    }
}

impl CmpOp {
    fn cmp_expr(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        let context = self.0.context.clone();
        crate::DataLayout::for_instance(&context, &self.0)?.pointer_size()?;

        crate::builtin::compare_expr(g, predicate(self)?, true)
    }
}

fn predicate(op: &impl Operation) -> Option<Predicate> {
    match op.attr("predicate")? {
        AttributeValue::Predicate(value) => Some(value),
        _ => None,
    }
}

// `base + offset` modulo 2^W, W the pointer width. The offset is exactly W bits
// wide: a narrower or wider index is sign- or zero-extended or truncated by an
// op of its own, so the IR spells which one the source language means.
operation! {
    PtrAddOp {
        name: "ptradd",
        dialect: "ptr",
        scheme: "(ptr.p<_>, int<_>) -> ptr.p<_>",
        verifier: "true",
        operands: O { base, offset },
        results: R { result },
        interfaces: [crate::Additive],
        sem: "(set result (add base offset))",
    }
}

impl crate::Additive for PtrAddOp {}

// `lhs - rhs` modulo 2^W, as a W-bit integer, W the pointer width. A narrower
// or wider result is a truncation or extension of its own.
operation! {
    PtrDiffOp {
        name: "ptrdiff",
        dialect: "ptr",
        scheme: "(ptr.p<_>, ptr.p<_>) -> int<_>",
        verifier: "true",
        operands: O { lhs, rhs },
        results: R { result },
        interfaces: [crate::Subtractive],
        sem: "(set result (sub lhs rhs))",
    }
}

impl crate::Subtractive for PtrDiffOp {}

impl tir::Verifiable for PtrAddOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        pointer_sized(context, &self.0, &[self.operands()[1]], "offset")
    }
}

impl tir::Verifiable for PtrDiffOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        pointer_sized(context, &self.0, &[self.result()], "result")
    }
}

/// The integer type of a pointer offset at `op`: as wide as the pointer the
/// layout in scope declares. With no layout nothing fixes the width or checks
/// it, so it is 64 bits, the width every byte count in this dialect has.
pub fn offset_type(context: &Context, op: crate::OpId) -> TypeId {
    let width = crate::DataLayout::for_op(context, op)
        .and_then(|layout| layout.pointer_size())
        .unwrap_or(64);
    crate::builtin::IntegerType::new(context, u64::from(width))
}

fn pointer_sized(
    context: &Context,
    op: &tir::OpHandle,
    values: &[crate::ValueId],
    what: &str,
) -> Result<(), Error> {
    let Some(width) =
        crate::DataLayout::for_instance(context, op).and_then(|layout| layout.pointer_size())
    else {
        return Ok(());
    };
    for &value in values {
        let actual = crate::builtin::IntegerType::view(context, context.get_value(value).ty())
            .map(|int| int.width());
        if actual != Some(u64::from(width)) {
            return Err(Error::VerificationError(format!(
                "ptr.{} {what} must be {width} bits wide, the pointer width",
                op.name()
            )));
        }
    }
    Ok(())
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
operation! {
    DisjointOp {
        name: "disjoint",
        dialect: "ptr",
        scheme: "(ptr.p<_>, int<_>, ptr.p<_>, int<_>) -> int<1>",
        verifier: "true",
        operands: O { lhs, lhs_size, rhs, rhs_size },
        results: R { result },
        sem: "(set result (or (ule (add lhs lhs_size) rhs) (ule (add rhs rhs_size) lhs)))",
    }
}

impl tir::Verifiable for DisjointOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let operands = self.operands();
        let (lhs_size, rhs_size) = (operands[1], operands[3]);
        if context.get_value(lhs_size).ty() != context.get_value(rhs_size).ty() {
            return Err(Error::VerificationError(
                "ptr.disjoint sizes must have one type".to_string(),
            ));
        }
        pointer_sized(context, &self.0, &[lhs_size, rhs_size], "size")
    }
}

operation! {
    LoadOp {
        name: "load",
        dialect: "ptr",
        scheme: "(ptr.p<_>) -> _",
        verifier: "true",
        operands: O { ptr },
        results: R { result },
        interfaces: [MemoryRead, ResourceEffects, crate::interp::Interp],
        state: "in_out",
    }
}

operation! {
    StoreOp {
        name: "store",
        dialect: "ptr",
        scheme: "(_, ptr.p<_>) -> ()",
        verifier: "true",
        operands: O { value, ptr },
        interfaces: [MemoryWrite, ResourceEffects, crate::interp::Interp],
        state: "in_out",
    }
}

impl tir::Verifiable for LoadOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        reject_function_value(context, self.result(), "load")
    }
}

impl tir::Verifiable for StoreOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        reject_function_value(context, self.operands()[0], "store")
    }
}

fn reject_function_value(context: &Context, value: crate::ValueId, op: &str) -> Result<(), Error> {
    match crate::builtin::FnType::view(context, context.get_value(value).ty()) {
        Some(_) => Err(Error::VerificationError(format!(
            "{op} moves a function value through memory"
        ))),
        None => Ok(()),
    }
}

// Copy exactly `size` bytes between non-overlapping ranges. A zero-sized copy
// accesses neither pointer. This operation has no volatile or atomic semantics.
operation! {
    MemcpyOp {
        name: "memcpy",
        dialect: "ptr",
        scheme: "(ptr.p<_>, ptr.p<_>, int<64>) -> ()",
        operands: O { destination, source, size },
        interfaces: [ResourceEffects, crate::interp::Interp, crate::Intrinsic],
        state: "in_out",
    }
}

operation! {
    MemsetOp {
        name: "memset",
        dialect: "ptr",
        scheme: "(ptr.p<_>, int<8>, int<64>) -> ()",
        operands: O { destination, value, size },
        interfaces: [MemoryWrite, ResourceEffects, crate::interp::Interp, crate::Intrinsic],
        state: "in_out",
    }
}

// The address of field `field` of the `ty` that `base` points at: a field by
// name, never a byte offset, which the layout in scope gives.
operation! {
    FieldOp {
        name: "field",
        dialect: "ptr",
        scheme: "(ptr.p<_>) -> ptr.p<_>",
        attributes: A {
            ty: "Type",
            field: "Str",
        },
        operands: O { base },
        results: R { result },
        interfaces: [crate::interp::Interp, crate::Speculatable],
    }
}

impl crate::Speculatable for FieldOp {}

// Copy one `ty` from `source` to `destination`: a struct assignment, as many
// bytes as the layout in scope gives `ty`.
operation! {
    CopyOp {
        name: "copy",
        dialect: "ptr",
        scheme: "(ptr.p<_>, ptr.p<_>) -> ()",
        attributes: A {
            ty: "Type",
        },
        operands: O { destination, source },
        interfaces: [ResourceEffects, crate::Congruent, crate::interp::Interp],
        state: "in_out",
    }
}

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
