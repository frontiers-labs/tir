//! Context tests: staged regions, port growth, interning and spine bumps.

use tir::{
    builtin, func, func::FuncOp, scf, BlockHandle, BlockId, Commutative, Context, IRFormatter,
    OpId, Operand, Operation, RegionId, StagedRegion, Use, ValueId,
};

use super::fixtures;

// An operation holding one ordered region and nothing else, so a commit to it
// is a commit to a region an op owns without also being a port contract the
// staged body would have to satisfy.
tir::helpers::operation! {
    NestOp {
        name: "nest",
        dialect: "nest_test",
        regions: R {
            body: Region {
                kind: Blocks,
            }
        },
    }
}

tir::helpers::dialect! {
    NestDialect {
        name: "nest_test",
        operations: [NestOp],
    }
}

/// `module { func demo() { %c = 1; test.nest { %old = 7; scf.yield }; return } }`
/// — a region owned by an op nested inside a function, so a commit to it has a
/// spine to bump and live values around it to reference.
struct Fixture {
    module: OpId,
    func: OpId,
    nest: OpId,
    body_region: RegionId,
    body_block: BlockId,
    /// `%old`, defined inside the region a commit replaces.
    old: ValueId,
    /// `%c`, defined outside it and still live after a commit.
    constant: ValueId,
    module_body: BlockHandle,
}

fn fixture(context: &Context) -> Fixture {
    context.register_dialect::<NestDialect>();
    let module = fixtures::parse_in(
        context,
        r#"module {
%fn_demo = func.func @demo() {
  %c = constant {value = 1} : !i32
  nest_test.nest {
    %old = constant {value = 7} : !i32
    scf.yield
  }
  func.return
}
module_end
}"#,
    );
    let func = fixtures::module_ops(context, module.id())[0];
    let body = context
        .get_op(func)
        .as_op::<FuncOp>()
        .expect("a func")
        .body();
    let [constant, nest, ..] = body.op_ids()[..] else {
        panic!("the function body holds the constant and the nest");
    };
    let body_region = context.get_op(nest).regions()[0];
    let body_block = context.get_region(body_region).block_ids()[0];
    let old = context.get_block(body_block).op_ids()[0];

    Fixture {
        module: module.id(),
        func,
        nest,
        body_region,
        body_block,
        old: context.get_op(old).results()[0],
        constant: context.get_op(constant).results()[0],
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

/// A staged `^block: scf.yield` body, ready to swap into the nested region.
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
    let nest_version = context.op_version(f.nest);

    let staged_op = {
        let mut staged = context.stage_region();
        let block = staged.append_block(&[]);
        let add = builtin::ops::addi(&context, f.constant, f.constant, i32_ty).build();
        staged.append_op(block, add.id());
        add.id()
    };

    assert_eq!(printed(&context, f.module), before, "the IR is unchanged");
    assert_eq!(context.op_version(f.module), module_version);
    assert_eq!(context.op_version(f.nest), nest_version);
    assert!(!context.has_operation(staged_op), "staged ops are dropped");
    assert!(
        !context.is_used(f.constant),
        "a discarded staging leaves no uses of live values behind"
    );
}

#[test]
fn a_commit_bumps_the_spine_once() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let module_version = context.op_version(f.module);
    let func_version = context.op_version(f.func);
    let nest_version = context.op_version(f.nest);

    context.replace_region_contents(f.body_region, staged_yield(&context));

    assert_eq!(context.op_version(f.nest), nest_version + 1);
    assert_eq!(context.op_version(f.func), func_version + 1);
    assert_eq!(context.op_version(f.module), module_version + 1);
}

