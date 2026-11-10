//! The `ptr` dialect: pointers plus the loads and stores that read and write
//! through them. It is deliberately tiny — just enough to express the
//! memory-based (non-SSA) lowering a simple C frontend produces, where every
//! local lives in a stack slot. Pointer arithmetic is byte-based and loads/stores
//! carry no offset. Every integer added to or taken from a pointer is exactly as
//! wide as the pointer, the width the data layout gives `p`, so a constant added
//! to an offset moves to the address unchanged and no width change is implicit.

mod intrinsics;

use tir_symbolic::term::{no_carrier, valid};

use crate::ty::{Ctor, Terms, TypeArg, TypeSpelling};
use crate::{
    Context, Error, MemoryWrite, PromotableAllocation, ResourceAccess, ResourceEffect,
    ResourceEffects, TypeId,
    attributes::{AttributeValue, Predicate},
    dialect,
};

use crate as tir;

pub mod ops {
    pub use super::{
        ALLOCA, CMP, COPY, DISJOINT, FIELD, LOAD, MEMCPY, MEMSET, NULL, PTRADD, PTRDIFF, STORE,
        alloca, cmp, copy, disjoint, field, load, memcpy, memset, null, ptradd, ptrdiff, store,
    };
}

dialect! {
    PtrDialect {
        name: "ptr",
        operations: [
            ALLOCA,
            CMP,
            NULL,
            PTRADD,
            PTRDIFF,
            DISJOINT,
            LOAD,
            STORE,
            MEMCPY,
            MEMSET,
            FIELD,
            COPY,
        ],
        types: [PtrType],
    }
}

static PTR: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_ptr_mod.cdsl", binding: constructors::PTR, hosts: { PTR_validate => valid, PTR_carrier => no_carrier } },
);

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
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser = |_, parser, context| {
                use tir::parse::common::Cursor;
                if !parser.parse_token("<") {
                    return Ok(PtrType::opaque(context));
                }
                let pointee = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                Ok(PtrType::typed(context, pointee))
            };
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                fmt.write("ptr.p")?;
                if let Some(pointee) = spellings.terms().child(ty, 0) {
                    fmt.write("<")?;
                    spellings.print(pointee, fmt)?;
                    fmt.write(">")?;
                }
                Ok(())
            };
            const __TIR_TYPE_CTOR_ID: fn(&::tir::Context) -> ::tir::CtorId =
                |context| PTR.id(context);
            ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_ptr_mod.cdsl", binding: spellings::PtrType, context: context, hosts: { PtrType_parse => __TIR_TYPE_PARSE, PtrType_print => __TIR_TYPE_PRINT, PtrType_ctor_id => __TIR_TYPE_CTOR_ID } }
        }
    }
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_ptr_mod.cdsl",
    hosts: {
        alloca_iface_PromotableAllocation => (PromotableAllocation, &crate::ptr::__TIR_IFACE_AllocaOp_PromotableAllocation),
        alloca_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_AllocaOp_Interp),
        null_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("null_address")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "null_address" => (crate::ptr::null_null_address)(operation, graph),
                    _ => None,
                },
            )
        },
        null_iface_ConstantLike => (crate :: ConstantLike, &crate::ptr::__TIR_IFACE_NullOp_ConstantLike),
        cmp_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[tir::sem::SemRecipeStep::Splice("cmp_expr")],
                root: 0,
            };
            tir::sem::build_recipe_with(
                operation,
                graph,
                &RECIPE,
                |operation, name, graph| match name {
                    "cmp_expr" => (crate::ptr::cmp_cmp_expr)(operation, graph),
                    _ => None,
                },
            )
        },
        ptradd_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Add,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        ptradd_verifier => crate::ptr::ptradd_verify_impl,
        ptradd_iface_Additive => (crate :: Additive, &crate::AdditiveCallbacks::DEFAULT),
        ptrdiff_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Sub,
                        arguments: &[0, 1],
                    },
                ],
                root: 2,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        ptrdiff_verifier => crate::ptr::ptrdiff_verify_impl,
        ptrdiff_iface_Subtractive => (crate :: Subtractive, &crate::SubtractiveCallbacks::DEFAULT),
        disjoint_exec_model => |operation, graph| {
            const RECIPE: tir::sem::SemRecipe = tir::sem::SemRecipe {
                steps: &[
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Operand(1),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Add,
                        arguments: &[0, 1],
                    },
                    tir::sem::SemRecipeStep::Operand(2),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ULe,
                        arguments: &[2, 3],
                    },
                    tir::sem::SemRecipeStep::Operand(2),
                    tir::sem::SemRecipeStep::Operand(3),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Add,
                        arguments: &[5, 6],
                    },
                    tir::sem::SemRecipeStep::Operand(0),
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::ULe,
                        arguments: &[7, 8],
                    },
                    tir::sem::SemRecipeStep::Apply {
                        kind: tir::sem::SymKind::Or,
                        arguments: &[4, 9],
                    },
                ],
                root: 10,
            };
            tir::sem::build_defined_recipe(operation, graph, &RECIPE)
        },
        disjoint_verifier => crate::ptr::disjoint_verify_impl,
        store_verifier => crate::ptr::store_verify_impl,
        store_iface_MemoryWrite => (MemoryWrite, &crate::ptr::__TIR_IFACE_StoreOp_MemoryWrite),
        store_iface_ResourceEffects => (ResourceEffects, &crate::ptr::__TIR_IFACE_StoreOp_ResourceEffects),
        store_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_StoreOp_Interp),
        memcpy_iface_ResourceEffects => (ResourceEffects, &crate::ptr::__TIR_IFACE_MemcpyOp_ResourceEffects),
        memcpy_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_MemcpyOp_Interp),
        memcpy_iface_Intrinsic => (crate :: Intrinsic, &crate::ptr::intrinsics::__TIR_IFACE_MemcpyOp_Intrinsic),
        memset_iface_MemoryWrite => (MemoryWrite, &crate::ptr::__TIR_IFACE_MemsetOp_MemoryWrite),
        memset_iface_ResourceEffects => (ResourceEffects, &crate::ptr::__TIR_IFACE_MemsetOp_ResourceEffects),
        memset_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_MemsetOp_Interp),
        memset_iface_Intrinsic => (crate :: Intrinsic, &crate::ptr::intrinsics::__TIR_IFACE_MemsetOp_Intrinsic),
        field_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_FieldOp_Interp),
        field_iface_Speculatable => (crate :: Speculatable, &crate::SpeculatableCallbacks::DEFAULT),
        copy_iface_ResourceEffects => (ResourceEffects, &crate::ptr::__TIR_IFACE_CopyOp_ResourceEffects),
        copy_iface_Congruent => (crate :: Congruent, &crate::CongruentCallbacks::DEFAULT),
        copy_iface_Interp => (crate :: interp :: Interp, &crate::interp::__TIR_IFACE_CopyOp_Interp),
    },
}

