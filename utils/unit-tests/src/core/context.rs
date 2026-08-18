//! Context tests: staged regions, port growth, interning and spine bumps.

use tir::{
    builtin, func, scf, BlockHandle, BlockId, Commutative, Context, IRFormatter, OpId, Operand,
    Operation, RegionId, StagedRegion, ValueId,
};

/// `module { func demo(%cond) { %c = 1; scf.if %cond { %old = 7; scf.yield }; return } }`
/// — a region owned by an op nested inside a function, so a commit to it has a
/// spine to bump and live values around it to reference.
struct Fixture {
    module: OpId,
    func: OpId,
    if_op: OpId,
    then_region: RegionId,
    then_block: BlockId,
    /// `%old`, defined inside the region a commit replaces.
    old: ValueId,
    /// `%c`, defined outside it and still live after a commit.
    constant: ValueId,
    module_body: BlockHandle,
}

fn fixture(context: &Context) -> Fixture {
    let i1 = builtin::IntegerType::new(context, 1);
    let i32_ty = builtin::IntegerType::new(context, 32);
    let unit = builtin::UnitType::new(context);

    let then_region = context.create_region();
    let then_block = context.create_block(vec![]);
    then_region.add_block(then_block.id());
    let old = builtin::ops::constant(context, 7, i32_ty).build();
    then_block.append(old.id());
    then_block.append(scf::ops::r#yield(context, vec![]).build().id());

    let else_region = context.create_region();
    let else_block = context.create_block(vec![]);
    else_region.add_block(else_block.id());
    else_block.append(scf::ops::r#yield(context, vec![]).build().id());

    let body = context.create_region();
    let cond = context.create_value(i1, None);
    let entry = context.create_block(vec![cond.clone()]);
    body.add_block(entry.id());
    let constant = builtin::ops::constant(context, 1, i32_ty).build();
    entry.append(constant.id());
    let if_op = scf::ops::r#if(
        context,
        cond.id(),
        vec![],
        vec![],
        Some(then_region.id()),
        Some(else_region.id()),
    )
    .build();
    entry.append(if_op.id());
    entry.append(func::ops::r#return(context, Operand::none()).build().id());

    let func = func::ops::func(context, "demo", unit, Some(body.id())).build();
    let module = builtin::ops::module(context, None).build();
    module.body().append(func.id());

    Fixture {
        module: module.id(),
        func: func.id(),
        if_op: if_op.id(),
        then_region: then_region.id(),
        then_block: then_block.id(),
        old: old.result(),
        constant: constant.result(),
        module_body: context.get_block(module.body().id()),
    }
}

fn printed(context: &Context, module: OpId) -> String {
    let mut out = String::new();
    let mut fmt = IRFormatter::new(&mut out);
    let op = context.get_op(module).as_dyn_op();
    tir::print_ir(op.as_ref(), context, &mut fmt).expect("print must succeed");
    out
}

/// A staged `^block: scf.yield` body, ready to swap into an `scf.if` region.
fn staged_yield(context: &Context) -> StagedRegion {
    let mut staged = context.stage_region();
    let block = staged.append_block(&[]);
    staged.append_op(block, scf::ops::r#yield(context, vec![]).build().id());
    staged
}

#[test]
fn a_discarded_staging_leaves_the_tree_untouched() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let before = printed(&context, f.module);
    let module_version = context.op_version(f.module);
    let if_version = context.op_version(f.if_op);

    let staged_op = {
        let mut staged = context.stage_region();
        let block = staged.append_block(&[]);
        let add = builtin::ops::addi(&context, f.constant, f.constant, i32_ty).build();
        staged.append_op(block, add.id());
        add.id()
    };

    assert_eq!(printed(&context, f.module), before, "the IR is unchanged");
    assert_eq!(context.op_version(f.module), module_version);
    assert_eq!(context.op_version(f.if_op), if_version);
    assert!(!context.has_operation(staged_op), "staged ops are dropped");
    assert!(
        !tir::analysis::DefUse::new(&context, f.func).is_used(f.constant.number()),
        "a discarded staging leaves no uses of live values behind"
    );
}

#[test]
fn a_commit_bumps_the_spine_once() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let module_version = context.op_version(f.module);
    let func_version = context.op_version(f.func);
    let if_version = context.op_version(f.if_op);

    context.replace_region_contents(f.then_region, staged_yield(&context));

    assert_eq!(context.op_version(f.if_op), if_version + 1);
    assert_eq!(context.op_version(f.func), func_version + 1);
    assert_eq!(context.op_version(f.module), module_version + 1);
}

#[test]
fn a_commit_detaches_the_old_subtree() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let old_op = context.get_value(f.old).defining_op().unwrap();

    context.replace_region_contents(f.then_region, staged_yield(&context));

    assert!(!context.has_operation(old_op));
    assert_eq!(context.parent_block(old_op), None);
    assert_eq!(context.parent_op(old_op), None);
    assert_eq!(context.parent_region(f.then_block), None);
    assert!(!printed(&context, f.module).contains("7"));
    // Nothing dirtied walks into the detached subtree.
    tir::verify_op_tree(&context, f.if_op).expect("the committed tree verifies");
}

