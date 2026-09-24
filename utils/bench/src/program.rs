//! Source preparation and compile/run cases for external program benchmarks.
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::sources::GitSource;
use crate::{Command as BenchCommand, Phase, ProcessCase, Suite};

/// Cargo entry point for a program definition also consumed by other tools.
#[macro_export]
macro_rules! program_main {
    ($definition:path) => {
        #[allow(dead_code)] // Tools can import the definition without running the benchmark.
        fn main() -> $crate::Result<()> {
            if std::env::args().any(|arg| arg == "--describe") {
                println!("{{\"kind\":\"program\"}}");
                return Ok(());
            }
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("benchmark package belongs to the workspace");
            let (namespace, candidate, llvm) = match env!("CARGO_PKG_NAME") {
                "fcc" => ("fcc", option_env!("CARGO_BIN_EXE_fcc"), false),
                "tir-tools" => ("tir", option_env!("CARGO_BIN_EXE_tir"), true),
                package => panic!("unsupported program benchmark package {package}"),
            };
            let candidate = candidate.expect("Cargo-built compiler is unavailable");
            let compilers = if llvm {
                $crate::program::CompilerSet::llvm(candidate)
            } else {
                $crate::program::CompilerSet::source(candidate)
            };
            let program = $definition(root);
            let mut suite = $crate::Suite::from_args(&format!("{namespace}/{}", program.name))?;
            $crate::program::register_program(&mut suite, &compilers, root, &program)?;
            suite.finish()
        }
    };
}

