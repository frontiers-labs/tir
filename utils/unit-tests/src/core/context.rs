//! Context tests: staged regions, port growth, interning and spine bumps.

use tir::{
    builtin, func::FuncOp, scf, BlockHandle, Commutative, Context, OpId, Operation, Use, ValueId,
};

use super::fixtures;

#[test]
fn classifies_types_interned_after_earlier_queries() {
    let context = Context::new();
    let memory = builtin::StateType::memory(&context);
    assert_eq!(
        context.state_resource(memory),
        Some(builtin::StateResource::Memory)
    );

    let shared = context.clone();
    let integer = builtin::IntegerType::new(&shared, 32);
    assert_eq!(context.state_resource(integer), None);
    assert_eq!(
        context.state_resource(memory),
        Some(builtin::StateResource::Memory)
    );
}

#[test]
fn symbol_errors_follow_definition_order() {
    for _ in 0..16 {
        let (context, module) = fixtures::parse(
            r#"module {
%z1 = func.func @zeta() { func.return }
%z2 = func.func @zeta() { func.return }
%a1 = func.func @alpha() { func.return }
%a2 = func.func @alpha() { func.return }
module_end
}"#,
        );
        let table = tir::SymbolTable::build(&context, module.id());
        assert_eq!(table.names().collect::<Vec<_>>(), ["zeta", "alpha"]);
        assert!(table
            .verify(&context)
            .unwrap_err()
            .to_string()
            .contains("@zeta()"));
    }
}

#[test]
fn state_errors_follow_operand_walk_order() {
    for _ in 0..16 {
        let (context, module, _, body) = fixtures::parse_function(
            r#"module {
%f = func.func @main(%p: !ptr.p<!i32>, %v: !i32) {
  %s0 = state.entry_state : !state<memory>
  %s1 = state.entry_state : !state<memory>
  %s2 = ptr.store %v, %p state(%s0)
  %s3 = ptr.store %v, %p state(%s0)
  %s4 = ptr.store %v, %p state(%s1)
  %s5 = ptr.store %v, %p state(%s1)
  func.return
}
module_end
}"#,
        );
        let first = context
            .get_op(context.get_region(body).op_ids()[0])
            .results()[0];
        let error = tir::verify_state_forks(&context, module.id())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "state %{} has incompatible concurrent uses",
                first.number()
            )),
            "{error}"
        );
    }
}

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

