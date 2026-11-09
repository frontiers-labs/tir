//! The dependence DAG of a machine block: value edges, register-resource
//! edges, and the orders they admit.
//!
//! Machine instructions have no textual form, so these build the IR directly
//! rather than through a `.tir` check.

use tir::analysis::defuse::CLOBBERS_ATTR;
use tir::attributes::{AttributeRole, AttributeValue, ImplicitReg, RegisterAttr};
use tir::backend::dependence::Dependences;
use tir::backend::regalloc::{RegClassId, RegClassInfo};
use tir::backend::{verify_machine_ir, RegAssignment, RegClassType};
use tir::{BlockHandle, Context, OpId, Operation, ValueId};

use super::fixtures::{asm_symbol, r, reg_class};

/// The one-register flag file the test opcodes touch implicitly, standing for
/// x86 `EFLAGS`.
static F_CLASS: RegClassInfo = reg_class("F", "F", &[0], 1, 0, false);

const fn f() -> RegClassId {
    RegClassId::new(&F_CLASS)
}

static WRITES_FLAGS: [ImplicitReg; 1] = [ImplicitReg {
    class: f(),
    index: 0,
    role: AttributeRole::Def,
}];

static READS_FLAGS: [ImplicitReg; 1] = [ImplicitReg {
    class: f(),
    index: 0,
    role: AttributeRole::Use,
}];

tir::helpers::op_def! {
    file: "../../core/defs/converted-tests/src_core_dependence.cdsl",
    hosts: {
        def_iface_MachineInstruction => (tir :: backend :: MachineInstruction, & super :: fixtures :: instr_info ! ( "def" , & [ ] )),
        update_param_iface_MachineInstruction => (tir :: backend :: MachineInstruction, & super :: fixtures :: instr_info ! ( "update_param" , & [ ] )),
        set_flags_iface_MachineInstruction => (tir :: backend :: MachineInstruction, & super :: fixtures :: instr_info ! ( "set_flags" , & WRITES_FLAGS )),
        read_flags_iface_MachineInstruction => (tir :: backend :: MachineInstruction, & super :: fixtures :: instr_info ! ( "read_flags" , & READS_FLAGS )),
    },
}

fn context() -> Context {
    let context = Context::with_default_dialects();
    context.register_operation(&DEF);
    context.register_operation(&SET_FLAGS);
    context.register_operation(&READ_FLAGS);
    context
}

fn result_of(context: &Context, op: OpId) -> ValueId {
    context.get_op(op).results()[0]
}

fn def_reg(context: &Context) -> (OpId, ValueId) {
    let op = r#def(
        context,
        vec![RegClassType::new(context, r())],
        Default::default(),
        tir::InsertionPoint::Root,
    )
    .id();
    (op, result_of(context, op))
}

fn flag_def(context: &Context, source: ValueId) -> OpId {
    r#set_flags(
        context,
        source,
        vec![RegClassType::new(context, r())],
        Default::default(),
        tir::InsertionPoint::Root,
    )
    .id()
}

fn flag_use(context: &Context, source: ValueId) -> OpId {
    r#read_flags(
        context,
        source,
        vec![RegClassType::new(context, r())],
        Default::default(),
        tir::InsertionPoint::Root,
    )
    .id()
}

fn graph(context: &Context, block: &BlockHandle, assignment: &RegAssignment) -> Dependences {
    Dependences::of_ops(context, &block.op_ids(), assignment)
}

/// `def -> %v` and `set_flags %v`, laid out in the order asked for.
struct DefinitionAndRead {
    symbol: tir::OpHandle,
    block: BlockHandle,
    define: OpId,
    read: OpId,
    value: ValueId,
}

fn definition_and_read(context: &Context, definition_first: bool) -> DefinitionAndRead {
    let (define, value) = def_reg(context);
    let read = flag_def(context, value);
    let order = if definition_first {
        [define, read]
    } else {
        [read, define]
    };
    let (symbol, block) = asm_symbol(context, &order);
    DefinitionAndRead {
        symbol,
        block,
        define,
        read,
        value,
    }
}

#[test]
fn a_register_read_must_follow_its_definition() {
    let context = context();

    let early = definition_and_read(&context, false);
    let error =
        verify_machine_ir(&context, early.symbol.id()).expect_err("use precedes its definition");
    assert!(
        error
            .to_string()
            .contains(&format!("%{}", early.value.number())),
        "the error names the value read too early: {error}",
    );

    let ordered = definition_and_read(&context, true);
    verify_machine_ir(&context, ordered.symbol.id()).expect("definition precedes its use");
}

