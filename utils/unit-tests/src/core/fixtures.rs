//! Fixtures shared by the core test modules.

use tir::attributes::AttributeValue;
use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterView};
use tir::parse::ir::parse_operation;
// Migration skip: selector-only test support.
#[cfg(any())]
use tir::sem::{SemGraph, SymKind, SymPayload};
// Migration skip: selector-only test support.
#[cfg(any())]
use tir::NodeId;
use tir::{BlockHandle, Context, OpId, Operation, RegionId};
// Migration skip: selector-only test support.
#[cfg(any())]
use tir_adt::APInt;

/// Parse `source` as a module into a fresh context holding the default dialects.
pub fn parse(source: &str) -> (Context, tir::OpHandle) {
    let context = Context::with_default_dialects();
    let module = parse_in(&context, source);
    (context, module)
}

/// Parse `source` as a module into `context`, which the caller has already
/// taught whatever dialects the source names.
pub fn parse_in(context: &Context, source: &str) -> tir::OpHandle {
    parse_operation(context, source, &tir::builtin::MODULE).expect("the fixture parses")
}

/// Parse a `module { func.func @… }` source, handing back its one function and
/// the body region a pass leaves its result in.
pub fn parse_function(source: &str) -> (Context, tir::OpHandle, tir::OpHandle, RegionId) {
    let (context, module) = parse(source);
    let func = module_ops(&context, module.id())
        .into_iter()
        .find_map(|op| context.get_op(op).as_operation(&tir::func::FUNC))
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

/// build the function around them rather than parsing one.
pub fn asm_symbol(context: &Context, ops: &[OpId]) -> (tir::OpHandle, BlockHandle) {
    let block = context.create_block(vec![]);
    for &op in ops {
        block.context.insert(block.end(), op);
    }
    let region = context.create_region();
    region.add_block(block.id());
    let symbol = tir::backend::symbol(
        context,
        Some(region.id()),
        tir::NewOpParts {
            attributes: vec![context.named_attribute("name", AttributeValue::Str("f".into()))],
        },
        tir::InsertionPoint::Root,
    );
    (symbol, block)
}

/// Isel pattern-graph builders: symbols, constants and operator nodes.
// Migration skip: selector-only test support.
#[cfg(any())]
pub fn symbol(g: &mut SemGraph, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

// Migration skip: selector-only test support.
#[cfg(any())]
pub fn constant(g: &mut SemGraph, value: u64, width: u32) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(u64::from(width), value)));
    node
}

// Migration skip: selector-only test support.
#[cfg(any())]
pub fn nary(g: &mut SemGraph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}

// Migration skip: selector-only test support.
#[cfg(any())]
pub fn binary(g: &mut SemGraph, kind: SymKind, lhs: NodeId, rhs: NodeId) -> NodeId {
    nary(g, kind, &[lhs, rhs])
}

/// A one-operator pattern over two fresh symbols.
// Migration skip: selector-only test support.
#[cfg(any())]
pub fn atomic_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let lhs = symbol(&mut g, 0);
    let rhs = symbol(&mut g, 1);
    binary(&mut g, kind, lhs, rhs);
    g
}

macro_rules! instr_info {
    ($name:tt, $implicit:expr) => {
        tir::backend::MachineInstructionCallbacks {
            info: |_| {
                static INFO: tir::backend::InstrInfo = tir::backend::InstrInfo {
                    name: $name,
                    mnemonic: $name,
                    control_flow: tir::backend::ControlFlow::None,
                    implicit_regs: $implicit,
                    ..tir::backend::InstrInfo::BASE
                };
                &INFO
            },
            instance: |operation| operation,
            sched_on: None,
            mnemonic: None,
            width_bytes: None,
            execute: None,
            control_flow: None,
        }
    };
}
pub(crate) use instr_info;