#[test]
fn adopting_a_value_makes_the_block_define_it() {
    let context = Context::with_default_dialects();
    let (_, _, body) = module_with_function(&context);
    let i32 = builtin::IntegerType::new(&context, 32);
    let input = context.create_value(i32, None);
    let produced = builtin::ops::addi(&context, input.id(), input.id(), i32).build();
    let result = context.get_op(produced.id()).results()[0];
    let reader = builtin::ops::addi(&context, result, result, i32).build();
    context.insert(body.end(), reader.id());

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
fn parent_block_tracks_membership() {
    let context = Context::with_default_dialects();
    let i32 = builtin::IntegerType::new(&context, 32);
    let a = context.create_value(i32, None);
    let b = context.create_value(i32, None);

    let block = context.create_block(vec![]);
    let add = builtin::ops::addi(&context, a.id(), b.id(), i32)
        .at(block.end())
        .build();

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
    context.insert(body_block.end(), nested.id());
    scf::ops::r#yield(&context, vec![])
        .at(body_block.end())
        .build();
    scf::nodes::ForOpBuilder::new(&context)
        .lb(bound.id())
        .inits(vec![])
        .ub(bound.id())
        .step(bound.id())
        .body(body_region.id())
        .result_types(vec![i32])
        .at(body.end())
        .build();

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
    context.insert(body.end(), c.id());
    let add = builtin::ops::addi(&context, c.result(), c.result(), i32_ty).build();
    context.insert(body.end(), add.id());

    assert_eq!(
        context.uses_of(c.result()),
        [Use::new(add.id(), 0), Use::new(add.id(), 1)]
    );
    assert_eq!(context.users_of(c.result()), [add.id(); 2]);
    assert_eq!(context.use_count(c.result()), 2);
    assert!(context.is_used(c.result()));
    assert!(!context.is_used(add.result()));
}

fn add_fixture(context: &Context) -> (ValueId, ValueId, OpId) {
    let (_, _, body) = module_with_function(context);
    let i32_ty = builtin::IntegerType::new(context, 32);
    let c = builtin::ops::constant(context, 1, i32_ty).build();
    context.insert(body.end(), c.id());
    let d = builtin::ops::constant(context, 2, i32_ty).build();
    context.insert(body.end(), d.id());
    let add = builtin::ops::addi(context, c.result(), c.result(), i32_ty).build();
    context.insert(body.end(), add.id());
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
    let token = context
        .create_value(tir::builtin::StateType::memory(&context), None)
        .id();

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

#[test]
fn stored_entities_keep_their_size_budget() {
    // An attribute is its interned name plus an `AttributeValue`, whose widest
    // variant is `RegisterAttr::Assigned`.
    assert_eq!(std::mem::size_of::<tir::attributes::NamedAttribute>(), 32);
}

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

proptest::proptest! {
    #[test]
    fn block_permutations_preserve_identity(keys in proptest::collection::vec(proptest::num::u32::ANY, 2..24)) {
        let context = Context::with_default_dialects();
        let (_, function, entry) = module_with_function(&context);
        let body = context.get_op(function).regions()[0];
        let region = context.get_region(body);
        let i32 = builtin::IntegerType::new(&context, 32);
        let value = context.create_value(i32, None);
        let add = builtin::ops::addi(&context, value.id(), value.id(), i32).build();
        context.insert(entry.end(), add.id());
        let result = context.get_op(add.id()).results()[0];
        for _ in &keys {
            region.add_block(context.create_block(vec![]).id());
        }
        let original = region.block_ids();
        let mut tail: Vec<_> = original[1..].iter().copied().zip(&keys).collect();
        tail.sort_by_key(|(_, key)| *key);
        let order: Vec<_> = std::iter::once(original[0]).chain(tail.into_iter().map(|(block, _)| block)).collect();
        let contents = entry.op_ids();
        context.reorder_region_blocks(body, order.clone()).unwrap();
        proptest::prop_assert_eq!(region.block_ids(), order.clone());
        proptest::prop_assert_eq!(entry.op_ids(), contents);
        proptest::prop_assert!(context.has_value(result));
        for block in original {
            proptest::prop_assert_eq!(context.parent_region(block), Some(body));
        }
        let mut duplicate = order.clone();
        duplicate[1] = duplicate[0];
        proptest::prop_assert!(context.reorder_region_blocks(body, duplicate).is_err());
        let mut changed_entry = order.clone();
        changed_entry.swap(0, 1);
        proptest::prop_assert!(context.reorder_region_blocks(body, changed_entry).is_err());
        proptest::prop_assert_eq!(region.block_ids(), order);
    }
}

#[test]
fn insertion_points_place_and_move_operations_without_duplicate_membership() {
    use tir::InsertionPoint;

    let context = Context::with_default_dialects();
    let ty = builtin::IntegerType::new(&context, 32);
    let block = context.create_block(vec![]);
    let graph = context.create_nodes_region(vec![], vec![], vec![]);
    let root = builtin::ops::constant(&context, 0, ty)
        .at(InsertionPoint::Root)
        .build();
    assert_eq!(context.insertion_point(root.id()), InsertionPoint::Root);

    let tail = builtin::ops::constant(&context, 1, ty)
        .at(block.end())
        .build();
    let head = builtin::ops::constant(&context, 2, ty)
        .at(InsertionPoint::Block {
            block: block.id(),
            position: 0,
        })
        .build();
    let middle = builtin::ops::constant(&context, 3, ty)
        .at(context.insertion_point(tail.id()))
        .build();
    assert_eq!(block.op_ids(), vec![head.id(), middle.id(), tail.id()]);

    context.insert(block.end(), head.id());
    assert_eq!(block.op_ids(), vec![middle.id(), tail.id(), head.id()]);
    let node = builtin::ops::constant(&context, 4, ty)
        .at(InsertionPoint::Region(graph.id()))
        .build();
    context.insert(InsertionPoint::Region(graph.id()), middle.id());
    assert_eq!(context.parent_nodes_region(node.id()), Some(graph.id()));
    assert_eq!(context.parent_nodes_region(middle.id()), Some(graph.id()));
    assert_eq!(block.op_ids(), vec![tail.id(), head.id()]);

    context.insert(InsertionPoint::Root, tail.id());
    assert_eq!(block.op_ids(), vec![head.id()]);
    assert_eq!(context.insertion_point(tail.id()), InsertionPoint::Root);
}

#[test]
fn egglog_rules_update_context_attributes_and_survive_repeated_access() {
    use tir::attributes::AttributeValue;

    let (context, module) = fixtures::parse("module { module_end }");
    let value =
        AttributeValue::Array([AttributeValue::Bool(true), AttributeValue::Bool(false)].into());
    context.set_op_attributes(
        module.id(),
        vec![context.named_attribute("seed", value.clone())],
    );

    for source in [
        r#"
        (relation TagRequest (i64 String))
        (rule ((TagRequest op tag) (= value (Attribute "operation" op "seed")))
              ((set (Attribute "operation" op tag) value)))
        (rule ((Operation op "builtin.module" placement container position display location))
              ((TagRequest op "first")))
        (run 2)
        "#,
        r#"
        (rule ((Operation op "builtin.module" placement container position display location))
              ((TagRequest op "second")))
        (run 2)
        "#,
    ] {
        let mut engine = context.egraph_with_rules().unwrap();
        let commands = engine.parse_program("context rules", source).unwrap();
        engine.run_program(commands).unwrap();
    }

    for name in ["first", "second"] {
        assert_eq!(context.get_op(module.id()).attr(name), Some(value.clone()));
    }
}
