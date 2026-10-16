//! PassManager and pass-driven analysis invalidation.

use std::collections::HashMap;

use tir::{
    builtin::{ops, AddIOp, IntegerType},
    func::FuncOp,
    AnalysisManager, Context, Operation, OperationRef, Pass, PassError, PassManager, PassTarget,
    Symbol,
};

use super::fixtures;

#[derive(Clone)]
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
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let add = op.as_op::<AddIOp>().expect("target guarantees AddIOp");
        let operands = add.operands();
        let result_ty = context.get_value(add.result()).ty();
        let new_op = ops::subi(context, operands[0], operands[1], result_ty).build();
        context.replace_op(op, &new_op)
    }
}

/// Erases the addi while the `return` still reads its result, leaving a
/// dangling operand.
#[derive(Clone)]
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
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        context.erase_op(op)
    }
}

/// Replaces the addi with a subi, then erases the subi. The root the
/// pipeline holds is replaced by an op that no longer exists.
#[derive(Clone)]
struct ReplaceThenErasePass;

impl Pass for ReplaceThenErasePass {
    fn name(&self) -> &'static str {
        "replace-then-erase"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<AddIOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let add = op.as_op::<AddIOp>().expect("target guarantees AddIOp");
        let operands = add.operands();
        let result_ty = context.get_value(add.result()).ty();
        let new_op = ops::subi(context, operands[0], operands[1], result_ty).build();
        context.replace_op(op, &new_op)?;
        context.erase_op(&OperationRef::new(context.get_op(new_op.id())))
    }
}

/// Reads the IR and leaves it exactly as it found it.
#[derive(Clone)]
struct ReadOnlyPass;

impl Pass for ReadOnlyPass {
    fn name(&self) -> &'static str {
        "read-only"
    }

    fn run(
        &mut self,
        _op: &OperationRef,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}

/// Mutates on every run, without changing what the IR means.
#[derive(Clone)]
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
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
        func.body()
            .set_attr("touched", tir::attributes::AttributeValue::Bool(true));
        Ok(())
    }
}

/// A function whose body block takes one argument, adds it to itself and
/// returns the sum.
const ADD_ITS_ARGUMENT: &str = r#"func.func @demo(%0: !i32) -> !i32 {
  %1 = addi %0, %0 : !i32
  func.return %1
}"#;

fn parse_func(source: &str) -> (Context, FuncOp) {
    let context = Context::with_default_dialects();
    let func = tir::parse::ir::parse_ir::<FuncOp>(&context, source).expect("parse");
    (context, func)
}

fn run_on_broken_candidate(pass: Box<dyn Pass>) -> Result<(), PassError> {
    let (context, func) = parse_func(ADD_ITS_ARGUMENT);
    let mut pm = PassManager::new();
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
fn splitting_a_block_moves_its_tail_into_a_new_block() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let value = context.create_value(i32, None);
    let block = context.create_block(vec![]);
    let head = block.append_op(ops::addi(&context, value.id(), value.id(), i32).build());
    let tail = block.append_op(ops::subi(&context, value.id(), value.id(), i32).build());

    let split = context.split_block(block.id(), 1);

    assert_eq!(context.get_block(block.id()).op_ids(), vec![head.id()]);
    assert_eq!(split.op_ids(), vec![tail.id()]);
    assert_eq!(context.parent_block(tail.id()), Some(split.id()));
}

#[test]
fn splicing_a_region_moves_its_blocks() {
    let context = Context::with_default_dialects();
    let source = context.create_region();
    let destination = context.create_region();
    let moved = context.create_block(vec![]);
    source.add_block(moved.id());

    context.splice_region(source.id(), destination.id());

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
    use super::analysis::Simple;

    let (context, func) = parse_func(ADD_ITS_ARGUMENT);
    let analyses = AnalysisManager::new();
    let before = analyses.get::<Simple>(&context, func.id());

    let mut pm = PassManager::new();
    pm.add_pass(ReadOnlyPass);
    let root = OperationRef::new(context.get_op(func.id()));
    pm.run_on_op_ref(&context, root, &analyses)
        .expect("the pass changes nothing");

    assert!(std::rc::Rc::ptr_eq(
        &before,
        &analyses.get::<Simple>(&context, func.id())
    ));
}

#[test]
fn repeated_pass_runs_do_not_grow_the_analysis_cache() {
    use super::analysis::Simple;

    let (context, func) = parse_func(ADD_ITS_ARGUMENT);
    let analyses = AnalysisManager::new();
    let mut pm = PassManager::new();
    pm.add_pass(TouchPass);

    let mut counts = Vec::new();
    for _ in 0..8 {
        let root = OperationRef::new(context.get_op(func.id()));
        pm.run_on_op_ref(&context, root, &analyses)
            .expect("touching a block attribute keeps the IR valid");
        analyses.get::<Simple>(&context, func.id());
        counts.push(analyses.cached_count());
    }

    assert!(
        counts.iter().all(|count| *count == counts[0]),
        "each rebuild must replace the stale result, got {counts:?}"
    );
}

