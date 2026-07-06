use tir::helpers::operation;

mod ptx_asm;

include!(concat!(env!("OUT_DIR"), "/ptx.rs"));

/// The PTX target, selected via `--march=ptx`. PTX is a text pseudo-ISA: the
/// front-end in [`ptx_asm`] parses `.ptx` kernels into a module and prints them
/// back, so this target implements the `parse_asm_text`/`print_asm_text` hooks
/// rather than the shared flat assembler.
pub struct PtxTarget;

impl tir::backend::TargetMachine for PtxTarget {
    fn name(&self) -> &'static str {
        "ptx"
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir::backend::AsmDialect>();
        context.register_dialect::<PtxDialect>();
    }

    fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
        tir::backend::isel::InstructionSelectPass::new(get_isel_rules(context, Feature::ALL))
    }

    fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
        tir::backend::regalloc::RegisterAllocationPass::new(Box::new(PtxRegAlloc))
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        register_info()
    }

    fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
        let (parsers, disabled) = get_instruction_parsers(Feature::ALL);
        tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
    }

    fn asm_printer(&self, _context: &tir::Context) -> tir::backend::AsmPrinter {
        tir::backend::AsmPrinter::new(get_instruction_printers())
    }

    fn machine_model(&self, name: &str) -> Option<tir::backend::sched::MachineModel> {
        machine_model(name, Feature::ALL)
    }

    fn machines(&self) -> Vec<&'static str> {
        machines(Feature::ALL)
    }

    fn isa_params(&self) -> Vec<(&'static str, i64)> {
        crate::ptx::isa_params(Feature::ALL)
    }

    fn register_widths(&self) -> Vec<(&'static str, u32)> {
        crate::ptx::register_widths(Feature::ALL)
    }

    fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String> {
        crate::ptx::register_name(class, index, prefer_abi)
    }

    fn parse_asm_text(
        &self,
        context: &tir::Context,
        text: &str,
    ) -> Option<Result<tir::builtin::ModuleOp, String>> {
        Some(ptx_asm::parse(context, text))
    }

    fn print_asm_text(
        &self,
        context: &tir::Context,
        module: &tir::builtin::ModuleOp,
    ) -> Option<Result<String, String>> {
        Some(ptx_asm::print(context, module))
    }
}

/// PTX has no stack, so register spilling is unsupported; the round-trip and
/// isel-demo paths never spill.
struct PtxRegAlloc;

impl tir::backend::regalloc::TargetRegAlloc for PtxRegAlloc {
    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        register_info()
    }

    fn frame_register(&self) -> tir::backend::liveness::PhysReg {
        (RegClass::RD.id(), 15)
    }

    fn emit_spill_store(
        &self,
        _context: &tir::Context,
        _value: u32,
        _class: tir::backend::regalloc::RegClassId,
        _frame: &tir::backend::liveness::PhysReg,
        _offset: i64,
    ) -> Box<dyn tir::Operation> {
        panic!("PTX has no stack; register spilling is unsupported")
    }

    fn emit_spill_reload(
        &self,
        _context: &tir::Context,
        _value: u32,
        _class: tir::backend::regalloc::RegClassId,
        _frame: &tir::backend::liveness::PhysReg,
        _offset: i64,
    ) -> Box<dyn tir::Operation> {
        panic!("PTX has no stack; register spilling is unsupported")
    }

    fn emit_prologue(
        &self,
        _context: &tir::Context,
        _size: u32,
        _saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn tir::Operation>> {
        Vec::new()
    }

    fn emit_epilogue(
        &self,
        _context: &tir::Context,
        _size: u32,
        _saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn tir::Operation>> {
        Vec::new()
    }
}

fn select_ptx(
    march: &str,
    _mcpu: Option<&str>,
    _mattr: Option<&str>,
) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
    if !march.trim().eq_ignore_ascii_case("ptx") {
        return Ok(None);
    }
    Ok(Some(Box::new(PtxTarget)))
}

tir::register_target!(select_ptx, ["ptx"]);
