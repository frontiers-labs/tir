use crate::operation;

use crate as tir;
use crate::{
    Any, Commutative, ConstantLike, Context, Error, IntegerArithmetic, OpCost, Operation,
    SameOperandType,
};

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
        interfaces: [Commutative, SameOperandType, IntegerArithmetic],
        sem: "(set result (add lhs rhs))",
    }
}

impl Commutative for AddIOp {}
impl SameOperandType for AddIOp {}
impl IntegerArithmetic for AddIOp {}

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
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (sub lhs rhs))",
    }
}

impl SameOperandType for SubIOp {}
impl IntegerArithmetic for SubIOp {}

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
        interfaces: [Commutative, SameOperandType, OpCost, IntegerArithmetic],
        sem: "(set result (mul lhs rhs))",
    }
}

impl Commutative for MulIOp {}
impl SameOperandType for MulIOp {}
impl IntegerArithmetic for MulIOp {}

impl crate::OpCost for MulIOp {
    fn cost(&self) -> u32 {
        4
    }
}

operation! {
    DivSIOp {
        name: "divsi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (div lhs rhs))",
    }
}

impl SameOperandType for DivSIOp {}
impl IntegerArithmetic for DivSIOp {}

operation! {
    DivUIOp {
        name: "divui",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (udiv lhs rhs))",
    }
}

impl SameOperandType for DivUIOp {}
impl IntegerArithmetic for DivUIOp {}

operation! {
    // Remainder is defined by the Euclidean identity rather than a primitive
    // srem/urem, so the semantic form matches the canonical sub-mul-div target
    // that TMDL rem/remu behaviors reduce to and selects through the e-graph
    // without an unprovable rewrite. This total form equals bvsrem/bvurem
    // everywhere, including rhs=0 (a - x*0 = a) and MIN/-1 (0). IR-level
    // partiality (C's UB at rhs=0) is unchanged.
    RemSIOp {
        name: "remsi",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (sub lhs (mul (div lhs rhs) rhs)))",
    }
}

impl SameOperandType for RemSIOp {}
impl IntegerArithmetic for RemSIOp {}

operation! {
    RemUIOp {
        name: "remui",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (sub lhs (mul (udiv lhs rhs) rhs)))",
    }
}

impl SameOperandType for RemUIOp {}
impl IntegerArithmetic for RemUIOp {}

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
        interfaces: [Commutative, SameOperandType, IntegerArithmetic],
        sem: "(set result (and lhs rhs))",
    }
}

impl Commutative for AndIOp {}
impl SameOperandType for AndIOp {}
impl IntegerArithmetic for AndIOp {}

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
        interfaces: [Commutative, SameOperandType, IntegerArithmetic],
        sem: "(set result (or lhs rhs))",
    }
}

impl Commutative for OrIOp {}
impl SameOperandType for OrIOp {}
impl IntegerArithmetic for OrIOp {}

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
        interfaces: [Commutative, SameOperandType, IntegerArithmetic],
        sem: "(set result (xor lhs rhs))",
    }
}

impl Commutative for XOrIOp {}
impl SameOperandType for XOrIOp {}
impl IntegerArithmetic for XOrIOp {}

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
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (shl lhs rhs))",
    }
}

impl SameOperandType for ShlIOp {}
impl IntegerArithmetic for ShlIOp {}

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
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (lshr lhs rhs))",
    }
}

impl SameOperandType for ShrUIOp {}
impl IntegerArithmetic for ShrUIOp {}

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
        interfaces: [SameOperandType, IntegerArithmetic],
        sem: "(set result (ashr lhs rhs))",
    }
}

impl SameOperandType for ShrSIOp {}
impl IntegerArithmetic for ShrSIOp {}

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
        sem: "(set result $cmp_expr)",
    }
}

