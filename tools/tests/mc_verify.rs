use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn mc_rejects_undefined_value_without_panicking() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tir"))
        .args(["mc", "--march=rv64i", "--stage=isel", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tir");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"module { func @f() -> !i64 { return %99 } module_end }")
        .expect("write input");

    let output = child.wait_with_output().expect("wait for tir");
    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("verification failed"), "{stderr}");
    assert!(stderr.contains("references unknown value %99"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}
