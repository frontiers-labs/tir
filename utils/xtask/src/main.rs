mod fcc_fuzz;
mod lint;
mod test;
mod verify_smt;

use std::path::PathBuf;

use xtask::fcc_torture;
pub use xtask::utils;

use clap::Parser;
use tmdl::{Action, Compiler, OutputKind};
use xshell::{cmd, Shell};

use crate::utils::project_root;

#[derive(Parser)]
enum Task {
    /// Build fresh tools and run the workspace tests with nextest or cargo test.
    Test(test::Options),
    /// Package tools, tests and fixtures for test workers.
    TestPack(test::PackOptions),
    /// Run the lints: ast-grep rules, duplicate code, clippy and rustdoc.
    Lint,
    /// Build the project documentation
    Docs,
    /// Run formal ISA verification against the Sail model
    Verify {
        /// One of riscv64, riscv32, armv8, x86_64
        isa: String,
        /// Passed through, e.g. `--shard k/N`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Update the recorded failures for the pinned GCC C torture corpus.
    FccTorture {
        #[arg(long)]
        fcc: Option<PathBuf>,
    },
    /// Generate random UB-free C programs, compile them under different pass
    /// pipelines and reference compilers, run the binaries and compare
    /// observable behavior
    FccFuzz(fcc_fuzz::Options),
}

fn main() -> anyhow::Result<()> {
    let sh = Shell::new()?;
    match Task::parse() {
        Task::Test(options) => test::run(&project_root(), options),
        Task::TestPack(options) => test::pack(&project_root(), options),
        Task::Lint => lint::run(&sh, &project_root()),
        Task::Docs => build_docs(&sh),
        Task::Verify { isa, args } => verify_smt::verify_smt(&sh, &isa, args.into_iter()),
        Task::FccTorture { fcc } => fcc_torture::bless(&sh, &project_root(), fcc.as_deref()),
        Task::FccFuzz(options) => fcc_fuzz::run(&sh, &project_root(), &options),
    }
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
                "backends/riscv/defs/zba.tmdl",
                "backends/riscv/defs/zbb.tmdl",
                "backends/riscv/defs/zbc.tmdl",
                "backends/riscv/defs/zbs.tmdl",
                "backends/riscv/defs/float.tmdl",
                "backends/riscv/defs/float_extra.tmdl",
                "backends/riscv/defs/float_round.tmdl",
                "backends/riscv/defs/compressed.tmdl",
                "backends/riscv/defs/atomics.tmdl",
                "backends/riscv/defs/zifencei.tmdl",
                "backends/riscv/defs/zicsr.tmdl",
                "backends/riscv/defs/perf.tmdl",
                "backends/riscv/defs/cpu/spacemit/x100_integer.tmdl",
                "backends/riscv/defs/cpu/spacemit/x100.tmdl",
                "backends/riscv/defs/vector.tmdl",
                "backends/riscv/defs/vector_int.tmdl",
                "backends/riscv/defs/vector_mask.tmdl",
                "backends/riscv/defs/vector_red.tmdl",
                "backends/riscv/defs/vector_perm.tmdl",
                "backends/riscv/defs/vector_widen.tmdl",
                "backends/riscv/defs/vector_fixed.tmdl",
                "backends/riscv/defs/vector_mem.tmdl",
                "backends/riscv/defs/vector_float.tmdl",
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
                "backends/arm64/defs/advsimd.tmdl",
                "backends/arm64/defs/data_processing.tmdl",
                "backends/arm64/defs/loads_stores.tmdl",
                "backends/arm64/defs/atomics.tmdl",
                "backends/arm64/defs/branches.tmdl",
                "backends/arm64/defs/perf.tmdl",
                "backends/arm64/defs/cpu/arm/cortex_a720.tmdl",
            ],
        ),
        (
            "x86_64",
            "x86-64",
            false,
            &[
                "backends/x86_64/defs/main.tmdl",
                "backends/x86_64/defs/encoding.tmdl",
                "backends/x86_64/defs/base.tmdl",
                "backends/x86_64/defs/arith_ext.tmdl",
                "backends/x86_64/defs/conditional.tmdl",
                "backends/x86_64/defs/memory_ext.tmdl",
                "backends/x86_64/defs/atomics.tmdl",
                "backends/x86_64/defs/bmi.tmdl",
                "backends/x86_64/defs/bmi2.tmdl",
                "backends/x86_64/defs/ordering.tmdl",
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