pub(crate) fn null_constantlike_constant_value(operation: &tir::OpHandle) -> tir::utils::APInt {
    let context = operation.context.clone();
    let layout = crate::DataLayout::for_instance(&context, operation);
    let width = layout.and_then(|layout| layout.pointer_size());
    tir::utils::APInt::new(u64::from(width.unwrap_or(64)), 0)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_NullOp_ConstantLike: crate::ConstantLikeCallbacks =
    crate::ConstantLikeCallbacks {
        constant_value: crate::ptr::null_constantlike_constant_value,
    };

pub(crate) fn null_null_address(
    operation: &tir::OpHandle,
    g: &mut tir::sem::SemGraph,
) -> Option<tir::NodeId> {
    let context = operation.context.clone();
    let width = crate::DataLayout::for_instance(&context, operation)?.pointer_size()?;
    let node = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(
        node,
        tir::sem::SymPayload::Int(tir::utils::APInt::new(u64::from(width), 0)),
    );
    Some(node)
}

pub(crate) fn cmp_cmp_expr(
    operation: &tir::OpHandle,
    g: &mut tir::sem::SemGraph,
) -> Option<tir::NodeId> {
    let context = operation.context.clone();
    crate::DataLayout::for_instance(&context, operation)?.pointer_size()?;

    crate::builtin::compare_expr(g, predicate(operation)?, true)
}

fn predicate(op: &tir::OpHandle) -> Option<Predicate> {
    match op.attr("predicate")? {
        AttributeValue::Predicate(value) => Some(value),
        _ => None,
    }
}

// `base + offset` modulo 2^W, W the pointer width. The offset is exactly W bits
// wide: a narrower or wider index is sign- or zero-extended or truncated by an
// op of its own, so the IR spells which one the source language means.

// `lhs - rhs` modulo 2^W, as a W-bit integer, W the pointer width. A narrower
// or wider result is a truncation or extension of its own.

pub(crate) fn ptradd_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    pointer_sized(context, operation, &[operation.operands()[1]], "offset")
}

pub(crate) fn ptrdiff_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    pointer_sized(
        context,
        operation,
        &[crate::ptr::ptrdiff_result(operation)],
        "result",
    )
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

