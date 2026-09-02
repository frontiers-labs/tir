//! PassManager, Rewriter and pass-driven analysis invalidation.

use std::collections::HashMap;

use tir::{
    builtin::{ops, AddIOp, IntegerType},
    cfg::ops as cfg_ops,
    func::{ops as func_ops, FuncOp},
    AnalysisManager, Context, Operation, OperationRef, Pass, PassError, PassManager, PassTarget,
    Rewriter,
};

struct AddToSubPass;

impl Pass for AddToSubPass {
    fn name(&self) -> &'static str {
        "add-to-sub"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<AddIOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let add = op.as_op::<AddIOp>().expect("target guarantees AddIOp");
        let operands = add.operands();
        let result_ty = context.get_value(add.result()).ty();
        let new_op = ops::subi(context, operands[0], operands[1], result_ty).build();
        rewriter.replace_op(op, &new_op)
    }
}

/// Erases the addi while the `return` still reads its result, leaving a
/// dangling operand.
struct BreakIRPass;

impl Pass for BreakIRPass {
    fn name(&self) -> &'static str {
        "break-ir"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<AddIOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        _context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        rewriter.erase_op(op)
    }
}

/// Reads the IR and leaves it exactly as it found it.
struct ReadOnlyPass;

impl Pass for ReadOnlyPass {
    fn name(&self) -> &'static str {
        "read-only"
    }

    fn run(
        &mut self,
        _op: &OperationRef,
        _context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}

/// Mutates on every run, without changing what the IR means.
struct TouchPass;

impl Pass for TouchPass {
    fn name(&self) -> &'static str {
        "touch"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        _context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
        func.body()
            .set_attr("touched", tir::attributes::AttributeValue::Bool(true));
        Ok(())
    }
}

/// `func.func demo(%0) { %1 = addi %0, %0; func.return %1 }`, with `pass` run over it.
fn run_on_broken_candidate(pass: Box<dyn Pass>) -> Result<(), PassError> {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let region = context.create_region();
    let arg = context.create_value(i32, None);
    let block = context.create_block(vec![arg]);
    region.add_block(block.id());
    let func = func_ops::lambda(&context, "demo", i32, &region).build();
    let body = func.body();

    let add = ops::addi(
        &context,
        body.arguments()[0].id(),
        body.arguments()[0].id(),
        i32,
    )
    .build();
    let add_result = add.result();
    body.append_op(add);
    body.append_op(func_ops::r#return(&context, add_result).build());

    let mut pm = PassManager::new();
    pm.verify_ir(true);
    pm.add_boxed_pass(pass);
    pm.run(&context, context.get_op(func.id()))
}

#[test]
fn invalid_ir_after_a_pass_names_that_pass() {
    let error = run_on_broken_candidate(Box::new(BreakIRPass))
        .expect_err("erasing a still-used op must be caught");
    assert!(
        error.to_string().contains("break-ir"),
        "error should name the offending pass, got: {error}"
    );
}

#[test]
fn appending_a_block_argument_keeps_the_block_id() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let block = context.create_block(vec![]);
    let mut rewriter = Rewriter::new(context.clone());

    let argument = rewriter.append_block_argument(block.id(), i32);

    let block = context.get_block(block.id());
    assert_eq!(
        block.arguments().iter().map(|a| a.id()).collect::<Vec<_>>(),
        vec![argument.id()]
    );
}

#[test]
fn splitting_a_block_moves_its_tail_into_a_new_block() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let value = context.create_value(i32, None);
    let block = context.create_block(vec![]);
    let head = block.append_op(ops::addi(&context, value.id(), value.id(), i32).build());
    let tail = block.append_op(ops::subi(&context, value.id(), value.id(), i32).build());
    let mut rewriter = Rewriter::new(context.clone());

    let split = rewriter.split_block(block.id(), 1);

    assert_eq!(context.get_block(block.id()).op_ids(), vec![head.id()]);
    assert_eq!(split.op_ids(), vec![tail.id()]);
    assert_eq!(context.parent_block(tail.id()), Some(split.id()));
}

