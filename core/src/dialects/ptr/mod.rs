//! The `ptr` dialect: pointers plus the loads and stores that read and write
//! through them. It is deliberately tiny — just enough to express the
//! memory-based (non-SSA) lowering a simple C frontend produces, where every
//! local lives in a stack slot. Pointer arithmetic is byte-based and loads/stores
//! carry no offset.

use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{
    Context, Error, IRFormatter, MemoryRead, MemoryWrite, Operation, PromotableAllocation, Type,
    TypeId, attributes::AttributeValue, dialect, operation, parse::Span,
};

use crate as tir;
use crate::Any as AnyConstraint;

pub mod ops {
    pub use super::{
        AllocaOp, CmpOp, CmpOpBuilder, LoadOp, MemcpyOp, MemsetOp, NullOp, PtrAddOp, PtrDiffOp,
        StoreOp, alloca, cmp, load, memcpy, memset, null, ptradd, ptrdiff, store,
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
            LoadOp,
            StoreOp,
            MemcpyOp,
            MemsetOp,
        ],
        types: [PtrType],
    }
}

/// A pointer type, written `!ptr.p` (opaque) or `!ptr.p<!i32>` (typed). A typed
/// pointer remembers its pointee so loads can recover their result type; an
/// opaque pointer carries no pointee.
pub struct PtrType {
    pointee: Option<Arc<dyn Type>>,
}

impl PtrType {
    /// An opaque pointer: `!ptr.p`.
    pub fn opaque(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self { pointee: None }))
    }

    /// A typed pointer to `pointee`: `!ptr.p<!pointee>`.
    pub fn typed(context: &Context, pointee: TypeId) -> TypeId {
        let pointee = context.get_type_data(pointee);
        context.get_type_id(Arc::new(Self {
            pointee: Some(pointee),
        }))
    }

    /// The pointee type id, or `None` for an opaque pointer.
    pub fn pointee(&self, context: &Context) -> Option<TypeId> {
        self.pointee
            .as_ref()
            .map(|p| context.get_type_id(p.clone()))
    }
}

// PtrType stores its pointee as an `Arc<dyn Type>`, which `#[derive(TirType)]`
// cannot map, so its schema is registered by hand. The builder accepts zero
// arguments (opaque) or one pointee type (typed).
#[crate::linkme::distributed_slice(crate::TYPE_SCHEMAS)]
#[linkme(crate = crate::linkme)]
static PTR_TYPE_SCHEMA: crate::TypeSchema = crate::TypeSchema {
    dialect: "ptr",
    name: "p",
    params: &[crate::TypeParam {
        name: "pointee",
        kind: crate::TypeParamKind::Type,
    }],
    build: |context, args| match args {
        [] => Ok(PtrType::opaque(context)),
        [crate::TypeArg::Type(pointee)] => Ok(PtrType::typed(context, *pointee)),
        _ => Err("type 'ptr.p' expects an optional pointee type".to_string()),
    },
};

impl TypeConstraint for PtrType {}

impl Type for PtrType {
    fn dialect(&self) -> &'static str {
        "ptr"
    }

    fn parse_key() -> &'static str {
        "p"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        if parser.parse_token("<") {
            let pointee = parser
                .parse_type(context)?
                .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
            if !parser.parse_token(">") {
                return Err((parser.span(), Error::ExpectedToken(">")));
            }
            Ok(Self::typed(context, pointee))
        } else {
            Ok(Self::opaque(context))
        }
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("p")?;
        if let Some(pointee) = &self.pointee {
            fmt.write("<!")?;
            if pointee.dialect() != "builtin" {
                fmt.write(format!("{}.", pointee.dialect()))?;
            }
            pointee.print(fmt)?;
            fmt.write(">")?;
        }
        Ok(())
    }

    fn eq(&self, other: &dyn Type) -> bool {
        let Some(other) = (other as &dyn Any).downcast_ref::<PtrType>() else {
            return false;
        };
        match (&self.pointee, &other.pointee) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write_usize(pointee_key(self.pointee.as_ref()));
    }
}

fn pointee_key(pointee: Option<&Arc<dyn Type>>) -> usize {
    pointee.map_or(0, |p| Arc::as_ptr(p) as *const () as usize)
}

operation! {
    AllocaOp {
        name: "alloca",
        dialect: "ptr",
        attributes: A {
            size: "UInt",
            align: "UInt",
        },
        results: R {
            result: "crate::ptr::PtrType",
        },
        interfaces: [PromotableAllocation, crate::interp::Interp],
        state: "out",
    }
}

