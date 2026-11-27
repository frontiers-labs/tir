mod ast_grep;
mod bench;
mod dupehound;
mod fcc_fuzz;
mod formal_test;
mod jevlint;
mod lint;
mod scripts;
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
    /// Regenerate Hax extractions and check configured Lean and Kani proofs.
    FormalTest(formal_test::Options),
    /// Run the benchmarks with nextest and save a benchmark summary.
    Bench(bench::Options),
    /// Package tools, tests and fixtures for test workers.
    TestPack(test::PackOptions),
    /// Run static checks, including optional Jevlint checks.
    Lint(lint::Options),
    /// Install the pre-commit hook.
    InstallGitHooks,
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
        Task::FormalTest(options) => formal_test::run(&project_root(), options),
        Task::Bench(options) => bench::run(&project_root(), options),
        Task::TestPack(options) => test::pack(&project_root(), options),
        Task::Lint(options) => lint::run(&project_root(), options),
        Task::InstallGitHooks => lint::install_git_hooks(&project_root()),
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

    let targets = [
        ("riscv", "riscv", Some("backends/riscv/isa_defs")),
        ("arm64", "aarch64", Some("backends/arm64/isa_defs")),
        ("x86_64", "x86-64", Some("backends/x86_64/isa_defs")),
        ("ptx", "ptx", None),
    ];

    for (dialect, directory, module_root) in targets {
        let mut compiler = Compiler::builder()
            .action(Action::EmitMarkdown)
            .dialect(Some(dialect.to_string()))
            .output(OutputKind::Batch(
                output.join(directory).display().to_string(),
            ));
        if let Some(module_root) = module_root {
            compiler = compiler.module_root(root.join(module_root));
        } else {
            compiler = compiler.text_only(true);
            let defs = root.join("gpu/defs/ptx");
            for input in [
                "main.tmdl",
                "versions.tmdl",
                "integer.tmdl",
                "logic.tmdl",
                "float.tmdl",
                "compare.tmdl",
                "movement.tmdl",
                "memory.tmdl",
                "control.tmdl",
                "sync.tmdl",
                "video.tmdl",
                "async.tmdl",
                "tensor.tmdl",
                "texture.tmdl",
            ] {
                compiler = compiler.add_input(&defs.join(input).display().to_string());
            }
        }
        compiler.build().compile()?;
    }

    Ok(())
}