#[test]
fn a_commit_detaches_the_old_subtree() {
    let context = Context::with_default_dialects();
    let f = fixture(&context);
    let old_op = context.get_value(f.old).defining_op().unwrap();

    context.replace_region_contents(f.body_region, staged_yield(&context));

    assert!(!context.has_operation(old_op));
    assert_eq!(context.parent_block(old_op), None);
    assert_eq!(context.parent_op(old_op), None);
    assert_eq!(context.parent_region(f.body_block), None);
    assert!(!printed(&context, f.module).contains("7"));
    // Nothing dirtied walks into the detached subtree.
    tir::verify_op_tree(&context, f.nest).expect("the committed tree verifies");
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
    context.replace_region_contents(f.body_region, staged);

    assert_eq!(
        context.get_op(add.id()).operands().as_slice(),
        vec![f.constant; 2]
    );
    assert_eq!(context.parent_block(add.id()), Some(block));
    assert_eq!(context.parent_region(block), Some(f.body_region));
    assert_eq!(context.users_of(f.constant), [add.id(); 2]);
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
    context.replace_region_contents(f.body_region, staged);

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
    context.replace_region_contents(f.body_region, staged);

    assert_eq!(
        context.get_op(user.id()).operands().as_slice(),
        vec![fresh.result(); 2],
        "surviving uses read the staged replacement"
    );
    assert!(!context.is_used(f.old));
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
    let sibling = func::ops::lambda(&context, "sib", unit, &sibling_body).build();
    f.module_body.append(sibling.id());

    let analyses = AnalysisManager::new();
    analyses.get::<Probe>(&context, sibling.id());
    analyses.get::<Probe>(&context, f.func);

    context.replace_region_contents(f.body_region, staged_yield(&context));

    assert!(
        analyses
            .get_cached::<Probe>(&context, sibling.id())
            .is_some(),
        "a sibling function's analyses survive a commit elsewhere"
    );
    assert!(analyses.get_cached::<Probe>(&context, f.func).is_none());
}

/// What the interner answers about a name.
#[test]
fn a_name_is_an_id_once_something_uses_it() {
    let first = Context::with_default_dialects();

    let attribute = first.named_attribute("size", tir::attributes::AttributeValue::UInt(4));
    assert_eq!(first.resolve(attribute.name), "size");
    assert_eq!(first.sym("size"), Some(attribute.name));

    // A name no one has used is not an id, so a lookup answers "absent"
    // instead of minting one.
    assert_eq!(first.sym("no_op_declares_this"), None);

    // Registered ops' attribute names are interned before any IR exists, so
    // they hold the low ids and a lookup never has to intern on a read path.
    let value = first
        .sym("value")
        .expect("builtin.constant declares 'value'");
    assert!(first.sym("sym_name").is_some());
    assert!(value.index() < tir::schema::OP_SCHEMAS.len());

    // Ids are per-context: a second context assigns them independently, and
    // the same spelling reaches the same attribute in each.
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
    let module = fixtures::parse_in(
        context,
        "module {\n%fn_demo = func.func @demo() -> !i32 {\n}\nmodule_end\n}",
    );
    let func = fixtures::module_ops(context, module.id())[0];
    let body = context
        .get_op(func)
        .as_op::<FuncOp>()
        .expect("a func")
        .body();
    (module.id(), func, body)
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
    let a = context.create_value(i32, None);
    let b = context.create_value(i32, None);
    let bound = context.create_value(i32, None);

    let body_region = context.create_region();
    let counter = context.create_value(i32, None);
    let body_block = context.create_block(vec![counter]);
    body_region.add_block(body_block.id());
    let nested = builtin::ops::addi(&context, a.id(), a.id(), i32).build();
    body_block.append(nested.id());
    body_block.append(scf::ops::r#yield(&context, vec![]).build().id());
    body.append(
        scf::ForOpBuilder::new(&context)
            .lb(bound.id())
            .inits(vec![])
            .ub(bound.id())
            .step(bound.id())
            .body(body_region.id())
            .result_types(vec![i32])
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

#[test]
fn uses_of_lists_each_operand_slot() {
    let context = Context::with_default_dialects();
    let (_, _, body) = module_with_function(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let c = builtin::ops::constant(&context, 1, i32_ty).build();
    body.append(c.id());
    let add = builtin::ops::addi(&context, c.result(), c.result(), i32_ty).build();
    body.append(add.id());

    assert_eq!(
        context.uses_of(c.result()),
        [Use::new(add.id(), 0), Use::new(add.id(), 1)]
    );
    assert_eq!(context.users_of(c.result()), [add.id(); 2]);
    assert_eq!(context.use_count(c.result()), 2);
    assert!(context.is_used(c.result()));
    assert!(!context.is_used(add.result()));
}

/// A function body holding `%c = 1` and `%add = addi %c, %c`.
fn add_fixture(context: &Context) -> (ValueId, ValueId, OpId) {
    let (_, _, body) = module_with_function(context);
    let i32_ty = builtin::IntegerType::new(context, 32);
    let c = builtin::ops::constant(context, 1, i32_ty).build();
    body.append(c.id());
    let d = builtin::ops::constant(context, 2, i32_ty).build();
    body.append(d.id());
    let add = builtin::ops::addi(context, c.result(), c.result(), i32_ty).build();
    body.append(add.id());
    (c.result(), d.result(), add.id())
}

#[test]
fn setting_one_operand_moves_one_use() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);

    context.set_op_operand(add, 1, d);

    assert_eq!(context.uses_of(c), [Use::new(add, 0)]);
    assert_eq!(context.uses_of(d), [Use::new(add, 1)]);
}

#[test]
fn setting_every_operand_relinks_the_uses() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);

    context.set_op_operands(add, vec![d]);

    assert!(!context.is_used(c));
    assert_eq!(context.uses_of(d), [Use::new(add, 0)]);
}

