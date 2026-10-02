//! External programs as benchmarks: pinned sources, compile cases and run cases.
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::sources::GitSource;
use crate::{Command as BenchCommand, Harness, Phase, ProcessCase, Variant};

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

/// Replaces the declared source list after checkout. Receives the workspace root
/// and the source directory, and returns the sources and extra workload dependencies.
pub type PrepareSources = fn(&Path, &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)>;

/// An external program, measured as one compile case per source and a run case.
pub struct Program {
    pub name: &'static str,
    pub sources: Vec<&'static str>,
    pub flags: Vec<&'static str>,
    pub link_flags: Vec<&'static str>,
    pub args: Vec<&'static str>,
    /// Workspace-relative directory of local sources and the validator with its data.
    resources: PathBuf,
    /// The file that declares this program. Editing it invalidates recorded baselines.
    definition: &'static str,
    git: Option<GitSource>,
    validator: Option<&'static str>,
    llvm: bool,
    separate: bool,
    prepare_sources: Option<PrepareSources>,
}

impl Program {
    /// Declare a program whose sources are in `resources`, a directory relative
    /// to the workspace root.
    #[track_caller]
    pub fn new(name: &'static str, resources: impl Into<PathBuf>) -> Self {
        Self {
            name,
            sources: Vec::new(),
            flags: Vec::new(),
            link_flags: Vec::new(),
            args: Vec::new(),
            resources: resources.into(),
            definition: std::panic::Location::caller().file(),
            git: None,
            validator: None,
            llvm: false,
            separate: false,
            prepare_sources: None,
        }
    }

    /// Take sources from a pinned commit. `resources` then holds only the validator.
    pub fn git(mut self, repository: &'static str, revision: &'static str) -> Self {
        self.git = Some(GitSource {
            repository,
            revision,
            subdir: "",
        });
        self
    }

    /// Check out one directory of the Git source and resolve sources inside it.
    pub fn subdir(mut self, subdir: &'static str) -> Self {
        self.git
            .as_mut()
            .expect("subdir narrows a Git source")
            .subdir = subdir;
        self
    }

    pub fn sources(mut self, sources: impl IntoIterator<Item = &'static str>) -> Self {
        self.sources = sources.into_iter().collect();
        self
    }

    /// Compiler flags for every source.
    pub fn flags(mut self, flags: impl IntoIterator<Item = &'static str>) -> Self {
        self.flags = flags.into_iter().collect();
        self
    }

    pub fn link_flags(mut self, flags: impl IntoIterator<Item = &'static str>) -> Self {
        self.link_flags = flags.into_iter().collect();
        self
    }

    /// Arguments of the measured run. Filtering never changes them.
    pub fn args(mut self, args: impl IntoIterator<Item = &'static str>) -> Self {
        self.args = args.into_iter().collect();
        self
    }

    /// Check the output of every run with this Python script from `resources`.
    pub fn verify(mut self, script: &'static str) -> Self {
        self.validator = Some(script);
        self
    }

    /// Also measure the `tir` backend on the LLVM IR that Clang emits for this program.
    pub fn llvm(mut self) -> Self {
        self.llvm = true;
        self
    }

    /// Link and run every source as a program of its own.
    pub fn separate(mut self) -> Self {
        self.separate = true;
        self
    }

    pub fn prepare_sources(mut self, prepare: PrepareSources) -> Self {
        self.prepare_sources = Some(prepare);
        self
    }

    /// Resolve the source directory and the files this workload depends on,
    /// fetching the pinned commit unless `offline`.
    pub fn prepare(
        &self,
        root: &Path,
        cache: &Path,
        offline: bool,
        timeout: std::time::Duration,
    ) -> Result<Prepared> {
        let local = root.join(&self.resources);
        let directory = match &self.git {
            None => local.clone(),
            Some(source) => source.prepare(cache, offline, timeout)?,
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
        dependencies.push(normalized(&root.join(self.definition)));
        dependencies.sort();
        dependencies.dedup();
        Ok(Prepared {
            directory,
            sources,
            dependencies,
        })
    }
}

pub struct Prepared {
    pub directory: PathBuf,
    pub sources: Vec<PathBuf>,
    pub dependencies: Vec<PathBuf>,
}

/// Resolve `..` lexically. A definition shared by another package arrives as
/// `tools/../fcc/benches/x.rs` and must keep one workspace-relative identity.
fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        if component == Component::ParentDir {
            result.pop();
        } else {
            result.push(component);
        }
    }
    result
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