/// Run preparation or verification, retaining the command and stderr on failure.
pub fn checked(command: &mut Command, timeout: Duration) -> Result<Output> {
    let output = crate::process::capture(command, timeout)
        .with_context(|| format!("running {command:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{command:?} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

/// Hash length-delimited parameters and declared input contents in caller order.
/// Paths are relative identities, so moving a checkout does not change its work.
pub fn digest_inputs(
    root: &Path,
    files: &[std::path::PathBuf],
    parameters: &[String],
) -> Result<String> {
    let mut digest = Sha256::new();
    for value in parameters {
        field(&mut digest, value.as_bytes());
    }
    for file in files {
        field(
            &mut digest,
            file.strip_prefix(root)
                .unwrap_or(file)
                .to_string_lossy()
                .as_bytes(),
        );
        field(
            &mut digest,
            &std::fs::read(file).with_context(|| format!("reading {}", file.display()))?,
        );
    }
    Ok(format!("{:x}", digest.result()))
}

fn field(digest: &mut Sha256, value: &[u8]) {
    digest.input((value.len() as u64).to_le_bytes());
    digest.input(value);
}

/// Resolve a process executable through PATH and hash its bytes for provenance.
pub fn executable_identity(program: &Path) -> Result<(std::path::PathBuf, String)> {
    let path = if program.components().count() > 1 || program.is_absolute() {
        program.canonicalize()?
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(program))
            .find(|path| path.is_file())
            .with_context(|| format!("executable {} is absent from PATH", program.display()))?
            .canonicalize()?
    };
    let hash = format!("{:x}", Sha256::digest(&std::fs::read(&path)?));
    Ok((path, hash))
}

#[derive(Default)]
pub enum Source {
    #[default]
    Local,
    Git(GitSource),
}

#[derive(Default)]
pub struct Program {
    pub name: &'static str,
    pub resources: PathBuf,
    pub definition: PathBuf,
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

impl Program {
    pub fn prepare(
        &self,
        root: &Path,
        cache: &Path,
        offline: bool,
        timeout: std::time::Duration,
    ) -> Result<Prepared> {
        let local = self.resources.clone();
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
        if local.is_dir() {
            for entry in std::fs::read_dir(&local)? {
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
        }
        dependencies.push(self.definition.clone());
        dependencies.sort();
        dependencies.dedup();
        Ok(Prepared {
            directory,
            sources,
            dependencies,
        })
    }
}

pub fn collect(directory: &Path, paths: &mut Vec<PathBuf>, extension: &str) -> Result<()> {
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

/// Select the Cargo-built compiler for one input mode and its reference controls.
pub struct CompilerSet {
    candidate: PathBuf,
    llvm: bool,
}

impl CompilerSet {
    pub fn source(candidate: impl Into<PathBuf>) -> Self {
        Self {
            candidate: candidate.into(),
            llvm: false,
        }
    }
    pub fn llvm(candidate: impl Into<PathBuf>) -> Self {
        Self {
            candidate: candidate.into(),
            llvm: true,
        }
    }
}

#[derive(Clone)]
struct Recipe {
    executable: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
}

impl Recipe {
    fn std(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.args(&self.args).current_dir(&self.cwd);
        command
    }
    fn measured(&self) -> BenchCommand {
        BenchCommand::new(&self.executable)
            .args(&self.args)
            .current_dir(&self.cwd)
    }
}

fn compile(
    set: &CompilerSet,
    compiler: &str,
    program: &Program,
    cwd: &Path,
    level: &str,
    input: &Path,
    output: &Path,
) -> Recipe {
    let executable = if matches!(compiler, "fcc" | "tir") {
        set.candidate.clone()
    } else if compiler == "gcc" {
        "gcc".into()
    } else {
        "clang".into()
    };
    let mut args: Vec<OsString> = Vec::new();
    if compiler == "tir" {
        args.extend(["mc", "--march", host_arch(), "--filetype", "obj"].map(OsString::from));
    } else {
        if !set.llvm {
            args.push("-std=gnu17".into());
        }
        args.push(level.into());
        if !set.llvm {
            args.extend(program.flags.iter().map(OsString::from));
        }
        if compiler == "clang-scalar" {
            args.extend(["-fno-vectorize", "-fno-slp-vectorize"].map(OsString::from));
        }
        if compiler == "clang-ir-backend" {
            args.extend(["-Xclang", "-disable-llvm-passes"].map(OsString::from));
        }
        args.push("-c".into());
    }
    args.extend([
        input.as_os_str().to_owned(),
        "-o".into(),
        output.as_os_str().to_owned(),
    ]);
    Recipe {
        executable,
        args,
        cwd: cwd.to_owned(),
    }
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    }
}

fn link(
    set: &CompilerSet,
    compiler: &str,
    program: &Program,
    cwd: &Path,
    objects: &[PathBuf],
    output: &Path,
) -> Recipe {
    let executable = if compiler == "fcc" {
        set.candidate.clone()
    } else if compiler == "gcc" {
        "gcc".into()
    } else {
        "clang".into()
    };
    let mut args = Vec::new();
    if set.llvm {
        args.push("-no-pie".into());
    }
    args.extend(objects.iter().map(|path| path.as_os_str().to_owned()));
    args.extend(program.link_flags.iter().map(OsString::from));
    args.extend(["-o".into(), output.as_os_str().to_owned()]);
    Recipe {
        executable,
        args,
        cwd: cwd.to_owned(),
    }
}

fn validator(program: &Program, root: &Path) -> Option<Recipe> {
    program.validator.map(|name| {
        let path = program.resources.join(name);
        Recipe {
            executable: "python3".into(),
            args: vec![path.into_os_string()],
            cwd: root.to_owned(),
        }
    })
}

fn verify_output(
    validator: &Option<Recipe>,
    args: &[OsString],
    output: &Path,
    timeout: Duration,
) -> Result<()> {
    if let Some(validator) = validator {
        checked(
            validator.std().arg("--stdout").arg(output).args(args),
            timeout,
        )?;
    }
    Ok(())
}

fn names(set: &CompilerSet, requested: Option<&str>) -> Vec<&'static str> {
    let all = if set.llvm {
        vec!["tir", "clang-ir", "clang-ir-backend"]
    } else {
        vec!["fcc", "gcc", "clang", "clang-scalar"]
    };
    all.into_iter()
        .filter(|name| {
            requested.map_or(*name != "clang-scalar", |requested| {
                requested.split(',').any(|part| part == *name)
            })
        })
        .collect()
}

pub fn register_program(
    suite: &mut Suite,
    set: &CompilerSet,
    root: &Path,
    program: &Program,
) -> Result<()> {
    if set.llvm && !program.llvm {
        return Ok(());
    }
    let compilers = names(set, suite.options().compiler.as_deref());
    let levels = ["-O0", "-O2"]
        .into_iter()
        .filter(|level| {
            suite.options().level.as_ref().is_none_or(|wanted| {
                wanted.trim_start_matches('-') == level.trim_start_matches('-')
            })
        })
        .collect::<Vec<_>>();
    let compile_enabled = !matches!(suite.options().phase, Phase::Run);
    let run_enabled = !matches!(suite.options().phase, Phase::Compile);
    let selected = select(suite, set, program, &compilers, &levels)?;
    if suite.options().list || !selected || compilers.is_empty() || levels.is_empty() {
        return Ok(());
    }
    let timeout = suite.timeout();
    let cache = suite.source_cache();
    let prepared = program.prepare(root, &cache, suite.options().offline, timeout)?;
    let validator = validator(program, root);
    let args = program.args.iter().map(OsString::from).collect::<Vec<_>>();
    for level in levels {
        let wanted = |compiler: &str, index: usize| {
            let ids = CaseIds::new(set, program, compiler, level);
            let source = prepared.sources[index]
                .strip_prefix(&prepared.directory)
                .expect("prepared source belongs to its directory");
            (compile_enabled && suite.matches(&ids.compile(source)))
                || (run_enabled && suite.matches(&ids.run(source)))
        };
        let selected_indices = compilers
            .iter()
            .map(|compiler| {
                (0..prepared.sources.len())
                    .filter(|index| wanted(compiler, *index))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        if selected_indices.iter().all(Vec::is_empty) {
            continue;
        }
        let scratch = suite
            .artifacts()
            .join("programs")
            .join(program.name)
            .join(level.trim_start_matches('-'));
        std::fs::create_dir_all(&scratch)?;
        let inputs = prepare_inputs(set, program, &prepared, level, &scratch, timeout)?;
        let mut groups = CaseGroups {
            compile: (0..inputs.len()).map(|_| Vec::new()).collect(),
            run: (0..if program.separate { inputs.len() } else { 1 })
                .map(|_| Vec::new())
                .collect(),
        };
        let context = PreparedLevel {
            set,
            program,
            prepared: &prepared,
            inputs: &inputs,
            level,
            validator: &validator,
            args: &args,
            timeout,
        };
        let metadata = context.metadata(root, &scratch)?;
        for (compiler, selected_indices) in compilers.iter().zip(&selected_indices) {
            if selected_indices.is_empty() {
                continue;
            }
            context.prepare_compiler(
                suite,
                compiler,
                selected_indices,
                &scratch,
                &metadata,
                &mut groups,
            )?;
        }
        for group in groups.compile.into_iter().chain(groups.run) {
            if !group.is_empty() {
                suite.process_group(group)?;
            }
        }
    }
    Ok(())
}

struct CaseIds {
    base: String,
    separate: bool,
}

impl CaseIds {
    fn new(set: &CompilerSet, program: &Program, compiler: &str, level: &str) -> Self {
        let mode = if set.llvm { "llvm" } else { "source" };
        Self {
            base: format!(
                "{compiler}/{}/{mode}/{}",
                program.name,
                level.trim_start_matches('-')
            ),
            separate: program.separate,
        }
    }

    fn compile(&self, source: &Path) -> String {
        format!(
            "{}/compile/{}",
            self.base,
            source.with_extension("").display()
        )
    }

    fn run(&self, source: &Path) -> String {
        if self.separate {
            format!("{}/run/{}", self.base, source.with_extension("").display())
        } else {
            format!("{}/run", self.base)
        }
    }
}

fn select(
    suite: &mut Suite,
    set: &CompilerSet,
    program: &Program,
    compilers: &[&str],
    levels: &[&str],
) -> Result<bool> {
    let compile_enabled = !matches!(suite.options().phase, Phase::Run);
    let run_enabled = !matches!(suite.options().phase, Phase::Compile);
    let mut selected = false;
    for compiler in compilers {
        for level in levels {
            let cases = CaseIds::new(set, program, compiler, level);
            let mut ids = Vec::new();
            if compile_enabled {
                ids.extend(
                    program
                        .sources
                        .iter()
                        .map(|source| cases.compile(Path::new(source))),
                );
            }
            if run_enabled {
                if program.separate {
                    ids.extend(
                        program
                            .sources
                            .iter()
                            .map(|source| cases.run(Path::new(source))),
                    );
                } else {
                    ids.push(cases.run(Path::new("")));
                }
            }
            for id in ids {
                selected |= suite.matches(&id);
                if suite.options().list {
                    suite.list_case(&id)?;
                }
            }
        }
    }
    Ok(selected)
}

struct CaseGroups {
    compile: Vec<Vec<ProcessCase>>,
    run: Vec<Vec<ProcessCase>>,
}

struct PreparedLevel<'a> {
    set: &'a CompilerSet,
    program: &'a Program,
    prepared: &'a Prepared,
    inputs: &'a [PathBuf],
    level: &'a str,
    validator: &'a Option<Recipe>,
    args: &'a [OsString],
    timeout: Duration,
}

impl PreparedLevel<'_> {
    fn metadata(&self, root: &Path, scratch: &Path) -> Result<serde_json::Value> {
        let Self {
            set,
            program,
            prepared,
            inputs,
            level,
            timeout,
            ..
        } = *self;
        let mode = if set.llvm { "llvm" } else { "source" };
        let mut parameters = vec![
            "program-contract-v1".into(),
            mode.into(),
            level.into(),
            format!("{}", program.separate),
        ];
        parameters.extend(
            program
                .flags
                .iter()
                .chain(program.link_flags)
                .chain(program.args)
                .map(|s| (*s).to_owned()),
        );
        if let Source::Git(source) = &program.source {
            parameters.extend([source.repository.into(), source.revision.into()]);
        }
        let mut dependencies = prepared.dependencies.clone();
        dependencies.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/program.rs"));
        let (source_dependencies, local_dependencies): (Vec<_>, Vec<_>) = dependencies
            .into_iter()
            .partition(|path| path.starts_with(&prepared.directory));
        parameters.push(digest_inputs(
            &prepared.directory,
            &source_dependencies,
            &[],
        )?);
        let workload_digest = digest_inputs(root, &local_dependencies, &parameters)?;
        // LLVM bytes and producer settings are compatibility requirements for same-IR comparisons.
        let llvm_digest = if set.llvm {
            Some(digest_inputs(scratch, inputs, &parameters)?)
        } else {
            None
        };
        let producer_version = if set.llvm {
            Some(
                String::from_utf8_lossy(
                    &checked(Command::new("clang").arg("--version"), timeout)?.stdout,
                )
                .into_owned(),
            )
        } else {
            None
        };
        Ok(json!({
            "workload_digest": workload_digest,
            "contract_version": 1,
            "input": mode,
            "level": level,
            "args": program.args,
            "flags": program.flags,
            "link_flags": program.link_flags,
            "llvm_digest": llvm_digest,
            "llvm_producer_version": producer_version,
            "llvm_producer_flags": [
                "-std=gnu17", level, "-fno-vectorize", "-fno-slp-vectorize",
                "-S", "-emit-llvm", "-Xclang", "-disable-O0-optnone"
            ],
            "build": crate::build_configuration(),
            "validation": "outside measurement",
        }))
    }

    fn prepare_objects(
        &self,
        compiler: &str,
        recipes: &[Recipe],
        selected_indices: &[usize],
        directory: &Path,
        mut metadata: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut phase_timings = serde_json::Map::new();
        for (index, recipe) in recipes.iter().enumerate() {
            if !self.program.separate || selected_indices.contains(&index) {
                let diagnostic = recipe.measured();
                let diagnostic = if matches!(compiler, "fcc" | "tir") {
                    diagnostic.env("TIR_TIME_PASSES", "1")
                } else {
                    diagnostic
                };
                let output =
                    diagnostic.run(&directory.join(format!("prepare-{index}")), self.timeout)?;
                let phases = parse_phases(&std::fs::read_to_string(output.stderr)?);
                if !phases.is_empty() {
                    phase_timings.insert(index.to_string(), json!(phases));
                }
            }
        }
        let tool = &recipes[0].executable;
        let version = if compiler == "fcc" {
            format!("fcc package {}", env!("CARGO_PKG_VERSION"))
        } else {
            String::from_utf8_lossy(
                &checked(Command::new(tool).arg("--version"), self.timeout)?.stdout,
            )
            .into_owned()
        };
        let (tool_path, tool_hash) = executable_identity(tool)?;
        metadata["compiler"] = json!(compiler);
        metadata["provenance"] = json!({
            "compiler_path": tool_path,
            "compiler_hash": tool_hash,
            "compiler_version": version,
            "phase_timings_ms": phase_timings,
            "phase_scope": "separate instrumented preparation invocation",
        });
        if !matches!(compiler, "fcc" | "tir") {
            metadata["reference_compiler_hash"] = json!(tool_hash);
            metadata["reference_compiler_version"] = json!(version);
        }
        let linker = match compiler {
            "fcc" => "cc",
            "gcc" => "gcc",
            _ => "clang",
        };
        // FCC is the direct link driver and delegates system linking to cc.
        metadata["link_driver"] = json!(if compiler == "fcc" { "fcc" } else { linker });
        metadata["system_linker_driver"] = reference_tool(linker, self.timeout)?;
        Ok(metadata)
    }

    fn prepare_compiler(
        &self,
        suite: &Suite,
        compiler: &str,
        selected_indices: &[usize],
        scratch: &Path,
        base_metadata: &serde_json::Value,
        groups: &mut CaseGroups,
    ) -> Result<()> {
        let Self {
            set,
            program,
            prepared,
            inputs,
            level,
            validator,
            args,
            timeout,
        } = *self;
        let compile_enabled = !matches!(suite.options().phase, Phase::Run);
        let run_enabled = !matches!(suite.options().phase, Phase::Compile);
        let directory = scratch.join(compiler);
        std::fs::create_dir_all(&directory)?;
        let objects = inputs
            .iter()
            .enumerate()
            .map(|(index, _)| directory.join(format!("{index}.o")))
            .collect::<Vec<_>>();
        let recipes = inputs
            .iter()
            .zip(&objects)
            .map(|(input, output)| {
                compile(
                    set,
                    compiler,
                    program,
                    &prepared.directory,
                    level,
                    input,
                    output,
                )
            })
            .collect::<Vec<_>>();
        let metadata = self.prepare_objects(
            compiler,
            &recipes,
            selected_indices,
            &directory,
            base_metadata.clone(),
        )?;
        for (group, run_group) in groups.run.iter_mut().enumerate() {
            if program.separate && !selected_indices.contains(&group) {
                continue;
            }
            let members = if program.separate {
                &objects[group..group + 1]
            } else {
                &objects[..]
            };
            let executable = directory.join(format!("program-{group}"));
            let linker = link(
                set,
                compiler,
                program,
                &prepared.directory,
                members,
                &executable,
            );
            let runner = Recipe {
                executable,
                args: args.to_vec(),
                cwd: prepared.directory.clone(),
            };
            let validation = Arc::new(Validation {
                linker,
                runner: runner.clone(),
                validator: validator.clone(),
                directory: directory.join(format!("verify-{group}")),
                timeout,
            });
            // Run validation even for compile-only selections before sampling.
            validation.verify()?;
            let cases = CaseIds::new(set, program, compiler, level);
            let run_source = prepared.sources[group].strip_prefix(&prepared.directory)?;
            let run_id = cases.run(run_source);
            if run_enabled && suite.matches(&run_id) {
                let verifier = (*validator).clone();
                let runtime_args = args.to_vec();
                run_group.push(ProcessCase {
                    id: run_id,
                    command: runner.measured(),
                    verify: Some(Box::new(move |stdout| {
                        verify_output(&verifier, &runtime_args, stdout, timeout)
                    })),
                    metadata: metadata.clone(),
                    gate: matches!(compiler, "fcc" | "tir"),
                });
            }
            let indices = if program.separate {
                group..group + 1
            } else {
                0..inputs.len()
            };
            for index in indices {
                let source = prepared.sources[index].strip_prefix(&prepared.directory)?;
                let id = cases.compile(source);
                if compile_enabled && suite.matches(&id) {
                    let validation = Arc::clone(&validation);
                    let object = objects[index].clone();
                    let validated = MemoizedValidation::new(object_digest(&object)?);
                    let trace_children = !matches!(compiler, "fcc" | "tir");
                    groups.compile[index].push(ProcessCase {
                        id,
                        command: recipes[index].measured().trace_children(trace_children),
                        verify: Some(Box::new(move |_| {
                            validated.verify(&object_digest(&object)?, || validation.verify())
                        })),
                        metadata: metadata.clone(),
                        gate: matches!(compiler, "fcc" | "tir"),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Cache only the last output whose full program validation succeeded.
struct MemoizedValidation {
    validated_hash: RefCell<String>,
}

impl MemoizedValidation {
    fn new(validated_hash: String) -> Self {
        Self {
            validated_hash: RefCell::new(validated_hash),
        }
    }

    fn verify(&self, hash: &str, validate: impl FnOnce() -> Result<()>) -> Result<()> {
        if *self.validated_hash.borrow() == hash {
            return Ok(());
        }
        validate()?;
        *self.validated_hash.borrow_mut() = hash.to_owned();
        Ok(())
    }
}

fn object_digest(object: &Path) -> Result<String> {
    digest_inputs(
        object.parent().expect("object has a directory"),
        &[object.to_owned()],
        &[],
    )
}

fn reference_tool(program: &str, timeout: Duration) -> Result<serde_json::Value> {
    let (_, hash) = executable_identity(Path::new(program))?;
    let output = checked(Command::new(program).arg("--version"), timeout)?;
    Ok(json!({
        "program": program,
        "hash": hash,
        "version": String::from_utf8_lossy(&output.stdout),
    }))
}

struct Validation {
    linker: Recipe,
    runner: Recipe,
    validator: Option<Recipe>,
    directory: PathBuf,
    timeout: Duration,
}

impl Validation {
    fn verify(&self) -> Result<()> {
        self.linker
            .measured()
            .run(&self.directory.join("link"), self.timeout)?;
        let output = self
            .runner
            .measured()
            .run(&self.directory.join("run"), self.timeout)?;
        verify_output(
            &self.validator,
            &self.runner.args,
            &output.stdout,
            self.timeout,
        )
    }
}

fn prepare_inputs(
    set: &CompilerSet,
    program: &Program,
    prepared: &Prepared,
    level: &str,
    scratch: &Path,
    timeout: Duration,
) -> Result<Vec<PathBuf>> {
    if !set.llvm {
        return Ok(prepared.sources.clone());
    }
    prepared
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let output = scratch.join(format!("input-{index}.ll"));
            BenchCommand::new("clang")
                .current_dir(&prepared.directory)
                .args(["-std=gnu17", level])
                .args(program.flags.iter().copied())
                .args([
                    "-fno-vectorize",
                    "-fno-slp-vectorize",
                    "-S",
                    "-emit-llvm",
                    "-Xclang",
                    "-disable-O0-optnone",
                ])
                .arg(source.strip_prefix(&prepared.directory)?)
                .arg("-o")
                .arg(output.as_os_str())
                .run(&scratch.join(format!("prepare-ir-{index}")), timeout)?;
            Ok(output)
        })
        .collect()
}

fn parse_phases(stderr: &str) -> std::collections::BTreeMap<String, f64> {
    stderr
        .lines()
        .filter(|line| {
            line.starts_with("fcc-time: frontend_ms=") || line.starts_with("tir-time: import_ms=")
        })
        .filter_map(|line| line.split_once(": ").map(|(_, values)| values))
        .flat_map(str::split_whitespace)
        .filter_map(|field| {
            let (name, value) = field.split_once('=')?;
            let value = value.parse::<f64>().ok()?;
            (value.is_finite() && value >= 0.0).then(|| (name.to_owned(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_filters_cases_without_preparing_sources() -> Result<()> {
        use clap::Parser;

        let program = Program {
            name: "example",
            sources: vec!["nested/a.c"],
            separate: true,
            ..Program::default()
        };
        let set = CompilerSet::source("nonexistent-compiler");
        for (filter, expected) in [
            ("unrelated", false),
            ("{*/compile/nested/a,*/run/nested/a}", true),
        ] {
            let options = crate::Options::try_parse_from(["bench", "--list", "--filter", filter])?;
            let mut suite = Suite::new("fixture/programs", options)?;
            assert_eq!(
                select(&mut suite, &set, &program, &["fcc"], &["-O2"])?,
                expected
            );
            let options = crate::Options::try_parse_from(["bench", "--list", "--filter", filter])?;
            let mut suite = Suite::new("fixture/programs", options)?;
            // No valid source directory or compiler exists for this declaration.
            register_program(&mut suite, &set, Path::new("missing"), &program)?;
        }
        Ok(())
    }

    #[test]
    fn phase_summary_does_not_mix_pass_counters_with_milliseconds() {
        let phases = parse_phases(
            "fcc-time: frontend_ms=1.0 passes_ms=2.0 backend_ms=3.0\ntir-time: threads=1 wall_ms=4.0 passes_ms=8.0\ntir-time: pass name=lower total_ms=0.1 runs=7\n",
        );
        assert_eq!(phases.len(), 3);
        assert_eq!(phases["passes_ms"], 2.0);
        assert_eq!(phases["backend_ms"], 3.0);
    }

    #[test]
    fn prepared_source_order_keeps_declared_case_names() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::create_dir(root.join("nested"))?;
        for source in ["z.c", "nested/a.c"] {
            std::fs::write(root.join(source), "int main(void) { return 0; }")?;
        }
        let program = Program {
            name: "example",
            resources: root.to_owned(),
            sources: vec!["z.c", "nested/a.c"],
            definition: root.join("definition.rs"),
            separate: true,
            ..Program::default()
        };
        let prepared = program.prepare(root, root, true, Duration::from_secs(1))?;
        let cases = CaseIds::new(&CompilerSet::source("fcc"), &program, "fcc", "-O2");
        let declared = program
            .sources
            .iter()
            .map(|source| cases.compile(Path::new(source)))
            .collect::<Vec<_>>();
        let prepared_ids = prepared
            .sources
            .iter()
            .map(|source| cases.compile(source.strip_prefix(root).unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(prepared_ids, [declared[1].clone(), declared[0].clone()]);
        assert_eq!(prepared_ids[0], "fcc/example/source/O2/compile/nested/a");
        assert_eq!(
            cases.run(Path::new("nested/a.c")),
            "fcc/example/source/O2/run/nested/a"
        );
        Ok(())
    }

    #[test]
    fn validation_cache_only_remembers_successful_outputs() -> Result<()> {
        let cache = MemoizedValidation::new("original".into());
        cache.verify("original", || {
            panic!("unchanged output is already validated")
        })?;
        assert!(
            cache
                .verify("changed", || anyhow::bail!("invalid output"))
                .is_err()
        );
        let validations = std::cell::Cell::new(0);
        let validate = || {
            validations.set(validations.get() + 1);
            Ok(())
        };
        cache.verify("changed", validate)?;
        cache.verify("changed", validate)?;
        cache.verify("original", validate)?;
        assert_eq!(validations.get(), 2);
        Ok(())
    }

    #[test]
    fn input_identity_tracks_headers_parameters_and_relative_names() -> Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        for directory in [first.path(), second.path()] {
            std::fs::write(directory.join("input.c"), "#include \"input.h\"\n")?;
            std::fs::write(directory.join("input.h"), "#define N 42\n")?;
        }
        let files = |root: &Path| vec![root.join("input.c"), root.join("input.h")];
        let original = digest_inputs(first.path(), &files(first.path()), &["1000".into()])?;
        assert_eq!(
            original,
            digest_inputs(second.path(), &files(second.path()), &["1000".into()])?
        );
        assert_ne!(
            original,
            digest_inputs(first.path(), &files(first.path()), &["1001".into()])?
        );
        std::fs::write(first.path().join("input.h"), "#define N 43\n")?;
        assert_ne!(
            original,
            digest_inputs(first.path(), &files(first.path()), &["1000".into()])?
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn checked_captures_stdout_and_bounds_preparation() {
        let output = checked(
            Command::new("/bin/sh").args(["-c", "printf prepared"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.stdout, b"prepared");
        let start = std::time::Instant::now();
        let error = checked(
            Command::new("/bin/sh").args(["-c", "sleep 30 & wait"]),
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn failed_preparation_is_an_error() {
        assert!(
            checked(
                Command::new("false").arg("preparation"),
                Duration::from_secs(5)
            )
            .is_err()
        );
    }
}
