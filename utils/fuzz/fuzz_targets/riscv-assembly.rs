#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 16 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    if let Ok(input) = std::str::from_utf8(data) {
        let context = tir::Context::with_default_dialects();
        context.register_dialect::<tir_be_common::AsmDialect>();
        context.register_dialect::<tir_riscv::RiscvDialect>();

        let rv = context.find_dialect::<tir_riscv::RiscvDialect>().unwrap();
        let parser = rv.get_asm_parser();

        let Ok(module) = parser.parse_asm(&context, input) else {
            return;
        };
        let _ = tir::Operation::verify(&module, &context);
    }
});