#[test]
fn an_analysis_is_rebuilt_after_a_pass_mutates() {
    use super::analysis::Simple;

    let (context, func) = parse_func(ADD_ITS_ARGUMENT);
    let analyses = AnalysisManager::new();
    let before = analyses.get::<Simple>(&context, func.id());

    let mut pm = PassManager::new();
    pm.add_pass(AddToSubPass);
    let root = OperationRef::new(context.get_op(func.id()));
    pm.run_on_op_ref(&context, root, &analyses)
        .expect("rewriting addi to subi keeps the IR valid");

    assert!(!std::rc::Rc::ptr_eq(
        &before,
        &analyses.get::<Simple>(&context, func.id())
    ));
}

#[test]
fn a_root_whose_replacement_was_erased_ends_the_pipeline_without_a_root() {
    let (context, func) = parse_func(
        r#"func.func @demo(%0: !i32) -> !i32 {
  %1 = addi %0, %0 : !i32
  func.return %0
}"#,
    );
    let add_id = func.body().op_ids()[0];
    let mut pm = PassManager::new();
    pm.add_pass(ReplaceThenErasePass);
    pm.run(&context, context.get_op(add_id))
        .expect("erasing the replaced root is a valid rewrite");
    assert_eq!(func.body().op_ids().len(), 1);
}

#[test]
fn nested_pass_manager_rewrites_ops() {
    let (context, module, func, _) = fixtures::parse_function(
        r#"module {
func.func @demo(%0: !i32, %1: !i32) -> !i32 {
  %2 = addi %0, %1 : !i32
  func.return %2
}
module_end
}"#,
    );
    let func_body = func.body();
    let add_id = func_body.op_ids()[0];

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
    assert_eq!(context.users_of(func_body.arguments()[0].id()), [subi_id]);

    // The replaced-out addi is gone from the arena, not just the block.
    assert!(
        !context.has_operation(add_id),
        "replaced op should leave the arena"
    );
}

#[test]
fn erasing_an_op_drops_its_operand_uses() {
    // The subi is the argument's only reader, and nothing reads the subi.
    let (context, func) = parse_func(
        r#"func.func @demo(%0: !i32) -> !i32 {
  %1 = subi %0, %0 : !i32
  %2 = constant {value = 0} : !i32
  func.return %2
}"#,
    );
    let body = func.body();

    let neg_id = body.op_ids()[0];
    let neg_ref = OperationRef::new(context.get_op(neg_id));
    let argument = body.arguments()[0].id();
    assert!(context.is_used(argument));

    context.erase_op(&neg_ref).expect("erase should succeed");

    assert!(
        !context.is_used(argument),
        "erasing the only consumer must leave the value unused"
    );
    // The erased op is gone from the arena, not just the block.
    assert!(
        !context.has_operation(neg_id),
        "erased op should leave the arena"
    );
}

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
        for nested in context.get_region(region).op_ids() {
            total += count_ops(context, nested);
        }
    }
    total
}

fn restructure_source(source: &str) -> (Context, FuncOp, usize) {
    let (context, func) = parse_func(source);
    let before = count_ops(&context, func.id());
    let mut manager = PassManager::new();
    manager.add_pass(tir::passes::RestructureNodesPass::new());
    manager
        .run(&context, context.get_op(func.id()))
        .expect("restructure");
    (context, func, before)
}

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

/// Edits the function on its first `edits` runs, then leaves it alone. Counts
/// every run so a fixpoint's iteration count is observable.
#[derive(Clone)]
struct CountingPass {
    runs: std::sync::Arc<std::sync::atomic::AtomicU32>,
    edits: u32,
}

impl Pass for CountingPass {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let run = self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if run <= self.edits {
            let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
            func.body()
                .set_attr("round", tir::attributes::AttributeValue::Int(run as i64));
        }
        Ok(())
    }
}

fn count_fixpoint_runs(cap: u8, edits: u32) -> u32 {
    let (context, func) = parse_func(ADD_ITS_ARGUMENT);
    let runs = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut pm = PassManager::new();
    pm.fixpoint(cap).add_pass(CountingPass {
        runs: runs.clone(),
        edits,
    });
    pm.run(&context, context.get_op(func.id()))
        .expect("the counting pass keeps the IR valid");
    runs.load(std::sync::atomic::Ordering::Relaxed)
}

#[test]
fn a_fixpoint_stops_when_the_version_stops_moving() {
    assert_eq!(count_fixpoint_runs(10, 3), 4);
}

