use tir_adt::{APFloat, APInt, FloatClass, FloatWidth, classify_float};

use super::arithmetic::{float_parts, width_of_float};
use crate::{SameOperandAndResultType, Speculatable, operation};

use crate as tir;

operation! {
    NegOp {
        name: "neg", dialect: "fp",
        operands: O { input: "crate::builtin::FloatType" },
        results: R { result: "crate::builtin::FloatType" },
        interfaces: [SameOperandAndResultType, Speculatable, crate::interp::Interp],
        sem: "(set result $value_semantics)",
    }
}

operation! {
    AbsOp {
        name: "abs", dialect: "fp",
        operands: O { input: "crate::builtin::FloatType" },
        results: R { result: "crate::builtin::FloatType" },
        interfaces: [SameOperandAndResultType, Speculatable, crate::interp::Interp],
        sem: "(set result $value_semantics)",
    }
}

operation! {
    CopySignOp {
        name: "copysign", dialect: "fp",
        operands: O { magnitude: "crate::builtin::FloatType", sign: "crate::builtin::FloatType" },
        results: R { result: "crate::builtin::FloatType" },
        interfaces: [SameOperandAndResultType, Speculatable, crate::interp::Interp],
    }
}

operation! {
    SignBitOp {
        name: "signbit", dialect: "fp",
        operands: O { input: "crate::builtin::FloatType" },
        results: R { result: "crate::Integer<1>" },
        interfaces: [Speculatable, crate::interp::Interp],
    }
}

operation! {
    ClassifyOp {
        name: "classify", dialect: "fp",
        operands: O { input: "crate::builtin::FloatType" },
        attributes: A { class: "Str" },
        results: R { result: "crate::Integer<1>" },
        interfaces: [Speculatable, crate::interp::Interp],
        verifier: "true",
    }
}

impl SameOperandAndResultType for NegOp {}
impl SameOperandAndResultType for AbsOp {}
impl SameOperandAndResultType for CopySignOp {}
impl Speculatable for NegOp {}
impl Speculatable for AbsOp {}
impl Speculatable for CopySignOp {}
impl Speculatable for SignBitOp {}
impl Speculatable for ClassifyOp {}