pub(crate) fn disjoint_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let operands = operation.operands();
    let (lhs_size, rhs_size) = (operands[1], operands[3]);
    if context.get_value(lhs_size).ty() != context.get_value(rhs_size).ty() {
        return Err(Error::VerificationError(
            "ptr.disjoint sizes must have one type".to_string(),
        ));
    }
    pointer_sized(context, operation, &[lhs_size, rhs_size], "size")
}

tir::helpers::op_def! {
    file: "defs/ptr-load.cdsl",
    hosts: {
        load_verifier => crate::ptr::load_verify_impl,
        ops::MemoryRead => (crate::MemoryRead, &crate::ptr::__TIR_IFACE_LoadOp_MemoryRead),
        ops::ResourceEffects => (crate::ResourceEffects, &crate::ptr::__TIR_IFACE_LoadOp_ResourceEffects),
        ops::Interp => (crate::interp::Interp, &crate::interp::__TIR_IFACE_LoadOp_Interp),
    },
}

pub(crate) fn load_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    reject_function_value(context, crate::ptr::load_result(operation), "load")
}

pub(crate) fn store_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    reject_function_value(context, operation.operands()[0], "store")
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

// The address of field `field` of the `ty` that `base` points at: a field by
// name, never a byte offset, which the layout in scope gives.

// Copy one `ty` from `source` to `destination`: a struct assignment, as many
// bytes as the layout in scope gives `ty`.

pub(crate) fn copy_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    memory_effect(operation, ResourceAccess::Change)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CopyOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::ptr::copy_resourceeffects_resource_effects,
    };

pub(crate) fn alloca_promotableallocation_promoted_location(
    operation: &tir::OpHandle,
) -> tir::ValueId {
    crate::ptr::alloca_result(operation)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_AllocaOp_PromotableAllocation: crate::PromotableAllocationCallbacks =
    crate::PromotableAllocationCallbacks {
        promoted_location: crate::ptr::alloca_promotableallocation_promoted_location,
    };

pub(crate) fn load_memoryread_read_location(operation: &tir::OpHandle) -> tir::ValueId {
    operation.operands()[0]
}

pub(crate) fn load_memoryread_read_value(operation: &tir::OpHandle) -> tir::ValueId {
    crate::ptr::load_result(operation)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_LoadOp_MemoryRead: crate::MemoryReadCallbacks =
    crate::MemoryReadCallbacks {
        read_location: crate::ptr::load_memoryread_read_location,
        read_value: crate::ptr::load_memoryread_read_value,
    };

pub(crate) fn store_memorywrite_write_location(operation: &tir::OpHandle) -> tir::ValueId {
    operation.operands()[1]
}

pub(crate) fn store_memorywrite_written_value(operation: &tir::OpHandle) -> tir::ValueId {
    operation.operands()[0]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_StoreOp_MemoryWrite: crate::MemoryWriteCallbacks =
    crate::MemoryWriteCallbacks {
        write_location: crate::ptr::store_memorywrite_write_location,
        written_value: crate::ptr::store_memorywrite_written_value,
    };

pub(crate) fn memset_memorywrite_write_location(operation: &tir::OpHandle) -> tir::ValueId {
    operation.operands()[0]
}

pub(crate) fn memset_memorywrite_written_value(operation: &tir::OpHandle) -> tir::ValueId {
    operation.operands()[1]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MemsetOp_MemoryWrite: crate::MemoryWriteCallbacks =
    crate::MemoryWriteCallbacks {
        write_location: crate::ptr::memset_memorywrite_write_location,
        written_value: crate::ptr::memset_memorywrite_written_value,
    };

fn memory_effect(op: &tir::OpHandle, access: ResourceAccess) -> Vec<ResourceEffect> {
    vec![ResourceEffect {
        resource: tir::builtin::StateResource::Memory,
        access,
        observed: op.state_operands().to_vec(),
        produced: op.state_results().to_vec(),
    }]
}

pub(crate) fn load_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    memory_effect(operation, ResourceAccess::Read)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_LoadOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::ptr::load_resourceeffects_resource_effects,
    };

pub(crate) fn store_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    memory_effect(operation, ResourceAccess::Change)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_StoreOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::ptr::store_resourceeffects_resource_effects,
    };

pub(crate) fn memcpy_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    memory_effect(operation, ResourceAccess::Change)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MemcpyOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::ptr::memcpy_resourceeffects_resource_effects,
    };

pub(crate) fn memset_resourceeffects_resource_effects(
    operation: &tir::OpHandle,
) -> Vec<ResourceEffect> {
    memory_effect(operation, ResourceAccess::Change)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MemsetOp_ResourceEffects: crate::ResourceEffectsCallbacks =
    crate::ResourceEffectsCallbacks {
        resource_effects: crate::ptr::memset_resourceeffects_resource_effects,
    };
