//! AnalysisManager caching, def-use analysis and the monotone fact solver.

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

/// Whether a slot was reached: the smallest lattice with something to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reached(bool);

impl tir::analysis::solver::Lattice for Reached {
    fn bottom() -> Self {
        Self(false)
    }

    fn join(&self, other: &Self) -> Self {
        Self(self.0 || other.0)
    }
}

/// A node of a key space with holes in it, and a ceiling the seed never
/// mentions.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Slot(usize);

/// Slot 0 reaches slot 250, which reaches slot 500, and there it stops.
struct Chain;

impl tir::analysis::solver::FactDomain for Chain {
    type Node = Slot;
    type Fact = Reached;

    fn seed(&self, facts: &mut tir::analysis::solver::Facts<Slot, Reached>) {
        facts.raise(Slot(0), Reached(true));
    }

    fn transfer(&self, node: Slot, facts: &mut tir::analysis::solver::Facts<Slot, Reached>) {
        if node.0 < 500 {
            facts.raise(Slot(node.0 + 250), Reached(true));
        }
    }
}

#[test]
fn a_fact_nothing_raised_reads_as_the_lattice_bottom() {
    let facts = tir::analysis::solver::solve(&Chain);

    assert_eq!(facts.get(Slot(0)), Reached(true));
    assert_eq!(facts.get(Slot(250)), Reached(true));
    assert_eq!(facts.get(Slot(500)), Reached(true));
    // A hole between two raised slots, and a slot past every one of them.
    assert_eq!(facts.get(Slot(251)), Reached(false));
    assert_eq!(facts.get(Slot(100_000)), Reached(false));
}