#[test]
fn staged_ops_keep_their_live_operands() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let mut staged = context.stage_region();
    let block = staged.append_block(&[]);
    let add = builtin::ops::addi(&context, f.constant, f.constant, i32_ty).build();
    staged.append_op(block, add.id());
    staged.append_op(block, scf::ops::r#yield(&context, vec![]).build().id());
    context.replace_region_contents(f.then_region, staged);

    assert_eq!(
        context.get_op(add.id()).operands().as_slice(),
        vec![f.constant; 2]
    );
    assert_eq!(context.parent_block(add.id()), Some(block));
    assert_eq!(context.parent_region(block), Some(f.then_region));
    assert_eq!(
        tir::analysis::DefUse::new(&context, f.func).users_of(f.constant.number()),
        [add.id(); 2]
    );
    tir::verify_op_tree(&context, f.func).expect("the committed tree verifies");
}

#[test]
fn staged_blocks_carry_their_arguments() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let mut staged = context.stage_region();
    let block = staged.append_block(&[i32_ty]);
    let argument = staged.block_argument(block, 0).id();
    let add = builtin::ops::addi(&context, argument, f.constant, i32_ty).build();
    staged.append_op(block, add.id());
    staged.append_op(block, scf::ops::r#yield(&context, vec![]).build().id());
    context.replace_region_contents(f.then_region, staged);

    let committed = context.get_block(block);
    assert_eq!(committed.arguments().len(), 1);
    assert_eq!(committed.arguments()[0].id(), argument);
    assert_eq!(context.get_op(add.id()).operands()[0], argument);
}

#[test]
fn a_commit_remaps_uses_of_replaced_values() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);
    // A use of the old region's value that outlives the swap.
    let user = builtin::ops::addi(&context, f.old, f.old, i32_ty).build();
    f.module_body.append(user.id());

    let mut staged = context.stage_region();
    let block = staged.append_block(&[]);
    let fresh = builtin::ops::constant(&context, 9, i32_ty).build();
    staged.append_op(block, fresh.id());
    staged.append_op(block, scf::ops::r#yield(&context, vec![]).build().id());
    staged.replace_value(f.old, fresh.result());
    context.replace_region_contents(f.then_region, staged);

    assert_eq!(
        context.get_op(user.id()).operands().as_slice(),
        vec![fresh.result(); 2],
        "surviving uses read the staged replacement"
    );
    assert!(!tir::analysis::DefUse::new(&context, f.module).is_used(f.old.number()));
}

#[test]
fn a_commit_keeps_analyses_of_untouched_functions() {
    use tir::{Analysis, AnalysisManager};
    struct Probe;
    impl Analysis for Probe {
        fn build(_: &AnalysisManager, _: &Context, _: OpId) -> Self {
            Probe
        }
    }

    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let unit = builtin::UnitType::new(&context);
    let sibling_body = context.create_region();
    let sibling_entry = context.create_block(vec![]);
    sibling_body.add_block(sibling_entry.id());
    sibling_entry.append(func::ops::r#return(&context, Operand::none()).build().id());
    let sibling = func::ops::func(&context, "sib", unit, Some(sibling_body.id())).build();
    f.module_body.append(sibling.id());

    let analyses = AnalysisManager::new();
    analyses.get::<Probe>(&context, sibling.id());
    analyses.get::<Probe>(&context, f.func);

    context.replace_region_contents(f.then_region, staged_yield(&context));

    assert!(
        analyses
            .get_cached::<Probe>(&context, sibling.id())
            .is_some(),
        "a sibling function's analyses survive a commit elsewhere"
    );
    assert!(analyses.get_cached::<Probe>(&context, f.func).is_none());
}

/// A loop with no carried port yet, and a constant outside it to carry in.
const LOOP: &str = r#"module {
  func.func @f(%0: !index, %1: !index, %2: !index) -> !i32 {
    %3 = constant {value = 7} : !i32
    scf.for %0, %1, %2 {
      scf.yield
    }
    func.return %3
  }
  module_end
}"#;

