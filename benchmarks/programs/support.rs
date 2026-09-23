use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tir_bench::program::{checked, digest_inputs, executable_identity};
use tir_bench::{Command, Phase, ProcessCase, Result, Suite};

use super::{Prepared, Program, Source};

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
    fn std(&self) -> StdCommand {
        let mut command = StdCommand::new(&self.executable);
        command.args(&self.args).current_dir(&self.cwd);
        command
    }
    fn measured(&self) -> Command {
        Command::new(&self.executable)
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
        let path = root
            .join("benchmarks/programs")
            .join(program.name)
            .join(name);
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

pub(super) fn register(
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
    let mode = if set.llvm { "llvm" } else { "source" };
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
            let base = case_prefix(set, program, compiler, level);
            let source = prepared.sources[index]
                .strip_prefix(&prepared.directory)
                .unwrap()
                .with_extension("");
            let run = if program.separate {
                format!("{base}/run/{}", source.display())
            } else {
                format!("{base}/run")
            };
            (compile_enabled && suite.matches(&format!("{base}/compile/{}", source.display())))
                || (run_enabled && suite.matches(&run))
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
        dependencies.push(root.join("benchmarks/programs/support.rs"));
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
            Some(digest_inputs(&scratch, &inputs, &parameters)?)
        } else {
            None
        };
        let producer_version = if set.llvm {
            Some(
                String::from_utf8_lossy(
                    &checked(StdCommand::new("clang").arg("--version"), timeout)?.stdout,
                )
                .into_owned(),
            )
        } else {
            None
        };
        let mut groups = CaseGroups {
            compile: (0..inputs.len()).map(|_| Vec::new()).collect(),
            run: (0..if program.separate { inputs.len() } else { 1 })
                .map(|_| Vec::new())
                .collect(),
        };
        let metadata = json!({"workload_digest":workload_digest, "contract_version":1, "input":mode, "level":level, "args":program.args, "flags":program.flags, "link_flags":program.link_flags, "llvm_digest":llvm_digest, "llvm_producer_version":producer_version, "llvm_producer_flags":["-std=gnu17",level,"-fno-vectorize","-fno-slp-vectorize","-S","-emit-llvm","-Xclang","-disable-O0-optnone"], "build":tir_bench::build_configuration(), "validation":"outside measurement"});
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

fn case_prefix(set: &CompilerSet, program: &Program, compiler: &str, level: &str) -> String {
    let mode = if set.llvm { "llvm" } else { "source" };
    format!(
        "{compiler}/{}/{mode}/{}",
        program.name,
        level.trim_start_matches('-')
    )
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
            let base = case_prefix(set, program, compiler, level);
            let mut ids = Vec::new();
            if compile_enabled {
                ids.extend(
                    program
                        .sources
                        .iter()
                        .map(|source| format!("{base}/compile/{}", source.trim_end_matches(".c"))),
                );
            }
            if run_enabled {
                if program.separate {
                    ids.extend(
                        program
                            .sources
                            .iter()
                            .map(|source| format!("{base}/run/{}", source.trim_end_matches(".c"))),
                    );
                } else {
                    ids.push(format!("{base}/run"));
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
    fn prepare_compiler(
        &self,
        suite: &Suite,
        compiler: &str,
        selected_indices: &[usize],
        scratch: &Path,
        base_metadata: &serde_json::Value,
        groups: &mut CaseGroups,
    ) -> Result<()> {
        let set = self.set;
        let program = self.program;
        let prepared = self.prepared;
        let inputs = self.inputs;
        let level = self.level;
        let validator = self.validator;
        let args = self.args;
        let timeout = self.timeout;
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
        let mut phase_timings = serde_json::Map::new();
        for (index, recipe) in recipes.iter().enumerate() {
            if !program.separate || selected_indices.contains(&index) {
                let diagnostic = recipe.measured();
                let diagnostic = if matches!(compiler, "fcc" | "tir") {
                    diagnostic.env("TIR_TIME_PASSES", "1")
                } else {
                    diagnostic
                };
                let output =
                    diagnostic.run(&directory.join(format!("prepare-{index}")), timeout)?;
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
                &checked(StdCommand::new(tool).arg("--version"), timeout)?.stdout,
            )
            .into_owned()
        };
        let (tool_path, tool_hash) = executable_identity(tool)?;
        let mut metadata = base_metadata.clone();
        metadata["compiler"] = json!(compiler);
        metadata["provenance"] = json!({"compiler_path":tool_path, "compiler_hash":tool_hash, "compiler_version":version, "phase_timings_ms":phase_timings, "phase_scope":"separate instrumented preparation invocation"});
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
        metadata["system_linker_driver"] = reference_tool(linker, timeout)?;
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
            let validation = Arc::new((
                linker,
                runner.clone(),
                (*validator).clone(),
                directory.join(format!("verify-{group}")),
                timeout,
            ));
            // Run validation even for compile-only selections before sampling.
            validate_program(&validation)?;
            let base = case_prefix(set, program, compiler, level);
            let suffix = if program.separate {
                format!(
                    "/{}",
                    prepared.sources[group]
                        .strip_prefix(&prepared.directory)?
                        .with_extension("")
                        .display()
                )
            } else {
                String::new()
            };
            let run_id = format!("{base}/run{suffix}");
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
                let source = prepared.sources[index]
                    .strip_prefix(&prepared.directory)?
                    .with_extension("");
                let id = format!("{base}/compile/{}", source.display());
                if compile_enabled && suite.matches(&id) {
                    let validation = Arc::clone(&validation);
                    let object = objects[index].clone();
                    let validated = MemoizedValidation::new(object_digest(&object)?);
                    let trace_children = !matches!(compiler, "fcc" | "tir");
                    groups.compile[index].push(ProcessCase {
                        id,
                        command: recipes[index].measured().trace_children(trace_children),
                        verify: Some(Box::new(move |_| {
                            validated
                                .verify(&object_digest(&object)?, || validate_program(&validation))
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
    let output = checked(StdCommand::new(program).arg("--version"), timeout)?;
    Ok(json!({"program":program, "hash":hash, "version":String::from_utf8_lossy(&output.stdout)}))
}

type Validation = (Recipe, Recipe, Option<Recipe>, PathBuf, Duration);

fn validate_program(validation: &Validation) -> Result<()> {
    let (linker, runner, validator, directory, timeout) = validation;
    linker.measured().run(&directory.join("link"), *timeout)?;
    let output = runner.measured().run(&directory.join("run"), *timeout)?;
    verify_output(validator, &runner.args, &output.stdout, *timeout)
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
            Command::new("clang")
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
    #[test]
    fn unrelated_filter_does_not_prepare_torture_and_brace_filters_select_it() {
        use clap::Parser;
        let program = super::super::torture::definition();
        let set = super::CompilerSet::source("fcc");
        let options =
            tir_bench::Options::try_parse_from(["bench", "--list", "--filter", "*large_asm*"])
                .unwrap();
        let mut suite = tir_bench::Suite::new("fcc/programs", options).unwrap();
        assert!(!super::select(&mut suite, &set, &program, &["fcc"], &["-O2"]).unwrap());
        let filter = format!(
            "{{*dhrystone*/run,*torture*/run/{}}}",
            program.sources[0].trim_end_matches(".c")
        );
        let options =
            tir_bench::Options::try_parse_from(["bench", "--list", "--filter", &filter]).unwrap();
        let mut suite = tir_bench::Suite::new("fcc/programs", options).unwrap();
        assert!(super::select(&mut suite, &set, &program, &["fcc"], &["-O2"]).unwrap());
    }

    #[test]
    fn phase_summary_does_not_mix_pass_counters_with_milliseconds() {
        let phases = super::parse_phases(
            "fcc-time: frontend_ms=1.0 passes_ms=2.0 backend_ms=3.0\ntir-time: threads=1 wall_ms=4.0 passes_ms=8.0\ntir-time: pass name=lower total_ms=0.1 runs=7\n",
        );
        assert_eq!(phases.len(), 3);
        assert_eq!(phases["passes_ms"], 2.0);
        assert_eq!(phases["backend_ms"], 3.0);
    }
    #[test]
    fn validation_cache_skips_unchanged_and_retries_rejected_outputs() -> tir_bench::Result<()> {
        let validated = super::MemoizedValidation::new("initial".into());
        let checks = std::cell::Cell::new(0);
        let accept = || {
            checks.set(checks.get() + 1);
            Ok(())
        };
        validated.verify("initial", accept)?;
        assert_eq!(checks.get(), 0);
        validated.verify("changed", accept)?;
        assert_eq!(checks.get(), 1);
        validated.verify("changed", accept)?;
        assert_eq!(checks.get(), 1);
        for expected in [2, 3] {
            let result = validated.verify("rejected", || {
                checks.set(checks.get() + 1);
                anyhow::bail!("invalid generated program")
            });
            assert!(result.is_err());
            assert_eq!(checks.get(), expected);
        }
        validated.verify("changed", accept)?;
        assert_eq!(checks.get(), 3);
        validated.verify("initial", accept)?;
        assert_eq!(checks.get(), 4);
        Ok(())
    }
}
