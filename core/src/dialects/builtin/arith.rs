use crate::operation;

use crate as tir;
use crate::{Commutative, ConstantLike, OpCost, Operation, SameOperandType};

operation! {
    ConstantOp {
        name: "constant",
        dialect: "builtin",
        attributes: A {
            value: "Int",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [ConstantLike],
    }
}

impl ConstantOpBuilder {
    pub fn value(self, v: i64) -> Self {
        self.attr("value", tir::attributes::AttributeValue::Int(v))
    }
}

impl crate::ConstantLike for ConstantOp {
    fn constant_value(&self) -> tir::utils::APInt {
        let context = self.0.context.upgrade();
        let value = self
            .0
            .attributes
            .iter()
            .find(|attr| attr.name == "value")
            .and_then(|attr| match attr.value {
                tir::attributes::AttributeValue::Int(v) => Some(v),
                _ => None,
            })
            .unwrap_or(0);
        let ty = context.get_value(self.result()).ty();
        let width = (context.get_type_data(ty).as_ref() as &dyn std::any::Any)
            .downcast_ref::<crate::builtin::IntegerType>()
            .map(crate::builtin::IntegerType::width)
            .unwrap_or(64);
        tir::utils::APInt::new_signed(width, value)
    }
}

operation! {
    AddIOp {
        name: "addi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [Commutative, SameOperandType],
        sem: "(set result (add lhs rhs))",
    }
}

impl Commutative for AddIOp {}
impl SameOperandType for AddIOp {}

operation! {
    SubIOp {
        name: "subi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [SameOperandType],
        sem: "(set result (sub lhs rhs))",
    }
}

impl SameOperandType for SubIOp {}

operation! {
    MulIOp {
        name: "muli",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [Commutative, SameOperandType, OpCost],
        sem: "(set result (mul lhs rhs))",
    }
}

impl Commutative for MulIOp {}
impl SameOperandType for MulIOp {}

impl crate::OpCost for MulIOp {
    fn cost(&self) -> u32 {
        4
    }
}

operation! {
    AndIOp {
        name: "andi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [Commutative],
        sem: "(set result (and lhs rhs))",
    }
}

impl Commutative for AndIOp {}

operation! {
    OrIOp {
        name: "ori",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [Commutative, SameOperandType],
        sem: "(set result (or lhs rhs))",
    }
}

impl Commutative for OrIOp {}
impl SameOperandType for OrIOp {}

operation! {
    XOrIOp {
        name: "xori",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [Commutative, SameOperandType],
        sem: "(set result (xor lhs rhs))",
    }
}

impl Commutative for XOrIOp {}
impl SameOperandType for XOrIOp {}

operation! {
    ShlIOp {
        name: "shli",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (shl lhs rhs))",
    }
}

operation! {
    ShrUIOp {
        name: "shrui",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (lshr lhs rhs))",
    }
}

operation! {
    ShrSIOp {
        name: "shrsi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (ashr lhs rhs))",
    }
}

operation! {
    CmpIOp {
        name: "cmpi",
        dialect: "builtin",
        attributes: A {
            predicate: "Str",
        },
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::Integer<1>",
        },
        sem: "custom",
    }
}

impl CmpIOpBuilder {
    pub fn predicate(self, pred: &str) -> Self {
        self.attr(
            "predicate",
            tir::attributes::AttributeValue::Str(pred.to_string()),
        )
    }
}

impl CmpIOp {
    /// Map the comparison predicate to its semantic operator, building
    /// `<cmp>(lhs, rhs)` over the two operand symbols so a comparison participates
    /// in the e-graph (and fuses into a compare-and-branch). The predicate is read
    /// at build time, so the static `sem` form cannot express it. `sle` has no
    /// signed-or-equal `ExprKind`, so it stays opaque for now.
    fn custom_semantic_expr(
        &self,
        g: &mut tir::sem_expr::ExprPostGraph,
    ) -> Option<tir::graph::NodeId> {
        use tir::graph::MutDag;
        use tir::sem_expr::{ExprKind, ExprPayload};

        let predicate = self.attributes().iter().find_map(|a| match &a.value {
            tir::attributes::AttributeValue::Str(s) if a.name == "predicate" => Some(s.as_str()),
            _ => None,
        })?;
        let kind = match predicate {
            "eq" => ExprKind::Eq,
            "ne" => ExprKind::Ne,
            "slt" => ExprKind::Lt,
            "sgt" => ExprKind::Gt,
            "sge" => ExprKind::Ge,
            "ult" => ExprKind::ULt,
            "ule" => ExprKind::ULe,
            "ugt" => ExprKind::UGt,
            "uge" => ExprKind::UGe,
            _ => return None,
        };

        let lhs = g.add_node(ExprKind::Symbol);
        g.set_leaf_data(lhs, ExprPayload::SymbolId(0));
        let rhs = g.add_node(ExprKind::Symbol);
        g.set_leaf_data(rhs, ExprPayload::SymbolId(1));
        let root = g.add_node(kind);
        g.add_edge(root, lhs);
        g.add_edge(root, rhs);

        g.set_original_op(root, <Self as tir::Operation>::id(self));
        let context = self.0.context.upgrade();
        g.set_actual_type(root, context.get_value(self.result()).ty());
        Some(root)
    }
}

