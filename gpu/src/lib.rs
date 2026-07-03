use tir::helpers::operation;

include!(concat!(env!("OUT_DIR"), "/ptx.rs"));

mod ptx_asm;

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
        crate::isa_params(Feature::ALL)
    }

    fn register_widths(&self) -> Vec<(&'static str, u32)> {
        crate::register_widths(Feature::ALL)
    }

    fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String> {
        crate::register_name(class, index, prefer_abi)
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

    fn frame_register(&self) -> (String, u16) {
        ("RD".to_string(), 15)
    }

    fn emit_spill_store(
        &self,
        _context: &tir::Context,
        _value: u32,
        _class: &str,
        _frame: &(String, u16),
        _offset: i64,
    ) -> Box<dyn tir::Operation> {
        panic!("PTX has no stack; register spilling is unsupported")
    }

    fn emit_spill_reload(
        &self,
        _context: &tir::Context,
        _value: u32,
        _class: &str,
        _frame: &(String, u16),
        _offset: i64,
    ) -> Box<dyn tir::Operation> {
        panic!("PTX has no stack; register spilling is unsupported")
    }

    fn emit_prologue(&self, _context: &tir::Context, _size: u32) -> Vec<Box<dyn tir::Operation>> {
        Vec::new()
    }

    fn emit_epilogue(&self, _context: &tir::Context, _size: u32) -> Vec<Box<dyn tir::Operation>> {
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

#[cfg(test)]
mod tests {
    #[test]
    fn all_instructions_have_unmodeled_semantics() {
        // Every PTX instruction uses `todo()`, so none generate a selection rule.
        let context = tir::Context::with_default_dialects();
        assert!(
            crate::get_isel_rules(&context, crate::Feature::ALL).is_empty(),
            "PTX instructions are text-only (todo semantics); no isel rules expected"
        );
    }

    #[test]
    fn asm_syntax_table_covers_compute_families() {
        let mnemonics: Vec<&str> = crate::asm_syntax().iter().map(|s| s.mnemonic).collect();
        // One representative from every §9.7 compute family the target models.
        for m in [
            "add.s64",        // integer arithmetic
            "mad.lo.s32",     // multiply-add
            "mul.wide.s32",   // widening multiply
            "and.b32",        // logic
            "shl.b32",        // shift
            "lop3.b32",       // 3-input logic
            "fma.rn.f32",     // fused multiply-add
            "div.approx.f32", // float division
            "sqrt.rn.f64",    // transcendental
            "setp.ge.s32",    // compare-set-predicate
            "selp.f32",       // select
            "mov.u32",        // move
            "cvt.rn.f32.s32", // conversion
            "cvta.to.global.u64",
            "ld.global.f32", // load
            "st.shared.f32", // store
            "atom.global.add.f32",
            "red.global.add.u32",
            "shfl.sync.down.b32",
            "bar.sync",  // barrier
            "membar.gl", // memory fence
            "vote.sync.all.pred",
            "bra", // control flow
            "ret",
            // §9.7.8/§9.7.13/§9.7.14 families:
            "cp.async.cg.shared.global",                  // async copy
            "wmma.load.a.sync.aligned.m16n16k16.row.f16", // tensor-core
            "mma.sync.aligned.m16n8k16.row.col.f32.f16.f16.f32",
            "tex.2d.v4.f32.f32", // textures
            "sust.b.2d.v4.b32.trap",
            "suq.width.b32",    // surface query
            "vadd.s32.s32.s32", // video SIMD
            "vadd2.u32.u32.u32",
        ] {
            assert!(mnemonics.contains(&m), "missing syntax for {m}");
        }
        // The generated set is exhaustive over the enumerated families.
        assert!(
            crate::asm_syntax().len() > 2000,
            "expected a comprehensive instruction set, got {}",
            crate::asm_syntax().len()
        );
    }
}