/// A compiler that takes part in a program benchmark.
#[derive(Clone, Copy)]
struct Compiler {
    name: &'static str,
    /// Executable of a reference compiler. `None` is the candidate that Cargo
    /// built from this checkout, the only compiler whose cases gate.
    reference: Option<&'static str>,
    flags: &'static [&'static str],
    /// Measured only when `--compiler` names it.
    opt_in: bool,
}

impl Compiler {
    fn candidate(&self) -> bool {
        self.reference.is_none()
    }
}

const SOURCE_COMPILERS: [Compiler; 4] = [
    Compiler {
        name: "fcc",
        reference: None,
        flags: &[],
        opt_in: false,
    },
    Compiler {
        name: "gcc",
        reference: Some("gcc"),
        flags: &[],
        opt_in: false,
    },
    Compiler {
        name: "clang",
        reference: Some("clang"),
        flags: &[],
        opt_in: false,
    },
    Compiler {
        name: "clang-scalar",
        reference: Some("clang"),
        flags: &["-fno-vectorize", "-fno-slp-vectorize"],
        opt_in: true,
    },
];

const LLVM_COMPILERS: [Compiler; 3] = [
    Compiler {
        name: "tir",
        reference: None,
        flags: &[],
        opt_in: false,
    },
    Compiler {
        name: "clang-ir",
        reference: Some("clang"),
        flags: &[],
        opt_in: false,
    },
    Compiler {
        name: "clang-ir-backend",
        reference: Some("clang"),
        flags: &["-Xclang", "-disable-llvm-passes"],
        opt_in: false,
    },
];

/// Flags Clang uses to produce the shared LLVM IR input of every LLVM-mode compiler.
const LLVM_PRODUCER_FLAGS: [&str; 6] = [
    "-fno-vectorize",
    "-fno-slp-vectorize",
    "-S",
    "-emit-llvm",
    "-Xclang",
    "-disable-O0-optnone",
];

/// The Cargo-built candidate and the form of input it compiles.
#[derive(Clone)]
pub(crate) struct CompilerSet {
    candidate: PathBuf,
    /// The candidate is `tir`, compiling LLVM IR. Otherwise it is `fcc`, compiling C.
    llvm: bool,
}

impl CompilerSet {
    pub(crate) fn source(candidate: impl Into<PathBuf>) -> Self {
        Self {
            candidate: candidate.into(),
            llvm: false,
        }
    }
    pub(crate) fn llvm(candidate: impl Into<PathBuf>) -> Self {
        Self {
            candidate: candidate.into(),
            llvm: true,
        }
    }

    fn mode(&self) -> &'static str {
        if self.llvm { "llvm" } else { "source" }
    }

    /// The candidate and its references, or the compilers named by `--compiler`.
    fn compilers(&self, requested: Option<&str>) -> Vec<Compiler> {
        let all: &[Compiler] = if self.llvm {
            &LLVM_COMPILERS
        } else {
            &SOURCE_COMPILERS
        };
        all.iter()
            .filter(|compiler| match requested {
                None => !compiler.opt_in,
                Some(requested) => requested.split(',').any(|name| name == compiler.name),
            })
            .copied()
            .collect()
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

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        arch => arch,
    }
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

/// Identifiers of one compiler's cases at one optimization level.
struct CaseIds {
    base: String,
    /// The program and level, which name a benchmark for every compiler.
    shared: String,
    compiler: Compiler,
    llvm: bool,
    separate: bool,
}

impl CaseIds {
    fn new(set: &CompilerSet, program: &Program, compiler: &Compiler, level: &str) -> Self {
        let level = level.trim_start_matches('-');
        Self {
            base: format!("{}/{}/{}/{level}", compiler.name, program.name, set.mode()),
            shared: format!("{}/{level}", program.name),
            compiler: *compiler,
            llvm: set.llvm,
            separate: program.separate,
        }
    }