#[test]
fn implicit_flag_registers_are_edges() {
    let context = context();
    let (define, value) = def_reg(&context);
    let compare = flag_def(&context, value);
    let reader = flag_use(&context, value);
    let clobber = flag_def(&context, value);
    let (_, block) = asm_symbol(&context, &[define, compare, reader, clobber]);

    let graph = graph(&context, &block, &RegAssignment::default());
    assert_eq!(
        graph.predecessors(2),
        [0, 1],
        "the reader follows its definer"
    );
    assert_eq!(
        graph.predecessors(3),
        [0, 1, 2],
        "a second definer follows the read of the first",
    );
}

#[test]
fn linearize_pulls_a_definition_before_its_use() {
    let context = context();
    let reversed = definition_and_read(&context, false);

    let graph = graph(&context, &reversed.block, &RegAssignment::default());
    assert_eq!(
        graph.linearize().expect("the edges form a DAG"),
        vec![reversed.define, reversed.read],
    );
}

#[test]
fn every_shuffled_order_respects_the_edges() {
    let context = context();
    let (define, value) = def_reg(&context);
    let compare = flag_def(&context, value);
    let reader = flag_use(&context, value);
    let (spare, _) = def_reg(&context);
    let (_, block) = asm_symbol(&context, &[define, compare, reader, spare]);

    let ops = block.op_ids();
    let graph = graph(&context, &block, &RegAssignment::default());
    for seed in 0..8 {
        let order = graph.shuffle(seed).expect("the edges form a DAG");
        assert_eq!(order.len(), ops.len());
        let position = |op: OpId| order.iter().position(|&other| other == op).unwrap();
        assert!(position(define) < position(compare), "seed {seed}");
        assert!(position(compare) < position(reader), "seed {seed}");
    }
}

#[test]
fn an_assignment_makes_values_register_resources() {
    let context = context();
    let (first, a) = def_reg(&context);
    let read = flag_def(&context, a);
    let (second, _) = def_reg(&context);
    let (_, block) = asm_symbol(&context, &[first, read, second]);

    let b = result_of(&context, second);
    let mut assignment = RegAssignment::default();
    assignment.insert(a, (r(), 0));
    assignment.insert(b, (r(), 0));

    let graph = graph(&context, &block, &assignment);
    assert_eq!(
        graph.predecessors(2),
        [0, 1],
        "the second definition follows the first and its reader",
    );
}

#[test]
fn clobbers_are_edges() {
    let context = context();
    let (define, value) = def_reg(&context);
    let barrier = flag_def(&context, value);
    let reader = flag_use(&context, value);
    context.set_op_attributes(
        barrier,
        vec![context.named_attribute(
            CLOBBERS_ATTR,
            AttributeValue::Array(
                vec![AttributeValue::Register(RegisterAttr::Physical {
                    class: f(),
                    index: 0,
                })]
                .into(),
            ),
        )],
    );
    let (_, block) = asm_symbol(&context, &[define, barrier, reader]);

    let graph = graph(&context, &block, &RegAssignment::default());
    assert_eq!(
        graph.predecessors(2),
        [0, 1],
        "the reader follows the clobber"
    );
}

proptest::proptest! {
    #[test]
    #[ignore = "IR migration: the fixture requires multiple definitions of one block parameter"]
    fn parameter_updates_admit_topological_orders(count in 1usize..16, seed in proptest::prelude::any::<u64>()) {
        let context = context();
        context.register_operation(&UPDATE_PARAM);
        let ty = RegClassType::new(&context, r());
        let parameter = context.create_value(ty, None);
        let value = parameter.id();
        let block = context.create_block(vec![parameter]);
        let mut updates = Vec::new();
        for _ in 0..count {
            let update = r#update_param(&context, value, vec![ty], Default::default(), tir::InsertionPoint::Root)
                .id();
            context.set_op_result(update, 0, value);
            context.insert(block.end(), update);
            updates.push(update);
        }
        let dependences = graph(&context, &block, &RegAssignment::default());
        proptest::prop_assert_eq!(dependences.shuffle(seed), Some(updates));
    }
}
