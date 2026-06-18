pub mod utils;
mod verify_btor2;
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
        Some("verify-btor2") => {
            let isa = env::args().nth(2);
            let impl_btor2 = env::args().nth(3);
            match (isa.as_deref(), impl_btor2.as_deref()) {
                (Some(isa), Some(path)) => verify_btor2::run(&sh, isa, std::path::Path::new(path))?,
                _ => print_help(),
            }
        }
        Some("isa-test-suite") => isa_test_suite(&sh)?,
        Some("capi-smoke") => capi_smoke(&sh)?,
        Some("python-smoke") => python_smoke(&sh)?,
        Some("haskell-smoke") => haskell_smoke(&sh)?,
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

/// Build the C ABI (whose build script regenerates `tir.h`), then compile and
/// run the C smoke test against the cdylib.
fn capi_smoke(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(&root);

    cmd!(sh, "cargo build -p tir-capi").run()?;

    let lib_dir = root.join("target/debug");
    let smoke = root.join("utils/capi/tests/smoke.c");
    let bin = lib_dir.join("tir_capi_smoke");
    let rpath = format!("-Wl,-rpath,{}", lib_dir.display());
    cmd!(
        sh,
        "cc {smoke} -I utils/capi/include -L {lib_dir} -ltir_capi {rpath} -o {bin}"
    )
    .run()?;
    cmd!(sh, "{bin}").run()?;

    Ok(())
}

/// Build the C ABI cdylib, then run the Python test suite against it. The
/// Python bindings build their typed op constructors from the schema at import,
/// so there is nothing to regenerate.
fn python_smoke(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(&root);

    cmd!(sh, "cargo build -p tir-capi").run()?;
    cmd!(sh, "python3 -m unittest discover -s utils/python/tests").run()?;
    Ok(())
}

/// Build the C ABI cdylib, then compile and run the Haskell bindings smoke test
/// against it. Requires `ghc` on PATH.
fn haskell_smoke(sh: &Shell) -> anyhow::Result<()> {
    let root = project_root();
    sh.change_dir(&root);

    cmd!(sh, "cargo build -p tir-capi").run()?;

    let lib_dir = root.join("target/debug");
    let out = root.join("target/haskell");
    std::fs::create_dir_all(&out)?;
    let bin = out.join("tir_hs_smoke");
    let lib_flag = format!("-L{}", lib_dir.display());
    let rpath = format!("-optl-Wl,-rpath,{}", lib_dir.display());
    cmd!(
        sh,
        "ghc -O0 -outputdir {out} -iutils/haskell/src utils/haskell/test/Main.hs
         {lib_flag} -ltir_capi {rpath} -o {bin}"
    )
    .run()?;
    cmd!(sh, "{bin}").run()?;
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
capi-smoke       check the C ABI header is current and run the C smoke test
python-smoke     build the C ABI and run the Python test suite
haskell-smoke    build the C ABI and run the Haskell bindings smoke test (needs ghc)
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
