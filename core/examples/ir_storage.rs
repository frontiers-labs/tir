use std::error::Error;

use tir::{Context, Operation, attributes::AttributeValue, builtin::ModuleOp, func::FuncOp};
use tir_egglog::{EGraph, Read, Value};

fn parse(context: &Context, source: &str) -> Result<ModuleOp, Box<dyn Error>> {
    tir::parse::ir::parse_ir(context, source)
        .map_err(|(span, error)| format!("IR parse at {}: {error}", span.0).into())
}

fn fixture_function(context: &Context, module: &ModuleOp) -> Result<FuncOp, Box<dyn Error>> {
    module
        .body()
        .op_ids()
        .into_iter()
        .find_map(|id| context.get_op(id).as_op::<FuncOp>())
        .ok_or_else(|| "fixture function".into())
}

fn reader_at(
    context: &Context,
    op: tir::OpId,
    region: tir::RegionId,
    member: tir::OpId,
) -> Result<bool, tir_egglog::Error> {
    context.egraph().read(|s| {
        s.contains(
            "ReaderAt",
            (
                i64::from(op.number()),
                i64::from(region.number()),
                i64::from(member.number()),
            ),
        )
    })
}

fn replace_operation(
    engine: &mut EGraph,
    old: &[Value],
    new: &[Value],
) -> Result<(), Box<dyn Error>> {
    let db = engine.database_mut();
    let table = db.table("Operation")?;
    assert!(matches!(
        db.insert(table, new),
        Err(tir_egglog::Error::DuplicateKey { .. })
    ));
    assert!(db.lookup(table, old).is_some());
    db.remove(table, &old[..db.schema().table(table).key_arity()]);
    db.insert(table, new)?;
    Ok(())
}

fn verify_type_read_after_placement_edit(
    context: &Context,
    source: &FuncOp,
    target: &FuncOp,
    block: &tir::BlockHandle,
) -> Result<(), Box<dyn Error>> {
    let moved = context
        .get_block(source.body_region().entry_block())
        .op_ids()[0];
    let moved_type = block.arguments()[0].ty();
    let (old, new) = {
        let mut engine = context.egraph();
        let db = engine.database();
        let table = db.table("Operation")?;
        let id = engine.base_to_value(i64::from(moved.number()));
        let row = db
            .rows(table)
            .find(|r| r[0] == id)
            .ok_or("original placement")?;
        let old: Vec<_> = row.values().collect();
        let mut new = old.clone();
        new[3] = engine.base_to_value(i64::from(block.id().number()));
        new[4] = engine.base_to_value(0_i64);
        replace_operation(&mut engine, &old, &new)?;
        (old, new)
    };
    context.terms().term(moved_type);
    assert!(reader_at(context, moved, target.body_region().id(), moved)?);
    assert!(!reader_at(
        context,
        moved,
        source.body_region().id(),
        moved
    )?);
    replace_operation(&mut context.egraph(), &new, &old)?;
    assert!(reader_at(context, moved, source.body_region().id(), moved)?);
    Ok(())
}

fn verify_float_attribute(context: &Context, branch: tir::OpId) -> Result<(), Box<dyn Error>> {
    let nan_bits = 0xfff8_0000_0000_0042;
    let mut attributes = context.get_op(branch).attributes();
    attributes.push(context.named_attribute("bits", AttributeValue::F64(f64::from_bits(nan_bits))));
    context.set_op_attributes(branch, attributes);
    let Some(AttributeValue::F64(value)) = context.get_op(branch).attr("bits") else {
        return Err("float attribute".into());
    };
    assert_eq!(value.to_bits(), nan_bits);
    assert!(context.egraph().read(|s| s.contains(
        "Attribute",
        ("operation", i64::from(branch.number()), "bits")
    ))?);
    Ok(())
}

fn resolved_edge_argument(
    context: &Context,
    branch: tir::OpId,
) -> Result<Option<i64>, tir_egglog::Error> {
    let engine = context.egraph();
    let branch = i64::from(branch.number());
    engine.read(|s| {
        let Some(slot) = s.lookup("SuccessorArgument", (branch, 0_i64, 0_i64))? else {
            return Ok(None);
        };
        Ok(s.lookup("Operand", (branch, slot))?
            .map(|v| engine.value_to_base(v)))
    })
}