#[test]
fn a_fixpoint_stops_at_its_cap() {
    assert_eq!(count_fixpoint_runs(2, 3), 2);
}

#[test]
fn a_fixpoint_runs_a_pass_that_changes_nothing_once() {
    assert_eq!(count_fixpoint_runs(10, 0), 1);
}

/// Records how many ops its sibling functions hold, then grows its own body:
/// what a sibling sees depends on which epoch it reads.
#[derive(Clone)]
struct PeerCountPass;

impl Pass for PeerCountPass {
    fn name(&self) -> &'static str {
        "peer-count"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let id = op.op().id;
        let module = context.parent_op(id).expect("a function sits in a module");
        let peers: i64 = context
            .get_region(context.get_op(module).regions()[0])
            .op_ids()
            .into_iter()
            .filter(|peer| *peer != id && context.get_op(*peer).is::<FuncOp>())
            .map(|peer| {
                context
                    .get_region(context.get_op(peer).regions()[0])
                    .op_ids()
                    .len() as i64
            })
            .sum();
        let mut attributes = op.op().attributes().to_vec();
        attributes
            .push(context.named_attribute("peer_ops", tir::attributes::AttributeValue::Int(peers)));
        context.set_op_attributes(id, attributes);
        let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
        let i32_ty = IntegerType::new(context, 32);
        func.body()
            .insert(0, ops::constant(context, 7, i32_ty).build().id());
        Ok(())
    }
}

const TWO_FUNCTIONS: &str = r#"module {
func.func @a() -> !i32 {
  %0 = constant {value = 1} : !i32
  func.return %0
}
func.func @b() -> !i32 {
  %0 = constant {value = 2} : !i32
  %1 = addi %0, %0 : !i32
  func.return %1
}
module_end
}"#;

fn module_text(context: &Context, module: tir::OpId) -> String {
    let module = context
        .get_op(module)
        .as_op::<tir::builtin::ModuleOp>()
        .unwrap();
    let mut out = String::new();
    let mut fmt = tir::IRFormatter::new(&mut out);
    tir::print_ir(&module, context, &mut fmt).unwrap();
    out
}

fn run_peer_count(workers: usize) -> (String, Vec<i64>) {
    let (context, module) = fixtures::parse(TWO_FUNCTIONS);
    let mut pm = PassManager::new();
    pm.set_workers(workers);
    pm.nest::<FuncOp>().add_pass(PeerCountPass);
    pm.run(&context, context.get_op(module.id()))
        .expect("the pass keeps the IR valid");
    let counts = fixtures::module_ops(&context, module.id())
        .into_iter()
        .filter_map(|op| match context.get_op(op).attr("peer_ops") {
            Some(tir::attributes::AttributeValue::Int(count)) => Some(count),
            _ => None,
        })
        .collect();
    (module_text(&context, module.id()), counts)
}

#[test]
fn function_tasks_read_one_epoch_at_every_worker_count() {
    let (sequential, counts) = run_peer_count(1);
    assert_eq!(counts, [3, 2]);
    assert_eq!(run_peer_count(2), (sequential.clone(), counts.clone()));
    assert_eq!(run_peer_count(8), (sequential, counts));
}

/// Sleeps longer for earlier functions, so with several workers the later
/// functions finish first; then edits its own body like [`PeerCountPass`].
#[derive(Clone)]
struct SlowFirstPass;

impl Pass for SlowFirstPass {
    fn name(&self) -> &'static str {
        "slow-first"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
        let delay = if func.symbol_name() == "a" { 30 } else { 0 };
        std::thread::sleep(std::time::Duration::from_millis(delay));
        let i32_ty = IntegerType::new(context, 32);
        func.body()
            .insert(0, ops::constant(context, 7, i32_ty).build().id());
        Ok(())
    }
}

#[test]
fn tasks_commit_in_callable_order_whatever_order_they_finish_in() {
    let run = |workers: usize| {
        let (context, module) = fixtures::parse(TWO_FUNCTIONS);
        let mut pm = PassManager::new();
        pm.set_workers(workers);
        pm.nest::<FuncOp>().add_pass(SlowFirstPass);
        pm.run(&context, context.get_op(module.id()))
            .expect("the pass keeps the IR valid");
        module_text(&context, module.id())
    };
    let sequential = run(1);
    assert!(
        sequential.contains("%2 = constant {value = 7}"),
        "{sequential}"
    );
    assert_eq!(run(2), sequential);
}

/// Edits the function after its own: what a task may not do.
#[derive(Clone)]
struct TouchSiblingPass;

