use tir::helpers::dialect;

mod ptx_asm;

include!(concat!(env!("OUT_DIR"), "/ptx.rs"));

dialect! {
    PtxDialect {
        name: "ptx",
        operations: [LABEL],
        operation_file: concat!(env!("OUT_DIR"), "/ptx_ops.rs"),
    }
}

tir::helpers::op_def! {
    file: "../core/defs/converted-gpu/ptx_mod.cdsl",
    hosts: {

    },
}

/// The PTX target, selected via `--march=ptx`. PTX is a text pseudo-ISA: the
/// front-end in `ptx_asm` parses `.ptx` kernels into a module and prints them
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

    fn isel_pass(&self, _context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
        tir::backend::isel::InstructionSelectPass::new()
    }

    fn data_layout(&self, context: &tir::Context) -> Option<tir::attributes::AttributeValue> {
        Some(tir::data_layout_spec(
            context,
            tir::Endianness::Little,
            // PTX has no ABI stack: locals live in `.local` space, and `.param`
            // space is 8-byte aligned.
            64,
            &[
                ("i1", 8, 8),
                ("i8", 8, 8),
                ("i16", 16, 16),
                ("i32", 32, 32),
                ("i64", 64, 64),
                ("f32", 32, 32),
                ("f64", 64, 64),
                ("p", 64, 64),
            ],
        ))
    }

    fn target_env(&self) -> Option<tir::attributes::AttributeValue> {
        let features: Vec<String> = Feature::ALL
            .iter()
            .map(|feature| feature.name().to_ascii_lowercase())
            .collect();
        Some(tir::target_env_spec(self.name(), &features))
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        register_info()
    }

    fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
        let (parsers, disabled) = get_instruction_parsers(Feature::ALL);
        tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
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
    ) -> Option<Result<tir::OpHandle, String>> {
        Some(ptx_asm::parse(context, text))
    }

    fn print_asm_text(
        &self,
        context: &tir::Context,
        module: &tir::OpHandle,
    ) -> Option<Result<String, String>> {
        Some(ptx_asm::print(context, module))
    }
}

fn select_ptx(
    march: &str,
    _mcpu: Option<&str>,
    _mattr: Option<&str>,
    _mabi: Option<&str>,
) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
    if !march.trim().eq_ignore_ascii_case("ptx") {
        return Ok(None);
    }
    Ok(Some(Box::new(PtxTarget)))
}

tir::register_target!(select_ptx, ["ptx"]);
