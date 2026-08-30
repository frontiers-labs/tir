//! The dependence DAG of a machine block: value edges, register-resource
//! edges, and the orders they admit.
//!
//! Machine instructions have no textual form, so these build the IR directly
//! rather than through a `.tir` check.

use tir::analysis::defuse::CLOBBERS_ATTR;
use tir::attributes::{AttributeRole, AttributeValue, ImplicitReg, RegisterAttr};
use tir::backend::dependence::Dependences;
use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterView};
use tir::backend::{
    verify_machine_ir, ControlFlow, InstrInfo, MachineInstruction, RegAssignment, RegClassType,
    RegPort, SymbolOp, SymbolOpBuilder,
};
use tir::{BlockHandle, Context, OpId, Operation, ValueId};

use super::fixtures::r;

/// The one-register flag file the test opcodes touch implicitly, standing for
/// x86 `EFLAGS`.
static F_CLASS: RegClassInfo = RegClassInfo {
    name: "F",
    dialect: "test",
    file: "F",
    registers: &[0],
    group_width: 1,
    view: RegisterView {
        bit_offset: 0,
        merge: false,
    },
    print_name: tir::backend::regalloc::no_register_name,
};

const fn f() -> RegClassId {
    RegClassId::new(&F_CLASS)
}

tir::helpers::operation! {
    DefOp {
        name: "def",
        dialect: "dep",
        results: R { regs: "*tir::backend::RegClassType" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

tir::helpers::operation! {
    SetFlagsOp {
        name: "set_flags",
        dialect: "dep",
        operands: O { rs: "?tir::backend::RegClassType", },
        results: R { regs: "*tir::backend::RegClassType" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

tir::helpers::operation! {
    ReadFlagsOp {
        name: "read_flags",
        dialect: "dep",
        operands: O { rs: "?tir::backend::RegClassType", },
        results: R { regs: "*tir::backend::RegClassType" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

static RD_ONLY: [RegPort; 1] = [RegPort {
    name: "rd",
    class: Some(r()),
    def: true,
    tied_to: None,
}];

static RD_RS: [RegPort; 2] = [
    RegPort {
        name: "rd",
        class: Some(r()),
        def: true,
        tied_to: None,
    },
    RegPort {
        name: "rs",
        class: Some(r()),
        def: false,
        tied_to: None,
    },
];

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

macro_rules! machine_op {
    ($op:ident, $name:literal, $ports:expr, $implicit:expr) => {
        impl MachineInstruction for $op {
            fn info(&self) -> &'static InstrInfo {
                static INFO: InstrInfo = InstrInfo {
                    name: $name,
                    mnemonic: $name,
                    control_flow: ControlFlow::None,
                    regs: $ports,
                    implicit_regs: $implicit,
                    ..InstrInfo::BASE
                };
                &INFO
            }

            fn instance(&self) -> &tir::OpHandle {
                &self.0
            }
        }
    };
}

machine_op!(DefOp, "def", &RD_ONLY, &[]);
machine_op!(SetFlagsOp, "set_flags", &RD_RS, &WRITES_FLAGS);
machine_op!(ReadFlagsOp, "read_flags", &RD_RS, &READS_FLAGS);

fn context() -> Context {
    let context = Context::with_default_dialects();
    DefOp::register_interfaces(&context);
    SetFlagsOp::register_interfaces(&context);
    ReadFlagsOp::register_interfaces(&context);
    context
}

fn result_of(context: &Context, op: OpId) -> ValueId {
    context.get_op(op).results()[0]
}

/// `def -> %v`: a fresh register out of nothing.
fn def_reg(context: &Context) -> (OpId, ValueId) {
    let op = DefOpBuilder::new(context)
        .result_types(vec![RegClassType::new(context, r())])
        .build()
        .id();
    (op, result_of(context, op))
}

/// `set_flags %rs -> %rd`: reads a register, writes one, writes the flags.
fn flag_def(context: &Context, source: ValueId) -> OpId {
    SetFlagsOpBuilder::new(context)
        .rs(source)
        .result_types(vec![RegClassType::new(context, r())])
        .build()
        .id()
}

/// `read_flags %rs -> %rd`: reads a register and the flags.
fn flag_use(context: &Context, source: ValueId) -> OpId {
    ReadFlagsOpBuilder::new(context)
        .rs(source)
        .result_types(vec![RegClassType::new(context, r())])
        .build()
        .id()
}

/// An `asm.symbol` whose body is one block holding `ops`, in that order.
fn symbol(context: &Context, ops: &[OpId]) -> (SymbolOp, BlockHandle) {
    let block = context.create_block(vec![]);
    for &op in ops {
        block.append(op);
    }
    let region = context.create_region();
    region.add_block(block.id());
    let symbol = SymbolOpBuilder::new(context)
        .body(region.id())
        .attr("name", AttributeValue::Str("f".into()))
        .build();
    (symbol, block)
}

fn graph(context: &Context, block: &BlockHandle, assignment: &RegAssignment) -> Dependences {
    Dependences::of_ops(context, &block.op_ids(), assignment)
}

/// A register operand read before the operation defining it is a silent
/// miscompile everywhere downstream; the machine verifier is what turns it
/// into an error.
#[test]
fn a_register_read_before_its_definition_is_rejected() {
    let context = context();
    let (define, value) = def_reg(&context);
    let read = flag_def(&context, value);
    let (symbol, _) = symbol(&context, &[read, define]);

    let error = verify_machine_ir(&context, symbol.id()).expect_err("use precedes its definition");
    assert!(
        error.to_string().contains(&format!("%{}", value.number())),
        "the error names the value read too early: {error}",
    );
}

/// The same operations, the definition first, are what the rule accepts.
#[test]
fn a_register_read_after_its_definition_verifies() {
    let context = context();
    let (define, value) = def_reg(&context);
    let read = flag_def(&context, value);
    let (symbol, _) = symbol(&context, &[define, read]);

    verify_machine_ir(&context, symbol.id()).expect("definition precedes its use");
}

/// A flag register no operand names still orders the operations that share it:
/// the reader follows its definer, and a second definer follows the reader.
#[test]
fn implicit_flag_registers_are_edges() {
    let context = context();
    let (define, value) = def_reg(&context);
    let compare = flag_def(&context, value);
    let reader = flag_use(&context, value);
    let clobber = flag_def(&context, value);
    let (_, block) = symbol(&context, &[define, compare, reader, clobber]);

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

/// A value edge is a fact, not a preserved order: it holds whichever way round
/// the reference reads, so the order that comes back puts the definition first
/// even where the one handed in did not.
#[test]
fn linearize_pulls_a_definition_before_its_use() {
    let context = context();
    let (define, value) = def_reg(&context);
    let read = flag_def(&context, value);
    let (_, block) = symbol(&context, &[read, define]);

    let graph = graph(&context, &block, &RegAssignment::default());
    assert_eq!(
        graph.linearize().expect("the edges form a DAG"),
        vec![define, read],
    );
}

/// Every order the shuffle picks is one the edges admit.
#[test]
fn every_shuffled_order_respects_the_edges() {
    let context = context();
    let (define, value) = def_reg(&context);
    let compare = flag_def(&context, value);
    let reader = flag_use(&context, value);
    let (spare, _) = def_reg(&context);
    let (_, block) = symbol(&context, &[define, compare, reader, spare]);

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

/// After allocation a value is a register, so two values the map places in one
/// register order each other even where no SSA edge does.
#[test]
fn an_assignment_makes_values_register_resources() {
    let context = context();
    let (first, a) = def_reg(&context);
    let read = flag_def(&context, a);
    let (second, _) = def_reg(&context);
    let (_, block) = symbol(&context, &[first, read, second]);

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

/// The registers an operation destroys are edges too: nothing reading one may
/// move across it.
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
    let (_, block) = symbol(&context, &[define, barrier, reader]);

    let graph = graph(&context, &block, &RegAssignment::default());
    assert_eq!(
        graph.predecessors(2),
        [0, 1],
        "the reader follows the clobber"
    );
}
