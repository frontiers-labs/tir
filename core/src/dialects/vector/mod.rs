//! The `vector` dialect: a small, target-independent vocabulary for SIMD-style
//! arithmetic. A [`VectorType`] is either statically sized (`vec<8xi32>`) or
//! dynamically sized (`vec<i32>`); a dynamic vector's elementwise operation takes
//! a vector-length (`vl`) operand bounding the active lanes, while a static one
//! reads its lane count straight from the type.

use std::any::Any;
use std::sync::Arc;

use crate::ty::TypeConstraint;
use crate::{Context, Error, IRFormatter, Type, TypeId, dialect, operation, parse::Span};

use crate as tir;

pub mod ops {
    pub use super::{AddOp, MulOp, SubOp, add, mul, sub};
}

dialect! {
    VectorDialect {
        name: "vector",
        operations: [
            AddOp,
            SubOp,
            MulOp,
        ],
        types: [VectorType],
    }
}

/// A vector type, written `vec<8xi32>` (static) or `vec<i32>` (dynamic). A static
/// vector fixes its lane count; a dynamic one leaves it unknown until run time.
pub struct VectorType {
    element: Arc<dyn Type>,
    length: Option<u32>,
}

impl VectorType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, element: TypeId, length: Option<u32>) -> TypeId {
        let element = context.get_type_data(element);
        context.get_type_id(Arc::new(Self { element, length }))
    }

    /// A statically sized vector of `length` lanes.
    pub fn fixed(context: &Context, element: TypeId, length: u32) -> TypeId {
        Self::new(context, element, Some(length))
    }

    /// A dynamically sized vector whose lane count is unknown at compile time.
    pub fn dynamic(context: &Context, element: TypeId) -> TypeId {
        Self::new(context, element, None)
    }

    pub fn element(&self, context: &Context) -> TypeId {
        context.get_type_id(self.element.clone())
    }

    pub fn length(&self) -> Option<u32> {
        self.length
    }
}

impl TypeConstraint for VectorType {}

impl Type for VectorType {
    fn dialect(&self) -> &'static str {
        "vector"
    }

    fn parse_key() -> &'static str {
        "vec"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        if !parser.parse_token("<") {
            return Err((parser.span(), Error::ExpectedToken("<")));
        }
        // A leading `N x` gives a static length; its absence means dynamic.
        let length = if let Some(n) = parser.parse_number() {
            if n < 0 || !parser.parse_token("x") {
                return Err((parser.span(), Error::ExpectedToken("x")));
            }
            Some(n as u32)
        } else {
            None
        };
        let name = parser
            .parse_ident()
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        let element = context
            .parse_type_mnemonic("builtin", name)
            .map_err(|err| (parser.span(), err))?;
        if !parser.parse_token(">") {
            return Err((parser.span(), Error::ExpectedToken(">")));
        }
        Ok(Self::new(context, element, length))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("vec<")?;
        if let Some(length) = self.length {
            fmt.write(format!("{length}x"))?;
        }
        self.element.print(fmt)?;
        fmt.write(">")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        let Some(other) = (other as &dyn Any).downcast_ref::<VectorType>() else {
            return false;
        };
        self.length == other.length && self.element.eq(other.element.as_ref())
    }
}

operation! {
    AddOp {
        name: "add",
        dialect: "vector",
        operands: O {
            lhs: "crate::vector::VectorType",
            rhs: "crate::vector::VectorType",
            vl: "?crate::builtin::IndexType",
        },
        results: R {
            result: "crate::vector::VectorType",
        },
        sem: "(set result (concat (map (zip (split lhs $get_vlen) (split rhs $get_vlen)) (lambda (a b) (add a b)))))",
    }
}

operation! {
    SubOp {
        name: "sub",
        dialect: "vector",
        operands: O {
            lhs: "crate::vector::VectorType",
            rhs: "crate::vector::VectorType",
            vl: "?crate::builtin::IndexType",
        },
        results: R {
            result: "crate::vector::VectorType",
        },
        sem: "(set result (concat (map (zip (split lhs $get_vlen) (split rhs $get_vlen)) (lambda (a b) (sub a b)))))",
    }
}

operation! {
    MulOp {
        name: "mul",
        dialect: "vector",
        operands: O {
            lhs: "crate::vector::VectorType",
            rhs: "crate::vector::VectorType",
            vl: "?crate::builtin::IndexType",
        },
        results: R {
            result: "crate::vector::VectorType",
        },
        sem: "(set result (concat (map (zip (split lhs $get_vlen) (split rhs $get_vlen)) (lambda (a b) (mul a b)))))",
    }
}

/// The active lane count for `$get_vlen`, the `n` each `split` cuts the operand
/// bits into: a fixed vector takes it from the result type's static length (a
/// constant); a scalable vector takes it from the dynamic `vl` operand (index 2).
fn vlen_node(
    op: &tir::OpInstance,
    g: &mut impl tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
) -> tir::graph::NodeId {
    if op.operands.len() > 2 {
        let n = g.add_node(tir::sem_expr::ExprKind::Symbol);
        g.set_leaf_data(n, tir::sem_expr::ExprPayload::SymbolId(2));
        return n;
    }
    let context = op.context.upgrade();
    let ty = context.get_value(op.results[0]).ty();
    let length = (context.get_type_data(ty).as_ref() as &dyn Any)
        .downcast_ref::<VectorType>()
        .and_then(|t| t.length())
        .unwrap_or(0) as u64;
    let n = g.add_node(tir::sem_expr::ExprKind::Constant);
    g.set_leaf_data(
        n,
        tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new(32, length)),
    );
    n
}

macro_rules! impl_get_vlen {
    ($op:ty) => {
        impl $op {
            fn get_vlen(
                &self,
                g: &mut impl tir::graph::MutDag<
                    Node = tir::sem_expr::ExprKind,
                    Leaf = tir::sem_expr::ExprPayload,
                >,
            ) -> tir::graph::NodeId {
                vlen_node(&self.0, g)
            }
        }
    };
}

impl_get_vlen!(AddOp);
impl_get_vlen!(SubOp);
impl_get_vlen!(MulOp);
