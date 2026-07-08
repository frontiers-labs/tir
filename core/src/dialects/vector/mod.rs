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
    pub use super::{AddOp, MulOp, SubOp, VectorLenOp, add, mul, sub, vector_len};
}

dialect! {
    VectorDialect {
        name: "vector",
        operations: [
            AddOp,
            SubOp,
            MulOp,
            VectorLenOp,
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
        sem: "(set result (concat (map (zip (split lhs $get_vlen $get_sew) (split rhs $get_vlen $get_sew)) (lambda (a b) (add a b)))))",
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
        sem: "(set result (concat (map (zip (split lhs $get_vlen $get_sew) (split rhs $get_vlen $get_sew)) (lambda (a b) (sub a b)))))",
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
        sem: "(set result (concat (map (zip (split lhs $get_vlen $get_sew) (split rhs $get_vlen $get_sew)) (lambda (a b) (mul a b)))))",
    }
}

// `vector_len(avl)` yields the number of lanes the target grants for a request
// of `avl` — `min(avl, VLMAX)` on RVV, where it selects as `vsetvli rd, avl`.
// The strip-mine primitive: a loop asks for the remaining trip count and
// advances by however many lanes the hardware grants, feeding the result to its
// elementwise ops' `vl` operands (EVL-style tail folding, no epilogue loop).
// The `sew` attribute names the element width the grant is for — how many
// lanes fit is a function of it (VLMAX = VLEN / SEW · LMUL).
operation! {
    VectorLenOp {
        name: "vector_len",
        dialect: "vector",
        operands: O {
            avl: "crate::builtin::IndexType",
        },
        results: R {
            result: "crate::builtin::IndexType",
        },
        attributes: A {
            sew: "Int",
        },
    }
}

/// The active lane count for `$get_vlen`, the `n` each `split` cuts the operand
/// bits into: a fixed vector takes it from the result type's static length (a
/// constant); a scalable vector takes it from the dynamic `vl` operand (index 2).
fn vlen_node(
    op: &tir::OpInstance,
    g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
) -> tir::graph::NodeId {
    if op.operands.len() > 2 {
        let n = g.add_node(tir::sem::SymKind::Symbol);
        g.set_leaf_data(n, tir::sem::SymPayload::SymbolId(2));
        return n;
    }
    let context = op.context.upgrade();
    let ty = context.get_value(op.results[0]).ty();
    let length = (context.get_type_data(ty).as_ref() as &dyn Any)
        .downcast_ref::<VectorType>()
        .and_then(|t| t.length())
        .unwrap_or(0) as u64;
    let n = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(
        n,
        tir::sem::SymPayload::Int(tir_adt::APInt::new(32, length)),
    );
    n
}

/// The lane width for `$get_sew`: the result type's element width, a constant
/// for fixed and scalable vectors alike (scalability varies the lane count, not
/// the element width). On RVV this is the demanded SEW.
fn sew_node(
    op: &tir::OpInstance,
    g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
) -> tir::graph::NodeId {
    let context = op.context.upgrade();
    let ty = context.get_value(op.results[0]).ty();
    let width = (context.get_type_data(ty).as_ref() as &dyn Any)
        .downcast_ref::<VectorType>()
        .map(|t| t.element(&context))
        .map(|elem| {
            (context.get_type_data(elem).as_ref() as &dyn Any)
                .downcast_ref::<crate::builtin::IntegerType>()
                .map(|i| i.width())
                .unwrap_or(0)
        })
        .unwrap_or(0) as u64;
    let n = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(n, tir::sem::SymPayload::Int(tir_adt::APInt::new(32, width)));
    n
}

macro_rules! impl_vlen_hooks {
    ($op:ty) => {
        impl $op {
            fn get_vlen(
                &self,
                g: &mut impl tir::graph::MutDag<
                    Node = tir::sem::SymKind,
                    Leaf = tir::sem::SymPayload<tir::ValueId>,
                >,
            ) -> tir::graph::NodeId {
                vlen_node(&self.0, g)
            }

            fn get_sew(
                &self,
                g: &mut impl tir::graph::MutDag<
                    Node = tir::sem::SymKind,
                    Leaf = tir::sem::SymPayload<tir::ValueId>,
                >,
            ) -> tir::graph::NodeId {
                sew_node(&self.0, g)
            }
        }
    };
}

impl_vlen_hooks!(AddOp);
impl_vlen_hooks!(SubOp);
impl_vlen_hooks!(MulOp);
