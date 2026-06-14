//! The `vector` dialect: a small, target-independent vocabulary for SIMD-style
//! arithmetic. A [`VectorType`] is either statically sized (`vec<8xi32>`) or
//! dynamically sized (`vec<i32>`), and the elementwise arithmetic operations take
//! an optional vector-length operand that bounds how many lanes are active.

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
        sem: "(set result (map vlen (add (lane lhs indvar) (lane rhs indvar))))",
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
        sem: "(set result (map vlen (sub (lane lhs indvar) (lane rhs indvar))))",
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
        sem: "(set result (map vlen (mul (lane lhs indvar) (lane rhs indvar))))",
    }
}

// A test-only operation exercising the `operation!` macro's `(loop ...)` sem
// construct: `sum_to n` folds `acc + indvar` over `[0, n)`, i.e. the sum
// `0 + 1 + ... + (n - 1)`.
#[cfg(test)]
operation! {
    SumToOp {
        name: "sum_to",
        dialect: "vector",
        operands: O {
            n: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (loop 0 n 0 (add acc indvar)))",
    }
}

// A constant-bound variant: `sum_eight` folds `acc + indvar` over `[0, 8)` with
// the bound written as a literal, so the lowered `Loop` unrolls to a constant.
#[cfg(test)]
operation! {
    SumEightOp {
        name: "sum_eight",
        dialect: "vector",
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (loop 0 8 0 (add acc indvar)))",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        IRBuilder, IRFormatter, Operation, ValueId,
        builtin::{FuncOp, IndexType, IntegerType, ops as builtin_ops},
        parse::ir::parse_ir,
    };

    /// A value that verifies as a block argument, like a function parameter.
    fn block_arg(context: &Context, ty: TypeId) -> ValueId {
        let value = context.create_value(ty, None);
        let id = value.id();
        let _block = context.create_block(vec![value]);
        id
    }

    #[test]
    fn static_and_dynamic_vector_roundtrip() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);

        let fixed = VectorType::fixed(&context, i32_ty, 8);
        assert_eq!(context.type_to_string(fixed), "!vector.vec<8xi32>");

        let dynamic = VectorType::dynamic(&context, i32_ty);
        assert_eq!(context.type_to_string(dynamic), "!vector.vec<i32>");

        // A static vector remembers its lane count and element.
        let data = context.get_type_data(fixed);
        let vec = (data.as_ref() as &dyn Any)
            .downcast_ref::<VectorType>()
            .unwrap();
        assert_eq!(vec.length(), Some(8));
        assert_eq!(vec.element(&context), i32_ty);

        // Static and dynamic vectors are distinct; identical ones are interned.
        assert_ne!(fixed, dynamic);
        assert_eq!(VectorType::fixed(&context, i32_ty, 8), fixed);
        assert_ne!(VectorType::fixed(&context, i32_ty, 4), fixed);
    }

    fn parse_type_text(context: &Context, src: &str) -> TypeId {
        let mut parser = tir::parse::text::Parser::new(src);
        parser
            .parse_type(context)
            .expect("type parses")
            .expect("type present")
    }

    #[test]
    fn vector_type_parses_from_text() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        assert_eq!(
            parse_type_text(&context, "!vector.vec<8xi32>"),
            VectorType::fixed(&context, i32_ty, 8)
        );
        assert_eq!(
            parse_type_text(&context, "!vector.vec<i32>"),
            VectorType::dynamic(&context, i32_ty)
        );
    }

    #[test]
    fn add_without_vl_roundtrips() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let vec_ty = VectorType::fixed(&context, i32_ty, 8);
        let lhs = block_arg(&context, vec_ty);
        let rhs = block_arg(&context, vec_ty);

        let op = ops::add(&context, lhs, rhs, tir::Operand::none(), vec_ty).build();
        assert_eq!(op.operands().len(), 2);
        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");
        assert!(buf.contains("vector.add"));
        assert!(buf.contains("!vector.vec<8xi32>"));

        let parsed = parse_ir::<AddOp>(&context, &buf).expect("parse vector.add");
        assert!(parsed.verify(&context).is_ok());
        assert_eq!(parsed.operands().len(), 2);
    }

    #[test]
    fn add_with_vl_roundtrips() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let vec_ty = VectorType::dynamic(&context, i32_ty);
        let lhs = block_arg(&context, vec_ty);
        let rhs = block_arg(&context, vec_ty);
        let vl = block_arg(&context, IndexType::new(&context));

        let op = ops::sub(&context, lhs, rhs, vl, vec_ty).build();
        assert_eq!(op.operands().len(), 3);
        assert!(op.verify(&context).is_ok());

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        op.print(&mut fmt).expect("print ok");

        let parsed = parse_ir::<SubOp>(&context, &buf).expect("parse vector.sub");
        assert!(parsed.verify(&context).is_ok());
        assert_eq!(parsed.operands().len(), 3);
    }

    #[test]
    fn mul_rejects_scalar_operand() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let vec_ty = VectorType::fixed(&context, i32_ty, 4);
        let lhs = block_arg(&context, vec_ty);
        let scalar = block_arg(&context, i32_ty);

        let op = ops::mul(&context, lhs, scalar, tir::Operand::none(), vec_ty).build();
        let err = op.verify(&context).expect_err("rhs must be a vector");
        assert!(err.to_string().contains("expected constraint"));
    }

    #[test]
    fn loop_sem_folds_over_induction_range() {
        use crate::graph::Dag;
        use crate::sem_expr::{AsSemExpr, ExprKind, ExprPostGraph, Value, execute, unroll_loops};
        use crate::utils::APInt;

        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let n = context.create_value(i32_ty, None);
        let op = sum_to(&context, n.id(), i32_ty).build();

        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &ExprKind::Loop);

        // 0 + 1 + 2 + 3 + 4 = 10 for n = 5.
        let result = execute(&g, &[Value::Int(APInt::new_signed(32, 5))]);
        match result {
            Value::Int(v) => assert_eq!(v.to_i64(), 10),
            other => panic!("expected int, got {other:?}"),
        }

        // A literal bound lets the lowered Loop unroll away entirely, leaving the
        // constant 0 + 1 + ... + 7 = 28.
        let const_op = sum_eight(&context, i32_ty).build();
        let mut cg = ExprPostGraph::new();
        let croot = const_op.convert(&mut cg);
        let (unrolled, new_root) = unroll_loops(&cg, croot);
        for idx in 0..unrolled.len() {
            let kind = *unrolled.get_node(crate::graph::NodeId::from_index(idx));
            assert!(!matches!(
                kind,
                ExprKind::Loop | ExprKind::IndVar | ExprKind::Acc
            ));
        }
        let _ = new_root;
        match execute(&unrolled, &[]) {
            Value::Int(v) => assert_eq!(v.to_i64(), 28),
            other => panic!("expected int, got {other:?}"),
        }
    }

    #[test]
    fn vector_add_sem_is_a_lanewise_map() {
        use crate::graph::{Dag, MutDag, NodeId};
        use crate::sem_expr::{AsSemExpr, ExprKind, ExprPayload, ExprPostGraph, Value, execute};
        use crate::utils::APInt;

        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let vec_ty = VectorType::fixed(&context, i32_ty, 4);
        let lhs = context.create_value(vec_ty, None);
        let rhs = context.create_value(vec_ty, None);
        let op = ops::add(&context, lhs.id(), rhs.id(), tir::Operand::none(), vec_ty).build();

        // The op lowers to `(map 4 (add (lane s0 i) (lane s1 i)))`.
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &ExprKind::VectorMap);
        let count = g.children(root).next().unwrap();
        assert!(matches!(
            g.get_leaf_data(count),
            Some(ExprPayload::Int(v)) if v.to_i64() == 4
        ));

        let lanes = |xs: [i64; 4]| {
            Value::Iterator(
                xs.iter()
                    .map(|&x| Value::Int(APInt::new_signed(32, x)))
                    .collect(),
            )
        };
        let a = lanes([1, 2, 3, 4]);
        let b = lanes([10, 20, 30, 40]);
        let Value::Iterator(out) = execute(&g, &[a.clone(), b.clone()]) else {
            panic!("vector op must produce a vector");
        };
        let out: Vec<i64> = out
            .iter()
            .map(|v| match v {
                Value::Int(i) => i.to_i64(),
                other => panic!("lane must be an int, got {other:?}"),
            })
            .collect();
        assert_eq!(out, vec![11, 22, 33, 44]);

        // The premise behind instruction selection: a target instruction that
        // independently lowers to the same map/lane DAG computes the same result,
        // so it can match this op. Build that DAG by hand and compare.
        let mut t = ExprPostGraph::new();
        let s0 = t.add_node(ExprKind::Symbol);
        t.set_leaf_data(s0, ExprPayload::SymbolId(0));
        let s1 = t.add_node(ExprKind::Symbol);
        t.set_leaf_data(s1, ExprPayload::SymbolId(1));
        let count = t.add_node(ExprKind::Constant);
        t.set_leaf_data(count, ExprPayload::Int(APInt::new(32, 4)));
        let iv0 = t.add_node(ExprKind::IndVar);
        let l0 = t.add_node(ExprKind::Lane);
        t.add_edge(l0, s0);
        t.add_edge(l0, iv0);
        let iv1 = t.add_node(ExprKind::IndVar);
        let l1 = t.add_node(ExprKind::Lane);
        t.add_edge(l1, s1);
        t.add_edge(l1, iv1);
        let add = t.add_node(ExprKind::Add);
        t.add_edge(add, l0);
        t.add_edge(add, l1);
        let map = t.add_node(ExprKind::VectorMap);
        t.add_edge(map, count);
        t.add_edge(map, add);
        let _ = NodeId::from_index(0);

        assert_eq!(
            execute(&t, &[a, b]),
            execute(&g, &[lanes([1, 2, 3, 4]), lanes([10, 20, 30, 40])])
        );
    }

    #[test]
    fn vector_ops_nest_in_function() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let vec_ty = VectorType::fixed(&context, i32_ty, 8);

        let p0 = context.create_value(vec_ty, None);
        let p1 = context.create_value(vec_ty, None);
        let region = context.create_region();
        let block = context.create_block(vec![p0.clone(), p1.clone()]);
        region.add_block(block.id());

        let func = builtin_ops::func(&context, "vadd", vec_ty, Some(region.id())).build();

        let mut builder = IRBuilder::new(func.body());
        let add = builder
            .insert(ops::add(&context, p0.id(), p1.id(), tir::Operand::none(), vec_ty).build());
        builder.insert(builtin_ops::r#return(&context, add.result()).build());

        assert!(func.verify(&context).is_ok());

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        func.print(&mut fmt).expect("print ok");

        let new_context = Context::with_default_dialects();
        let new_func = parse_ir::<FuncOp>(&new_context, &buf).expect("parse func");
        let mut new_buf = String::new();
        let mut fmt = IRFormatter::new(&mut new_buf);
        new_func.print(&mut fmt).expect("print ok");
        assert_eq!(buf, new_buf);
    }
}
