#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

//! End-to-end driver tests: `fcc cc` compiles, links via the system `cc`, and
//! the produced binaries run. Skipped when `cc` is unavailable.

use std::fs;

use super::link_support::{cc_available, compile_fcc, exit_code, run_fcc, run_program};

const SOURCE: &str = "int main(void) { return 42; }\n";

#[test]
fn compile_and_link_in_one_step() {
    if !cc_available() {
        return;
    }
    let dir = tir_adt::TempDir::new().unwrap();
    std::fs::write(dir.path().join("r.c"), SOURCE).unwrap();
    run_fcc(dir.path(), &["cc", "r.c", "-o", "r"]);
    assert_eq!(exit_code(&run_program(dir.path(), "r")), 42);
}

#[test]
fn system_headers_compile_across_translation_units() {
    if !cc_available() {
        return;
    }
    let dir = tir_adt::TempDir::new().unwrap();
    fs::write(
        dir.path().join("helper.c"),
        "#include <stdio.h>\nint helper(void) { return printf(\"fcc\"); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.c"),
        "#include <stdio.h>\nint helper(void); int main(void) { return helper() != 3; }\n",
    )
    .unwrap();
    run_fcc(
        dir.path(),
        &["cc", "-O2", "helper.c", "main.c", "-o", "program"],
    );
    assert_eq!(exit_code(&run_program(dir.path(), "program")), 0);
}

#[test]
fn separate_compile_then_link() {
    if !cc_available() {
        return;
    }
    let dir = tir_adt::TempDir::new().unwrap();
    std::fs::write(dir.path().join("r.c"), SOURCE).unwrap();
    run_fcc(dir.path(), &["cc", "-c", "r.c"]);
    assert!(dir.path().join("r.o").exists(), "r.o was not produced");
    run_fcc(dir.path(), &["cc", "r.o", "-o", "r2"]);
    assert_eq!(exit_code(&run_program(dir.path(), "r2")), 42);
}

#[test]
fn captures_program_output() {
    if !cc_available() {
        return;
    }
    let source = r#"int puts(const char *text);
int main(void) { puts("fcc output"); return 0; }
"#;
    let dir = tir_adt::TempDir::new().unwrap();
    compile_fcc(dir.path(), source, "output");
    let output = run_program(dir.path(), "output");
    assert_eq!(exit_code(&output), 0);
    assert_eq!(output.stdout, b"fcc output\n");
}
