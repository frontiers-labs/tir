//! Fixtures shared by the core test modules.

use tir::attributes::AttributeValue;
use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterInfo, RegisterView};
use tir::backend::{RegPort, SymbolOp, SymbolOpBuilder};
use tir::builtin::ModuleOp;
use tir::func::FuncOp;
use tir::parse::ir::parse_ir;
use tir::sem::{SemGraph, SymKind, SymPayload};
use tir::NodeId;
use tir::{BlockHandle, Context, OpId, Operation, RegionId};
use tir_adt::APInt;

/// Parse `source` as a module into a fresh context holding the default dialects.
pub fn parse(source: &str) -> (Context, ModuleOp) {
    let context = Context::with_default_dialects();
    let module = parse_in(&context, source);
    (context, module)
}

/// Parse `source` as a module into `context`, which the caller has already
/// taught whatever dialects the source names.
pub fn parse_in(context: &Context, source: &str) -> ModuleOp {
    parse_ir::<ModuleOp>(context, source).expect("the fixture parses")
}

/// Parse a `module { func.func @… }` source, handing back its one function and
/// the body region a pass leaves its result in.
pub fn parse_function(source: &str) -> (Context, ModuleOp, FuncOp, RegionId) {
    let (context, module) = parse(source);
    let func = module_ops(&context, module.id())
        .into_iter()
        .find_map(|op| context.get_op(op).as_op::<FuncOp>())
        .expect("the module declares a function");
    let body = context.get_op(func.id()).regions()[0];
    (context, module, func, body)
}

/// The ops of `module`'s body block.
pub fn module_ops(context: &Context, module: OpId) -> Vec<OpId> {
    context
        .get_region(context.get_op(module).regions()[0])
        .iter(context.clone())
        .next()
        .expect("module body")
        .op_ids()
}

/// A test register class named `name` over the register file `file`, encoding
/// `registers` as groups of `group_width` file indices, viewed at `bit_offset`
/// (writes merging into the wider register iff `merge`).
pub const fn reg_class(
    name: &'static str,
    file: &'static str,
    registers: &'static [u16],
    group_width: u16,
    bit_offset: u32,
    merge: bool,
) -> RegClassInfo {
    RegClassInfo {
        name,
        dialect: "test",
        file,
        registers,
        group_width,
        view: RegisterView { bit_offset, merge },
        print_name: tir::backend::regalloc::no_register_name,
    }
}

/// A single eight-register class `R` over its own file, the shared
/// register-class fixture for the regalloc, liveness and encoding tests.
pub static R_CLASSES: [RegClassInfo; 1] =
    [reg_class("R", "R", &[0, 1, 2, 3, 4, 5, 6, 7], 1, 0, false)];

pub const fn r() -> RegClassId {
    RegClassId::new(&R_CLASSES[0])
}

/// Same file and indices as `Rlow`, but an x86 high-byte view: no register
/// satisfies both it and an offset-0 class.
pub static R_HIGH_CLASS: RegClassInfo = reg_class("Rhigh", "R", &[0, 1], 1, 8, true);

pub const fn r_high() -> RegClassId {
    RegClassId::new(&R_HIGH_CLASS)
}

/// `rd, rs`: one destination slot and one source slot, both of class `R`.
pub static RD_RS_PORTS: [RegPort; 2] = [
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

/// An `asm.symbol` named `f` whose body is one block holding `ops`, in that
/// order. Machine instructions have no textual form, so the machine-IR tests
/// build the function around them rather than parsing one.
pub fn asm_symbol(context: &Context, ops: &[OpId]) -> (SymbolOp, BlockHandle) {
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

pub fn register_info() -> RegisterInfo {
    RegisterInfo {
        classes: &R_CLASSES,
    }
}

/// Isel pattern-graph builders: symbols, constants and operator nodes.
pub fn symbol(g: &mut SemGraph, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

pub fn constant(g: &mut SemGraph, value: u64, width: u32) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}

pub fn nary(g: &mut SemGraph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}

pub fn binary(g: &mut SemGraph, kind: SymKind, lhs: NodeId, rhs: NodeId) -> NodeId {
    nary(g, kind, &[lhs, rhs])
}

/// A one-operator pattern over two fresh symbols.
pub fn atomic_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let lhs = symbol(&mut g, 0);
    let rhs = symbol(&mut g, 1);
    binary(&mut g, kind, lhs, rhs);
    g
}

/// A machine test opcode: the operation plus the `MachineInstruction` facts
/// (`$ports`, `$implicit`) its info reports. The longer form also declares one
/// operand slot named `$operand`.
macro_rules! machine_op {
    ($op:ident, $dialect:tt, $name:tt, $operand:ident, $ports:expr, $implicit:expr) => {
        tir::helpers::operation! {
            $op {
                name: $name,
                dialect: $dialect,
                operands: O { $operand: "?tir::backend::RegClassType", },
                results: R { regs: "*tir::backend::RegClassType" },
                interfaces: [tir::backend::MachineInstruction],
            }
        }
        $crate::core::fixtures::instr_info!($op, $name, $ports, $implicit);
    };
    ($op:ident, $dialect:tt, $name:tt, $ports:expr, $implicit:expr) => {
        tir::helpers::operation! {
            $op {
                name: $name,
                dialect: $dialect,
                results: R { regs: "*tir::backend::RegClassType" },
                interfaces: [tir::backend::MachineInstruction],
            }
        }
        $crate::core::fixtures::instr_info!($op, $name, $ports, $implicit);
    };
}
pub(crate) use machine_op;

/// A selection marker: an instruction saying only what the isel assertions
/// read, its mnemonic, over two untyped operands.
macro_rules! marker_op {
    ($op:ident, $name:tt) => {
        tir::helpers::operation! {
            $op {
                name: $name,
                dialect: "test",
                operands: O { a: "?tir::Any", b: "?tir::Any", },
                results: R { regs: "*tir::Any" },
                interfaces: [tir::backend::MachineInstruction],
            }
        }
        $crate::core::fixtures::instr_info!($op, $name, &[], &[]);
    };
}
pub(crate) use marker_op;

/// The `MachineInstruction` facts a test opcode reports: its mnemonic, the
/// register slots `$ports`, and the registers `$implicit` its behavior touches.
macro_rules! instr_info {
    ($op:ident, $name:tt, $ports:expr, $implicit:expr) => {
        impl tir::backend::MachineInstruction for $op {
            fn info(&self) -> &'static tir::backend::InstrInfo {
                static INFO: tir::backend::InstrInfo = tir::backend::InstrInfo {
                    name: $name,
                    mnemonic: $name,
                    control_flow: tir::backend::ControlFlow::None,
                    regs: $ports,
                    implicit_regs: $implicit,
                    ..tir::backend::InstrInfo::BASE
                };
                &INFO
            }

            fn instance(&self) -> &tir::OpHandle {
                &self.0
            }
        }
    };
}
pub(crate) use instr_info;