fn verify_metadata_ownership(context: &Context, branch: tir::OpId) -> Result<(), Box<dyn Error>> {
    let terms = context.terms();
    let sym = context.intern("old_spelling");
    let name = terms.name("old_spelling");
    {
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = db.table("Symbol")?;
        let index = db.add_kept_index(table, &[0], &[]);
        let row = db
            .listed_nth(index, &[db.base(sym.index() as i64)], 0)
            .ok_or("symbol")?;
        let old: Vec<_> = row.values().collect();
        let mut renamed = old.clone();
        renamed[1] = db.base("new_spelling".to_owned());
        db.remove(table, &old);
        db.insert(table, &renamed)?;
    }
    assert_eq!(context.sym("old_spelling"), None);
    assert_eq!(context.sym("new_spelling"), Some(sym));
    assert_eq!(terms.name_text(name), "new_spelling");

    let ctor = terms
        .ctor_named("builtin", "i")
        .expect("integer constructor");
    let (old, alias) = {
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = db.table("TypeCtor")?;
        let index = db.add_kept_index(table, &[0], &[]);
        let row = db
            .listed_nth(index, &[db.base(i64::from(ctor.number()))], 0)
            .expect("constructor");
        let old: Vec<_> = row.values().collect();
        let mut alias = old.clone();
        alias[2] = db.base("integer_alias".to_owned());
        db.remove(table, &old);
        db.insert(table, &alias)?;
        (old, alias)
    };
    assert_eq!(terms.ctor_named("builtin", "integer_alias"), Some(ctor));
    assert_eq!(terms.ctor_named("builtin", "i"), None);
    {
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = db.table("TypeCtor")?;
        db.remove(table, &alias);
        db.insert(table, &old)?;
    }

    let left = terms.int(257);
    let right = terms.int(258);
    {
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = db.table("TypeTerm")?;
        let index = db.add_kept_index(table, &[0], &[]);
        let row = |id: tir::TypeId| {
            db.listed_nth(index, &[db.base(i64::from(id.number()))], 0)
                .expect("type handle")
                .values()
                .collect::<Vec<_>>()
        };
        let (mut a, mut b) = (row(left), row(right));
        db.remove(table, &a);
        db.remove(table, &b);
        std::mem::swap(&mut a[2], &mut b[2]);
        db.insert(table, &a)?;
        db.insert(table, &b)?;
    }
    assert_eq!(terms.nat(left, 0), Some(258));
    assert_eq!(terms.int(257), right);
    assert!(
        context
            .get_op(branch)
            .has_interface::<dyn tir::Terminator>()
    );
    {
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = db.table("OpSpec")?;
        let index = db.add_kept_index(table, &[0, 1], &[]);
        let key = [
            db.base("cfg.br".to_owned()),
            db.base(std::any::type_name::<dyn tir::Terminator>().to_owned()),
        ];
        let row = db
            .listed(index, &key)
            .next()
            .ok_or("registered branch interface")?;
        let key = [row[0], row[1], row[2]];
        db.remove(table, &key);
    }
    assert!(
        !context
            .get_op(branch)
            .has_interface::<dyn tir::Terminator>()
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let context = Context::with_default_dialects();
    let module = parse(
        &context,
        include_str!("../checks/IRRoundtrip/br-with-args.tir"),
    )?;
    let function = fixture_function(&context, &module)?;
    let block = context.get_block(function.body_region().entry_block());
    let branch = block.op_ids()[0];
    let module_region = context
        .parent_region(module.body().id())
        .ok_or("module body region")?;
    let parameter = block.arguments()[0].id();
    tir::check_types(&context)?;
    assert!(context.egraph().database().table_count() <= 20);
    assert_eq!(context.egraph().get_size("Successor"), 1);
    assert_eq!(
        resolved_edge_argument(&context, branch)?,
        Some(i64::from(parameter.number()))
    );
    assert!(reader_at(&context, branch, module_region, function.id())?);
    assert_eq!(context.uses_of(parameter).len(), 1);
    context.set_op_operands(branch, vec![]);
    assert!(context.uses_of(parameter).is_empty());
    assert_eq!(resolved_edge_argument(&context, branch)?, None);
    context.append_operand(branch, parameter);
    assert_eq!(
        resolved_edge_argument(&context, branch)?,
        Some(i64::from(parameter.number()))
    );
    context.set_op_operands(branch, vec![]);
    assert!(block.remove_op(branch));
    assert!(!reader_at(&context, branch, module_region, function.id())?);

    let loop_module = parse(
        &context,
        include_str!("../checks/IRRoundtrip/loop-basic.tir"),
    )?;
    let loop_function = fixture_function(&context, &loop_module)?;
    let gamma_module = parse(
        &context,
        include_str!("../checks/IRRoundtrip/switch2-basic.tir"),
    )?;
    let gamma_function = fixture_function(&context, &gamma_module)?;
    {
        let engine = context.egraph();
        assert_eq!(engine.get_size("Control"), 2);
        assert_eq!(engine.get_size("Binding"), 7);
        assert_eq!(engine.get_size("Arm"), 2);
    }
    verify_type_read_after_placement_edit(&context, &loop_function, &function, &block)?;
    verify_float_attribute(&context, branch)?;
    let body = function.body_region();
    let next_entry = body.block_ids()[1];
    assert!(body.remove_block(block.id()));
    assert_eq!(body.entry_block(), next_entry);
    context.erase_op(&tir::OperationRef::new(context.get_op(gamma_function.id())))?;
    {
        let engine = context.egraph();
        assert_eq!(engine.get_size("Control"), 1);
        assert_eq!(engine.get_size("Arm"), 0);
        assert_eq!(engine.get_size("Binding"), 3);
    }
    context.erase_op(&tir::OperationRef::new(context.get_op(loop_function.id())))?;
    {
        let engine = context.egraph();
        assert_eq!(engine.get_size("Control"), 0);
        assert_eq!(engine.get_size("Binding"), 0);
    }
    verify_metadata_ownership(&context, branch)?;
    println!("IR schema, control bindings, exact attributes and edit invalidation pass");
    Ok(())
}