fn unary_bit_semantics(
    context: &tir::Context,
    result: tir::ValueId,
    graph: &mut impl tir::graph::MutDag<
        Node = tir::sem::SymKind,
        Leaf = tir::sem::SymPayload<tir::ValueId>,
    >,
    op: tir::sem::SymKind,
    mask: u64,
) -> Option<tir::graph::NodeId> {
    use tir::sem::{SymKind, SymPayload};

    let ty = context.get_type_data(context.get_value(result).ty());
    let width = (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::builtin::FloatType>()?
        .bit_width();
    let input = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(input, SymPayload::SymbolId(0));
    let bits = graph.add_node(SymKind::Bitcast);
    graph.add_edge(bits, input);
    let constant = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(constant, SymPayload::Int(APInt::new(width, mask)));
    let changed = graph.add_node(op);
    graph.add_edge(changed, bits);
    graph.add_edge(changed, constant);
    let output = graph.add_node(SymKind::Bitcast);
    graph.add_edge(output, changed);
    Some(output)
}

impl NegOp {
    fn value_semantics(
        &self,
        graph: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        let ty = self
            .0
            .context
            .get_type_data(self.0.context.get_value(self.result()).ty());
        let width = (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<crate::builtin::FloatType>()?
            .bit_width();
        let sign = 1u64 << (width - 1);
        unary_bit_semantics(
            &self.0.context,
            self.result(),
            graph,
            tir::sem::SymKind::Xor,
            sign,
        )
    }
}

impl AbsOp {
    fn value_semantics(
        &self,
        graph: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> Option<tir::graph::NodeId> {
        let ty = self
            .0
            .context
            .get_type_data(self.0.context.get_value(self.result()).ty());
        let width = (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<crate::builtin::FloatType>()?
            .bit_width();
        let mask = (1u64 << (width - 1)) - 1;
        unary_bit_semantics(
            &self.0.context,
            self.result(),
            graph,
            tir::sem::SymKind::And,
            mask,
        )
    }
}

impl tir::Verifiable for ClassifyOp {
    fn verify_impl(&self, _: &tir::Context) -> Result<(), tir::Error> {
        parse_class(&self.class())
            .ok_or_else(|| tir::Error::VerificationError("unknown floating-point class".into()))?;
        Ok(())
    }
}

fn parse_class(name: &str) -> Option<FloatClass> {
    Some(match name {
        "signaling_nan" => FloatClass::SignalingNaN,
        "quiet_nan" => FloatClass::QuietNaN,
        "negative_infinity" => FloatClass::NegativeInfinity,
        "negative_normal" => FloatClass::NegativeNormal,
        "negative_subnormal" => FloatClass::NegativeSubnormal,
        "negative_zero" => FloatClass::NegativeZero,
        "positive_zero" => FloatClass::PositiveZero,
        "positive_subnormal" => FloatClass::PositiveSubnormal,
        "positive_normal" => FloatClass::PositiveNormal,
        "positive_infinity" => FloatClass::PositiveInfinity,
        _ => return None,
    })
}

fn float(value: &crate::interp::Value) -> Result<&APFloat, crate::interp::InterpError> {
    let crate::interp::Value::Float(value) = value else {
        return Err(crate::interp::InterpError::Message(
            "floating bit operation requires a float".into(),
        ));
    };
    Ok(value)
}

fn sign_mask(width: FloatWidth) -> u64 {
    match width {
        FloatWidth::W32 => 1 << 31,
        FloatWidth::W64 => 1 << 63,
    }
}

fn from_bits(width: FloatWidth, bits: u64) -> crate::interp::Value {
    let (exponent, mantissa) = float_parts(width);
    crate::interp::Value::Float(APFloat::from_bits(exponent, mantissa, false, bits as u128))
}

macro_rules! unary_bits {
    ($op:ident, $apply:expr) => {
        impl crate::interp::Interp for $op {
            fn evaluate(
                &self,
                operands: &[crate::interp::Value],
                _: &mut crate::interp::ExecutionState,
            ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
                let value = float(&operands[0])?;
                let width = width_of_float(value)?;
                Ok(vec![from_bits(
                    width,
                    $apply(value.to_bits() as u64, sign_mask(width)),
                )])
            }
        }
    };
}

unary_bits!(NegOp, |bits: u64, sign: u64| bits ^ sign);
unary_bits!(AbsOp, |bits: u64, sign: u64| bits & !sign);

impl crate::interp::Interp for CopySignOp {
    fn evaluate(
        &self,
        operands: &[crate::interp::Value],
        _: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        let magnitude = float(&operands[0])?;
        let sign = float(&operands[1])?;
        let width = width_of_float(magnitude)?;
        if width_of_float(sign)? != width {
            return Err(crate::interp::InterpError::Message(
                "fp.copysign operands have different formats".into(),
            ));
        }
        let mask = sign_mask(width);
        let bits = (magnitude.to_bits() as u64 & !mask) | (sign.to_bits() as u64 & mask);
        Ok(vec![from_bits(width, bits)])
    }
}

impl crate::interp::Interp for SignBitOp {
    fn evaluate(
        &self,
        operands: &[crate::interp::Value],
        _: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        let value = float(&operands[0])?;
        let width = width_of_float(value)?;
        Ok(vec![crate::interp::Value::Int(APInt::new(
            1,
            u64::from(value.to_bits() as u64 & sign_mask(width) != 0),
        ))])
    }
}

impl crate::interp::Interp for ClassifyOp {
    fn evaluate(
        &self,
        operands: &[crate::interp::Value],
        _: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        let value = float(&operands[0])?;
        let width = width_of_float(value)?;
        let class = parse_class(&self.class()).expect("verified class");
        Ok(vec![crate::interp::Value::Int(APInt::new(
            1,
            u64::from(classify_float(width, value.to_bits() as u64) == class),
        ))])
    }
}
