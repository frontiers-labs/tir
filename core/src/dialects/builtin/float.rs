//! Floating-point arithmetic over any [`crate::builtin::FloatType`], plus the
//! block-scoped fast-math mechanism: FP rewrites are only valid under the
//! [`FastMathFlags`] in effect for the op, resolved by [`fp_math_flags`] from
//! the nearest enclosing `fpmath` attribute (on a block, or on a region-owning
//! op). The default, with no attribute anywhere, is strict IEEE 754 — no
//! value-changing FP transform is allowed. Isolating an expression in its own
//! block therefore scopes a relaxation (e.g. `contract`) to just that
//! expression.

use crate::operation;

use crate as tir;
use crate::{Commutative, Context, OpId, SameOperandAndResultType, attributes::AttributeValue};

/// The attribute consulted by [`fp_math_flags`], valid on blocks and on
/// region-owning operations. Its value is a string accepted by
/// [`FastMathFlags::parse`].
pub const FPMATH_ATTR: &str = "fpmath";

/// IEEE-relaxation flags gating FP optimizations, LLVM-style. Empty means
/// strict IEEE 754.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FastMathFlags(u8);

/// `(flag, name)` for each individual flag.
const FAST_MATH_NAMES: [(FastMathFlags, &str); 6] = [
    (FastMathFlags::CONTRACT, "contract"),
    (FastMathFlags::REASSOC, "reassoc"),
    (FastMathFlags::NNAN, "nnan"),
    (FastMathFlags::NINF, "ninf"),
    (FastMathFlags::NSZ, "nsz"),
    (FastMathFlags::ARCP, "arcp"),
];

impl FastMathFlags {
    /// Strict IEEE 754: no value-changing transforms.
    pub const NONE: Self = Self(0);
    /// Allow fusing a multiply and an add into an fma.
    pub const CONTRACT: Self = Self(1 << 0);
    /// Allow reassociation of a chain of same-kind ops.
    pub const REASSOC: Self = Self(1 << 1);
    /// Assume no NaN operands or results.
    pub const NNAN: Self = Self(1 << 2);
    /// Assume no infinite operands or results.
    pub const NINF: Self = Self(1 << 3);
    /// Treat the sign of a zero as insignificant.
    pub const NSZ: Self = Self(1 << 4);
    /// Allow replacing a division with a multiply by the reciprocal.
    pub const ARCP: Self = Self(1 << 5);
    /// All of the above.
    pub const FAST: Self = Self(0x3f);

    pub fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }

    pub fn union(self, flags: Self) -> Self {
        Self(self.0 | flags.0)
    }

    /// Parse `"none"`, `"fast"`, or a comma-separated flag list
    /// (e.g. `"contract,nnan"`). `None` on any unknown flag.
    pub fn parse(spec: &str) -> Option<Self> {
        match spec.trim() {
            "none" => return Some(Self::NONE),
            "fast" => return Some(Self::FAST),
            _ => {}
        }
        spec.split(',').try_fold(Self::NONE, |acc, name| {
            FAST_MATH_NAMES
                .iter()
                .find(|(_, n)| *n == name.trim())
                .map(|&(flag, _)| acc.union(flag))
        })
    }
}

impl std::fmt::Display for FastMathFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::NONE => f.write_str("none"),
            Self::FAST => f.write_str("fast"),
            _ => {
                let mut first = true;
                for (flag, name) in FAST_MATH_NAMES {
                    if self.contains(flag) {
                        if !first {
                            f.write_str(",")?;
                        }
                        first = false;
                        f.write_str(name)?;
                    }
                }
                Ok(())
            }
        }
    }
}

/// The fast-math flags in effect for `op`: the nearest `fpmath` attribute on
/// the enclosing chain block → region-owning op → its block → ..., defaulting
/// to strict IEEE. A block attribute shadows the owning op's, so a relaxation
/// (or an opt-out back to `"none"`) can be scoped to a single block.
pub fn fp_math_flags(context: &Context, op: OpId) -> FastMathFlags {
    let mut block = context.parent_block(op);
    while let Some(block_id) = block {
        if let Some(AttributeValue::Str(spec)) = context.get_block(block_id).attr(FPMATH_ATTR) {
            return FastMathFlags::parse(&spec).unwrap_or(FastMathFlags::NONE);
        }
        let Some(owner) = context
            .parent_region(block_id)
            .and_then(|region| context.get_region(region).parent_op())
        else {
            break;
        };
        let owner_attr = match context.get_op(owner).attr(FPMATH_ATTR) {
            Some(AttributeValue::Str(spec)) => Some(spec.clone()),
            _ => None,
        };
        if let Some(spec) = owner_attr {
            return FastMathFlags::parse(&spec).unwrap_or(FastMathFlags::NONE);
        }
        block = context.parent_block(owner);
    }
    FastMathFlags::NONE
}

