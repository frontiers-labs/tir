use crate::operation;
use crate::utils::APInt;

use crate as tir;
use crate::{
    Additive, Commutative, ConstantLike, Context, Error, IntegerArithmetic, OpCost, Operation,
    SameOperandAndResultType, Speculatable, Subtractive,
};

operation! {
    ConstantOp {
        name: "constant",
        dialect: "builtin",
        scheme: "() -> int<_>",
        attributes: A {
            value: "Int",
        },
        results: R { result },
        interfaces: [ConstantLike, crate::interp::Interp, crate::Speculatable],
    }
}

impl crate::Speculatable for ConstantOp {}

impl crate::ConstantLike for ConstantOp {
    fn constant_value(&self) -> tir::utils::APInt {
        let context = self.0.context.clone();
        let ty = context.get_value(self.result()).ty();
        let width = crate::builtin::IntegerType::view(&context, ty).map_or(64, |int| int.width());
        self.0
            .attr("value")
            .and_then(|value| value.int_at(width))
            .unwrap_or_else(|| tir::utils::APInt::zero(width).with_signed(true))
    }
}

/// A `builtin.constant` of `ty` holding `value`, wider than 64 bits or not.
pub fn literal(context: &Context, value: &tir::utils::APInt, ty: tir::TypeId) -> ConstantOpBuilder {
    ConstantOpBuilder::new(context)
        .attr("value", tir::attributes::AttributeValue::from_int(value))
        .result_type(ty)
}

// The rest of an invocation is more fields of the op: its flags, and where
// it is poison or must not run.
macro_rules! int_binop {
    ($op:ident, $name:tt, [$($iface:ident),*], [$($extra:ident),*], $sem:tt $(, $($rest:tt)*)?) => {
        operation! {
            $op {
                name: $name,
                dialect: "builtin",
                scheme: "forall W: nat. (int<W>, int<W>) -> int<W>",
                operands: O { lhs, rhs },
                results: R { result },
                interfaces: [$($iface,)* $($extra),*],
                sem: $sem,
                $($($rest)*)?
            }
        }

        $(impl $iface for $op {})*
    };
    ($op:ident, $name:tt, [$($iface:ident),*], $sem:tt $(, $($rest:tt)*)?) => {
        int_binop!($op, $name, [$($iface),*], [], $sem $(, $($rest)*)?);
    };
}

trait IntegerConditions: Operation {
    fn at_width(&self, g: &mut tir::sem::SemGraph, value: fn(u64) -> APInt) -> Option<tir::NodeId> {
        let context = &self.handle().context;
        let ty = context.get_value(self.handle().operands()[0]).ty();
        let width = crate::builtin::IntegerType::view(context, ty)?.width();
        let node = g.add_node(crate::sem::SymKind::Constant);
        g.set_leaf_data(node, crate::sem::SymPayload::Int(value(width)));
        Some(node)
    }

    fn zero(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        self.at_width(g, APInt::zero)
    }

    fn width(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        self.at_width(g, |width| APInt::new(width, width))
    }

    fn smin(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        self.at_width(g, |width| APInt::new(width, 1).shl(width - 1))
    }

    fn ones(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        self.at_width(g, |width| APInt::zero(width).not())
    }

    fn flag(&self, g: &mut tir::sem::SemGraph, name: &str) -> tir::NodeId {
        let set = matches!(
            self.handle().attr(name),
            Some(tir::attributes::AttributeValue::Bool(true))
        );
        crate::sem::con(g, u64::from(set), 1)
    }

    fn nsw_flag(&self, g: &mut tir::sem::SemGraph) -> tir::NodeId {
        self.flag(g, "nsw")
    }

    fn nuw_flag(&self, g: &mut tir::sem::SemGraph) -> tir::NodeId {
        self.flag(g, "nuw")
    }
}

impl<T: Operation> IntegerConditions for T {}

