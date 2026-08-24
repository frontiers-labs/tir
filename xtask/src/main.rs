mod fcc_bench;
mod fcc_corpus;
mod fcc_fuzz;
mod fcc_torture;
pub mod utils;
mod verify_smt;

use std::{env, path::PathBuf};
use tmdl::{Action, Compiler, OutputKind};
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
                Some(isa) => verify_smt::verify_smt(&sh, isa, env::args().skip(3))?,
                _ => print_help(),
            }
        }
        Some("isa-test-suite") => isa_test_suite(&sh)?,
        Some("fcc-torture") => {
            let options = fcc_torture_options(env::args().skip(2))?;
            fcc_torture::run(&sh, &project_root(), options.bless, options.fcc.as_deref())?;
        }
        Some("fcc-bench") => {
            let options = fcc_bench_options(env::args().skip(2))?;
            fcc_bench::run(&sh, &project_root(), options)?;
        }
        Some("fcc-corpus") => {
            fcc_corpus::run(&sh, &project_root(), fcc_corpus_mode(env::args().skip(2))?)?;
        }
        Some("fcc-fuzz") => {
            fcc_fuzz::run(
                &sh,
                &project_root(),
                &fcc_fuzz_options(env::args().skip(2))?,
            )?;
        }
        Some("capi-smoke") => capi_smoke(&sh)?,
        Some("python-smoke") => python_smoke(&sh)?,
        Some("haskell-smoke") => haskell_smoke(&sh)?,
        _ => print_help(),
    }
    Ok(())
}

struct TortureOptions {
    bless: bool,
    fcc: Option<PathBuf>,
}

fn fcc_torture_options(mut args: impl Iterator<Item = String>) -> anyhow::Result<TortureOptions> {
    let mut options = TortureOptions {
        bless: false,
        fcc: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bless" => options.bless = true,
            "--fcc" => options.fcc = Some(PathBuf::from(take_value(&mut args, "--fcc")?)),
            other => anyhow::bail!("unknown fcc-torture flag: {other}"),
        }
    }
    Ok(options)
}

fn take_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}

fn fcc_bench_options(mut args: impl Iterator<Item = String>) -> anyhow::Result<fcc_bench::Options> {
    let mut options = fcc_bench::Options {
        fcc: None,
        output: None,
        baseline: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fcc" => options.fcc = Some(PathBuf::from(take_value(&mut args, "--fcc")?)),
            "--output" => options.output = Some(PathBuf::from(take_value(&mut args, "--output")?)),
            "--baseline" => {
                options.baseline = Some(PathBuf::from(take_value(&mut args, "--baseline")?))
            }
            other => anyhow::bail!("unknown fcc-bench flag: {other}"),
        }
    }
    Ok(options)
}

fn fcc_corpus_mode(mut args: impl Iterator<Item = String>) -> anyhow::Result<fcc_corpus::Mode> {
    let Some(flag) = args.next() else {
        return Ok(fcc_corpus::Mode::Report);
    };
    let mut directory = || {
        args.next()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a directory"))
    };
    match flag.as_str() {
        "--baseline" => Ok(fcc_corpus::Mode::Baseline(directory()?)),
        "--diff" => Ok(fcc_corpus::Mode::Diff(directory()?)),
        "--determinism" => Ok(fcc_corpus::Mode::Determinism),
        other => anyhow::bail!("unknown fcc-corpus flag: {other}"),
    }
}

fn fcc_fuzz_options(mut args: impl Iterator<Item = String>) -> anyhow::Result<fcc_fuzz::Options> {
    let mut options = fcc_fuzz::Options {
        seed: 0,
        iterations: 100,
        corpus: false,
        self_test: false,
        replay: None,
        render: None,
        extract: None,
        fcc: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => options.seed = take_value(&mut args, "--seed")?.parse()?,
            "--iterations" => {
                options.iterations = take_value(&mut args, "--iterations")?.parse()?
            }
            "--fcc" => options.fcc = Some(PathBuf::from(take_value(&mut args, "--fcc")?)),
            "--corpus" => options.corpus = true,
            "--self-test" => options.self_test = true,
            "--replay" => options.replay = Some(take_value(&mut args, "--replay")?.into()),
            "--render" => options.render = Some(take_value(&mut args, "--render")?.into()),
            "--extract" => options.extract = Some(take_value(&mut args, "--extract")?.into()),
            other => anyhow::bail!("unknown fcc-fuzz flag: {other}"),
        }
    }
    Ok(options)
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

    build_isa_reference(&root)?;

    cmd!(sh, "mdbook build").run()?;

    Ok(())
}