fn loop_fixture(context: &Context) -> (OpId, OpId, ValueId) {
    let module: builtin::ModuleOp =
        tir::parse::ir::parse_ir(context, LOOP).expect("the fixture parses");
    let func = context
        .get_region(context.get_op(module.id()).regions()[0])
        .iter(context.clone())
        .next()
        .expect("module body")
        .op_ids()[0];
    let body = context
        .get_region(context.get_op(func).regions()[0])
        .iter(context.clone())
        .next()
        .expect("function body");
    let constant = context.get_op(body.op_ids()[0]).results()[0];
    (module.id(), body.op_ids()[1], constant)
}

fn loop_owner(context: &Context, module: OpId) -> OpId {
    context
        .get_region(context.get_op(module).regions()[0])
        .iter(context.clone())
        .next()
        .expect("module body")
        .op_ids()[0]
}

fn single_block(context: &Context, region: RegionId) -> BlockHandle {
    context.get_block(context.get_region(region).block_ids()[0])
}

#[test]
fn growing_a_loop_port_carries_one_more_value() {
    let context = Context::with_default_dialects();
    let (module, loop_op, constant) = loop_fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let result = context.grow_port(loop_op, i32_ty, Some(constant), |_, carried| carried);

    let grown = context.get_op(loop_op);
    assert_eq!(
        grown.results().as_slice(),
        vec![result],
        "the port's value leaves the op"
    );
    assert_eq!(
        grown.operands().last(),
        Some(&constant),
        "the port's initial value enters as one more operand"
    );
    let body = single_block(&context, grown.regions()[0]);
    let carried = body.arguments()[0].id();
    assert_eq!(body.arguments().len(), 1);
    assert_eq!(
        context
            .get_op(*body.op_ids().last().unwrap())
            .operands()
            .as_slice(),
        vec![carried],
        "the region yields what the port carries"
    );
    tir::verify_op_tree(&context, module).expect("the grown loop verifies");
}

#[test]
fn growing_a_conditional_port_yields_from_every_arm() {
    let context = Context::with_default_dialects();
    let (module, _, constant) = loop_fixture(&context);
    let i1 = builtin::IntegerType::new(&context, 1);
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let condition = builtin::ops::constant(&context, 1, i1).build();
    let arms: Vec<RegionId> = (0..2)
        .map(|_| {
            let region = context.create_region();
            let block = context.create_block(vec![]);
            region.add_block(block.id());
            block.append(scf::ops::r#yield(&context, vec![]).build().id());
            region.id()
        })
        .collect();
    let conditional = scf::ops::r#if(
        &context,
        condition.result(),
        vec![],
        vec![],
        Some(arms[0]),
        Some(arms[1]),
    )
    .build();
    let function_body = context.get_block(
        context
            .get_region(context.get_op(loop_owner(&context, module)).regions()[0])
            .block_ids()[0],
    );
    function_body.insert(0, condition.id());
    function_body.insert(1, conditional.id());

    let result = context.grow_port(conditional.id(), i32_ty, None, |_, carried| {
        assert!(carried.is_none(), "a conditional carries nothing in");
        Some(constant)
    });

    let grown = context.get_op(conditional.id());
    assert_eq!(grown.results().as_slice(), vec![result]);
    assert_eq!(
        grown.operands().as_slice(),
        vec![condition.result()],
        "a conditional carries nothing in"
    );
    for arm in grown.regions() {
        let block = single_block(&context, arm);
        assert!(block.arguments().is_empty(), "an arm takes no argument");
        assert_eq!(
            context
                .get_op(*block.op_ids().last().unwrap())
                .operands()
                .as_slice(),
            vec![constant],
            "every arm yields the port's value"
        );
    }
    tir::verify_op_tree(&context, module).expect("the grown conditional verifies");
}