    /// The comparison of this compiler's case with the same case of the others.
    fn variant(&self, phase: &str, source: Option<&Path>) -> Variant {
        let source = source.map(|source| format!("/{}", source.with_extension("").display()));
        Variant {
            benchmark: format!("{}{}", self.shared, source.unwrap_or_default()),
            group: if self.llvm {
                format!("{phase} from LLVM IR")
            } else {
                phase.into()
            },
            variant: self.compiler.name.into(),
            subject: self.compiler.candidate(),
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

/// Which case kinds `--phase` leaves enabled, as (compile, run).
fn phases(phase: Phase) -> (bool, bool) {
    (phase != Phase::Run, phase != Phase::Compile)
}

/// Measure `program` with the target's compilers, or list its cases. Nothing is
/// fetched or compiled unless a case passes the filter.
pub(crate) fn measure(harness: &mut Harness, program: &Program) -> Result<()> {
    if harness.skips_programs() {
        return Ok(());
    }
    let set = harness
        .compilers
        .clone()
        .context("program benchmarks need the package's Cargo-built fcc or tir binary")?;
    if set.llvm && !program.llvm {
        return Ok(());
    }
    let compilers = set.compilers(harness.options.compiler.as_deref());
    let levels = ["-O0", "-O2"]
        .into_iter()
        .filter(|level| {
            harness.options.level.as_ref().is_none_or(|wanted| {
                wanted.trim_start_matches('-') == level.trim_start_matches('-')
            })
        })
        .collect::<Vec<_>>();
    let selected = select(harness, &set, program, &compilers, &levels)?;
    if harness.listing() || !selected {
        return Ok(());
    }
    let root = harness.workspace.clone();
    let timeout = harness.timeout();
    let prepared = program.prepare(
        &root,
        &harness.source_cache(),
        harness.options.offline,
        timeout,
    )?;
    let validator = program.validator.map(|name| Recipe {
        executable: "python3".into(),
        args: vec![root.join(&program.resources).join(name).into_os_string()],
        cwd: root.clone(),
    });
    let (compile_enabled, run_enabled) = phases(harness.options.phase);
    for level in levels {
        // A compile case needs its own source. A run case needs every source
        // of its program, which is one source when programs are separate.
        let wanted = |compiler: &Compiler, index: usize| {
            let ids = CaseIds::new(&set, program, compiler, level);
            let source = prepared.sources[index]
                .strip_prefix(&prepared.directory)
                .expect("prepared source belongs to its directory");
            (compile_enabled && harness.matches(&ids.compile(source)))
                || (run_enabled && harness.matches(&ids.run(source)))
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
        let scratch = harness
            .bundle()?
            .join("programs")
            .join(program.name)
            .join(level.trim_start_matches('-'));
        std::fs::create_dir_all(&scratch)?;
        let build = Build {
            set: &set,
            program,
            prepared: &prepared,
            inputs: &prepare_inputs(&set, program, &prepared, level, &scratch, timeout)?,
            level,
            validator: &validator,
            timeout,
        };
        let metadata = build.metadata(&root, &scratch)?;
        // Compilers are interleaved within a group: one group per source for
        // compilation and one per linked program for its run.
        let mut compile_groups: Vec<Vec<ProcessCase>> =
            build.inputs.iter().map(|_| Vec::new()).collect();
        let mut run_groups: Vec<Vec<ProcessCase>> =
            (0..build.programs()).map(|_| Vec::new()).collect();
        for (compiler, selected_indices) in compilers.iter().zip(&selected_indices) {
            if selected_indices.is_empty() {
                continue;
            }
            let cases = build.cases(compiler, selected_indices, &scratch, &metadata)?;
            for (group, case) in cases.compile {
                if compile_enabled && harness.matches(&case.id) {
                    compile_groups[group].push(case);
                }
            }
            for (group, case) in cases.run {
                if run_enabled && harness.matches(&case.id) {
                    run_groups[group].push(case);
                }
            }
        }
        for group in compile_groups.into_iter().chain(run_groups) {
            if !group.is_empty() {
                harness.process_group(group)?;
            }
        }
    }
    Ok(())
}

/// Report whether the filter selects any case of the program, listing the
/// selected cases in list mode.
fn select(
    harness: &mut Harness,
    set: &CompilerSet,
    program: &Program,
    compilers: &[Compiler],
    levels: &[&str],
) -> Result<bool> {
    let (compile_enabled, run_enabled) = phases(harness.options.phase);
    let mut selected = false;
    for compiler in compilers {
        for level in levels {
            let cases = CaseIds::new(set, program, compiler, level);
            let sources = || program.sources.iter().map(Path::new);
            let mut ids = Vec::new();
            if compile_enabled {
                ids.extend(sources().map(|source| cases.compile(source)));
            }
            if run_enabled && program.separate {
                ids.extend(sources().map(|source| cases.run(source)));
            } else if run_enabled {
                ids.push(cases.run(Path::new("")));
            }
            for id in ids {
                if harness.matches(&id) {
                    selected = true;
                    if harness.listing() {
                        harness.list_case(&id)?;
                    }
                }
            }
        }
    }
    Ok(selected)
}

/// Cases of one compiler, each tagged with the group it is interleaved in.
struct Cases {
    compile: Vec<(usize, ProcessCase)>,
    run: Vec<(usize, ProcessCase)>,
}

/// One program at one optimization level, with its inputs ready to compile.
struct Build<'a> {
    set: &'a CompilerSet,
    program: &'a Program,
    prepared: &'a Prepared,
    /// What the compilers read: the C sources, or LLVM IR generated from them.
    inputs: &'a [PathBuf],
    level: &'a str,
    validator: &'a Option<Recipe>,
    timeout: Duration,
}

impl Build<'_> {
    /// Number of linked programs: one, or one per source when they are separate.
    fn programs(&self) -> usize {
        if self.program.separate {
            self.inputs.len()
        } else {
            1
        }
    }

    /// FCC links its own objects. Every other compiler's objects go through a C driver.
    fn links_itself(&self, compiler: &Compiler) -> bool {
        compiler.candidate() && !self.set.llvm
    }

    fn compile(&self, compiler: &Compiler, input: &Path, output: &Path) -> Recipe {
        let mut args: Vec<OsString> = Vec::new();
        if compiler.candidate() && self.set.llvm {
            args.extend(["mc", "--march", host_arch(), "--filetype", "obj"].map(OsString::from));
        } else {
            if !self.set.llvm {
                args.push("-std=gnu17".into());
            }
            args.push(self.level.into());
            if !self.set.llvm {
                args.extend(self.program.flags.iter().map(OsString::from));
            }
            args.extend(compiler.flags.iter().map(OsString::from));
            args.push("-c".into());
        }
        args.extend([
            input.as_os_str().to_owned(),
            "-o".into(),
            output.as_os_str().to_owned(),
        ]);
        Recipe {
            executable: compiler
                .reference
                .map_or_else(|| self.set.candidate.clone(), PathBuf::from),
            args,
            cwd: self.prepared.directory.clone(),
        }
    }

    fn link(&self, compiler: &Compiler, objects: &[PathBuf], output: &Path) -> Recipe {
        let mut args = Vec::new();
        if self.set.llvm {
            args.push("-no-pie".into());
        }
        args.extend(objects.iter().map(|path| path.as_os_str().to_owned()));
        args.extend(self.program.link_flags.iter().map(OsString::from));
        args.extend(["-o".into(), output.as_os_str().to_owned()]);
        Recipe {
            executable: if self.links_itself(compiler) {
                self.set.candidate.clone()
            } else {
                compiler.reference.unwrap_or("clang").into()
            },
            args,
            cwd: self.prepared.directory.clone(),
        }
    }

    /// Workload identity shared by every compiler. Baselines with a different
    /// identity are rejected, so it covers everything that defines the work.
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
        let mut parameters = vec![
            "program-contract-v1".into(),
            set.mode().into(),
            level.into(),
            format!("{}", program.separate),
        ];
        parameters.extend(
            program
                .flags
                .iter()
                .chain(&program.link_flags)
                .chain(&program.args)
                .map(|s| (*s).to_owned()),
        );
        if let Some(source) = &program.git {
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
        let mut producer_flags = vec!["-std=gnu17", level];
        producer_flags.extend(LLVM_PRODUCER_FLAGS);
        Ok(json!({
            "workload_digest": workload_digest,
            "contract_version": 1,
            "input": set.mode(),
            "level": level,
            "args": program.args,
            "flags": program.flags,
            "link_flags": program.link_flags,
            "llvm_digest": llvm_digest,
            "llvm_producer_version": producer_version,
            "llvm_producer_flags": producer_flags,
            "build": crate::build_configuration(),
            "validation": "outside measurement",
        }))
    }

    /// Compile the selected objects once outside measurement, which also
    /// records the candidate's phase timings, and describe the compiler.
    fn prepare_objects(
        &self,
        compiler: &Compiler,
        recipes: &[Recipe],
        selected_indices: &[usize],
        directory: &Path,
        mut metadata: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut phase_timings = serde_json::Map::new();
        for (index, recipe) in recipes.iter().enumerate() {
            if !self.program.separate || selected_indices.contains(&index) {
                let mut diagnostic = recipe.measured();
                if compiler.candidate() {
                    diagnostic = diagnostic.env("TIR_TIME_PASSES", "1");
                }
                let output =
                    diagnostic.run(&directory.join(format!("prepare-{index}")), self.timeout)?;
                let phases = parse_phases(&std::fs::read_to_string(output.stderr)?);
                if !phases.is_empty() {
                    phase_timings.insert(index.to_string(), json!(phases));
                }
            }
        }
        let tool = &recipes[0].executable;
        let version = if self.links_itself(compiler) {
            format!("fcc package {}", env!("CARGO_PKG_VERSION"))
        } else {
            String::from_utf8_lossy(
                &checked(Command::new(tool).arg("--version"), self.timeout)?.stdout,
            )
            .into_owned()
        };
        let (tool_path, tool_hash) = executable_identity(tool)?;
        metadata["compiler"] = json!(compiler.name);
        metadata["provenance"] = json!({
            "compiler_path": tool_path,
            "compiler_hash": tool_hash,
            "compiler_version": version,
            "phase_timings_ms": phase_timings,
            "phase_scope": "separate instrumented preparation invocation",
        });
        if !compiler.candidate() {
            metadata["reference_compiler_hash"] = json!(tool_hash);
            metadata["reference_compiler_version"] = json!(version);
        }
        // FCC is the direct link driver and delegates system linking to cc.
        let system_linker = if self.links_itself(compiler) {
            "cc"
        } else {
            compiler.reference.unwrap_or("clang")
        };
        metadata["link_driver"] = json!(if self.links_itself(compiler) {
            "fcc"
        } else {
            system_linker
        });
        metadata["system_linker_driver"] = reference_tool(system_linker, self.timeout)?;
        Ok(metadata)
    }

    /// Build, link and validate with one compiler, then describe its measured cases.
    fn cases(
        &self,
        compiler: &Compiler,
        selected_indices: &[usize],
        scratch: &Path,
        base_metadata: &serde_json::Value,
    ) -> Result<Cases> {
        let Self {
            set,
            program,
            prepared,
            inputs,
            level,
            validator,
            timeout,
        } = *self;
        let directory = scratch.join(compiler.name);
        std::fs::create_dir_all(&directory)?;
        let objects = (0..inputs.len())
            .map(|index| directory.join(format!("{index}.o")))
            .collect::<Vec<_>>();
        let recipes = inputs
            .iter()
            .zip(&objects)
            .map(|(input, output)| self.compile(compiler, input, output))
            .collect::<Vec<_>>();
        let metadata = self.prepare_objects(
            compiler,
            &recipes,
            selected_indices,
            &directory,
            base_metadata.clone(),
        )?;
        let args = program.args.iter().map(OsString::from).collect::<Vec<_>>();
        let ids = CaseIds::new(set, program, compiler, level);
        let mut cases = Cases {
            compile: Vec::new(),
            run: Vec::new(),
        };
        for group in 0..self.programs() {
            if program.separate && !selected_indices.contains(&group) {
                continue;
            }
            let members = if program.separate {
                group..group + 1
            } else {
                0..inputs.len()
            };
            let executable = directory.join(format!("program-{group}"));
            let runner = Recipe {
                executable: executable.clone(),
                args: args.clone(),
                cwd: prepared.directory.clone(),
            };
            let validation = Arc::new(Validation {
                linker: self.link(compiler, &objects[members.clone()], &executable),
                runner: runner.clone(),
                validator: validator.clone(),
                directory: directory.join(format!("verify-{group}")),
                timeout,
            });
            // Run validation even for compile-only selections before sampling.
            validation.verify()?;
            let verifier = validator.clone();
            let runtime_args = args.clone();
            let source = prepared.sources[group].strip_prefix(&prepared.directory)?;
            cases.run.push((
                group,
                ProcessCase {
                    id: ids.run(source),
                    variant: Some(ids.variant("Run", program.separate.then_some(source))),
                    command: runner.measured(),
                    verify: Some(Box::new(move |stdout| {
                        verify_output(&verifier, &runtime_args, stdout, timeout)
                    })),
                    metadata: metadata.clone(),
                    gate: compiler.candidate(),
                },
            ));
            for index in members {
                let validation = Arc::clone(&validation);
                let object = objects[index].clone();
                let validated = MemoizedValidation::new(object_digest(&object)?);
                let source = prepared.sources[index].strip_prefix(&prepared.directory)?;
                cases.compile.push((
                    index,
                    ProcessCase {
                        id: ids.compile(source),
                        variant: Some(ids.variant("Compile", Some(source))),
                        // A reference driver execs its real compiler, which must be counted too.
                        command: recipes[index]
                            .measured()
                            .trace_children(!compiler.candidate()),
                        verify: Some(Box::new(move |_| {
                            validated.verify(&object_digest(&object)?, || validation.verify())
                        })),
                        metadata: metadata.clone(),
                        gate: compiler.candidate(),
                    },
                ));
            }
        }
        Ok(cases)
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

/// The compilers' inputs: the C sources, or LLVM IR that Clang emits from them.
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
                .args(LLVM_PRODUCER_FLAGS)
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

        let program = Program::new("example", "missing")
            .sources(["nested/a.c"])
            .separate();
        let set = CompilerSet::source("nonexistent-compiler");
        for (filter, expected) in [("unrelated", false), ("/(compile|run)/nested/a$", true)] {
            let options = crate::Options::try_parse_from(["bench", "--list", filter])?;
            let mut harness = Harness::new("fixture/programs", options)?;
            harness.compilers = Some(set.clone());
            assert_eq!(
                select(
                    &mut harness,
                    &set,
                    &program,
                    &SOURCE_COMPILERS[..1],
                    &["-O2"]
                )?,
                expected
            );
            let options = crate::Options::try_parse_from(["bench", "--list", filter])?;
            let mut harness = Harness::new("fixture/programs", options)?;
            harness.compilers = Some(set.clone());
            // No valid source directory or compiler exists for this declaration.
            measure(&mut harness, &program)?;
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
        let directory = tir_adt::TempDir::new()?;
        let root = directory.path();
        std::fs::create_dir(root.join("nested"))?;
        for source in ["z.c", "nested/a.c"] {
            std::fs::write(root.join(source), "int main(void) { return 0; }")?;
        }
        let program = Program::new("example", root)
            .sources(["z.c", "nested/a.c"])
            .separate();
        let prepared = program.prepare(root, root, true, Duration::from_secs(1))?;
        let cases = CaseIds::new(
            &CompilerSet::source("fcc"),
            &program,
            &SOURCE_COMPILERS[0],
            "-O2",
        );
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
        let first = tir_adt::TempDir::new()?;
        let second = tir_adt::TempDir::new()?;
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
