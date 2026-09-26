use tir::fp::{
    ops, ArithmeticSemantics, Exceptions, IntegerConversionSemantics, InvalidConversion, NaNPolicy,
    Rounding, RoundingMode, SubnormalMode, Tininess,
};
use tir::sem::{SemGraph, SymKind, Value};
use tir::{
    builtin::{FloatType, IntegerType},
    ConstantFold, Context, Operation,
};
use tir_adt::{APFloat, APInt};

#[test]
fn canonical_nan_arithmetic_folds_to_the_canonical_payload() {
    let context = Context::with_default_dialects();
    let ty = FloatType::f64(&context);
    let lhs = context.create_value(ty, None);
    let rhs = context.create_value(ty, None);
    let op = ops::AddOpBuilder::new(&context)
        .lhs(lhs.id())
        .rhs(rhs.id())
        .semantics(context.intern_fp_semantics(ArithmeticSemantics {
            rounding: Rounding::Fixed(RoundingMode::TiesToEven),
            exceptions: Exceptions::Ignore,
            nan: NaNPolicy::Canonical,
            subnormals: SubnormalMode::Gradual,
            tininess: Tininess::AfterRounding,
        }))
        .result_type(ty)
        .build();

    let folded = context
        .get_op(op.id())
        .as_interface::<dyn ConstantFold>()
        .expect("FP arithmetic exposes constant folding")
        .fold(&[
            Value::Float(APFloat::from_bits(11, 52, false, 0x7ff8_0000_0000_1234)),
            Value::Float(APFloat::from_bits(11, 52, false, 0)),
        ])
        .expect("folds exact operands");

    let Value::Float(value) = folded else {
        panic!("expected a floating result")
    };
    assert_eq!(value.to_bits(), 0x7ff8_0000_0000_0000);
}

#[test]
fn rounded_float_conversion_folds_to_its_numeric_value() {
    let context = Context::with_default_dialects();
    let input_type = FloatType::f64(&context);
    let input = context.create_value(input_type, None);
    let op = ops::ConvertOpBuilder::new(&context)
        .input(input.id())
        .semantics(context.intern_fp_semantics(ArithmeticSemantics::strict(
            RoundingMode::TowardPositive,
            Exceptions::Ignore,
        )))
        .result_type(FloatType::f32(&context))
        .build();

    let folded = context
        .get_op(op.id())
        .as_interface::<dyn ConstantFold>()
        .expect("float conversion exposes constant folding")
        .fold(&[Value::Float(APFloat::from_bits(
            11,
            52,
            false,
            0x3ff0_0000_1000_0000,
        ))])
        .expect("folds exact operand");

    let Value::Float(value) = folded else {
        panic!("expected a floating result")
    };
    assert_eq!(value.to_bits(), 0x3f80_0001);
}

#[test]
fn canonical_nan_float_conversion_folds_to_the_canonical_payload() {
    let context = Context::with_default_dialects();
    let input_type = FloatType::f64(&context);
    let input = context.create_value(input_type, None);
    let op = ops::ConvertOpBuilder::new(&context)
        .input(input.id())
        .semantics(context.intern_fp_semantics(ArithmeticSemantics {
            rounding: Rounding::Fixed(RoundingMode::TiesToEven),
            exceptions: Exceptions::Ignore,
            nan: NaNPolicy::Canonical,
            subnormals: SubnormalMode::Gradual,
            tininess: Tininess::AfterRounding,
        }))
        .result_type(FloatType::f32(&context))
        .build();

    let folded = context
        .get_op(op.id())
        .as_interface::<dyn ConstantFold>()
        .expect("float conversion exposes constant folding")
        .fold(&[Value::Float(APFloat::from_bits(
            11,
            52,
            false,
            0x7ff8_0000_2000_0000,
        ))])
        .expect("folds exact operand");

    let Value::Float(value) = folded else {
        panic!("expected a floating result")
    };
    assert_eq!(value.to_bits(), 0x7fc0_0000);
}

#[test]
fn rounded_integer_conversion_folds_to_its_numeric_value() {
    let context = Context::with_default_dialects();
    let input_type = FloatType::f64(&context);
    let input = context.create_value(input_type, None);
    let op = ops::ToSiOpBuilder::new(&context)
        .input(input.id())
        .semantics(
            context.intern_fp_semantics(tir::fp::Semantics::IntegerConversion(
                IntegerConversionSemantics {
                    rounding: Rounding::Fixed(RoundingMode::TiesToEven),
                    exceptions: Exceptions::Ignore,
                    subnormals: SubnormalMode::Gradual,
                    invalid: InvalidConversion::Indeterminate,
                },
            )),
        )
        .result_type(IntegerType::new(&context, 64))
        .build();

    let folded = context
        .get_op(op.id())
        .as_interface::<dyn ConstantFold>()
        .expect("integer conversion exposes constant folding")
        .fold(&[Value::Float(APFloat::from_bits(
            11,
            52,
            false,
            0x3ff8_0000_0000_0000,
        ))])
        .expect("folds exact operand");

    assert_eq!(folded, Value::Int(APInt::new(64, 2)));
}

#[test]
fn fixed_fp_ops_fold_via_exact_semantics() {
    let context = Context::with_default_dialects();
    let f32_ty = FloatType::f32(&context);
    let lhs = context.create_value(f32_ty, None);
    let rhs = context.create_value(f32_ty, None);
    let op = ops::MulOpBuilder::new(&context)
        .lhs(lhs.id())
        .rhs(rhs.id())
        .semantics(context.intern_fp_semantics(ArithmeticSemantics::strict(
            RoundingMode::TowardPositive,
            Exceptions::Ignore,
        )))
        .result_type(f32_ty)
        .build();

    let folded = context
        .get_op(op.id())
        .as_interface::<dyn ConstantFold>()
        .expect("fixed fp operation exposes constant folding")
        .fold(&[
            Value::Float(APFloat::from_bits(8, 23, false, 0x3f80_0001)),
            Value::Float(APFloat::from_bits(8, 23, false, 0x3f7f_fffe)),
        ])
        .expect("folds exact operands");

    let Value::Float(value) = folded else {
        panic!("expected a floating result")
    };
    assert_eq!(value.to_bits(), 0x3f80_0000);
}

#[test]
fn preserve_payload_arithmetic_exposes_an_exact_bit_observation() {
    let context = Context::with_default_dialects();
    let ty = FloatType::f64(&context);
    let lhs = context.create_value(ty, None);
    let rhs = context.create_value(ty, None);
    let op = ops::AddOpBuilder::new(&context)
        .lhs(lhs.id())
        .rhs(rhs.id())
        .semantics(context.intern_fp_semantics(ArithmeticSemantics {
            rounding: Rounding::Fixed(RoundingMode::TiesToEven),
            exceptions: Exceptions::Ignore,
            nan: NaNPolicy::PreservePayload,
            subnormals: SubnormalMode::Gradual,
            tininess: Tininess::AfterRounding,
        }))
        .result_type(ty)
        .build();

    let mut graph = SemGraph::new();
    let root = context
        .get_op(op.id())
        .as_dyn_op()
        .semantic_expr(&mut graph)
        .unwrap();
    assert_eq!(*graph.get_node(root), SymKind::AsFloat);
    let bits = graph.children(root).next().unwrap();
    assert_eq!(*graph.get_node(bits), SymKind::Bitcast);
}
