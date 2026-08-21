use crate::operation;

use crate as tir;
use crate::{
    Any, Commutative, ConstantLike, Context, Error, IntegerArithmetic, OpCost, Operation,
    SameOperandAndResultType,
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
        interfaces: [ConstantLike, crate::interp::Interp],
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
        let value = match self.0.attr("value") {
            Some(tir::attributes::AttributeValue::Int(v)) => v,
            _ => 0,
        };
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
        interfaces: [Commutative, SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (add lhs rhs))",
    }
}

impl Commutative for AddIOp {}
impl SameOperandAndResultType for AddIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (sub lhs rhs))",
    }
}

impl SameOperandAndResultType for SubIOp {}
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
        interfaces: [Commutative, SameOperandAndResultType, OpCost, IntegerArithmetic],
        sem: "(set result (mul lhs rhs))",
    }
}

impl Commutative for MulIOp {}
impl SameOperandAndResultType for MulIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (div lhs rhs))",
    }
}

impl SameOperandAndResultType for DivSIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (udiv lhs rhs))",
    }
}

impl SameOperandAndResultType for DivUIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (sub lhs (mul (div lhs rhs) rhs)))",
    }
}

impl SameOperandAndResultType for RemSIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (sub lhs (mul (udiv lhs rhs) rhs)))",
    }
}

impl SameOperandAndResultType for RemUIOp {}
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
        interfaces: [Commutative, SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (and lhs rhs))",
    }
}

impl Commutative for AndIOp {}
impl SameOperandAndResultType for AndIOp {}
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
        interfaces: [Commutative, SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (or lhs rhs))",
    }
}

impl Commutative for OrIOp {}
impl SameOperandAndResultType for OrIOp {}
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
        interfaces: [Commutative, SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (xor lhs rhs))",
    }
}

impl Commutative for XOrIOp {}
impl SameOperandAndResultType for XOrIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (shl lhs rhs))",
    }
}

impl SameOperandAndResultType for ShlIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (lshr lhs rhs))",
    }
}

impl SameOperandAndResultType for ShrUIOp {}
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
        interfaces: [SameOperandAndResultType, IntegerArithmetic],
        sem: "(set result (ashr lhs rhs))",
    }
}

impl SameOperandAndResultType for ShrSIOp {}
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

        let predicate = match self.0.attr("predicate")? {
            tir::attributes::AttributeValue::Str(s) => s,
            _ => return None,
        };
        let (kind, swap) = match &*predicate {
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
            tir::attributes::AttributeValue::Str(pred.to_string().into()),
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
        // An index is a scalar too, but only a data layout says how wide.
        let layout = crate::DataLayout::for_op(context, self.0.id);
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
                .or_else(|| {
                    ty.downcast_ref::<crate::builtin::IndexType>()
                        .and_then(|_| layout.as_ref()?.index_width())
                })
        };
        let input_width = width(self.operands()[0]);
        let result_width = width(self.result());
        if input_width.is_none() || result_width.is_none() {
            return Err(Error::VerificationError(
                "bitcast requires scalar integer, floating-point or index types".to_string(),
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
