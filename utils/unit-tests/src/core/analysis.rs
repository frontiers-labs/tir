//! AnalysisManager caching, def-use analysis and the monotone fact solver.

use tir::{
    analysis::DefUse,
    builtin::{ops, IntegerType, UnitType},
    func::ops as func_ops,
    Analysis, AnalysisManager, Context, OpId, Operation,
};

pub(crate) struct Simple;

impl Analysis for Simple {
    fn build(_: &AnalysisManager, _: &Context, _: OpId) -> Self {
        Simple
    }
}

#[test]
fn chains_over_ssa_ops() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);

    let region = context.create_region();
    let arg = context.create_value(i32, None);
    let arg_id = arg.id();
    let block = context.create_block(vec![arg]);
    region.add_block(block.id());
    let func = func_ops::lambda(&context, "f", UnitType::new(&context), &region).build();

    let b = block;
    let dead = ops::constant(&context, 7, i32).at(b.end()).build();
    let sum = ops::addi(&context, arg_id, arg_id, i32).at(b.end()).build();
    let sum_val = sum.result();
    let call = tir::func::CallOpBuilder::new(&context)
        .args(vec![sum_val])
        .attr("callee", tir::attributes::AttributeValue::Str("g".into()))
        .result_type(i32)
        .at(b.end())
        .build();
    let call_result = call.result();
    let ret = func_ops::r#return(&context, call_result)
        .at(b.end())
        .build();

    let am = AnalysisManager::new();
    let du = am.get::<DefUse>(&context, func.id());

    // The block argument is read twice by the add and defined by no op.
    assert_eq!(context.users_of(arg_id), [sum.id(), sum.id()]);
    assert!(du.defs_of(arg_id.number()).is_empty());

    assert_eq!(du.defs_of(sum_val.number()), [sum.id()]);
    assert_eq!(context.users_of(sum_val), [call.id()]);

    // Call arguments count as uses, and its result chains on.
    assert!(context.is_used(sum_val));
    assert_eq!(du.defs_of(call_result.number()), [call.id()]);
    assert_eq!(context.users_of(call_result), [ret.id()]);

    assert!(!context.is_used(dead.result()));
    assert_eq!(context.use_count(arg_id), 2);
}
