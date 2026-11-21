//! Fixtures shared by the core test modules.

use tir::attributes::AttributeValue;
use tir::backend::regalloc::{RegClassId, RegClassInfo, RegisterView};
use tir::backend::{SymbolOp, SymbolOpBuilder};
use tir::builtin::ModuleOp;
use tir::func::FuncOp;
use tir::parse::ir::parse_ir;
use tir::{BlockHandle, Context, OpId, Operation, RegionId};
#[allow(dead_code)]
pub mod ops {
    include!(concat!(env!("OUT_DIR"), "/core_fixtures.rs"));

    __cdsl_dep_DefOp!();
    __cdsl_dep_UpdateParamOp!();
    __cdsl_dep_SetFlagsOp!();
    __cdsl_dep_ReadFlagsOp!();
    __cdsl_test_AddTestOp!();
    __cdsl_test_ExitOp!();
    __cdsl_test_BeqDecodeOp!();
    __cdsl_test_CmvDecodeOp!();
    __cdsl_test_NarrowDecodeOp!();
}

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

/// An `asm.symbol` named `f` whose body is one block holding `ops`, in that
/// order. Machine instructions have no textual form, so the machine-IR tests
/// build the function around them rather than parsing one.
pub fn asm_symbol(context: &Context, ops: &[OpId]) -> (SymbolOp, BlockHandle) {
    let block = context.create_block(vec![]);
    for &op in ops {
        context.insert(block.end(), op);
    }
    let region = context.create_region();
    region.add_block(block.id());
    let symbol = SymbolOpBuilder::new(context)
        .body(region.id())
        .attr("name", AttributeValue::Str("f".into()))
        .build();
    (symbol, block)
}

/// The `MachineInstruction` facts a test opcode reports: its mnemonic, the
/// registers `$implicit` its behavior touches.
macro_rules! instr_info {
    ($op:ident, $name:tt, $implicit:expr) => {
        impl tir::backend::MachineInstruction for $op {
            fn info(&self) -> &'static tir::backend::InstrInfo {
                static INFO: tir::backend::InstrInfo = tir::backend::InstrInfo {
                    name: $name,
                    mnemonic: $name,
                    control_flow: tir::backend::ControlFlow::None,
                    implicit_regs: $implicit,
                    ..tir::backend::InstrInfo::BASE
                };
                &INFO
            }

            fn instance(&self) -> &tir::OpHandle {
                tir::Operation::handle(self)
            }
        }
    };
}
pub(crate) use instr_info;
