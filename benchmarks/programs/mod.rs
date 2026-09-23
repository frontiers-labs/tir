//! Shared compiler workloads. Definitions and compiler recipes are ordinary Rust.
mod coremark;
mod dhrystone;
mod support;
mod torture;
mod whetstone;

use std::path::{Path, PathBuf};

use tir_bench::sources::GitSource;
use tir_bench::{Result, Suite};

pub use support::CompilerSet;

#[derive(Default)]
pub enum Source {
    #[default]
    Local,
    Git(GitSource),
}

#[derive(Default)]
pub struct Program {
    pub name: &'static str,
    pub source: Source,
    pub sources: Vec<&'static str>,
    pub flags: &'static [&'static str],
    pub link_flags: &'static [&'static str],
    pub args: &'static [&'static str],
    pub validator: Option<&'static str>,
    pub llvm: bool,
    pub separate: bool,
    pub prepare_sources: Option<PrepareSources>,
}

pub type PrepareSources = fn(&Path, &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)>;

pub struct Prepared {
    pub directory: PathBuf,
    pub sources: Vec<PathBuf>,
    pub dependencies: Vec<PathBuf>,
}

pub fn definitions() -> [Program; 4] {
    [
        coremark::definition(),
        dhrystone::definition(),
        torture::definition(),
        whetstone::definition(),
    ]
}

impl Program {
    pub fn prepare(
        &self,
        root: &Path,
        cache: &Path,
        offline: bool,
        timeout: std::time::Duration,
    ) -> Result<Prepared> {
        let local = root.join("benchmarks/programs").join(self.name);
        let directory = match &self.source {
            Source::Local => local.clone(),
            Source::Git(source) => source.prepare(cache, offline, timeout)?,
        }
        .canonicalize()?;
        let mut sources = self
            .sources
            .iter()
            .map(|name| directory.join(name))
            .collect::<Vec<_>>();
        let mut dependencies = Vec::new();
        if let Some(prepare) = self.prepare_sources {
            (sources, dependencies) = prepare(root, &directory)?;
        }
        sources.sort();
        let mut declared = self
            .sources
            .iter()
            .map(|name| directory.join(name))
            .collect::<Vec<_>>();
        declared.sort();
        anyhow::ensure!(
            sources == declared,
            "{} prepared sources differ from its Rust declaration",
            self.name
        );
        anyhow::ensure!(!sources.is_empty(), "{} has no selected sources", self.name);
        dependencies.extend(sources.iter().cloned());
        collect(&directory, &mut dependencies, "h")?;
        // Validator dependencies include expected data and the workload recipe itself.
        for entry in if local.is_dir() {
            Some(std::fs::read_dir(&local)?)
        } else {
            None
        }
        .into_iter()
        .flatten()
        {
            let path = entry?.path();
            if path.is_file()
                && matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("py" | "out" | "txt")
                )
            {
                dependencies.push(path);
            }
        }
        dependencies.push(
            root.join("benchmarks/programs")
                .join(format!("{}.rs", self.name)),
        );
        dependencies.sort();
        dependencies.dedup();
        Ok(Prepared {
            directory,
            sources,
            dependencies,
        })
    }
}

fn collect(directory: &Path, paths: &mut Vec<PathBuf>, extension: &str) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            if !entry.file_name().to_string_lossy().starts_with('.') {
                collect(&path, paths, extension)?;
            }
        } else if path.extension().is_some_and(|value| value == extension) {
            paths.push(path);
        }
    }
    Ok(())
}

pub fn register(suite: &mut Suite, compilers: CompilerSet, root: &Path) -> Result<()> {
    for program in definitions() {
        support::register(suite, &compilers, root, &program)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn migrated_inventory_preserves_work_and_input_modes() {
        let programs = super::definitions();
        assert_eq!(
            programs
                .iter()
                .map(|program| program.name)
                .collect::<Vec<_>>(),
            ["coremark", "dhrystone", "torture", "whetstone"]
        );
        assert_eq!(programs[0].args, ["0", "0", "0", "1000000"]);
        assert_eq!(programs[1].args, ["100000000"]);
        assert_eq!(programs[3].args, ["1000000"]);
        assert_eq!(programs.iter().filter(|program| program.llvm).count(), 2);
        assert!(programs[2].separate);
        assert_eq!(programs[0].sources.len(), 6);
        assert_eq!(programs[1].sources.len(), 2);
    }
}
