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
use crate::{Commutative, Context, OpId, SameOperandType, attributes::AttributeValue};

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
        let owner_attr =
            context
                .get_op(owner)
                .attributes
                .iter()
                .find_map(|a| match (&*a.name, &a.value) {
                    (FPMATH_ATTR, AttributeValue::Str(spec)) => Some(spec.clone()),
                    _ => None,
                });
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
    op: &tir::OpInstance,
    g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    format: impl FnOnce(&crate::builtin::FloatType) -> u32,
) -> tir::graph::NodeId {
    let context = op.context.upgrade();
    let ty = context.get_value(op.results[0]).ty();
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
        interfaces: [Commutative, SameOperandType],
        sem: "(set result (fadd lhs rhs))",
    }
}

impl Commutative for AddFOp {}
impl SameOperandType for AddFOp {}

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
        interfaces: [SameOperandType],
        sem: "(set result (fsub lhs rhs))",
    }
}

impl SameOperandType for SubFOp {}

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
        interfaces: [Commutative, SameOperandType],
        sem: "(set result (fmul lhs rhs))",
    }
}

impl Commutative for MulFOp {}
impl SameOperandType for MulFOp {}

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
        interfaces: [SameOperandType],
        sem: "(set result (fdiv lhs rhs))",
    }
}

