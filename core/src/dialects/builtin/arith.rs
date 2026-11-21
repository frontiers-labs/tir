pub use super::generated::{
    AddIOp, AddIOpBuilder, AndIOp, AndIOpBuilder, BitcastOp, BitcastOpBuilder, CmpIOp,
    CmpIOpBuilder, ConstantOp, ConstantOpBuilder, DivSIOp, DivSIOpBuilder, DivUIOp, DivUIOpBuilder,
    ExtSIOp, ExtSIOpBuilder, ExtUIOp, ExtUIOpBuilder, MulIOp, MulIOpBuilder, OrIOp, OrIOpBuilder,
    RemSIOp, RemSIOpBuilder, RemUIOp, RemUIOpBuilder, ShlIOp, ShlIOpBuilder, ShrSIOp,
    ShrSIOpBuilder, ShrUIOp, ShrUIOpBuilder, SubIOp, SubIOpBuilder, TruncIOp, TruncIOpBuilder,
    XOrIOp, XOrIOpBuilder, addi, andi, bitcast, cmpi, constant, divsi, divui, extsi, extui, muli,
    ori, remsi, remui, shli, shrsi, shrui, subi, trunci, xori,
};

use crate as tir;
use crate::Context;

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
