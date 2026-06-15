//! The `ptr` dialect: pointers plus the loads and stores that read and write
//! through them. It is deliberately tiny — just enough to express the
//! memory-based (non-SSA) lowering a simple C frontend produces, where every
//! local lives in a stack slot. There is no pointer arithmetic and loads/stores
//! carry no offset.

use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{
    Context, Error, IRFormatter, MemoryRead, MemoryWrite, Operation, PromotableAllocation, Type,
    TypeId, dialect, operation, parse::Span,
};

use crate as tir;
use crate::Any as AnyConstraint;

pub mod ops {
    pub use super::{AllocaOp, LoadOp, StoreOp, alloca, load, store};
}

dialect! {
    PtrDialect {
        name: "ptr",
        operations: [
            AllocaOp,
            LoadOp,
            StoreOp,
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
            (Some(a), Some(b)) => a.eq(b.as_ref()),
            _ => false,
        }
    }
}

operation! {
    AllocaOp {
        name: "alloca",
        dialect: "ptr",
        results: R {
            result: "crate::ptr::PtrType",
        },
        interfaces: [PromotableAllocation],
    }
}

operation! {
    LoadOp {
        name: "load",
        dialect: "ptr",
        operands: O {
            ptr: "crate::ptr::PtrType",
        },
        results: R {
            result: "AnyConstraint",
        },
        interfaces: [MemoryRead],
    }
}

operation! {
    StoreOp {
        name: "store",
        dialect: "ptr",
        operands: O {
            value: "AnyConstraint",
            ptr: "crate::ptr::PtrType",
        },
        interfaces: [MemoryWrite],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::IntegerType;

    #[test]
    fn opaque_and_typed_pointer_roundtrip() {
        let context = Context::with_default_dialects();

        let opaque = PtrType::opaque(&context);
        assert_eq!(context.type_to_string(opaque), "!ptr.p");

        let i32_ty = IntegerType::new(&context, 32);
        let typed = PtrType::typed(&context, i32_ty);
        assert_eq!(context.type_to_string(typed), "!ptr.p<!i32>");

        // Typed pointer remembers its pointee.
        let data = context.get_type_data(typed);
        let ptr = (data.as_ref() as &dyn Any)
            .downcast_ref::<PtrType>()
            .unwrap();
        assert_eq!(ptr.pointee(&context), Some(i32_ty));

        // An opaque pointer carries no pointee.
        let opaque_data = context.get_type_data(opaque);
        let opaque_ptr = (opaque_data.as_ref() as &dyn Any)
            .downcast_ref::<PtrType>()
            .unwrap();
        assert_eq!(opaque_ptr.pointee(&context), None);

        // Typed and opaque pointers are distinct, identical ones are interned.
        assert_ne!(opaque, typed);
        assert_eq!(PtrType::typed(&context, i32_ty), typed);
    }

    // The alloca/store/load roundtrip inside a function is covered by the
    // FileCheck suite at core/checks/IRRoundtrip/ptr-alloca-store-load.tir.
}
