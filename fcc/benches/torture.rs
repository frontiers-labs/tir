use tir_bench::{Criterion, Program};

/// Every passing case of the GCC torture execute corpus, each a program of its own.
pub fn torture() -> Program {
    Program::new("torture", "fcc/extbench/torture")
        .git(
            "https://github.com/gcc-mirror/gcc.git",
            "9aab80ddc5b2fa0eef80008e718067ab45f42c50",
        )
        .subdir("gcc/testsuite/gcc.c-torture")
        .sources(declared_sources())
        .separate()
        .prepare_sources(prepare_sources)
        .link_flags(["-lm"])
}

fn prepare_sources(
    root: &std::path::Path,
    directory: &std::path::Path,
) -> tir_bench::Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>)> {
    let dependencies = [
        "gcc-torture-known-failures.txt",
        "gcc-torture-execute-known-failures.txt",
    ]
    .map(|name| root.join("fcc/tests").join(name))
    .to_vec();
    let mut sources = Vec::new();
    tir_bench::program::collect(&directory.join("execute"), &mut sources, "c")?;
    sources.sort();
    let expected = inventory()
        .map(|name| directory.join(name))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        sources == expected,
        "update torture/sources.txt to match the pinned Git tree"
    );
    let sources = declared_sources()
        .into_iter()
        .map(|name| directory.join(name))
        .collect();
    Ok((sources, dependencies))
}

// Generated from the pinned Git tree, so filtering and listing never need a fetch.
const INVENTORY: &str = include_str!("../extbench/torture/sources.txt");

fn inventory() -> impl Iterator<Item = &'static str> {
    INVENTORY
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
}

fn declared_sources() -> Vec<&'static str> {
    let excluded = [
        include_str!("../tests/gcc-torture-known-failures.txt"),
        include_str!("../tests/gcc-torture-execute-known-failures.txt"),
    ]
    .into_iter()
    .flat_map(str::lines)
    .map(|line| line.split('#').next().unwrap().trim())
    .collect::<std::collections::BTreeSet<_>>();
    inventory()
        .filter(|name| !excluded.contains(name))
        .collect()
}

fn bench(c: &mut Criterion) {
    c.bench_program(&torture());
}

// xtask compiles this file too, under an edition that orders a mixed import
// list of types and macros differently, so the macros are named by path.
tir_bench::criterion_group!(benches, bench);
tir_bench::criterion_main!(benches);
