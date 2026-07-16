//! End-to-end linking: compile a libc-free program with `fcc`, link it via the
//! system `cc`, run it, and check its exit status. LIT cannot express this
//! (no `%t`, no way to execute a produced file), so it lives here. Gated to
//! supported host backends and skipped when `cc` is unavailable.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const FCC: &str = env!("CARGO_BIN_EXE_fcc");
const SOURCE: &str = "int main(void) { return 42; }\n";

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_fcc(dir: &Path, args: &[&str]) {
    let status = Command::new(FCC)
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn fcc");
    assert!(status.success(), "fcc {args:?} failed");
}

fn run_program(dir: &Path, program: &str) -> Output {
    Command::new(dir.join(program))
        .output()
        .expect("run linked program")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("program exited via signal")
}

fn compile_fcc(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("test.c"), source).unwrap();
    run_fcc(dir, &["cc", "test.c", "-o", output]);
}

fn compile_host(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("host.c"), source).unwrap();
    let status = Command::new("cc")
        .args(["host.c", "-o", output])
        .current_dir(dir)
        .status()
        .expect("spawn host cc");
    assert!(status.success(), "host cc failed");
}

fn assert_fcc_matches_host(source: &str) {
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "fcc-program");
    compile_host(dir.path(), source, "host-program");
    let fcc = run_program(dir.path(), "fcc-program");
    let host = run_program(dir.path(), "host-program");
    assert_eq!(exit_code(&fcc), exit_code(&host));
    assert_eq!(fcc.stdout, host.stdout);
    assert_eq!(fcc.stderr, host.stderr);
}

fn compile_host_object(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("host.c"), source).unwrap();
    let status = Command::new("cc")
        .args(["-c", "host.c", "-o", output])
        .current_dir(dir)
        .status()
        .expect("spawn host cc");
    assert!(status.success(), "host cc failed");
}

#[test]
fn compile_and_link_in_one_step() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("r.c"), SOURCE).unwrap();
    run_fcc(dir.path(), &["cc", "r.c", "-o", "r"]);
    assert_eq!(exit_code(&run_program(dir.path(), "r")), 42);
}

#[test]
fn separate_compile_then_link() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "output");
    let output = run_program(dir.path(), "output");
    assert_eq!(exit_code(&output), 0);
    assert_eq!(output.stdout, b"fcc output\n");
}

#[test]
fn compares_program_with_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int puts(const char *text);
int main(void) { puts("same output"); return 17; }
"#,
    );
}

#[test]
fn loops_execute_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"int loop_break(int n) {
    for (;;) { if (n) break; return 4; }
    return 7;
}
int main(void) {
    if (loop_break(0) != 4) return 1;
    if (loop_break(1) != 7) return 2;
    return 0;
}
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "loops");
    assert_eq!(exit_code(&run_program(dir.path(), "loops")), 0);
}

#[test]
fn struct_fields_execute_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"struct Pair { char tag; int value; };
int read(void) { struct Pair pair; pair.value = 42; return pair.value; }
int main(void) { if (read() == 42) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "struct-fields");
    assert_eq!(exit_code(&run_program(dir.path(), "struct-fields")), 0);
}

#[test]
fn pointer_member_access_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("read.c"),
        "struct Pair { char tag; int value; }; int read(struct Pair *pair) { return pair->value; }\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-c", "read.c", "-o", "read.o"]);
    compile_host_object(
        dir.path(),
        "struct Pair { char tag; int value; }; int read(struct Pair *); int main(void) { struct Pair pair = { 1, 73 }; return read(&pair) == 73 ? 0 : 1; }\n",
        "host.o",
    );
    run_fcc(
        dir.path(),
        &["cc", "read.o", "host.o", "-o", "pointer-member"],
    );
    assert_eq!(exit_code(&run_program(dir.path(), "pointer-member")), 0);
}

#[test]
fn whole_struct_copy_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"struct Pair { char tag; int value; };
int copy(void) {
    struct Pair source;
    struct Pair destination;
    source.tag = 3;
    source.value = 91;
    destination = source;
    return destination.tag + destination.value;
}
int main(void) { if (copy() == 94) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "struct-copy");
    assert_eq!(exit_code(&run_program(dir.path(), "struct-copy")), 0);
}

#[test]
fn anonymous_struct_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"typedef struct { int value; } Pair;
int read(void) { Pair pair; pair.value = 29; return pair.value; }
int main(void) { if (read() == 29) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "anonymous-struct");
    assert_eq!(exit_code(&run_program(dir.path(), "anonymous-struct")), 0);
}

#[test]
fn sizeof_struct_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("size.c"),
        "struct Pair { char tag; int value; }; int size(void) { return sizeof(struct Pair); }\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-c", "size.c", "-o", "size.o"]);
    compile_host_object(
        dir.path(),
        "int size(void); int main(void) { return size() == 8 ? 0 : 1; }\n",
        "host.o",
    );
    run_fcc(
        dir.path(),
        &["cc", "size.o", "host.o", "-o", "sizeof-struct"],
    );
    assert_eq!(exit_code(&run_program(dir.path(), "sizeof-struct")), 0);
}

#[test]
fn nested_struct_member_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"struct Inner { int value; };
struct Outer { char tag; struct Inner inner; };
int read(void) { struct Outer outer; outer.inner.value = 61; return outer.inner.value; }
int main(void) { if (read() == 61) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "nested-struct");
    assert_eq!(exit_code(&run_program(dir.path(), "nested-struct")), 0);
}