impl Pass for TouchSiblingPass {
    fn name(&self) -> &'static str {
        "touch-sibling"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let id = op.op().id;
        let module = context.parent_op(id).expect("a function sits in a module");
        let functions: Vec<_> = context
            .get_region(context.get_op(module).regions()[0])
            .op_ids()
            .into_iter()
            .filter(|peer| context.get_op(*peer).is::<FuncOp>())
            .collect();
        for target in [id, functions[0]] {
            context
                .get_op(target)
                .as_op::<FuncOp>()
                .expect("a function")
                .body()
                .set_attr("touched", tir::attributes::AttributeValue::Bool(true));
        }
        Ok(())
    }
}

#[test]
fn overlapping_write_sets_are_refused_before_anything_commits() {
    let (context, module) = fixtures::parse(TWO_FUNCTIONS);
    let before = module_text(&context, module.id());
    let mut pm = PassManager::new();
    pm.nest::<FuncOp>().add_pass(TouchSiblingPass);
    let error = pm
        .run(&context, context.get_op(module.id()))
        .expect_err("two tasks edited one block");
    assert!(matches!(error, PassError::OverlappingEdits(_)), "{error}");
    assert_eq!(module_text(&context, module.id()), before);
}

#[test]
fn a_function_task_rebuilds_an_analysis_its_earlier_pass_invalidated() {
    let (context, module) = fixtures::parse(TWO_FUNCTIONS);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut pm = PassManager::new();
    pm.set_workers(2);
    let functions = pm.nest::<FuncOp>();
    functions.add_pass(RecordOpCountPass { seen: seen.clone() });
    functions.add_pass(SlowFirstPass);
    functions.add_pass(RecordOpCountPass { seen: seen.clone() });
    pm.run(&context, context.get_op(module.id()))
        .expect("the passes keep the IR valid");
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_unstable();
    assert_eq!(seen, [("a", 2), ("a", 3), ("b", 3), ("b", 4)]);
}

/// Records `(function, op count)` as the [`OpCount`] analysis answers it.
#[derive(Clone)]
struct RecordOpCountPass {
    seen: std::sync::Arc<std::sync::Mutex<Vec<(&'static str, usize)>>>,
}

struct OpCount(usize);

impl tir::Analysis for OpCount {
    fn build(_analyses: &AnalysisManager, context: &Context, op: tir::OpId) -> Self {
        OpCount(
            context
                .get_region(context.get_op(op).regions()[0])
                .op_ids()
                .len(),
        )
    }
}

impl Pass for RecordOpCountPass {
    fn name(&self) -> &'static str {
        "record-op-count"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
        let name: &'static str = if func.symbol_name() == "a" { "a" } else { "b" };
        let count = analyses.get::<OpCount>(context, op.op().id).0;
        self.seen.lock().unwrap().push((name, count));
        Ok(())
    }
}

/// Advances the body's `round` attribute up to three, then changes nothing;
/// counts every run over every function.
#[derive(Clone)]
struct ThreeRoundsPass {
    runs: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl Pass for ThreeRoundsPass {
    fn name(&self) -> &'static str {
        "three-rounds"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let body = op
            .as_op::<FuncOp>()
            .expect("target guarantees FuncOp")
            .body();
        let round = match body.attr("round") {
            Some(tir::attributes::AttributeValue::Int(round)) => round,
            _ => 0,
        };
        if round < 3 {
            body.set_attr("round", tir::attributes::AttributeValue::Int(round + 1));
        }
        Ok(())
    }
}

#[test]
fn a_fixpoint_inside_a_function_task_runs_until_the_version_settles() {
    let (context, module) = fixtures::parse(TWO_FUNCTIONS);
    let runs = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut pm = PassManager::new();
    pm.set_workers(2);
    pm.nest::<FuncOp>()
        .fixpoint(10)
        .add_pass(ThreeRoundsPass { runs: runs.clone() });
    pm.run(&context, context.get_op(module.id()))
        .expect("the pass keeps the IR valid");
    assert_eq!(runs.load(std::sync::atomic::Ordering::Relaxed), 8);
    let text = module_text(&context, module.id());
    assert_eq!(text.matches("round = 3").count(), 2, "{text}");
}

#[test]
fn sibling_roots_replaced_in_one_block_commit_without_conflict() {
    let (context, func) = parse_func(
        r#"func.func @demo(%0: !i32, %1: !i32) -> !i32 {
  %2 = addi %0, %1 : !i32
  %3 = addi %2, %1 : !i32
  func.return %3
}"#,
    );
    let mut pm = PassManager::new();
    pm.set_workers(2);
    pm.nest::<AddIOp>().add_pass(AddToSubPass);
    pm.run(&context, context.get_op(func.id()))
        .expect("each task replaces only its own root");
    let names: Vec<_> = func
        .body()
        .op_ids()
        .into_iter()
        .map(|op| context.get_op(op).name().as_str())
        .collect();
    assert_eq!(names, ["subi", "subi", "return"]);
}
