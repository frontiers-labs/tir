use super::{Program, Source};
use tir_bench::sources::GitSource;

pub fn definition() -> Program {
    Program {
        name: "torture",
        source: Source::Git(GitSource {
            repository: "https://github.com/gcc-mirror/gcc.git",
            revision: "9aab80ddc5b2fa0eef80008e718067ab45f42c50",
            subdir: "gcc/testsuite/gcc.c-torture",
        }),
        sources: declared_sources(),
        separate: true,
        prepare_sources: Some(prepare_sources),
        link_flags: &["-lm"],
        ..Program::default()
    }
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
    super::collect(&directory.join("execute"), &mut sources, "c")?;
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
const INVENTORY: &str = include_str!("torture/sources.txt");

fn inventory() -> impl Iterator<Item = &'static str> {
    INVENTORY
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
}

fn declared_sources() -> Vec<&'static str> {
    let excluded = [
        include_str!("../../fcc/tests/gcc-torture-known-failures.txt"),
        include_str!("../../fcc/tests/gcc-torture-execute-known-failures.txt"),
    ]
    .into_iter()
    .flat_map(str::lines)
    .map(|line| line.split('#').next().unwrap().trim())
    .collect::<std::collections::BTreeSet<_>>();
    inventory()
        .filter(|name| !excluded.contains(name))
        .collect()
}