#[test]
fn use_indices_follow_a_port_into_its_place() {
    let context = Context::with_default_dialects();
    let (_, d, add) = add_fixture(&context);
    let token = context.create_state();

    context.append_operand(add, token);
    context.append_operand(add, d);

    let operands = context.get_op(add).operands();
    for r#use in context.uses_of(d) {
        assert_eq!(operands[r#use.index], d);
    }
    assert_eq!(context.uses_of(token), [Use::new(add, 2)]);
}

#[test]
fn replacing_value_uses_reaches_a_detached_op() {
    let context = Context::with_default_dialects();
    let (c, d, _) = add_fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let detached = builtin::ops::addi(&context, c, c, i32_ty).build();

    context.replace_value_uses(c, d);

    assert_eq!(context.get_op(detached.id()).operands().as_slice(), [d; 2]);
    assert!(!context.is_used(c));
    assert_eq!(context.use_count(d), 4);
}

/// The whole point of the storage layout: an operation is twenty-eight bytes
/// and a value twelve, so a hive chunk holds thousands of either and neither
/// costs an allocation of its own. A field added without a plan breaks this.
#[test]
fn stored_entities_keep_their_size_budget() {
    assert_eq!(std::mem::size_of::<tir::OpInstance>(), 28);
    assert_eq!(std::mem::size_of::<tir::Value>(), 12);
    // An attribute is its interned name plus an `AttributeValue`, whose widest
    // variant is `RegisterAttr::Assigned`.
    assert_eq!(std::mem::size_of::<tir::attributes::NamedAttribute>(), 32);
}

/// Growing an op past its run's size class moves the run to the next class,
/// hands the old cell back, and leaves every use list resolving to the op.
#[test]
fn growing_an_op_promotes_its_run_and_keeps_its_uses() {
    let context = Context::with_default_dialects();
    let (c, d, add) = add_fixture(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);

    let extra: Vec<ValueId> = (0..5)
        .map(|_| context.create_value(i32_ty, None).id())
        .collect();
    for value in &extra {
        context.append_operand(add, *value);
    }

    let operands = context.get_op(add).operands();
    assert_eq!(operands.len(), 7);
    for (index, value) in operands.iter().enumerate() {
        assert!(
            context.uses_of(*value).contains(&Use::new(add, index)),
            "operand {index} lost its use list entry"
        );
    }
    assert!(context.uses_of(c).iter().all(|r#use| r#use.op == add));
    assert!(context.uses_of(d).iter().all(|r#use| r#use.op == add));
}

/// Erasing an operation gives its ports' storage back: once the context is
/// told the runs are free, the same operations built again are served out of
/// the freed spans and the arena does not grow. Entity ids are deliberately
/// not recycled, so the op hive keeps its chunks.
#[test]
fn erasing_operations_returns_their_run_storage() {
    let context = Context::with_default_dialects();
    let (_, _, body) = module_with_function(&context);
    let i32_ty = builtin::IntegerType::new(&context, 32);
    let seed = builtin::ops::constant(&context, 1, i32_ty).build();
    body.append(seed.id());
    let build = || {
        (0..10_000)
            .map(|_| {
                let op = builtin::ops::addi(&context, seed.result(), seed.result(), i32_ty).build();
                body.append(op.id());
                op.id()
            })
            .collect::<Vec<OpId>>()
    };
    let ops = build();
    context.commit();
    let peak = context.slab_census();

    for op in ops {
        context
            .erase_op(&tir::OperationRef::new(context.get_op(op)))
            .expect("erase should succeed");
    }
    context.commit();
    let after = context.slab_census();

    assert!(
        after.runs_live < peak.runs_live,
        "live runs {} -> {}",
        peak.runs_live,
        after.runs_live
    );
    assert_eq!(after.ops_chunks, peak.ops_chunks, "op ids are not recycled");

    build();
    context.commit();
    let reused = context.slab_census();
    assert_eq!(reused.runs_live, peak.runs_live, "the same runs are live");
    assert_eq!(
        reused.runs_bytes, peak.runs_bytes,
        "erased run storage is handed to the operations built after it"
    );
}