#[test]
fn an_attribute_name_resolves_back_to_its_spelling() {
    let context = Context::with_default_dialects();

    let attribute = context.named_attribute("size", tir::attributes::AttributeValue::UInt(4));

    assert_eq!(context.resolve(attribute.name), "size");
    assert_eq!(context.sym("size"), Some(attribute.name));
}

/// A name no one has used is not an id, so a lookup answers "absent" instead
/// of minting one.
#[test]
fn an_unused_name_has_no_id() {
    let context = Context::with_default_dialects();

    assert_eq!(context.sym("no_op_declares_this"), None);
}

/// Registered ops' attribute names are interned before any IR exists, so they
/// hold the low ids and a lookup never has to intern on a read path.
#[test]
fn schema_attribute_names_are_interned_up_front() {
    let context = Context::with_default_dialects();

    let value = context
        .sym("value")
        .expect("builtin.constant declares 'value'");

    assert!(context.sym("sym_name").is_some());
    assert!(value.index() < tir::schema::OP_SCHEMAS.len());
}

/// Ids are per-context: two contexts assign them independently, and the same
/// spelling reaches the same attribute in each.
#[test]
fn ids_are_local_to_one_context() {
    let first = Context::with_default_dialects();
    let second = Context::with_default_dialects();

    let only_in_first = first.intern("a_name_only_the_first_context_sees");

    assert_eq!(
        first.resolve(only_in_first),
        "a_name_only_the_first_context_sees"
    );
    assert_eq!(second.sym("a_name_only_the_first_context_sees"), None);
    assert_eq!(first.sym("value"), second.sym("value"));
}

/// `module { func demo { ^entry: } }` — the func body sits two regions deep,
/// so an edit there must reach the module to prove root-ward propagation.
fn module_with_function(context: &Context) -> (OpId, OpId, BlockHandle) {
    let i32 = builtin::IntegerType::new(context, 32);
    let region = context.create_region();
    let block = context.create_block(vec![]);
    region.add_block(block.id());
    let func = func::ops::func(context, "demo", i32, Some(region.id())).build();
    let module = builtin::ops::module(context, None).build();
    module.body().append(func.id());
    (module.id(), func.id(), context.get_block(block.id()))
}