fn build_isa_reference(root: &std::path::Path) -> anyhow::Result<()> {
    let output = root.join("docs/generated/isa");
    if output.exists() {
        std::fs::remove_dir_all(&output)?;
    }
    std::fs::create_dir_all(&output)?;
    std::fs::write(
        output.join("index.md"),
        "# ISA Programmer's Reference\n\n\
         - [RISC-V](./riscv/index.md)\n\
         - [AArch64](./aarch64/index.md)\n\
         - [x86-64](./x86-64/index.md)\n\
         - [PTX](./ptx/index.md)\n",
    )?;

    let targets: &[(&str, &str, bool, &[&str])] = &[
        (
            "riscv",
            "riscv",
            false,
            &[
                "backends/riscv/defs/main.tmdl",
                "backends/riscv/defs/base.tmdl",
                "backends/riscv/defs/multiplication.tmdl",
                "backends/riscv/defs/float.tmdl",
                "backends/riscv/defs/compressed.tmdl",
                "backends/riscv/defs/atomics.tmdl",
                "backends/riscv/defs/zifencei.tmdl",
                "backends/riscv/defs/zicsr.tmdl",
                "backends/riscv/defs/perf.tmdl",
                "backends/riscv/defs/vector.tmdl",
                "backends/riscv/defs/syntacore_scr1.tmdl",
            ],
        ),
        (
            "arm64",
            "aarch64",
            false,
            &[
                "backends/arm64/defs/main.tmdl",
                "backends/arm64/defs/versions.tmdl",
                "backends/arm64/defs/float.tmdl",
                "backends/arm64/defs/data_processing.tmdl",
                "backends/arm64/defs/loads_stores.tmdl",
                "backends/arm64/defs/atomics.tmdl",
                "backends/arm64/defs/branches.tmdl",
                "backends/arm64/defs/perf.tmdl",
            ],
        ),
        (
            "x86_64",
            "x86-64",
            false,
            &[
                "backends/x86_64/defs/main.tmdl",
                "backends/x86_64/defs/base.tmdl",
                "backends/x86_64/defs/arith_ext.tmdl",
                "backends/x86_64/defs/conditional.tmdl",
                "backends/x86_64/defs/memory_ext.tmdl",
                "backends/x86_64/defs/float.tmdl",
                "backends/x86_64/defs/perf.tmdl",
                "backends/x86_64/defs/cpu/intel/tiger_lake.tmdl",
            ],
        ),
        (
            "ptx",
            "ptx",
            true,
            &[
                "gpu/defs/ptx/main.tmdl",
                "gpu/defs/ptx/versions.tmdl",
                "gpu/defs/ptx/integer.tmdl",
                "gpu/defs/ptx/logic.tmdl",
                "gpu/defs/ptx/float.tmdl",
                "gpu/defs/ptx/compare.tmdl",
                "gpu/defs/ptx/movement.tmdl",
                "gpu/defs/ptx/memory.tmdl",
                "gpu/defs/ptx/control.tmdl",
                "gpu/defs/ptx/sync.tmdl",
                "gpu/defs/ptx/video.tmdl",
                "gpu/defs/ptx/async.tmdl",
                "gpu/defs/ptx/tensor.tmdl",
                "gpu/defs/ptx/texture.tmdl",
            ],
        ),
    ];

    for (dialect, directory, text_only, inputs) in targets {
        let mut compiler = Compiler::builder()
            .action(Action::EmitMarkdown)
            .dialect(Some((*dialect).to_string()))
            .text_only(*text_only)
            .output(OutputKind::Batch(
                output.join(directory).display().to_string(),
            ));
        for input in *inputs {
            compiler = compiler.add_input(&root.join(input).display().to_string());
        }
        compiler.build().compile()?;
    }

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
verify <isa> [--shard k/N]
                 run formal ISA verification. Available ISAs: riscv64, riscv32, armv8, x86_64
isa-test-suite   run differential ISA tests against a golden oracle (riscv/Spike)
fcc-torture [--bless] [--fcc <path>]
                 compile the pinned GCC C torture corpus through codegen and
                 compare the failures against the recorded baseline
fcc-bench [--fcc <path>] [--output <file.json>] [--baseline <file.json>]
                 time fcc and gcc -O0 on the passing torture execute cases and
                 coremark; print the sum, median ratio and slowest cases, and
                 fail on a >10 % sum regression against the baseline
fcc-corpus [--baseline <dir> | --diff <dir> | --determinism]
                 compile the fcc .c corpus to x86_64 asm and capture, diff or
                 double-compile it
fcc-fuzz [--seed N] [--iterations N] [--corpus] [--self-test] [--fcc <path>]
         [--replay <dir>] [--render <file>] [--extract <dir>]
                 generate random UB-free C programs, compile them under
                 different pass pipelines and reference compilers, run the
                 binaries and compare observable behavior
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