#[test]
fn splicing_a_block_appends_its_operations_to_another() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let value = context.create_value(i32, None);
    let destination = context.create_block(vec![]);
    let source = context.create_block(vec![]);
    let head = destination
        .clone()
        .append_op(ops::addi(&context, value.id(), value.id(), i32).build());
    let moved = source
        .clone()
        .append_op(ops::subi(&context, value.id(), value.id(), i32).build());
    let mut rewriter = Rewriter::new(context.clone());

    rewriter.splice_block(source.id(), destination.id());

    assert_eq!(
        context.get_block(destination.id()).op_ids(),
        vec![head.id(), moved.id()]
    );
    assert!(context.get_block(source.id()).is_empty());
    assert_eq!(context.parent_block(moved.id()), Some(destination.id()));
}

/// A function whose body block takes one argument, adds it to itself and
/// returns the sum.
fn function_with_one_argument(context: &Context) -> FuncOp {
    let i32 = IntegerType::new(context, 32);
    let region = context.create_region();
    let argument = context.create_value(i32, None);
    let block = context.create_block(vec![argument.clone()]);
    region.add_block(block.id());
    let add = block.append_op(ops::addi(context, argument.id(), argument.id(), i32).build());
    block.append_op(func_ops::r#return(context, add.result()).build());
    func_ops::lambda(context, "demo", i32, &region).build()
}

#[test]
fn cloning_an_op_remaps_values_defined_inside_it() {
    let context = Context::with_default_dialects();
    let source = function_with_one_argument(&context);
    let mut rewriter = Rewriter::new(context.clone());

    let clone = rewriter.clone_op(source.id());

    let clone = context.get_op(clone);
    assert_ne!(clone.id, source.id());
    let body = context
        .get_region(clone.regions()[0])
        .iter(context.clone())
        .next()
        .expect("the clone keeps the body block");
    let argument = body.arguments()[0].id();
    assert_ne!(argument, source.body().arguments()[0].id());
    let add = context.get_op(body.op_ids()[0]);
    assert_eq!(add.operands().as_slice(), vec![argument, argument]);
    let r#return = context.get_op(body.op_ids()[1]);
    assert_eq!(r#return.operands().as_slice(), vec![add.results()[0]]);
}

#[test]
fn cloning_a_region_remaps_branch_destinations() {
    let context = Context::with_default_dialects();
    let region = context.create_region();
    let entry = context.create_block(vec![]);
    let target = context.create_block(vec![]);
    region.add_block(entry.id());
    region.add_block(target.id());
    entry.append_op(cfg_ops::br(&context, vec![], target.id()).build());
    target
        .clone()
        .append_op(func_ops::r#return(&context, tir::Operand::none()).build());
    let mut rewriter = Rewriter::new(context.clone());

    let clone = rewriter.clone_region(region.id());

    let blocks: Vec<_> = context
        .get_region(clone)
        .iter(context.clone())
        .map(|block| block.id())
        .collect();
    assert_eq!(blocks.len(), 2);
    assert!(!blocks.contains(&target.id()));
    let branch = context
        .get_op(context.get_block(blocks[0]).op_ids()[0])
        .as_op::<tir::cfg::BranchOp>()
        .expect("the clone keeps the branch");
    assert_eq!(branch.dest(), blocks[1]);
}

#[test]
fn splicing_a_region_moves_its_blocks() {
    let context = Context::with_default_dialects();
    let source = context.create_region();
    let destination = context.create_region();
    let moved = context.create_block(vec![]);
    source.add_block(moved.id());
    let mut rewriter = Rewriter::new(context.clone());

    rewriter.splice_region(source.id(), destination.id());

    assert_eq!(source.iter(context.clone()).count(), 0);
    let blocks: Vec<_> = destination
        .iter(context.clone())
        .map(|block| block.id())
        .collect();
    assert_eq!(blocks, vec![moved.id()]);
    assert_eq!(context.parent_region(moved.id()), Some(destination.id()));
}

#[test]
fn a_pass_that_changes_nothing_is_not_verified() {
    run_on_broken_candidate(Box::new(ReadOnlyPass))
        .expect("a pass that left no version behind skips verification");
}

#[test]
fn an_analysis_survives_a_pass_that_changes_nothing() {
    use tir::analysis::DominatorTree;

    let context = Context::with_default_dialects();
    let func = function_with_one_argument(&context);
    let analyses = AnalysisManager::new();
    let before = analyses.get::<DominatorTree>(&context, func.id());

    let mut pm = PassManager::new();
    pm.add_pass(ReadOnlyPass);
    let root = OperationRef::new(context.get_op(func.id()));
    pm.run_on_op_ref(&context, root, &analyses)
        .expect("the pass changes nothing");

    assert!(std::rc::Rc::ptr_eq(
        &before,
        &analyses.get::<DominatorTree>(&context, func.id())
    ));
}

#[test]
fn repeated_pass_runs_do_not_grow_the_analysis_cache() {
    use tir::analysis::DominatorTree;

    let context = Context::with_default_dialects();
    let func = function_with_one_argument(&context);
    let analyses = AnalysisManager::new();
    let mut pm = PassManager::new();
    pm.add_pass(TouchPass);

    let mut counts = Vec::new();
    for _ in 0..8 {
        let root = OperationRef::new(context.get_op(func.id()));
        pm.run_on_op_ref(&context, root, &analyses)
            .expect("touching a block attribute keeps the IR valid");
        analyses.get::<DominatorTree>(&context, func.id());
        counts.push(analyses.cached_count());
    }

    assert!(
        counts.iter().all(|count| *count == counts[0]),
        "each rebuild must replace the stale result, got {counts:?}"
    );
}

#[test]
fn an_analysis_is_rebuilt_after_a_pass_mutates() {
    use tir::analysis::DominatorTree;

    let context = Context::with_default_dialects();
    let func = function_with_one_argument(&context);
    let analyses = AnalysisManager::new();
    let before = analyses.get::<DominatorTree>(&context, func.id());

    let mut pm = PassManager::new();
    pm.add_pass(AddToSubPass);
    let root = OperationRef::new(context.get_op(func.id()));
    pm.run_on_op_ref(&context, root, &analyses)
        .expect("rewriting addi to subi keeps the IR valid");

    assert!(!std::rc::Rc::ptr_eq(
        &before,
        &analyses.get::<DominatorTree>(&context, func.id())
    ));
}

#[test]
fn nested_pass_manager_rewrites_ops() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let param0 = context.create_value(IntegerType::new(&context, 32), None);
    let param1 = context.create_value(IntegerType::new(&context, 32), None);

    let region = context.create_region();
    let block = context.create_block(vec![param0, param1]);
    region.add_block(block.id());

    let func = func_ops::lambda(&context, "demo", IntegerType::new(&context, 32), &region).build();
    let func_body = func.body();
    let func_id = func.id();

    let func_builder = func_body.clone();
    let add = ops::addi(
        &context,
        func_body.arguments()[0].id(),
        func_body.arguments()[1].id(),
        IntegerType::new(&context, 32),
    )
    .build();
    let add_result = add.result();
    let add_id = add.id();
    func_builder.append_op(add);
    func_builder.append_op(func_ops::r#return(&context, add_result).build());

    module.body().append_op(func);

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>().add_pass(AddToSubPass);

    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let op_names: Vec<_> = func_body
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();

    assert_eq!(op_names, vec!["subi", "return"]);

    // The def-use chain followed the rewrite: param0 is now read by the subi
    // (the replacement), not by the erased addi.
    let subi_id = func_body.op_ids()[0];
    assert_eq!(
        tir::analysis::DefUse::new(&context, func_id)
            .users_of(func_body.arguments()[0].id().number()),
        [subi_id]
    );

    // The replaced-out addi is gone from the arena, not just the block.
    assert!(
        !context.has_operation(add_id),
        "replaced op should leave the arena"
    );
}

#[test]
fn erasing_an_op_drops_its_operand_uses() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);

    let region = context.create_region();
    let arg = context.create_value(i32, None);
    let block = context.create_block(vec![arg.clone()]);
    region.add_block(block.id());
    let func = func_ops::lambda(&context, "demo", i32, &region).build();
    let body = func.body();

    let neg = ops::subi(
        &context,
        body.arguments()[0].id(),
        body.arguments()[0].id(),
        i32,
    )
    .build();
    let neg_id = neg.id();
    let neg_ref = OperationRef::new(context.get_op(neg_id));
    body.append_op(neg);
    let argument = body.arguments()[0].id().number();
    assert!(tir::analysis::DefUse::new(&context, func.id()).is_used(argument));

    let mut rewriter = Rewriter::new(context.clone());
    rewriter.erase_op(&neg_ref).expect("erase should succeed");

    assert!(
        !tir::analysis::DefUse::new(&context, func.id()).is_used(argument),
        "erasing the only consumer must leave the value unused"
    );
    // The erased op is gone from the arena, not just the block.
    assert!(
        !context.has_operation(neg_id),
        "erased op should leave the arena"
    );
}

/// A ring of blocks, each branching to the next two: strongly connected,
/// entered at two of its members, and irreducible however it is traversed.
fn tangle(blocks: usize) -> String {
    let mut source = String::from("func.func @tangle(%0: !i1, %1: !i32) -> !i32 {\n");
    source.push_str("  cfg.cond_br %0, ^bb1, ^bb2\n");
    for block in 1..=blocks {
        let next = block % blocks + 1;
        let after = (block + 1) % blocks + 1;
        source.push_str(&format!("^bb{block}:\n"));
        source.push_str(&format!("  %v{block} = addi %1, %1 : !i32\n"));
        if block == blocks {
            source.push_str(&format!("  cfg.cond_br %0, ^bb{next}, ^bb{}\n", blocks + 1));
        } else {
            source.push_str(&format!("  cfg.cond_br %0, ^bb{next}, ^bb{after}\n"));
        }
    }
    source.push_str(&format!("^bb{}:\n  func.return %1\n}}\n", blocks + 1));
    source
}

fn count_ops(context: &Context, op: tir::OpId) -> usize {
    let instance = context.get_op(op);
    let mut total = 1;
    for region in instance.regions() {
        for block in context.get_region(region).block_ids() {
            for nested in context.get_block(block).op_ids() {
                total += count_ops(context, nested);
            }
        }
    }
    total
}

fn restructure_source(source: &str) -> (Context, FuncOp, usize) {
    let context = Context::with_default_dialects();
    let func = tir::parse::ir::parse_ir::<FuncOp>(&context, source).expect("parse");
    let before = count_ops(&context, func.id());
    let mut manager = PassManager::new();
    manager.add_pass(tir::passes::RestructurePass::new());
    manager
        .run(&context, context.get_op(func.id()))
        .expect("restructure");
    (context, func, before)
}

/// Restructuring copies no node, so the output grows linearly: over a
/// pathological irreducible graph every added block costs the same fixed
/// number of operations (the dispatch constants, one conditional and its
/// yields), and the output stays within 6x the input.
#[test]
fn a_pathological_graph_grows_linearly() {
    let mut measured = Vec::new();
    for blocks in [4, 8, 16, 32] {
        let (context, func, before) = restructure_source(&tangle(blocks));
        assert!(
            func.verify(&context).is_ok(),
            "restructured IR must verify: {:?}",
            func.verify(&context)
        );
        let after = count_ops(&context, func.id());
        assert!(
            after <= 6 * before,
            "{blocks} blocks: {before} operations became {after}"
        );
        measured.push((blocks, after));
    }

    let per_block = measured
        .windows(2)
        .map(|step| (step[1].1 - step[0].1) / (step[1].0 - step[0].0))
        .collect::<Vec<_>>();
    assert!(
        per_block.windows(2).all(|step| step[0] == step[1]),
        "every added block must cost the same: {per_block:?} from {measured:?}"
    );
}

/// A copy that runs somewhere else names its inputs under the caller's names —
/// the region's own arguments included, so the copy's arguments go unused rather
/// than shadowing what was bound.
#[test]
fn cloning_a_region_with_a_mapping_binds_arguments_and_outside_values() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let outside = context.create_block(Vec::new());
    let x = outside
        .append_op(ops::constant(&context, 1, i32).build())
        .result();
    let y = outside
        .append_op(ops::constant(&context, 2, i32).build())
        .result();
    let z = outside
        .append_op(ops::constant(&context, 3, i32).build())
        .result();
    let region = context.create_region();
    let argument = context.create_value(i32, None);
    let block = context.create_block(vec![argument.clone()]);
    region.add_block(block.id());
    block.append_op(ops::addi(&context, argument.id(), x, i32).build());

    let bindings = HashMap::from([(argument.id(), y), (x, z)]);
    let copy = tir::clone_region_with_mapping(&context, region.id(), &bindings);

    let body = context.get_block(context.get_region(copy).block_ids()[0]);
    let add = context.get_op(body.op_ids()[0]);
    assert_eq!(add.operands().as_slice(), vec![y, z]);
}
