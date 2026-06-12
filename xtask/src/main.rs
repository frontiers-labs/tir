pub mod utils;
mod verify_smt;

use std::{env, path::PathBuf};
use xshell::{cmd, Shell};

fn main() -> anyhow::Result<()> {
    let task = env::args().nth(1);
    let sh = Shell::new()?;
    match task.as_deref() {
        Some("help") => print_help(),
        Some("build") => build(&sh)?,
        Some("check") => {
            build(&sh)?;
            check(&sh)?
        }
        Some("check-only") => check(&sh)?,
        Some("docs") => build_docs(&sh)?,
        Some("verify") => {
            let isa = env::args().nth(2);
            match isa.as_deref() {
                Some(isa) => verify_smt::verify_smt(&sh, isa)?,
                _ => print_help(),
            }
        }
        Some("isa-test-suite") => isa_test_suite(&sh)?,
        _ => print_help(),
    }
    Ok(())
}

fn build(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(root);

    cmd!(sh, "cargo build").run()?;

    Ok(())
}

fn check(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(root);

    // FileCheck-style tests now run as ordinary integration tests (the `lit`
    // harnesses in each crate's `tests/` directory), so running the test suite
    // exercises them alongside the unit tests.
    cmd!(sh, "cargo test --workspace").run()?;

    Ok(())
}

fn build_docs(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(&root);

    cmd!(sh, "cargo doc --no-deps").run()?;

    let api_dest = root.join("docs/api");
    if std::fs::read_dir(&api_dest).is_ok() {
        std::fs::remove_dir_all(&api_dest)?;
    }

    let api_src = root.join("target/doc");
    std::fs::rename(api_src, api_dest)?;

    cmd!(sh, "mdbook build").run()?;

    Ok(())
}

/// Run the differential ISA test suite: build the `tir-isasim` binary (the
/// simulator under test), then compare each snippet's architectural state
/// against a golden reference model (Spike for RISC-V).
fn isa_test_suite(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(&root);

    cmd!(sh, "cargo build -p tir-isasim").run()?;
    let isasim_bin = root.join("target/debug/tir-isasim");

    let all_passed = tir_isa_test_suite::run(&isasim_bin)?;
    if !all_passed {
        anyhow::bail!("ISA test suite reported failures");
    }
    Ok(())
}

fn print_help() {
    eprintln!(
        "Tasks:

build            builds TIR project
check            builds project and runs check tests
check-only       only runs check tests without building the project
verify <isa>     run formal ISA verification. Available ISAs: riscv64, riscv32, armv8
isa-test-suite   run differential ISA tests against a golden oracle (riscv/Spike)
docs             builds project documentation
help             shows this message
"
    )
}

fn project_root() -> PathBuf {
    let dir =
        env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
    PathBuf::from(dir).parent().unwrap().to_owned()
}
