//! The one register notation: register slots as SSA ports, physical literals,
//! the assignment map, and the rules the machine-IR verifier enforces over them.
//!
//! Machine instructions have no textual form, so these build the IR directly
//! rather than through a `.tir` check.

use tir::attributes::AttributeValue;
use tir::backend::regalloc::RegClassId;
use tir::backend::{
    phys_attr, verify_machine_ir, RegAssignment, RegClassType, SymbolOp, ASSIGNMENT_ATTR, PINS_ATTR,
};
use tir::{Context, Operation, ValueId};

use super::fixtures::{asm_symbol, r, r_high};

// `add rd, rs`: one destination slot and one source slot, both of class `R`,
// plus the implicit flag register the behavior writes.

static ADD_IMPLICIT: [tir::attributes::ImplicitReg; 1] = [tir::attributes::ImplicitReg {
    class: r(),
    index: 7,
    role: tir::attributes::AttributeRole::Def,
}];

tir::operation! {
    AddTestOp {
        name: "add", dialect: "test", scheme: "(_?) -> _?",
        operands: O { rs }, results: R { rd },
        attributes: A { rd: "?Register", rs: "?Register" },
        interfaces: [tir::backend::MachineInstruction],
    }
}
super::fixtures::instr_info!(AddTestOp, "add", &ADD_IMPLICIT);

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

fn function(build: impl FnOnce(&Context) -> AddTestOp) -> Function {
    let context = Context::with_default_dialects();
    AddTestOp::register_interfaces(&context);
    let add = build(&context);
    let handle = add.get_handle();
    let (symbol, _) = asm_symbol(&context, &[add.id()]);
    Function {
        context,
        symbol,
        add: handle,
    }
}

impl AddTestOp {
    fn get_handle(&self) -> tir::OpHandle {
        self.0.clone()
    }
}

#[test]
fn def_use_reads_ports_and_leaves_physical_registers_out() {
    let f = function(|context| {
        let source = value_of(context, r());
        AddTestOp::new(
            context,
            source,
            vec![RegClassType::new(context, r())],
            Default::default(),
            tir::InsertionPoint::Root,
        )
    });
    let regs = tir::analysis::op_regs(&f.add);
    assert_eq!(regs.defs.len(), 1, "the destination slot is a result");
    assert_eq!(regs.uses.len(), 1, "the source slot is an operand");
    assert!(regs.phys_defs.is_empty());
    assert!(regs.phys_uses.is_empty());

    // The same instruction reading a register it names directly: no operand,
    // one physical read.
    let f = function(|context| {
        AddTestOp::new(
            context,
            tir::Operand::none(),
            vec![RegClassType::new(context, r())],
            tir::NewOpParts {
                attributes: vec![context.named_attribute("rs", phys_attr((r(), 3)))],
            },
            tir::InsertionPoint::Root,
        )
    });
    let regs = tir::analysis::op_regs(&f.add);
    assert!(regs.uses.is_empty());
    assert_eq!(regs.phys_uses, vec![(r(), 3)]);

    // Execution additionally sees the register the behavior writes by path.
    let execution = tir::analysis::execution_regs(&f.add);
    assert_eq!(execution.phys_defs, vec![(r(), 7)]);
}

#[test]
fn data_values_are_not_physical_register_views() {
    let f = function(|context| {
        let source = value_of(context, r_high());
        AddTestOp::new(
            context,
            source,
            vec![RegClassType::new(context, r())],
            Default::default(),
            tir::InsertionPoint::Root,
        )
    });
    verify_machine_ir(&f.context, f.symbol.id())
        .expect("data values do not select physical registers");
}

#[test]
fn scalar_values_need_no_register_class() {
    let f = function(|context| {
        let source = context
            .create_value(tir::builtin::IntegerType::new(context, 32), None)
            .id();
        AddTestOp::new(
            context,
            source,
            vec![RegClassType::new(context, r())],
            Default::default(),
            tir::InsertionPoint::Root,
        )
    });
    verify_machine_ir(&f.context, f.symbol.id()).expect("ordinary scalar operands are supported");
}

#[test]
fn a_result_without_a_port_is_rejected() {
    let f = function(|context| {
        let source = value_of(context, r());
        AddTestOp::new(
            context,
            source,
            vec![
                RegClassType::new(context, r()),
                RegClassType::new(context, r()),
            ],
            Default::default(),
            tir::InsertionPoint::Root,
        )
    });
    let error = verify_machine_ir(&f.context, f.symbol.id()).expect_err("surplus result");
    assert!(format!("{error:?}").contains("results"), "{error:?}");
}

#[test]
fn assignment_missing_a_value_is_rejected() {
    let f = function(|context| {
        let source = value_of(context, r());
        AddTestOp::new(
            context,
            source,
            vec![RegClassType::new(context, r())],
            Default::default(),
            tir::InsertionPoint::Root,
        )
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
    let f = function(|context| {
        let source = value_of(context, r());
        AddTestOp::new(
            context,
            source,
            vec![RegClassType::new(context, r())],
            Default::default(),
            tir::InsertionPoint::Root,
        )
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
    let f = function(|context| {
        AddTestOp::new(
            context,
            tir::Operand::none(),
            vec![],
            Default::default(),
            tir::InsertionPoint::Root,
        )
    });
    let error = verify_machine_ir(&f.context, f.symbol.id()).expect_err("empty slot");
    assert!(
        format!("{error:?}").contains("missing register attribute"),
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
    let symbol = SymbolOp::new(
        &context,
        None,
        tir::NewOpParts {
            attributes: vec![
                context.named_attribute("name", AttributeValue::Str("f".into())),
                context.named_attribute(ASSIGNMENT_ATTR, parsed),
            ],
        },
        tir::InsertionPoint::Root,
    );
    let read_back = RegAssignment::of_op(symbol.handle(), ASSIGNMENT_ATTR);
    assert_eq!(read_back, assignment);
}
