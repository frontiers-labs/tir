#[path = "../../benchmarks/programs/mod.rs"]
#[allow(dead_code)]
mod programs;

fn main() -> tir_bench::Result<()> {
    let mut suite = tir_bench::Suite::from_args("fcc/programs")?;
    programs::register(
        &mut suite,
        programs::CompilerSet::source(env!("CARGO_BIN_EXE_fcc")),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap(),
    )?;
    suite.finish()
}