operation! {
    ExtSIOp {
        name: "extsi",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (sext input))",
    }
}

operation! {
    ExtUIOp {
        name: "extui",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (zext input))",
    }
}

operation! {
    TruncIOp {
        name: "trunci",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (trunc input))",
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, Operation,
        builtin::{AddIOp, IntegerType, ops},
        graph::Dag,
        parse::ir::parse_ir,
        sem_expr::{AsSemExpr, ExprKind, ExprPayload, ExprPostGraph},
    };

    #[test]
    fn constant_fold_derived_from_sem() {
        use crate::ConstantFold;
        use crate::sem_expr::Value;
        use crate::utils::APInt;

        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let a = context.create_value(i32_ty, None);
        let b = context.create_value(i32_ty, None);
        let op = ops::addi(&context, a.id(), b.id(), i32_ty).build();

        let fold = context
            .get_op(op.id())
            .as_interface::<dyn ConstantFold>()
            .expect("addi derives ConstantFold from its sem");
        let folded = fold
            .fold(&[
                Value::Int(APInt::new_signed(32, 2)),
                Value::Int(APInt::new_signed(32, 3)),
            ])
            .expect("folds two constants");
        match folded {
            Value::Int(v) => assert_eq!(v.to_i64(), 5),
            other => panic!("expected an integer, got {other:?}"),
        }
    }

    #[test]
    fn op_cost_read_through_interface() {
        use crate::OpCost;

        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let a = context.create_value(i32_ty, None);
        let b = context.create_value(i32_ty, None);

        let mul = ops::muli(&context, a.id(), b.id(), i32_ty).build();
        let cost = context
            .get_op(mul.id())
            .as_interface::<dyn OpCost>()
            .expect("muli opts into OpCost");
        assert_eq!(cost.cost(), 4);

        // An op that does not opt in has no OpCost interface; callers default to 1.
        let add = ops::addi(&context, a.id(), b.id(), i32_ty).build();
        assert!(
            context
                .get_op(add.id())
                .as_interface::<dyn OpCost>()
                .is_none()
        );
    }

    #[test]
    fn addi_construction() {
        let context = Context::with_default_dialects();
        let lhs = context.create_value(IntegerType::new(&context, 32), None);
        let rhs = context.create_value(IntegerType::new(&context, 32), None);

        let op = ops::addi(&context, lhs.id(), rhs.id(), IntegerType::new(&context, 32)).build();

        assert_eq!(op.operands().len(), 2);
        assert_eq!(
            context.get_value(op.result()).ty(),
            IntegerType::new(&context, 32)
        );
    }

    // Arithmetic roundtrip in a function lives in the FileCheck suite at
    // core/checks/IRRoundtrip/arith.tir.

    #[test]
    fn parse_single_addi_with_result_prefix() {
        let context = Context::with_default_dialects();
        let c0 = ops::constant(&context, 1, IntegerType::new(&context, 32)).build();
        let c1 = ops::constant(&context, 2, IntegerType::new(&context, 32)).build();
        let src = format!(
            "%2 = addi %{}, %{} : !i32\n",
            c0.result().number(),
            c1.result().number()
        );

        let op = parse_ir::<AddIOp>(&context, &src).expect("Failed to parse addi");
        assert!(op.verify(&context).is_ok());
        assert_eq!(op.operands().len(), 2);
        assert_eq!(
            context.get_value(op.result()).ty(),
            IntegerType::new(&context, 32)
        );
    }

    // The SameOperandType verifier failure is covered by the FileCheck suite at
    // core/checks/Verifier/addi-operands-same-type.tir.

    fn make_binary_op_context() -> (Context, crate::ValueId, crate::ValueId) {
        let context = Context::with_default_dialects();
        let lhs = context.create_value(IntegerType::new(&context, 32), None);
        let rhs = context.create_value(IntegerType::new(&context, 32), None);
        (context, lhs.id(), rhs.id())
    }

    fn check_binary_sem(g: &ExprPostGraph, root: crate::graph::NodeId, expected_kind: ExprKind) {
        assert_eq!(g.len(), 3, "expected 3 nodes: lhs symbol, rhs symbol, op");
        assert_eq!(g.get_kind(root), &expected_kind);
        let children: Vec<_> = g.children(root).collect();
        assert_eq!(children.len(), 2);
        assert_eq!(g.get_kind(children[0]), &ExprKind::Symbol);
        assert_eq!(g.get_kind(children[1]), &ExprKind::Symbol);
        assert!(
            matches!(g.get_leaf_data(children[0]), Some(ExprPayload::SymbolId(0))),
            "lhs should be symbol 0"
        );
        assert!(
            matches!(g.get_leaf_data(children[1]), Some(ExprPayload::SymbolId(1))),
            "rhs should be symbol 1"
        );
    }

    fn check_sem_metadata(
        context: &Context,
        op: &impl Operation,
        g: &ExprPostGraph,
        root: crate::graph::NodeId,
        result: crate::ValueId,
    ) {
        assert_eq!(g.get_original_op(root), Some(op.id()));
        assert_eq!(
            g.get_actual_type(root),
            Some(context.get_value(result).ty())
        );
    }

    #[test]
    fn addi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::addi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::Add);
        check_sem_metadata(&context, &op, &g, root, op.result());
    }

    #[test]
    fn subi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::subi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::Sub);
    }

    #[test]
    fn muli_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::muli(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::Mul);
    }

    #[test]
    fn andi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::andi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::And);
    }

    #[test]
    fn ori_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::ori(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::Or);
    }

    #[test]
    fn xori_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::xori(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::Xor);
    }

    #[test]
    fn shli_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::shli(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::ShiftLeft);
    }

    #[test]
    fn shrui_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::shrui(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::ShiftRightLogic);
    }

    #[test]
    fn shrsi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::shrsi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, ExprKind::ShiftRightArithmetic);
    }

    /// The width-changing ops take their width from the result type via the unary
    /// sem-DSL forms: `extsi -> SExt(x, W)`, `extui -> ZExt(x, W)`,
    /// `trunci -> Extract(x, W-1, 0)`.
    fn const_value(g: &ExprPostGraph, node: crate::graph::NodeId) -> u64 {
        match g.get_leaf_data(node) {
            Some(ExprPayload::Int(v)) => v.to_u64(),
            other => panic!("expected an integer constant, got {other:?}"),
        }
    }

    #[test]
    fn extsi_sem_expr_uses_result_width() {
        let context = Context::with_default_dialects();
        let input = context.create_value(IntegerType::new(&context, 16), None);
        let op = ops::extsi(&context, input.id(), IntegerType::new(&context, 64)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &ExprKind::SExt);
        let children: Vec<_> = g.children(root).collect();
        assert_eq!(g.get_kind(children[0]), &ExprKind::Symbol);
        assert_eq!(const_value(&g, children[1]), 64);
    }

    #[test]
    fn extui_sem_expr_uses_result_width() {
        let context = Context::with_default_dialects();
        let input = context.create_value(IntegerType::new(&context, 8), None);
        let op = ops::extui(&context, input.id(), IntegerType::new(&context, 32)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &ExprKind::ZExt);
        assert_eq!(const_value(&g, g.children(root).nth(1).unwrap()), 32);
    }

    #[test]
    fn trunci_sem_expr_is_low_bit_extract() {
        let context = Context::with_default_dialects();
        let input = context.create_value(IntegerType::new(&context, 64), None);
        let op = ops::trunci(&context, input.id(), IntegerType::new(&context, 16)).build();
        let mut g = ExprPostGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &ExprKind::Extract);
        let children: Vec<_> = g.children(root).collect();
        assert_eq!(g.get_kind(children[0]), &ExprKind::Symbol);
        assert_eq!(const_value(&g, children[1]), 15); // high = W - 1
        assert_eq!(const_value(&g, children[2]), 0); // low
    }
}