int_binop!(
    AddIOp,
    "addi",
    [
        Additive,
        Commutative,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    "(set result (add lhs rhs))",
    attributes: A { nsw: "?Flag", nuw: "?Flag" },
    poison: "(or (and $nsw_flag (lt (and (xor (add lhs rhs) lhs) (xor (add lhs rhs) rhs)) $zero))
                 (and $nuw_flag (ult (add lhs rhs) lhs)))"
);
int_binop!(
    SubIOp,
    "subi",
    [
        Subtractive,
        SameOperandAndResultType,
        IntegerArithmetic,
        Speculatable
    ],
    "(set result (sub lhs rhs))",
    attributes: A { nsw: "?Flag", nuw: "?Flag" },
    poison: "(or (and $nsw_flag (lt (and (xor lhs rhs) (xor lhs (sub lhs rhs))) $zero))
                 (and $nuw_flag (ult lhs rhs)))"
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
    "(set result (div lhs rhs))",
    requires: "(and (ne rhs $zero) (not (and (eq lhs $smin) (eq rhs $ones))))"
);
int_binop!(
    DivUIOp,
    "divui",
    [SameOperandAndResultType, IntegerArithmetic],
    "(set result (udiv lhs rhs))",
    requires: "(ne rhs $zero)"
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
    "(set result (sub lhs (mul (div lhs rhs) rhs)))",
    requires: "(and (ne rhs $zero) (not (and (eq lhs $smin) (eq rhs $ones))))"
);
int_binop!(
    RemUIOp,
    "remui",
    [SameOperandAndResultType, IntegerArithmetic],
    "(set result (sub lhs (mul (udiv lhs rhs) rhs)))",
    requires: "(ne rhs $zero)"
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
    "(set result (shl lhs rhs))",
    poison: "(uge rhs $width)"
);
int_binop!(
    ShrUIOp,
    "shrui",
    [SameOperandAndResultType, IntegerArithmetic, Speculatable],
    "(set result (lshr lhs rhs))",
    poison: "(uge rhs $width)"
);
int_binop!(
    ShrSIOp,
    "shrsi",
    [SameOperandAndResultType, IntegerArithmetic, Speculatable],
    "(set result (ashr lhs rhs))",
    poison: "(uge rhs $width)"
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
        scheme: "forall W: nat. (int<W>, int<W>) -> int<1>",
        attributes: A {
            predicate: "Predicate in INTEGER",
        },
        operands: O { lhs, rhs },
        results: R { result },
        sem: "(set result $cmp_expr)",
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for CmpIOp {}

impl CmpIOp {
    fn cmp_expr(&self, g: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        let tir::attributes::AttributeValue::Predicate(predicate) = self.0.attr("predicate")?
        else {
            return None;
        };
        compare_expr(g, predicate, false)
    }
}

pub(crate) fn compare_expr(
    g: &mut tir::sem::SemGraph,
    predicate: tir::attributes::Predicate,
    unsigned_only: bool,
) -> Option<tir::NodeId> {
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
        scheme: "(int<_>) -> int<_>",
        operands: O { input },
        results: R { result },
        sem: "(set result (sext input))",
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for ExtSIOp {}

operation! {
    ExtUIOp {
        name: "extui",
        dialect: "builtin",
        scheme: "(int<_>) -> int<_>",
        operands: O { input },
        results: R { result },
        sem: "(set result (zext input))",
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for ExtUIOp {}

operation! {
    TruncIOp {
        name: "trunci",
        dialect: "builtin",
        scheme: "(int<_>) -> int<_>",
        operands: O { input },
        results: R { result },
        sem: "(set result (trunc input))",
        interfaces: [crate::Speculatable],
    }
}

impl crate::Speculatable for TruncIOp {}

operation! {
    BitcastOp {
        name: "bitcast",
        dialect: "builtin",
        scheme: "(_) -> _",
        verifier: "true",
        operands: O { input },
        results: R { result },
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
            crate::builtin::IntegerType::view(context, ty)
                .map(|int| int.width())
                .or_else(|| {
                    crate::builtin::FloatType::view(context, ty)
                        .map(|float| u64::from(float.bit_width()))
                })
                .or_else(|| {
                    crate::builtin::IndexType::view(context, ty)
                        .and_then(|_| layout.as_ref()?.index_width().map(u64::from))
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