impl CmpIOp {
    /// The comparison in canonical form: `sgt`/`sle`/`ugt`/`ule` become the
    /// swapped-operand `Lt`/`Ge`/`ULt`/`UGe`, matching how TMDL lowers target
    /// behaviors, so only six comparison kinds ever appear in patterns.
    fn cmp_expr(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        use tir::sem::SymKind;

        let predicate = self.0.attributes.iter().find_map(|a| {
            (a.name == "predicate").then_some(match &a.value {
                tir::attributes::AttributeValue::Str(s) => Some(s.as_str()),
                _ => None,
            })
        })??;
        let (kind, swap) = match predicate {
            "eq" => (SymKind::Eq, false),
            "ne" => (SymKind::Ne, false),
            "slt" => (SymKind::Lt, false),
            "sgt" => (SymKind::Lt, true),
            "sge" => (SymKind::Ge, false),
            "sle" => (SymKind::Ge, true),
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

impl CmpIOpBuilder {
    pub fn predicate(self, pred: &str) -> Self {
        self.attr(
            "predicate",
            tir::attributes::AttributeValue::Str(pred.to_string()),
        )
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

operation! {
    BitcastOp {
        name: "bitcast",
        dialect: "builtin",
        verifier: "true",
        operands: O {
            input: "Any",
        },
        results: R {
            result: "Any",
        },
        sem: "(set result (bitcast input))",
    }
}

impl tir::Verifiable for BitcastOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let width = |value| {
            let ty = context.get_value(value).ty();
            let ty = context.get_type_data(ty);
            let ty = ty.as_ref() as &dyn std::any::Any;
            ty.downcast_ref::<crate::builtin::IntegerType>()
                .map(crate::builtin::IntegerType::width)
                .or_else(|| {
                    ty.downcast_ref::<crate::builtin::FloatType>()
                        .map(crate::builtin::FloatType::bit_width)
                })
        };
        let input_width = width(self.operands()[0]);
        let result_width = width(self.result());
        if input_width.is_none() || result_width.is_none() {
            return Err(Error::VerificationError(
                "bitcast requires scalar integer or floating-point types".to_string(),
            ));
        }
        if input_width != result_width {
            return Err(Error::VerificationError(
                "bitcast source and result widths must match".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, Operation,
        builtin::{AddIOp, IntegerType, ops},
        graph::{Dag, MetaDag},
        parse::ir::parse_ir,
        sem::{AsSemExpr, SemGraph, SymKind, SymPayload},
    };

    #[test]
    fn constant_fold_derived_from_sem() {
        use crate::ConstantFold;
        use crate::sem::Value;
        use tir_adt::APInt;

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

    fn check_binary_sem(g: &SemGraph, root: crate::graph::NodeId, expected_kind: SymKind) {
        assert_eq!(g.len(), 3, "expected 3 nodes: lhs symbol, rhs symbol, op");
        assert_eq!(g.get_kind(root), &expected_kind);
        let children: Vec<_> = g.children(root).collect();
        assert_eq!(children.len(), 2);
        assert_eq!(g.get_kind(children[0]), &SymKind::Symbol);
        assert_eq!(g.get_kind(children[1]), &SymKind::Symbol);
        assert!(
            matches!(g.get_leaf_data(children[0]), Some(SymPayload::SymbolId(0))),
            "lhs should be symbol 0"
        );
        assert!(
            matches!(g.get_leaf_data(children[1]), Some(SymPayload::SymbolId(1))),
            "rhs should be symbol 1"
        );
    }

    fn check_sem_metadata(
        context: &Context,
        op: &impl Operation,
        g: &SemGraph,
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
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::Add);
        check_sem_metadata(&context, &op, &g, root, op.result());
    }

    #[test]
    fn subi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::subi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::Sub);
    }

    #[test]
    fn muli_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::muli(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::Mul);
    }

    #[test]
    fn andi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::andi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::And);
    }

    #[test]
    fn ori_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::ori(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::Or);
    }

    #[test]
    fn xori_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::xori(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::Xor);
    }

    #[test]
    fn shli_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::shli(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::ShiftLeft);
    }

    #[test]
    fn shrui_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::shrui(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::ShiftRightLogic);
    }

    #[test]
    fn shrsi_sem_expr() {
        let (context, lhs, rhs) = make_binary_op_context();
        let op = ops::shrsi(&context, lhs, rhs, IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        check_binary_sem(&g, root, SymKind::ShiftRightArithmetic);
    }

    /// The width-changing ops take their width from the result type via the unary
    /// sem-DSL forms: `extsi -> SExt(x, W)`, `extui -> ZExt(x, W)`,
    /// `trunci -> Extract(x, W-1, 0)`.
    fn const_value(g: &SemGraph, node: crate::graph::NodeId) -> u64 {
        match g.get_leaf_data(node) {
            Some(SymPayload::Int(v)) => v.to_u64(),
            other => panic!("expected an integer constant, got {other:?}"),
        }
    }

    #[test]
    fn extsi_sem_expr_uses_result_width() {
        let context = Context::with_default_dialects();
        let input = context.create_value(IntegerType::new(&context, 16), None);
        let op = ops::extsi(&context, input.id(), IntegerType::new(&context, 64)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &SymKind::SExt);
        let children: Vec<_> = g.children(root).collect();
        assert_eq!(g.get_kind(children[0]), &SymKind::Symbol);
        assert_eq!(const_value(&g, children[1]), 64);
    }

    #[test]
    fn extui_sem_expr_uses_result_width() {
        let context = Context::with_default_dialects();
        let input = context.create_value(IntegerType::new(&context, 8), None);
        let op = ops::extui(&context, input.id(), IntegerType::new(&context, 32)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &SymKind::ZExt);
        assert_eq!(const_value(&g, g.children(root).nth(1).unwrap()), 32);
    }

    #[test]
    fn trunci_sem_expr_is_low_bit_extract() {
        let context = Context::with_default_dialects();
        let input = context.create_value(IntegerType::new(&context, 64), None);
        let op = ops::trunci(&context, input.id(), IntegerType::new(&context, 16)).build();
        let mut g = SemGraph::new();
        let root = op.convert(&mut g);
        assert_eq!(g.get_kind(root), &SymKind::Extract);
        let children: Vec<_> = g.children(root).collect();
        assert_eq!(g.get_kind(children[0]), &SymKind::Symbol);
        assert_eq!(const_value(&g, children[1]), 15); // high = W - 1
        assert_eq!(const_value(&g, children[2]), 0); // low
    }
}
