//! The one register notation: register slots as SSA ports, physical literals,
//! the assignment map, and the rules the machine-IR verifier enforces over them.
//!
//! Machine instructions have no textual form, so these build the IR directly
//! rather than through a `.tir` check.

use tir::attributes::AttributeValue;
use tir::backend::regalloc::RegClassId;
use tir::backend::{
    phys_attr, verify_machine_ir, RegAssignment, RegClassType, SymbolOpBuilder, ASSIGNMENT_ATTR,
    PINS_ATTR,
};
use tir::{Context, NewOp, Operation, TypeCheckError, ValueId};

use super::fixtures::ops::{AddTestOp, AddTestOpBuilder};
use super::fixtures::{asm_symbol, instr_info, r, r_high};

// `add rd, rs`: one destination slot and one source slot, both of class `R`,
// plus the implicit flag register the behavior writes.
static ADD_IMPLICIT: [tir::attributes::ImplicitReg; 1] = [tir::attributes::ImplicitReg {
    class: r(),
    index: 7,
    role: tir::attributes::AttributeRole::Def,
}];

instr_info!(AddTestOp, "add", &ADD_IMPLICIT);

fn value_of(context: &Context, class: RegClassId) -> ValueId {
    context
        .create_value(RegClassType::new(context, class), None)
        .id()
}

/// An `asm.symbol` holding one `add` in its body, built with whatever slots the
/// caller asks for.
struct Function {
    context: Context,
    symbol: tir::backend::SymbolOp,
    add: tir::OpHandle,
}

fn function(build: impl FnOnce(&Context, AddTestOpBuilder) -> AddTestOpBuilder) -> Function {
    let context = Context::with_default_dialects();
    AddTestOp::register_interfaces(&context);
    let add = build(&context, AddTestOpBuilder::new(&context)).build();
    let handle = add.handle().clone();
    let (symbol, _) = asm_symbol(&context, &[add.id()]);
    Function {
        context,
        symbol,
        add: handle,
    }
}

#[test]
fn def_use_reads_ports_and_leaves_physical_registers_out() {
    let f = function(|context, builder| {
        let source = value_of(context, r());
        builder
            .rs(source)
            .result_type(RegClassType::new(context, r()))
    });
    let regs = tir::analysis::op_regs(&f.add);
    assert_eq!(regs.defs.len(), 1, "the destination slot is a result");
    assert_eq!(regs.uses.len(), 1, "the source slot is an operand");
    assert!(regs.phys_defs.is_empty());
    assert!(regs.phys_uses.is_empty());

    // The same instruction reading a register it names directly: no operand,
    // one physical read.
    let f = function(|context, builder| {
        builder
            .attr(
                tir::analysis::defuse::USES_ATTR,
                AttributeValue::Array(vec![phys_attr((r(), 3))].into()),
            )
            .result_type(RegClassType::new(context, r()))
    });
    let regs = tir::analysis::op_regs(&f.add);
    assert!(regs.uses.is_empty());
    assert_eq!(regs.phys_uses, vec![(r(), 3)]);

    // Execution additionally sees the register the behavior writes by path.
    let execution = tir::analysis::execution_regs(&f.add);
    assert_eq!(execution.phys_defs, vec![(r(), 7)]);
}

#[test]
fn cross_view_assignment_is_rejected() {
    let f = function(|context, builder| {
        let source = value_of(context, r());
        builder
            .rs(source)
            .result_type(RegClassType::new(context, r()))
    });
    let mut assignment = RegAssignment::default();
    assignment.insert(f.add.operands()[0], (r_high(), 0));
    assignment.insert(f.add.results()[0], (r(), 0));
    let mut attributes = f.symbol.attributes();
    attributes.push(
        f.context
            .named_attribute(ASSIGNMENT_ATTR, assignment.to_attribute()),
    );
    f.context.set_op_attributes(f.symbol.id(), attributes);
    let error = verify_machine_ir(&f.context, f.symbol.id()).expect_err("cross-view operand");
    assert!(
        format!("{error:?}").contains("different register view"),
        "{error:?}",
    );
}

#[test]
fn operand_without_a_register_class_is_rejected() {
    let f = function(|context, builder| {
        let source = context
            .create_value(tir::builtin::IntegerType::new(context, 32), None)
            .id();
        builder
            .rs(source)
            .result_type(RegClassType::new(context, r()))
    });
    let error = tir::check_types(&f.context).expect_err("class-less operand");
    let TypeCheckError::IllTyped { first, rest } = error else {
        panic!("expected a typing diagnostic, got {error:?}");
    };
    assert!(
        std::iter::once(first).chain(rest).any(|diagnostic| {
            diagnostic.operation == f.add.id && diagnostic.detail() == "signature"
        }),
        "the add operand must have a register type"
    );
}

