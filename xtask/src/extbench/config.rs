use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use globset::Glob;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Default, clap::ValueEnum, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    #[default]
    Source,
    Llvm,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub suite: Package,
    pub llvm: Option<LlvmInput>,
    pub compiler: Vec<Compiler>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlvmInput {
    pub prepare: Vec<String>,
    pub version: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub package: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compiler {
    pub name: String,
    #[serde(default)]
    pub opt_in: bool,
    #[serde(default)]
    pub input: Input,
    #[serde(default)]
    pub build: Vec<String>,
    pub compile: Vec<String>,
    pub link: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub metrics: BTreeMap<String, String>,
}

pub struct Selection {
    pub directory: PathBuf,
    pub name: String,
    pub suite: Suite,
}

pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    toml::from_str(&fs::read_to_string(path).with_context(|| path.display().to_string())?)
        .with_context(|| path.display().to_string())
}

pub fn discover(
    root: &Path,
    manifest: Option<&Path>,
    package: Option<&str>,
    bench: &str,
) -> anyhow::Result<Vec<Selection>> {
    let mut manifests = Vec::new();
    match manifest {
        Some(path) => manifests.push(path.canonicalize()?),
        None => visit(root, &mut manifests)?,
    }
    manifests.sort();
    let filter = Glob::new(bench)?.compile_matcher();
    let mut selected = Vec::new();
    for manifest in manifests {
        let suite: Suite = read(&manifest)?;
        if package.is_some_and(|package| package != suite.suite.package) {
            continue;
        }
        for entry in fs::read_dir(manifest.parent().unwrap())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || !entry.path().join("benchmark.toml").is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if filter.is_match(&name) {
                selected.push(Selection {
                    directory: entry.path(),
                    name,
                    suite: read(&manifest)?,
                });
            }
        }
    }
    selected
        .sort_by(|a, b| (&a.suite.suite.package, &a.name).cmp(&(&b.suite.suite.package, &b.name)));
    anyhow::ensure!(!selected.is_empty(), "no benchmarks match the filters");
    Ok(selected)
}

fn visit(directory: &Path, manifests: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let manifest = directory.join("bench_suite.toml");
    if manifest.is_file() {
        manifests.push(manifest);
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_dir()
            && !name.starts_with('.')
            && !matches!(
                name.as_ref(),
                "target" | "build" | "node_modules" | "Inputs"
            )
        {
            visit(&entry.path(), manifests)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Benchmark {
    pub sources: Vec<String>,
    #[serde(default = "inputs")]
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub link_flags: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub verify: Vec<String>,
    #[serde(default = "levels")]
    pub levels: Vec<String>,
    #[serde(default)]
    pub separate: bool,
    #[serde(default)]
    pub exclude_files: Vec<PathBuf>,
    pub source: Option<GitSource>,
}

pub fn supports_input(selection: &Selection, input: Input) -> anyhow::Result<bool> {
    Ok(
        read::<Benchmark>(&selection.directory.join("benchmark.toml"))?
            .inputs
            .contains(&input),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    pub repository: String,
    pub revision: String,
    #[serde(default)]
    pub subdir: PathBuf,
}

fn levels() -> Vec<String> {
    vec!["-O0".into(), "-O2".into()]
}

fn inputs() -> Vec<Input> {
    vec![Input::Source]
}

pub struct Prepared {
    pub config: Benchmark,
    pub directory: PathBuf,
    pub sources: Vec<PathBuf>,
}

pub fn prepare(selection: &Selection, cache: &Path) -> anyhow::Result<Prepared> {
    use std::collections::BTreeSet;
    use xshell::{cmd, Shell};

    let config: Benchmark = read(&selection.directory.join("benchmark.toml"))?;
    let directory = match &config.source {
        None => selection.directory.clone(),
        Some(source) => {
            anyhow::ensure!(
                source.revision.len() == 40
                    && source.revision.bytes().all(|c| c.is_ascii_hexdigit()),
                "source.revision must be a full Git commit hash"
            );
            let checkout = cache.join(&source.revision);
            let sh = Shell::new()?;
            if !checkout.join(".git").is_dir() {
                fs::create_dir_all(&checkout)?;
                cmd!(sh, "git -C {checkout} init").run()?;
            }
            let revision = &source.revision;
            if !cmd!(sh, "git -C {checkout} cat-file -e {revision}")
                .quiet()
                .ignore_status()
                .output()?
                .status
                .success()
            {
                let repository = &source.repository;
                cmd!(
                    sh,
                    "git -C {checkout} fetch --depth 1 --filter=blob:none {repository} {revision}"
                )
                .run()?;
            }
            if !source.subdir.as_os_str().is_empty() {
                let subdir = &source.subdir;
                cmd!(sh, "git -C {checkout} sparse-checkout set {subdir}").run()?;
            }
            cmd!(sh, "git -C {checkout} checkout --detach {revision}").run()?;
            checkout.join(&source.subdir)
        }
    }
    .canonicalize()?;
    let mut include = globset::GlobSetBuilder::new();
    let mut exclude = globset::GlobSetBuilder::new();
    for pattern in &config.sources {
        match pattern.strip_prefix('!') {
            Some(pattern) => exclude.add(Glob::new(pattern)?),
            None => include.add(Glob::new(pattern)?),
        };
    }
    let include = include.build()?;
    let exclude = exclude.build()?;
    let mut ignored = BTreeSet::new();
    for path in &config.exclude_files {
        let text = fs::read_to_string(selection.directory.join(path))?;
        ignored.extend(
            text.lines()
                .map(|line| line.split('#').next().unwrap().trim())
                .filter(|line| !line.is_empty())
                .map(PathBuf::from),
        );
    }
    let mut sources = Vec::new();
    let mut pending = vec![directory.clone()];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(&directory)?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name();
                if !name.to_string_lossy().starts_with('.')
                    && !matches!(name.to_str(), Some("target" | "build" | "node_modules"))
                {
                    pending.push(path);
                }
            } else if entry.file_type()?.is_file()
                && include.is_match(relative)
                && !exclude.is_match(relative)
                && !ignored.contains(relative)
            {
                sources.push(path);
            }
        }
    }
    sources.sort();
    anyhow::ensure!(
        !sources.is_empty(),
        "{}/{} has no selected sources",
        selection.suite.suite.package,
        selection.name
    );
    anyhow::ensure!(
        !config.levels.is_empty(),
        "benchmark levels cannot be empty"
    );
    Ok(Prepared {
        config,
        directory,
        sources,
    })
}