operation! {
    ConstantFOp {
        name: "constantf",
        dialect: "builtin",
        attributes: A {
            value: "F64",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
    }
}

fn float_format_node(
    op: &tir::OpHandle,
    g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    format: impl FnOnce(&crate::builtin::FloatType) -> u32,
) -> tir::graph::NodeId {
    let context = op.context.upgrade();
    let ty = context.get_value(op.results()[0]).ty();
    let ty = context.get_type_data(ty);
    let float = (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::builtin::FloatType>()
        .expect("floating conversion must have a float result");
    let value = format(float);
    let width = (u32::BITS - value.leading_zeros()).max(1);
    let node = g.add_node(tir::sem::SymKind::Constant);
    g.set_leaf_data(
        node,
        tir::sem::SymPayload::Int(tir_adt::APInt::new(width, value as u64)),
    );
    node
}

operation! {
    SIToFPOp {
        name: "sitofp",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
        sem: "(set result (sitofp input $float_exponent $float_mantissa))",
    }
}

impl SIToFPOp {
    fn float_exponent(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> tir::graph::NodeId {
        float_format_node(&self.0, g, crate::builtin::FloatType::exp_width)
    }

    fn float_mantissa(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> tir::graph::NodeId {
        float_format_node(&self.0, g, crate::builtin::FloatType::mant_width)
    }
}

operation! {
    UIToFPOp {
        name: "uitofp",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::IntegerType",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
        sem: "(set result (uitofp input $float_exponent $float_mantissa))",
    }
}

impl UIToFPOp {
    fn float_exponent(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> tir::graph::NodeId {
        float_format_node(&self.0, g, crate::builtin::FloatType::exp_width)
    }

    fn float_mantissa(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> tir::graph::NodeId {
        float_format_node(&self.0, g, crate::builtin::FloatType::mant_width)
    }
}

operation! {
    CmpFOp {
        name: "cmpf",
        dialect: "builtin",
        attributes: A {
            predicate: "Str",
        },
        operands: O {
            lhs: "crate::builtin::FloatType",
            rhs: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::Integer<1>",
        },
        sem: "(set result $cmp_expr)",
    }
}

pub use tir_symbolic::sem::cmpf_semantics;

impl CmpFOp {
    fn cmp_expr(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        let predicate = match self.0.attr("predicate")? {
            tir::attributes::AttributeValue::Str(value) => value,
            _ => return None,
        };
        cmpf_semantics(g, &predicate)
    }
}

impl CmpFOpBuilder {
    pub fn predicate(self, predicate: &str) -> Self {
        self.attr(
            "predicate",
            tir::attributes::AttributeValue::Str(predicate.to_string().into()),
        )
    }
}

operation! {
    FPToSIOp {
        name: "fptosi",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (fptosi input))",
    }
}

operation! {
    FPToUIOp {
        name: "fptoui",
        dialect: "builtin",
        operands: O {
            input: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::builtin::IntegerType",
        },
        sem: "(set result (fptoui input))",
    }
}

impl ConstantFOpBuilder {
    /// The constant, held as `f64`; every supported format embeds in it exactly.
    pub fn value(self, v: f64) -> Self {
        self.attr("value", AttributeValue::F64(v))
    }
}

operation! {
    AddFOp {
        name: "addf",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::FloatType",
            rhs: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
        interfaces: [Commutative, SameOperandAndResultType],
        sem: "(set result (fadd lhs rhs))",
    }
}

impl Commutative for AddFOp {}
impl SameOperandAndResultType for AddFOp {}

operation! {
    SubFOp {
        name: "subf",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::FloatType",
            rhs: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
        interfaces: [SameOperandAndResultType],
        sem: "(set result (fsub lhs rhs))",
    }
}

impl SameOperandAndResultType for SubFOp {}

operation! {
    MulFOp {
        name: "mulf",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::FloatType",
            rhs: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
        interfaces: [Commutative, SameOperandAndResultType],
        sem: "(set result (fmul lhs rhs))",
    }
}

impl Commutative for MulFOp {}
impl SameOperandAndResultType for MulFOp {}

operation! {
    DivFOp {
        name: "divf",
        dialect: "builtin",
        operands: O {
            lhs: "crate::builtin::FloatType",
            rhs: "crate::builtin::FloatType",
        },
        results: R {
            result: "crate::builtin::FloatType",
        },
        interfaces: [SameOperandAndResultType],
        sem: "(set result (fdiv lhs rhs))",
    }
}

impl SameOperandAndResultType for DivFOp {}