#[test]
fn a_result_without_a_port_is_rejected() {
    let context = Context::with_default_dialects();
    AddTestOp::register_interfaces(&context);
    let (symbol, block) = asm_symbol(&context, &[]);
    let invalid = NewOp::new_dynamic(
        ("test", "add"),
        context.clone(),
        vec![value_of(&context, r())],
        vec![value_of(&context, r()), value_of(&context, r())],
        vec![],
        vec![],
    );
    context.add_operation(block.end(), invalid);
    let error = verify_machine_ir(&context, symbol.id()).expect_err("surplus result");
    assert!(
        format!("{error:?}").contains("expects 1 results, got 2"),
        "{error:?}"
    );
}

#[test]
fn assignment_missing_a_value_is_rejected() {
    let f = function(|context, builder| {
        let source = value_of(context, r());
        builder
            .rs(source)
            .result_type(RegClassType::new(context, r()))
    });
    let mut assignment = RegAssignment::default();
    for value in f.add.results().iter().chain(f.add.operands().iter()) {
        assignment.insert(*value, (r(), 0));
    }
    let attrs = |context: &Context, map: &RegAssignment| {
        let mut attrs = f.symbol.attributes();
        attrs.retain(|attr| context.resolve(attr.name) != ASSIGNMENT_ATTR);
        attrs.push(context.named_attribute(ASSIGNMENT_ATTR, map.to_attribute()));
        attrs
    };
    f.context
        .set_op_attributes(f.symbol.id(), attrs(&f.context, &assignment));
    verify_machine_ir(&f.context, f.symbol.id()).expect("a total assignment verifies");

    // Drop the source's entry: the instruction still names it.
    let mut partial = RegAssignment::default();
    partial.insert(f.add.results()[0], (r(), 0));
    f.context
        .set_op_attributes(f.symbol.id(), attrs(&f.context, &partial));
    let error = verify_machine_ir(&f.context, f.symbol.id()).expect_err("partial assignment");
    assert!(
        format!("{error:?}").contains("no register in the assignment"),
        "{error:?}",
    );
}

#[test]
fn cross_view_pin_is_rejected() {
    let f = function(|context, builder| {
        let source = value_of(context, r());
        builder
            .rs(source)
            .result_type(RegClassType::new(context, r()))
    });
    let mut pins = std::collections::BTreeMap::new();
    pins.insert("rs".to_string(), phys_attr((r_high(), 0)));
    let mut attrs = f.add.attributes();
    attrs.push(
        f.context
            .named_attribute(PINS_ATTR, AttributeValue::Dict(Box::new(pins))),
    );
    f.context.set_op_attributes(f.add.id, attrs);

    let error = verify_machine_ir(&f.context, f.symbol.id()).expect_err("cross-view pin");
    assert!(
        format!("{error:?}").contains("is pinned to Rhigh[0]"),
        "{error:?}"
    );
}

#[test]
fn a_slot_holding_nothing_is_rejected() {
    let f = function(|context, builder| builder.result_type(RegClassType::new(context, r())));
    let error = verify_machine_ir(&f.context, f.symbol.id()).expect_err("empty slot");
    assert!(
        format!("{error:?}").contains("missing required operand 'rs'"),
        "{error:?}",
    );
}

#[test]
fn the_assignment_round_trips_through_the_text_form() {
    let context = Context::with_default_dialects();
    let mut assignment = RegAssignment::default();
    assignment.insert(ValueId::from_number(3), (r(), 1));
    assignment.insert(ValueId::from_number(7), (r(), 2));

    let mut printed = String::new();
    let mut formatter = tir::IRFormatter::new(&mut printed);
    assignment
        .to_attribute()
        .print(&mut formatter, &context)
        .expect("prints");
    assert_eq!(printed, "[%3:R[1], %7:R[2]]");

    context.register_reg_classes(&super::fixtures::R_CLASSES);
    let mut parser = tir::parse::text::Parser::new(&printed);
    let parsed = parser
        .parse_attribute_value(&context)
        .expect("parses")
        .expect("an attribute");
    let symbol = SymbolOpBuilder::new(&context)
        .attr("name", AttributeValue::Str("f".into()))
        .attr(ASSIGNMENT_ATTR, parsed)
        .build();
    let read_back = RegAssignment::of_op(symbol.handle(), ASSIGNMENT_ATTR);
    assert_eq!(read_back, assignment);
}