impl SameOperandType for DivFOp {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Operation,
        builtin::{FloatType, UnitType, ops},
        parse::ir::parse_ir,
        sem::Value,
    };
    use tir_adt::APFloat;

    #[test]
    fn float_type_roundtrip() {
        let context = Context::with_default_dialects();
        for (make, spelled) in [
            (
                FloatType::f8e4m3 as fn(&Context) -> crate::TypeId,
                "!f8e4m3",
            ),
            (FloatType::f8e5m2, "!f8e5m2"),
            (FloatType::f16, "!f16"),
            (FloatType::bf16, "!bf16"),
            (FloatType::f32, "!f32"),
            (FloatType::f64, "!f64"),
        ] {
            let ty = make(&context);
            assert_eq!(context.type_to_string(ty), spelled);
            assert_eq!(context.parse_type_spec(spelled).unwrap(), ty);
        }
        assert_ne!(FloatType::bf16(&context), FloatType::f16(&context));
    }

    #[test]
    fn addf_works_for_every_float_type() {
        let context = Context::with_default_dialects();
        for ty in [
            FloatType::f8e4m3(&context),
            FloatType::f8e5m2(&context),
            FloatType::f16(&context),
            FloatType::bf16(&context),
            FloatType::f32(&context),
            FloatType::f64(&context),
        ] {
            let lhs = ops::constantf(&context, 1.5, ty).build();
            let rhs = ops::constantf(&context, 2.5, ty).build();
            let op = ops::addf(&context, lhs.result(), rhs.result(), ty).build();
            assert!(op.verify(&context).is_ok());
            assert_eq!(context.get_value(op.result()).ty(), ty);
        }
    }

    #[test]
    fn addf_rejects_integer_operands() {
        let context = Context::with_default_dialects();
        let i32_ty = crate::builtin::IntegerType::new(&context, 32);
        let lhs = ops::constant(&context, 1, i32_ty).build();
        let rhs = ops::constant(&context, 2, i32_ty).build();
        let op = ops::addf(
            &context,
            lhs.result(),
            rhs.result(),
            FloatType::f32(&context),
        )
        .build();
        assert!(op.verify(&context).is_err());
    }

    #[test]
    fn parse_single_addf() {
        let context = Context::with_default_dialects();
        let c0 = ops::constantf(&context, 1.5, FloatType::f32(&context)).build();
        let c1 = ops::constantf(&context, 2.5, FloatType::f32(&context)).build();
        let src = format!(
            "%2 = addf %{}, %{} : !f32\n",
            c0.result().number(),
            c1.result().number()
        );
        let op = parse_ir::<AddFOp>(&context, &src).expect("Failed to parse addf");
        assert!(op.verify(&context).is_ok());
    }

    #[test]
    fn fp_ops_fold_via_sem() {
        use crate::ConstantFold;

        let context = Context::with_default_dialects();
        let f32_ty = FloatType::f32(&context);
        let a = context.create_value(f32_ty, None);
        let b = context.create_value(f32_ty, None);
        let op = ops::mulf(&context, a.id(), b.id(), f32_ty).build();

        let fold = context
            .get_op(op.id())
            .as_interface::<dyn ConstantFold>()
            .expect("mulf derives ConstantFold from its sem");
        let folded = fold
            .fold(&[
                Value::Float(APFloat::from_f64(3.0)),
                Value::Float(APFloat::from_f64(0.5)),
            ])
            .expect("folds two constants");
        match folded {
            Value::Float(v) => assert_eq!(v.to_f64(), 1.5),
            other => panic!("expected a float, got {other:?}"),
        }
    }

    #[test]
    fn fast_math_flags_parse_and_print() {
        assert_eq!(FastMathFlags::parse("none"), Some(FastMathFlags::NONE));
        assert_eq!(FastMathFlags::parse("fast"), Some(FastMathFlags::FAST));
        let flags = FastMathFlags::parse("contract, nnan").unwrap();
        assert!(flags.contains(FastMathFlags::CONTRACT));
        assert!(flags.contains(FastMathFlags::NNAN));
        assert!(!flags.contains(FastMathFlags::REASSOC));
        assert_eq!(flags.to_string(), "contract,nnan");
        assert_eq!(FastMathFlags::parse("wibble"), None);
        assert_eq!(FastMathFlags::FAST.to_string(), "fast");
        assert_eq!(
            FastMathFlags::parse(&FastMathFlags::FAST.to_string()),
            Some(FastMathFlags::FAST)
        );
    }

    /// A func with `fpmath` and an op in its body: the op inherits the func's
    /// flags through the region chain; without any attribute the default is
    /// strict.
    #[test]
    fn fp_math_flags_inherited_from_region_owner() {
        let context = Context::with_default_dialects();
        let f32_ty = FloatType::f32(&context);
        let func = ops::func(&context, "fma_candidate", UnitType::new(&context), None)
            .attr(FPMATH_ATTR, AttributeValue::Str("contract".into()))
            .build();
        let a = context.create_value(f32_ty, None);
        let b = context.create_value(f32_ty, None);
        let add = ops::addf(&context, a.id(), b.id(), f32_ty).build();
        func.body().insert(0, add.id());

        assert_eq!(fp_math_flags(&context, add.id()), FastMathFlags::CONTRACT);

        // Detached op: no enclosing scope, strict by default.
        let stray = ops::addf(&context, a.id(), b.id(), f32_ty).build();
        assert_eq!(fp_math_flags(&context, stray.id()), FastMathFlags::NONE);
    }

    /// A block-level `fpmath` shadows the enclosing op's, both to relax further
    /// and to restore strictness inside a fast region.
    #[test]
    fn fp_math_flags_block_overrides_owner() {
        let context = Context::with_default_dialects();
        let f32_ty = FloatType::f32(&context);
        let func = ops::func(&context, "scoped", UnitType::new(&context), None)
            .attr(FPMATH_ATTR, AttributeValue::Str("fast".into()))
            .build();
        let a = context.create_value(f32_ty, None);
        let b = context.create_value(f32_ty, None);

        // An op right in the body sees the func's flags.
        let fast_add = ops::addf(&context, a.id(), b.id(), f32_ty).build();
        func.body().insert(0, fast_add.id());
        assert_eq!(fp_math_flags(&context, fast_add.id()), FastMathFlags::FAST);

        // An op in a nested block marked strict does not.
        let cond = context.create_value(crate::builtin::IntegerType::new(&context, 1), None);
        let if_op = crate::dialects::scf::r#if(&context, cond.id(), None, None, None).build();
        let strict_add = ops::addf(&context, a.id(), b.id(), f32_ty).build();
        let then_block = if_op.then_body();
        then_block.insert(0, strict_add.id());
        func.body().insert(1, if_op.id());

        // Before the override the inner block inherits `fast` through scf.if.
        assert_eq!(
            fp_math_flags(&context, strict_add.id()),
            FastMathFlags::FAST
        );
        then_block.set_attr(FPMATH_ATTR, AttributeValue::Str("none".into()));
        assert_eq!(
            fp_math_flags(&context, strict_add.id()),
            FastMathFlags::NONE
        );
    }

    /// `fpmath` on a block survives print → parse via the block label.
    #[test]
    fn fpmath_block_attribute_roundtrip() {
        let context = Context::with_default_dialects();
        let src = r#"module {
func @scoped(%0: !f32) -> !f32 {
^bb0 {fpmath = "contract"}:
%1 = addf %0, %0 : !f32
return %1
}
module_end
}"#;
        let module = parse_ir::<crate::builtin::ModuleOp>(&context, src).expect("parses");
        let mut printed = String::new();
        let mut fmt = crate::IRFormatter::new(&mut printed);
        module.print(&mut fmt).expect("print ok");
        drop(fmt);
        assert!(
            printed.contains("{fpmath = \"contract\"}:"),
            "block attribute lost in: {printed}"
        );
        parse_ir::<crate::builtin::ModuleOp>(&context, &printed).expect("reparses");
    }
}