impl AllocaOpBuilder {
    pub fn size(self, size: u64) -> Self {
        self.attr("size", AttributeValue::UInt(size))
    }

    pub fn align(self, align: u64) -> Self {
        self.attr("align", AttributeValue::UInt(align))
    }
}

impl AllocaOp {
    pub fn size(&self) -> u64 {
        uint_attribute(self, "size")
    }

    pub fn align(&self) -> u64 {
        uint_attribute(self, "align")
    }
}

fn uint_attribute(op: &impl Operation, name: &str) -> u64 {
    match op.attr(name) {
        Some(AttributeValue::UInt(value)) => value,
        _ => panic!("{name} must be an unsigned integer attribute"),
    }
}

operation! {
    NullOp {
        name: "null",
        dialect: "ptr",
        results: R {
            result: "crate::ptr::PtrType",
        },
        sem: "(set result $null_address)",
    }
}

impl NullOp {
    /// The null pointer: the all-zero address, at the pointer width the data
    /// layout in scope declares. Without a layout the address width is unknown,
    /// so the op offers no semantics rather than guessing one.
    fn null_address(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        let context = self.0.context.upgrade();
        let width = crate::DataLayout::for_instance(&context, &self.0)?.pointer_size()?;
        let node = g.add_node(tir::sem::SymKind::Constant);
        g.set_leaf_data(
            node,
            tir::sem::SymPayload::Int(tir::utils::APInt::new(width, 0)),
        );
        Some(node)
    }
}

operation! {
    CmpOp {
        name: "cmp",
        dialect: "ptr",
        verifier: "true",
        attributes: A {
            predicate: "Str",
        },
        operands: O {
            lhs: "crate::ptr::PtrType",
            rhs: "crate::ptr::PtrType",
        },
        results: R {
            result: "crate::Integer<1>",
        },
        sem: "(set result $cmp_expr)",
    }
}

/// Predicates a pointer comparison can express. Addresses are unsigned, so a
/// signed comparison of two of them has no meaning to declare.
const POINTER_PREDICATES: [&str; 6] = ["eq", "ne", "ult", "ule", "ugt", "uge"];

impl CmpOp {
    /// The comparison of the two addresses, as unsigned integers at the width
    /// the data layout in scope declares for a pointer. Without a layout the
    /// address width is unknown, so the op offers no semantics rather than
    /// guessing one.
    fn cmp_expr(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        use tir::sem::SymKind;

        let context = self.0.context.upgrade();
        crate::DataLayout::for_instance(&context, &self.0)?.pointer_size()?;

        let (kind, swap) = match predicate(self)?.as_str() {
            "eq" => (SymKind::Eq, false),
            "ne" => (SymKind::Ne, false),
            "ult" => (SymKind::ULt, false),
            "ugt" => (SymKind::ULt, true),
            "uge" => (SymKind::UGe, false),
            "ule" => (SymKind::UGe, true),
            _ => return None,
        };

        let mut operand = |index: u32| {
            let leaf = g.add_node(SymKind::Symbol);
            g.set_leaf_data(leaf, tir::sem::SymPayload::SymbolId(index));
            leaf
        };
        let (lhs, rhs) = if swap {
            (operand(1), operand(0))
        } else {
            (operand(0), operand(1))
        };
        let node = g.add_node(kind);
        g.add_edge(node, lhs);
        g.add_edge(node, rhs);
        Some(node)
    }
}

impl CmpOpBuilder {
    pub fn predicate(self, pred: &str) -> Self {
        self.attr("predicate", AttributeValue::Str(pred.to_string().into()))
    }
}

impl tir::Verifiable for CmpOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        match predicate(self) {
            Some(pred) if POINTER_PREDICATES.contains(&pred.as_str()) => Ok(()),
            Some(pred) => Err(Error::VerificationError(format!(
                "ptr.cmp predicate '{pred}' is not a pointer comparison (expected {})",
                POINTER_PREDICATES.join(", ")
            ))),
            None => Err(Error::VerificationError(
                "ptr.cmp requires a string 'predicate' attribute".to_string(),
            )),
        }
    }
}

fn predicate(op: &impl Operation) -> Option<String> {
    match op.attr("predicate")? {
        AttributeValue::Str(value) => Some(value.into_string()),
        _ => None,
    }
}

operation! {
    PtrAddOp {
        name: "ptradd",
        dialect: "ptr",
        operands: O {
            base: "crate::ptr::PtrType",
            offset: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::ptr::PtrType",
        },
        sem: "(set result (add base offset))",
    }
}

