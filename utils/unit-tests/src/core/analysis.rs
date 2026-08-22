//! AnalysisManager caching and def-use analysis.

use std::rc::Rc;

use tir::{
    analysis::DefUse,
    builtin::{ops, IntegerType, UnitType},
    func::ops as func_ops,
    Analysis, AnalysisManager, Context, OpId, Operand, Operation,
};

struct Simple;

impl Analysis for Simple {
    fn build(_: &AnalysisManager, _: &Context, _: OpId) -> Self {
        Simple
    }
}

/// Depends on [`Simple`]: built through the manager, so both are keyed on the
/// same op version and go stale together.
struct Dependent;

impl Analysis for Dependent {
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
        analyses.get::<Simple>(context, op);
        Dependent
    }
}

fn test_op(context: &Context) -> OpId {
    ops::module(context, None).build().id()
}

/// Appends an op to `module`'s body, bumping its version.
fn edit(context: &Context, module: OpId) {
    let body = context.get_op(module).regions()[0];
    let block = context
        .get_region(body)
        .iter(context.clone())
        .next()
        .expect("module has an entry block");
    block.append(func_ops::r#return(context, Operand::none()).build().id());
}

#[test]
fn caches_per_op() {
    let context = Context::with_default_dialects();
    let a = test_op(&context);
    let b = test_op(&context);
    let am = AnalysisManager::new();

    let first = am.get::<Simple>(&context, a);
    assert!(Rc::ptr_eq(&first, &am.get::<Simple>(&context, a)));
    assert!(!Rc::ptr_eq(&first, &am.get::<Simple>(&context, b)));
}

#[test]
fn get_cached_never_computes() {
    let context = Context::with_default_dialects();
    let op = test_op(&context);
    let am = AnalysisManager::new();

    assert!(am.get_cached::<Simple>(&context, op).is_none());
    let built = am.get::<Simple>(&context, op);
    assert!(Rc::ptr_eq(
        &built,
        &am.get_cached::<Simple>(&context, op).unwrap()
    ));
}

#[test]
fn an_edit_makes_the_cached_analysis_stale() {
    let context = Context::with_default_dialects();
    let op = test_op(&context);
    let am = AnalysisManager::new();
    let first = am.get::<Simple>(&context, op);

    edit(&context, op);

    assert!(am.get_cached::<Simple>(&context, op).is_none());
    assert!(!Rc::ptr_eq(&first, &am.get::<Simple>(&context, op)));
}

#[test]
fn an_edit_elsewhere_leaves_the_cached_analysis_alone() {
    let context = Context::with_default_dialects();
    let op = test_op(&context);
    let other = test_op(&context);
    let am = AnalysisManager::new();
    let first = am.get::<Simple>(&context, op);

    edit(&context, other);

    assert!(Rc::ptr_eq(&first, &am.get::<Simple>(&context, op)));
}

#[test]
fn a_dependency_goes_stale_with_its_dependent() {
    let context = Context::with_default_dialects();
    let op = test_op(&context);
    let am = AnalysisManager::new();

    am.get::<Dependent>(&context, op);
    assert!(am.get_cached::<Simple>(&context, op).is_some());

    edit(&context, op);

    assert!(am.get_cached::<Dependent>(&context, op).is_none());
    assert!(am.get_cached::<Simple>(&context, op).is_none());
}

#[test]
fn stale_entries_do_not_accumulate() {
    let context = Context::with_default_dialects();
    let op = test_op(&context);
    let am = AnalysisManager::new();

    for _ in 0..8 {
        am.get::<Simple>(&context, op);
        edit(&context, op);
    }

    assert_eq!(am.cached_count(), 1);
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
    let dead = b.append_op(ops::constant(&context, 7, i32).build());
    let sum = b.append_op(ops::addi(&context, arg_id, arg_id, i32).build());
    let sum_val = sum.result();
    let call = b.append_op(
        tir::func::CallOpBuilder::new(&context)
            .args(vec![sum_val])
            .attr("callee", tir::attributes::AttributeValue::Str("g".into()))
            .result_type(i32)
            .build(),
    );
    let call_result = call.result();
    let ret = b.append_op(func_ops::r#return(&context, call_result).build());

    let am = AnalysisManager::new();
    let du = am.get::<DefUse>(&context, func.id());

    // The block argument is read twice by the add and defined by no op.
    assert_eq!(du.users_of(arg_id.number()), [sum.id(), sum.id()]);
    assert!(du.defs_of(arg_id.number()).is_empty());

    assert_eq!(du.defs_of(sum_val.number()), [sum.id()]);
    assert_eq!(du.users_of(sum_val.number()), [call.id()]);

    // Call arguments count as uses, and its result chains on.
    assert!(du.is_used(sum_val.number()));
    assert_eq!(du.defs_of(call_result.number()), [call.id()]);
    assert_eq!(du.users_of(call_result.number()), [ret.id()]);

    assert!(!du.is_used(dead.result().number()));
    assert_eq!(du.use_counts()[&arg_id.number()], 2);
}
