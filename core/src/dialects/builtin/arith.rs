use crate::operation;

use crate as tir;
use crate::{
    Any, Commutative, ConstantLike, Context, Error, IntegerArithmetic, OpCost, Operation,
    SameOperandAndResultType, Speculatable,
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
        interfaces: [ConstantLike, crate::interp::Interp, crate::Speculatable],
    }
}

impl crate::Speculatable for ConstantOp {}

impl crate::ConstantLike for ConstantOp {
    fn constant_value(&self) -> tir::utils::APInt {
        let context = self.0.context.clone();
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

macro_rules! int_binop {
    ($op:ident, $name:tt, [$($iface:ident),*], $sem:tt) => {
        int_binop!($op, $name, [$($iface),*], [], $sem);
    };
    ($op:ident, $name:tt, [$($iface:ident),*], [$($extra:ident),*], $sem:tt) => {
        operation! {
            $op {
                name: $name,
                dialect: "builtin",
                operands: O {
                    lhs: "crate::builtin::IntegerType",
                    rhs: "crate::builtin::IntegerType",
                },
                results: R {
                    result: "crate::builtin::IntegerType",
                },
                interfaces: [$($iface,)* $($extra),*],
                sem: $sem,
            }
        }

        $(impl $iface for $op {})*
    };
}

int_binop!(
    AddIOp,
    "addi",
    [
        Commutative,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    "(set result (add lhs rhs))"
);
int_binop!(
    SubIOp,
    "subi",
    [SameOperandAndResultType, IntegerArithmetic, Speculatable],
    "(set result (sub lhs rhs))"
);
int_binop!(
    MulIOp,
    "muli",
    [
        Commutative,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    [OpCost],
    "(set result (mul lhs rhs))"
);
int_binop!(
    DivSIOp,
    "divsi",
    [SameOperandAndResultType, IntegerArithmetic],
    "(set result (div lhs rhs))"
);
int_binop!(
    DivUIOp,
    "divui",
    [SameOperandAndResultType, IntegerArithmetic],
    "(set result (udiv lhs rhs))"
);
// Remainder is defined by the Euclidean identity rather than a primitive
// srem/urem, so the semantic form matches the canonical sub-mul-div target
// that TMDL rem/remu behaviors reduce to and selects through the e-graph
// without an unprovable rewrite. This total form equals bvsrem/bvurem
// everywhere, including rhs=0 (a - x*0 = a) and MIN/-1 (0). IR-level
// partiality (C's UB at rhs=0) is unchanged.
int_binop!(
    RemSIOp,
    "remsi",
    [SameOperandAndResultType, IntegerArithmetic],
    "(set result (sub lhs (mul (div lhs rhs) rhs)))"
);
int_binop!(
    RemUIOp,
    "remui",
    [SameOperandAndResultType, IntegerArithmetic],
    "(set result (sub lhs (mul (udiv lhs rhs) rhs)))"
);
int_binop!(
    AndIOp,
    "andi",
    [
        Commutative,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    "(set result (and lhs rhs))"
);
int_binop!(
    OrIOp,
    "ori",
    [
        Commutative,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    "(set result (or lhs rhs))"
);
int_binop!(
    XOrIOp,
    "xori",
    [
        Commutative,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    "(set result (xor lhs rhs))"
);
int_binop!(
    ShlIOp,
    "shli",
    [SameOperandAndResultType, IntegerArithmetic, Speculatable],
    "(set result (shl lhs rhs))"
);
int_binop!(
    ShrUIOp,
    "shrui",
    [SameOperandAndResultType, IntegerArithmetic, Speculatable],
    "(set result (lshr lhs rhs))"
);
int_binop!(
    ShrSIOp,
    "shrsi",
    [SameOperandAndResultType, IntegerArithmetic, Speculatable],
    "(set result (ashr lhs rhs))"
);

impl crate::OpCost for MulIOp {
    fn cost(&self) -> u32 {
        4
    }
}

operation! {
    CmpIOp {
        name: "cmpi",
        dialect: "builtin",
        attributes: A {
            predicate: "Predicate in INTEGER",
        },
        operands: O {
            lhs: "crate::builtin::IntegerType",
            rhs: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::Integer<1>",
        },
        sem: "(set result $cmp_expr)",
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for CmpIOp {}

impl CmpIOp {
    fn cmp_expr(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        let tir::attributes::AttributeValue::Predicate(predicate) = self.0.attr("predicate")?
        else {
            return None;
        };
        compare_expr(g, predicate, false)
    }
}

/// The comparison in canonical form: `sgt`/`sle`/`ugt`/`ule` become the
/// swapped-operand `Lt`/`Ge`/`ULt`/`UGe`, matching how TMDL lowers target
/// behaviors, so only six comparison kinds ever appear in patterns.
/// `unsigned_only` offers no semantics for a signed comparison, which an
/// address ordering has no meaning for.
pub(crate) fn compare_expr(
    g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    predicate: tir::attributes::Predicate,
    unsigned_only: bool,
) -> Option<tir::graph::NodeId> {
    use tir::attributes::Predicate;
    use tir::sem::SymKind;

    let swap = matches!(
        predicate,
        Predicate::Sgt | Predicate::Sle | Predicate::Ugt | Predicate::Ule
    );
    let kind = match if swap { predicate.swapped() } else { predicate } {
        Predicate::Eq => SymKind::Eq,
        Predicate::Ne => SymKind::Ne,
        Predicate::Slt if !unsigned_only => SymKind::Lt,
        Predicate::Sge if !unsigned_only => SymKind::Ge,
        Predicate::Ult => SymKind::ULt,
        Predicate::Uge => SymKind::UGe,
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
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for ExtSIOp {}

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
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for ExtUIOp {}

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
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for TruncIOp {}

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
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for BitcastOp {}

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