operation! {
    PtrDiffOp {
        name: "ptrdiff",
        dialect: "ptr",
        operands: O {
            lhs: "crate::ptr::PtrType",
            rhs: "crate::ptr::PtrType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (sub lhs rhs))",
    }
}

operation! {
    LoadOp {
        name: "load",
        dialect: "ptr",
        verifier: "true",
        operands: O {
            ptr: "crate::ptr::PtrType",
        },
        results: R {
            result: "AnyConstraint",
        },
        interfaces: [MemoryRead, crate::interp::Interp],
        state: "in_out",
    }
}

operation! {
    StoreOp {
        name: "store",
        dialect: "ptr",
        verifier: "true",
        operands: O {
            value: "AnyConstraint",
            ptr: "crate::ptr::PtrType",
        },
        interfaces: [MemoryWrite, crate::interp::Interp],
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

/// A λ value names a function, not a machine word: it never lives in memory.
/// A language that stores function pointers converts to and from `!ptr.p`
/// around the store.
fn reject_function_value(context: &Context, value: crate::ValueId, op: &str) -> Result<(), Error> {
    let ty = context.get_type_data(context.get_value(value).ty());
    match (ty.as_ref() as &dyn Any).downcast_ref::<crate::builtin::FnType>() {
        Some(_) => Err(Error::VerificationError(format!(
            "{op} moves a function value through memory"
        ))),
        None => Ok(()),
    }
}

operation! {
    MemcpyOp {
        name: "memcpy",
        dialect: "ptr",
        verifier: "true",
        operands: O {
            destination: "crate::ptr::PtrType",
            source: "crate::ptr::PtrType",
            size: "crate::builtin::IntegerType",
        },
        interfaces: [crate::interp::Interp],
        state: "in_out",
    }
}

impl tir::Verifiable for MemcpyOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let size_type = context.get_type_data(context.get_value(self.operands()[2]).ty());
        let Some(size_type) =
            (size_type.as_ref() as &dyn Any).downcast_ref::<crate::builtin::IntegerType>()
        else {
            return Err(Error::VerificationError(
                "ptr.memcpy size must have integer type".to_string(),
            ));
        };
        if size_type.width() != 64 {
            return Err(Error::VerificationError(
                "ptr.memcpy size must have type i64".to_string(),
            ));
        }
        Ok(())
    }
}

operation! {
    MemsetOp {
        name: "memset",
        dialect: "ptr",
        verifier: "true",
        operands: O {
            destination: "crate::ptr::PtrType",
            value: "crate::builtin::IntegerType",
            size: "crate::builtin::IntegerType",
        },
        interfaces: [MemoryWrite, crate::interp::Interp],
        state: "in_out",
    }
}

impl tir::Verifiable for MemsetOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let value_type = context.get_type_data(context.get_value(self.operands()[1]).ty());
        let value_type = (value_type.as_ref() as &dyn Any)
            .downcast_ref::<crate::builtin::IntegerType>()
            .unwrap();
        if value_type.width() != 8 {
            return Err(Error::VerificationError(
                "ptr.memset value must have type i8".to_string(),
            ));
        }
        let size_type = context.get_type_data(context.get_value(self.operands()[2]).ty());
        let size_type = (size_type.as_ref() as &dyn Any)
            .downcast_ref::<crate::builtin::IntegerType>()
            .unwrap();
        if size_type.width() != 64 {
            return Err(Error::VerificationError(
                "ptr.memset size must have type i64".to_string(),
            ));
        }
        Ok(())
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

    fn state_operand(&self) -> Option<tir::ValueId> {
        LoadOp::state_operand(self)
    }

    fn state_result(&self) -> Option<tir::ValueId> {
        LoadOp::state_result(self)
    }
}

impl MemoryWrite for StoreOp {
    fn write_location(&self) -> tir::ValueId {
        self.operands()[1]
    }

    fn written_value(&self) -> tir::ValueId {
        self.operands()[0]
    }

    fn state_operand(&self) -> Option<tir::ValueId> {
        StoreOp::state_operand(self)
    }

    fn state_result(&self) -> Option<tir::ValueId> {
        StoreOp::state_result(self)
    }
}

impl MemoryWrite for MemsetOp {
    fn write_location(&self) -> tir::ValueId {
        self.operands()[0]
    }

    fn written_value(&self) -> tir::ValueId {
        self.operands()[1]
    }

    fn state_operand(&self) -> Option<tir::ValueId> {
        MemsetOp::state_operand(self)
    }

    fn state_result(&self) -> Option<tir::ValueId> {
        MemsetOp::state_result(self)
    }
}