/// Every kind of IR edit dirties the edited op's owner and propagates the
/// version bump root-ward to the module.
#[test]
fn every_edit_bumps_the_spine() {
    type Edit = fn(&Context, &BlockHandle);

    let cases: &[(&str, Edit)] = &[
        ("append op", |context, body| {
            body.append(func::ops::r#return(context, Operand::none()).build().id());
        }),
        ("insert op", |context, body| {
            body.insert(
                0,
                func::ops::r#return(context, Operand::none()).build().id(),
            );
        }),
        ("remove op", |context, body| {
            let ret = func::ops::r#return(context, Operand::none()).build();
            body.append(ret.id());
            assert!(body.remove_op(ret.id()));
        }),
        ("replace op", |context, body| {
            let old = func::ops::r#return(context, Operand::none()).build();
            body.append(old.id());
            let new = func::ops::r#return(context, Operand::none()).build();
            assert!(body.replace_op(old.id(), new.id()));
        }),
        ("append block argument", |context, body| {
            let i32 = builtin::IntegerType::new(context, 32);
            context.append_block_argument(body.id(), i32);
        }),
        ("set block attribute", |_context, body| {
            body.set_attr("fpmath", tir::attributes::AttributeValue::Bool(true));
        }),
        ("add block to region", |context, body| {
            let region = context.parent_region(body.id()).unwrap();
            let extra = context.create_block(vec![]);
            context.get_region(region).add_block(extra.id());
        }),
        ("remove block from region", |context, body| {
            let region = context.get_region(context.parent_region(body.id()).unwrap());
            let extra = context.create_block(vec![]);
            region.add_block(extra.id());
            assert!(region.remove_block(extra.id()));
        }),
        ("set op attributes", |context, body| {
            let ret = func::ops::r#return(context, Operand::none()).build();
            body.append(ret.id());
            context.set_op_attributes(ret.id(), vec![]);
        }),
        ("set op operand", |context, body| {
            let i32 = builtin::IntegerType::new(context, 32);
            let a = context.create_value(i32, None);
            let b = context.create_value(i32, None);
            let add = builtin::ops::addi(context, a.id(), a.id(), i32).build();
            body.append(add.id());
            context.set_op_operand(add.id(), 1, b.id());
        }),
        ("set op operands", |context, body| {
            let i32 = builtin::IntegerType::new(context, 32);
            let a = context.create_value(i32, None);
            let b = context.create_value(i32, None);
            let add = builtin::ops::addi(context, a.id(), a.id(), i32).build();
            body.append(add.id());
            context.set_op_operands(add.id(), vec![b.id(), b.id()]);
        }),
        ("replace value uses", |context, body| {
            let i32 = builtin::IntegerType::new(context, 32);
            let a = context.create_value(i32, None);
            let b = context.create_value(i32, None);
            let add = builtin::ops::addi(context, a.id(), a.id(), i32).build();
            body.append(add.id());
            context.replace_value_uses(a.id(), b.id());
        }),
    ];

    for (name, edit) in cases {
        let context = Context::with_default_dialects();
        let (module, func, body) = module_with_function(&context);
        let module_before = context.op_version(module);
        let func_before = context.op_version(func);
        edit(&context, &body);
        assert!(
            context.op_version(func) > func_before,
            "{name}: the edited op's owner must be dirtied"
        );
        assert!(
            context.op_version(module) > module_before,
            "{name}: the bump must propagate root-ward"
        );
    }
}

#[test]
fn removing_a_block_argument_drops_it() {
    let context = Context::with_default_dialects();
    let (_, _, body) = module_with_function(&context);
    let i32 = builtin::IntegerType::new(&context, 32);
    let first = context.append_block_argument(body.id(), i32);
    let second = context.append_block_argument(body.id(), i32);

    context.remove_block_argument(body.id(), 0);

    let block = context.get_block(body.id());
    assert_eq!(block.arguments().len(), 1);
    assert_eq!(block.arguments()[0].id(), second.id());
    assert!(!context.is_block_argument(first.id()));
}

#[test]
fn adopting_a_value_makes_the_block_define_it() {
    let context = Context::with_default_dialects();
    let (_, _, body) = module_with_function(&context);
    let i32 = builtin::IntegerType::new(&context, 32);
    let input = context.create_value(i32, None);
    let produced = builtin::ops::addi(&context, input.id(), input.id(), i32).build();
    let result = context.get_op(produced.id()).results()[0];
    let reader = builtin::ops::addi(&context, result, result, i32).build();
    body.append(reader.id());

    context.adopt_block_argument(body.id(), result);

    let block = context.get_block(body.id());
    assert_eq!(block.arguments().len(), 1);
    assert_eq!(block.arguments()[0].id(), result);
    assert!(context.is_block_argument(result));
    assert_eq!(context.get_value(result).defining_op(), None);
    assert_eq!(
        context.get_op(reader.id()).operands().as_slice(),
        vec![result; 2]
    );
}

#[test]
fn an_untouched_function_keeps_its_version() {
    let context = Context::with_default_dialects();
    let (_, edited, body) = module_with_function(&context);
    let (_, untouched, _) = module_with_function(&context);
    let before = context.op_version(untouched);

    body.append(func::ops::r#return(&context, Operand::none()).build().id());

    assert!(context.op_version(edited) > 0);
    assert_eq!(context.op_version(untouched), before);
}

#[test]
fn parent_block_tracks_membership() {
    let context = Context::with_default_dialects();
    let i32 = builtin::IntegerType::new(&context, 32);
    let a = context.create_value(i32, None);
    let b = context.create_value(i32, None);

    let block = context.create_block(vec![]);
    let add = block.append_op(builtin::ops::addi(&context, a.id(), b.id(), i32).build());

    // Inserting into a block records the parent, reachable from just the op.
    assert_eq!(context.parent_block(add.id()), Some(block.id()));
    assert_eq!(context.get_op(add.id()).parent_block(), Some(block.id()));

    // Replacing swaps the parent over to the new op; the old op is detached.
    let sub = builtin::ops::subi(&context, a.id(), b.id(), i32).build();
    assert!(block.replace_op(add.id(), sub.id()));
    assert_eq!(context.parent_block(add.id()), None);
    assert_eq!(context.parent_block(sub.id()), Some(block.id()));

    // Removing clears it.
    assert!(block.remove_op(sub.id()));
    assert_eq!(context.parent_block(sub.id()), None);
}

#[test]
fn parent_region_tracks_membership() {
    let context = Context::with_default_dialects();
    let region = context.create_region();
    let block = context.create_block(vec![]);

    region.add_block(block.id());
    assert_eq!(context.parent_region(block.id()), Some(region.id()));

    assert!(region.remove_block(block.id()));
    assert_eq!(context.parent_region(block.id()), None);
}

#[test]
fn replacing_value_uses_reaches_a_nested_region() {
    let context = Context::with_default_dialects();
    let (_, _, body) = module_with_function(&context);
    let i32 = builtin::IntegerType::new(&context, 32);
    let i1 = builtin::IntegerType::new(&context, 1);
    let a = context.create_value(i32, None);
    let b = context.create_value(i32, None);
    let cond = context.create_value(i1, None);

    let then_region = context.create_region();
    let then_block = context.create_block(vec![]);
    then_region.add_block(then_block.id());
    let nested = builtin::ops::addi(&context, a.id(), a.id(), i32).build();
    then_block.append(nested.id());
    then_block.append(scf::ops::r#yield(&context, vec![]).build().id());
    let else_region = context.create_region();
    let else_block = context.create_block(vec![]);
    else_region.add_block(else_block.id());
    else_block.append(scf::ops::r#yield(&context, vec![]).build().id());
    body.append(
        scf::ops::r#if(
            &context,
            cond.id(),
            vec![],
            vec![],
            Some(then_region.id()),
            Some(else_region.id()),
        )
        .build()
        .id(),
    );

    context.replace_value_uses(a.id(), b.id());

    assert_eq!(
        context.get_op(nested.id()).operands().as_slice(),
        vec![b.id(); 2]
    );
}

#[test]
fn replacing_uses_of_a_block_argument_rewrites_its_readers() {
    let context = Context::with_default_dialects();
    let i32 = builtin::IntegerType::new(&context, 32);
    let region = context.create_region();
    let argument = context.create_value(i32, None);
    let block = context.create_block(vec![argument.clone()]);
    region.add_block(block.id());
    let func = func::ops::func(&context, "demo", i32, Some(region.id())).build();
    let module = builtin::ops::module(&context, None).build();
    module.body().append(func.id());
    let reader = builtin::ops::addi(&context, argument.id(), argument.id(), i32).build();
    block.append(reader.id());
    let replacement = context.create_value(i32, None);

    context.replace_value_uses(argument.id(), replacement.id());

    assert_eq!(
        context.get_op(reader.id()).operands().as_slice(),
        vec![replacement.id(); 2]
    );
}

#[test]
fn custom_interface_for_existing_op() {
    let context = Context::with_default_dialects();

    let lhs = context.create_value(builtin::IntegerType::new(&context, 32), None);
    let rhs = context.create_value(builtin::IntegerType::new(&context, 32), None);
    let add = builtin::ops::addi(
        &context,
        lhs.id(),
        rhs.id(),
        builtin::IntegerType::new(&context, 32),
    )
    .build();

    assert!(context.get_op(add.id()).has_interface::<dyn Commutative>());
    let iface = context
        .get_op(add.id())
        .as_interface::<dyn Commutative>()
        .expect("interface should be available");
    assert!(iface.is_commutative());
}

#[test]
fn identifies_operations_by_type() {
    use tir::builtin::{BuiltinDialect, ModuleOp, ModuleOpBuilder};
    use tir::{DialectName, OperationName};

    let context = Context::new();
    let module = ModuleOpBuilder::new(&context).build();
    let instance = context.get_op(module.id());

    assert!(instance.is::<ModuleOp>());
    assert!(!instance.is::<tir::func::FuncOp>());
    assert_eq!(instance.name(), OperationName::of::<ModuleOp>());
    assert_eq!(instance.dialect(), DialectName::of::<BuiltinDialect>());
}
