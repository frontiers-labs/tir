//! Encoding tests: assemble an instruction and check the generated machine-code
//! bytes. Only the operand-free and immediate-only encodings are fully
//! expressible in TMDL; the register forms encode the eight base GPRs with a
//! fixed REX.W prefix (see BLOCKERS.md).

use tir::Context;
use tir_be_common::{AsmDialect, SectionOp, SymbolOp};
use tir_x86_64::{X86_64Dialect, instruction_encoders};

/// Assemble `body` and return the concatenated machine-code bytes of its
/// instructions, in program order.
fn assemble(body: &str) -> Vec<u8> {
    let context = Context::with_default_dialects();
    context.register_dialect::<AsmDialect>();
    context.register_dialect::<X86_64Dialect>();
    let dialect = context
        .find_dialect::<X86_64Dialect>()
        .expect("x86_64 dialect registered");

    let src = format!(".global f\nf:\n{body}\n");
    let module = dialect
        .get_asm_parser()
        .parse_asm(&context, &src)
        .expect("assembly parses");

    let encoders = instruction_encoders();
    let mut out = Vec::new();
    // module body -> section -> symbol -> instructions
    for sec_id in module.body().op_ids() {
        let sec = context.get_op(sec_id);
        let Some(section) = sec.as_op::<SectionOp>() else {
            continue;
        };
        for sym_id in section.body().op_ids() {
            let sym = context.get_op(sym_id);
            let Some(symbol) = sym.as_op::<SymbolOp>() else {
                continue;
            };
            for op_id in symbol.body().op_ids() {
                let op = context.get_op(op_id);
                if let Some(encoder) = encoders.get(op.name) {
                    out.extend(encoder(&op).expect("instruction encodes").bytes);
                }
            }
        }
    }
    out
}

#[test]
fn control_flow_encodings() {
    assert_eq!(assemble("ret"), vec![0xC3]);
    // jmp rel32 = E9 + little-endian displacement.
    assert_eq!(assemble("jmp 16"), vec![0xE9, 0x10, 0x00, 0x00, 0x00]);
    // call rel32 = E8 + displacement.
    assert_eq!(assemble("call 256"), vec![0xE8, 0x00, 0x01, 0x00, 0x00]);
}

#[test]
fn register_register_encodings() {
    // add rax, rbx: REX.W=48, opcode 01, ModR/M=11 reg(rbx=3) rm(rax=0) = 0xD8.
    assert_eq!(assemble("add rax, rbx"), vec![0x48, 0x01, 0xD8]);
    // mov rax, rbx: REX.W=48, opcode 89, ModR/M=11 011 000 = 0xD8.
    assert_eq!(assemble("mov rax, rbx"), vec![0x48, 0x89, 0xD8]);
    // sub rsi, rdi: opcode 29, ModR/M=11 reg(rdi=7) rm(rsi=6) = 0xFE.
    assert_eq!(assemble("sub rsi, rdi"), vec![0x48, 0x29, 0xFE]);
}

#[test]
fn register_immediate_encodings() {
    // mov rax, 1: REX.W=48, opcode C7, ModR/M=11 000 000 = 0xC0, imm32=1.
    assert_eq!(
        assemble("mov rax, 1"),
        vec![0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]
    );
    // add rcx, 42: REX.W=48, opcode 81, ModR/M=11 000(EXT /0) 001(rcx) = 0xC1.
    assert_eq!(
        assemble("add rcx, 42"),
        vec![0x48, 0x81, 0xC1, 0x2A, 0x00, 0x00, 0x00]
    );
}

#[test]
fn shift_immediate_encoding() {
    // shl rax, 3: REX.W=48, opcode C1, ModR/M=11 100(EXT /4) 000(rax) = 0xE0, imm8=3.
    assert_eq!(assemble("shl rax, 3"), vec![0x48, 0xC1, 0xE0, 0x03]);
}
