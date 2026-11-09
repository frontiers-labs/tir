use std::error::Error;

use tir::{Context, OpHandle, Operation, attributes::AttributeValue, builtin, func};
use tir_egglog::{Read, Write};
use tir_symbolic::term::constructor_fields;

fn parse(context: &Context, source: &str) -> Result<OpHandle, Box<dyn Error>> {
    tir::parse::ir::parse_operation(context, source, &builtin::MODULE)
        .map_err(|(span, error)| format!("IR parse at {}: {error}", span.0).into())
}

fn fixture_function(context: &Context, module: &OpHandle) -> Result<OpHandle, Box<dyn Error>> {
    builtin::module_body(module)
        .op_ids()
        .into_iter()
        .find_map(|id| context.get_op(id).as_operation(&func::FUNC))
        .ok_or_else(|| "fixture function".into())
}

fn verify_type_read_after_placement_edit(
    context: &Context,
    source: &OpHandle,
    target: &OpHandle,
    block: &tir::BlockHandle,
) -> Result<(), Box<dyn Error>> {
    let moved = context
        .get_block(func::func_body_region(source).entry_block())
        .op_ids()[0];
    let moved_type = block.arguments()[0].ty();
    let original_placement;
    {
        let mut engine = context.egraph();
        original_placement = engine
            .read(|state| state.lookup("OpPlacement", i64::from(moved.number())))?
            .ok_or("original placement")?;
        engine.update(|mut state| state.remove("OpPlacement", i64::from(moved.number())))?;
        let placement = engine
            .update(|mut state| state.add("InBlock", (i64::from(block.id().number()), 0_i64)))?;
        engine
            .update(|mut state| state.set("OpPlacement", i64::from(moved.number()), placement))?;
    }
    context.terms().term(moved_type);
    {
        let mut engine = context.egraph();
        let owner = engine
            .read(|state| state.lookup("OwningFunction", i64::from(moved.number())))?
            .ok_or("moved owner")?;
        assert_eq!(
            engine.value_to_base::<i64>(owner),
            i64::from(target.id().number())
        );
        engine.update(|mut state| state.remove("OpPlacement", i64::from(moved.number())))?;
        engine.update(|mut state| {
            state.set("OpPlacement", i64::from(moved.number()), original_placement)
        })?;
    }

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
    let mut engine = context.egraph();
    let attr = engine
        .read(|state| state.lookup("Attribute", (i64::from(branch.number()), "bits")))?
        .ok_or("stored attribute")?;
    let fields = constructor_fields(&mut engine, "FloatAttr", attr).ok_or("typed float")?;
    assert_eq!(engine.value_to_base::<String>(fields[1]), "f64");
    assert_eq!(
        engine.value_to_base::<String>(fields[2]),
        "fff8000000000042"
    );
    drop(engine);
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let context = Context::with_default_dialects();
    let module = parse(
        &context,
        include_str!("../checks/IRRoundtrip/br-with-args.tir"),
    )?;
    let function = fixture_function(&context, &module)?;
    let block = context.get_block(func::func_body_region(&function).entry_block());
    let branch = block.op_ids()[0];
    let parameter = block.arguments()[0].id();
    {
        let mut engine = context.egraph();
        assert_eq!(engine.get_size("Successor"), 1);
        assert_eq!(engine.get_size("SuccessorArg"), 1);
        assert_eq!(engine.get_size("EdgeArgument"), 1);
        engine.update(|mut state| {
            state.add("Scope", i64::from(module.id().number()))?;
            Ok(())
        })?;
    }
    assert!(context.egraph().read(|state| state.contains(
        "Within",
        (i64::from(module.id().number()), i64::from(branch.number()))
    ))?);
    assert_eq!(context.uses_of(parameter).len(), 1);
    context.set_op_operands(branch, vec![]);
    assert!(context.uses_of(parameter).is_empty());
    context.append_operand(branch, parameter);
    {
        let engine = context.egraph();
        assert_eq!(engine.get_size("SuccessorArg"), 1);
        assert_eq!(engine.get_size("EdgeArgument"), 1);
    }
    context.set_op_operands(branch, vec![]);
    assert!(block.remove_op(branch));
    assert!(!context.egraph().read(|state| state.contains(
        "Within",
        (i64::from(module.id().number()), i64::from(branch.number()))
    ))?);

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
        assert_eq!(engine.get_size("Theta"), 1);
        assert_eq!(engine.get_size("Gamma"), 1);
        assert_eq!(engine.get_size("EntryBinding"), 3);
        assert_eq!(engine.get_size("ExitBinding"), 3);
        assert_eq!(engine.get_size("FeedbackBinding"), 1);
    }

    verify_type_read_after_placement_edit(&context, &loop_function, &function, &block)?;

    verify_float_attribute(&context, branch)?;
    let body = func::func_body_region(&function);
    let next_entry = body.block_ids()[1];
    assert!(body.remove_block(block.id()));
    {
        let engine = context.egraph();
        let entry = engine
            .read(|state| state.lookup("Entry", i64::from(body.id().number())))?
            .ok_or("entry block")?;
        assert_eq!(
            engine.value_to_base::<i64>(entry),
            i64::from(next_entry.number())
        );
    }
    context.erase_op(&tir::OperationRef::new(context.get_op(gamma_function.id())))?;
    {
        let engine = context.egraph();
        assert_eq!(engine.get_size("Gamma"), 0);
        assert_eq!(engine.get_size("GammaArm"), 0);
        assert_eq!(engine.get_size("GammaDefaultArm"), 0);
        assert_eq!(engine.get_size("EntryBinding"), 1);
        assert_eq!(engine.get_size("ExitBinding"), 1);
    }
    context.erase_op(&tir::OperationRef::new(context.get_op(loop_function.id())))?;
    {
        let engine = context.egraph();
        for table in [
            "Theta",
            "ThetaBody",
            "ThetaPredicate",
            "ThetaContinueOn",
            "EntryBinding",
            "ExitBinding",
            "FeedbackBinding",
        ] {
            assert_eq!(engine.get_size(table), 0, "{table}");
        }
    }
    println!("IR schema, control bindings, exact attributes and edit invalidation pass");
    Ok(())
}
