//! Assembly parse -> print roundtrip tests.

use tir::Context;
use tir_be_common::AsmDialect;
use tir_x86_64::X86_64Dialect;

/// Parse `body` (the instructions of a single function), print the module back
/// to assembly, and return the printed instruction lines (leading tab stripped).
fn roundtrip(body: &str) -> Vec<String> {
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
    let printed = dialect
        .get_asm_printer()
        .print_module(&context, &module)
        .expect("module prints");

    printed
        .lines()
        .filter(|l| l.starts_with('\t'))
        .map(|l| l.trim().to_string())
        .collect()
}

fn check(line: &str) {
    assert_eq!(roundtrip(line), vec![line.to_string()]);
}

#[test]
fn register_register_alu_roundtrips() {
    check("add rax, rbx");
    check("sub rcx, rdx");
    check("and rsi, rdi");
    check("or rbp, rsp");
    check("xor rax, rax");
    check("mov rbx, rcx");
}

#[test]
fn register_immediate_alu_roundtrips() {
    check("add rax, 42");
    check("and rcx, 255");
    check("or rdx, 1");
    check("xor rsi, 7");
    check("mov rdi, 100");
}

#[test]
fn shift_immediate_roundtrips() {
    check("shl rax, 3");
    check("shr rbx, 1");
    check("sar rcx, 63");
}

#[test]
fn control_flow_roundtrips() {
    check("jmp 16");
    check("call 256");
    check("ret");
}

#[test]
fn negative_immediate_roundtrips() {
    check("add rax, -8");
}

#[test]
fn shared_mnemonic_picks_register_form() {
    // `rbx` must select the register form, not be misread as a symbol immediate.
    assert_eq!(roundtrip("add rax, rbx"), vec!["add rax, rbx".to_string()]);
    assert_eq!(roundtrip("add rax, 9"), vec!["add rax, 9".to_string()]);
}

#[test]
fn multiple_instructions_roundtrip() {
    let out = roundtrip("mov rax, 1\n    add rax, rbx\n    ret");
    assert_eq!(
        out,
        vec![
            "mov rax, 1".to_string(),
            "add rax, rbx".to_string(),
            "ret".to_string(),
        ]
    );
}
