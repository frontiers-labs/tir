mod model;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use model::{
    Case, CaseResult, CompilerIdentity, HostIdentity, Manifest, Observation, Probe, Report, Stage,
    Status,
};
use sha2::{Digest, Sha256};

const DEFAULT_MANIFEST: &str = "fcc/checks/Inputs/fp/cases.toml";

struct ReferenceEnvironment {
    compiler: CompilerIdentity,
    host: HostIdentity,
    commands: Vec<Vec<String>>,
}

#[derive(clap::Subcommand)]
pub enum Task {
    /// Record pinned GCC observations and provenance.
    Reference {
        /// GCC executable to probe.
        #[arg(long)]
        gcc: PathBuf,
        /// JSON reference report to write.
        #[arg(long)]
        output: PathBuf,
        /// Run one stable case ID.
        #[arg(long)]
        case: Option<String>,
        /// Name a non-default compiler profile explicitly.
        #[arg(long)]
        profile: Option<String>,
        /// Use a non-default case manifest.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Compare saved observations with cumulative stage requirements.
    Check {
        /// Highest cumulative stage to check.
        #[arg(long)]
        stage: Stage,
        /// GCC reference report to compare.
        #[arg(long)]
        reference: PathBuf,
        /// JSON report to write.
        #[arg(long)]
        output: PathBuf,
        /// Run one stable case ID.
        #[arg(long)]
        case: Option<String>,
        /// Use a non-default case manifest.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Summarize a saved report without rerunning any compiler.
    Report {
        /// JSON report to summarize.
        input: PathBuf,
    },
}

pub fn run(root: &Path, task: Task) -> anyhow::Result<()> {
    match task {
        Task::Reference {
            gcc,
            output,
            case,
            profile,
            manifest,
        } => reference(
            root,
            &gcc,
            &output,
            case.as_deref(),
            profile.as_deref(),
            manifest.as_deref(),
        ),
        Task::Check {
            stage,
            reference,
            output,
            case,
            manifest,
        } => check(
            root,
            stage,
            &reference,
            &output,
            case.as_deref(),
            manifest.as_deref(),
        ),
        Task::Report { input } => report(&input),
    }
}

fn check(
    root: &Path,
    stage: Stage,
    reference_path: &Path,
    output_path: &Path,
    case_filter: Option<&str>,
    manifest_path: Option<&Path>,
) -> anyhow::Result<()> {
    let manifest_path = manifest_path
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(DEFAULT_MANIFEST));
    let manifest = read_manifest(&manifest_path)?;
    let manifest_digest =
        digest(&fs::read(&manifest_path).with_context(|| manifest_path.display().to_string())?);

    let selected = manifest
        .cases
        .iter()
        .filter(|case| case.stage <= stage)
        .filter(|case| case_filter.is_none_or(|id| case.id == id))
        .collect::<Vec<_>>();
    anyhow::ensure!(!selected.is_empty(), "no cases selected");

    let reference: Report = serde_json::from_str(
        &fs::read_to_string(reference_path)
            .with_context(|| reference_path.display().to_string())?,
    )
    .with_context(|| reference_path.display().to_string())?;
    anyhow::ensure!(
        reference.schema_version == 1,
        "unsupported report schema version"
    );
    anyhow::ensure!(
        reference.profile == manifest.reference.profile,
        "reference profile mismatch"
    );
    let by_id = reference
        .results
        .iter()
        .map(|result| (result.case_id.as_str(), result))
        .collect::<BTreeMap<_, _>>();
    let manifest_provenance_error = (reference.manifest_digest != manifest_digest).then(|| {
        format!(
            "reference manifest digest mismatch: expected {manifest_digest}, observed {}",
            reference.manifest_digest
        )
    });

    let mut results = Vec::with_capacity(selected.len());
    for case in selected {
        let Some(reference_result) = by_id.get(case.id.as_str()) else {
            results.push(CaseResult {
                case_id: case.id.clone(),
                stage: case.stage,
                compiler: CompilerIdentity {
                    version: manifest.reference.compiler_version.clone(),
                    executable: String::new(),
                },
                source_digest: String::new(),
                commands: Vec::new(),
                exit_status: None,
                observation: None,
                resolved_policy: None,
                artifacts: None,
                status: Status::MissingInfrastructure,
                detail: "case is absent from the reference report".into(),
            });
            continue;
        };
        if matches!(
            reference_result.status,
            Status::UnsupportedCapability | Status::MissingInfrastructure | Status::Fail
        ) {
            results.push((*reference_result).clone());
            continue;
        }
        if let Some(detail) = &manifest_provenance_error {
            let mut result = (*reference_result).clone();
            result.stage = case.stage;
            result.status = Status::Fail;
            result.detail = detail.clone();
            results.push(result);
            continue;
        }
        if let Err(detail) = validate_reference_evidence(
            root,
            &manifest,
            case,
            reference_result,
            &reference.host,
            &reference.profile,
        ) {
            let mut result = (*reference_result).clone();
            result.stage = case.stage;
            result.status = Status::Fail;
            result.detail = detail;
            results.push(result);
            continue;
        }
        if case.stage > Stage::Reference {
            let mut result = (*reference_result).clone();
            result.stage = case.stage;
            result.status = Status::UnsupportedCapability;
            result.detail = format!(
                "TIR {} stage checks are not implemented",
                case.stage.as_str()
            );
            results.push(result);
            continue;
        }
        let comparison = case
            .expectation
            .compare(reference_result.observation.as_ref());
        let mut result = (*reference_result).clone();
        result.stage = case.stage;
        match comparison {
            Ok(()) => {
                result.status = Status::Pass;
                result.detail = case.oracle_detail();
            }
            Err(detail) => {
                result.status = Status::Fail;
                result.detail = detail;
            }
        }
        results.push(result);
    }

    let checked = Report {
        schema_version: 1,
        manifest_digest: reference.manifest_digest,
        profile: reference.profile,
        generated_at_unix_seconds: timestamp()?,
        host: reference.host,
        results,
    };
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, serde_json::to_vec_pretty(&checked)?)?;
    anyhow::ensure!(
        checked
            .results
            .iter()
            .all(|result| result.status == Status::Pass),
        "floating-point comparison failed; report written to {}",
        output_path.display()
    );
    Ok(())
}

fn validate_reference_evidence(
    root: &Path,
    manifest: &Manifest,
    case: &Case,
    result: &CaseResult,
    host: &HostIdentity,
    profile: &str,
) -> Result<(), String> {
    if result.stage != case.stage {
        return Err(format!(
            "reference stage mismatch: expected {}, observed {}",
            case.stage.as_str(),
            result.stage.as_str()
        ));
    }
    if profile == manifest.reference.profile
        && result.compiler.version != manifest.reference.compiler_version
    {
        return Err(format!(
            "reference compiler version mismatch: expected {}, observed {}",
            manifest.reference.compiler_version, result.compiler.version
        ));
    }
    if result.compiler.version.is_empty() || result.compiler.executable.is_empty() {
        return Err("reference compiler identity is incomplete".into());
    }
    if host.target.is_empty() || host.library.is_empty() {
        return Err("reference host identity is incomplete".into());
    }
    let source = root.join(&case.source);
    let contents = fs::read(&source).map_err(|error| {
        format!(
            "cannot validate reference source {}: {error}",
            source.display()
        )
    })?;
    let expected_digest = digest(&contents);
    if result.source_digest != expected_digest {
        return Err(format!(
            "reference source digest mismatch: expected {expected_digest}, observed {}",
            result.source_digest
        ));
    }
    if result.commands.is_empty() || result.commands.iter().any(Vec::is_empty) {
        return Err("reference commands are missing".into());
    }
    if !result.commands.iter().any(|command| {
        command
            .first()
            .is_some_and(|program| program == &result.compiler.executable)
    }) {
        return Err("reference commands do not identify the recorded compiler".into());
    }
    Ok(())
}

fn reference(
    root: &Path,
    gcc: &Path,
    output_path: &Path,
    case_filter: Option<&str>,
    requested_profile: Option<&str>,
    manifest_path: Option<&Path>,
) -> anyhow::Result<()> {
    let manifest_path = manifest_path
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(DEFAULT_MANIFEST));
    let manifest = read_manifest(&manifest_path)?;
    let manifest_digest =
        digest(&fs::read(&manifest_path).with_context(|| manifest_path.display().to_string())?);
    let selected = manifest
        .cases
        .iter()
        .filter(|case| case_filter.is_none_or(|id| case.id == id))
        .collect::<Vec<_>>();
    anyhow::ensure!(!selected.is_empty(), "no cases selected");

    let requested_report_profile = requested_profile.unwrap_or(&manifest.reference.profile);
    let compiler_path = match resolve_executable(gcc) {
        Ok(path) => path,
        Err(error) => {
            return write_missing_reference_report(
                root,
                &selected,
                output_path,
                requested_report_profile,
                &manifest_digest,
                ReferenceEnvironment {
                    compiler: CompilerIdentity {
                        version: String::new(),
                        executable: gcc.display().to_string(),
                    },
                    host: HostIdentity {
                        target: String::new(),
                        library: String::new(),
                    },
                    commands: vec![vec![gcc.display().to_string()]],
                },
                &error.to_string(),
            );
        }
    };
    let version_command = vec![
        compiler_path.display().to_string(),
        "-dumpfullversion".into(),
        "-dumpversion".into(),
    ];
    let compiler_version = match stdout(&version_command) {
        Ok(version) => version,
        Err(error) => {
            return write_missing_reference_report(
                root,
                &selected,
                output_path,
                requested_report_profile,
                &manifest_digest,
                ReferenceEnvironment {
                    compiler: CompilerIdentity {
                        version: String::new(),
                        executable: compiler_path.display().to_string(),
                    },
                    host: HostIdentity {
                        target: String::new(),
                        library: String::new(),
                    },
                    commands: vec![version_command],
                },
                &error.to_string(),
            );
        }
    };
    let profile = match requested_profile {
        Some(profile) if profile != manifest.reference.profile => profile.to_string(),
        Some(_) | None if compiler_version == manifest.reference.compiler_version => {
            manifest.reference.profile.clone()
        }
        _ => anyhow::bail!(
            "GCC reference requires version {}; observed {}. Use --profile with a distinct name for a separate reference",
            manifest.reference.compiler_version,
            compiler_version
        ),
    };
    let target_command = vec![compiler_path.display().to_string(), "-dumpmachine".into()];
    let target = match stdout(&target_command) {
        Ok(target) => target,
        Err(error) => {
            return write_missing_reference_report(
                root,
                &selected,
                output_path,
                &profile,
                &manifest_digest,
                ReferenceEnvironment {
                    compiler: CompilerIdentity {
                        version: compiler_version,
                        executable: compiler_path.display().to_string(),
                    },
                    host: HostIdentity {
                        target: String::new(),
                        library: String::new(),
                    },
                    commands: vec![version_command, target_command],
                },
                &error.to_string(),
            );
        }
    };
    let library_command = vec!["getconf".into(), "GNU_LIBC_VERSION".into()];
    let library = match stdout(&library_command) {
        Ok(library) => library,
        Err(error) => {
            return write_missing_reference_report(
                root,
                &selected,
                output_path,
                &profile,
                &manifest_digest,
                ReferenceEnvironment {
                    compiler: CompilerIdentity {
                        version: compiler_version,
                        executable: compiler_path.display().to_string(),
                    },
                    host: HostIdentity {
                        target,
                        library: String::new(),
                    },
                    commands: vec![version_command, target_command, library_command],
                },
                &error.to_string(),
            );
        }
    };

    let identity_commands = [version_command, target_command, library_command];
    let mut results = Vec::with_capacity(selected.len());
    for case in selected {
        results.push(run_reference_case(
            root,
            case,
            &compiler_path,
            &compiler_version,
            &identity_commands,
        )?);
    }
    let report = Report {
        schema_version: 1,
        manifest_digest,
        profile,
        generated_at_unix_seconds: timestamp()?,
        host: HostIdentity { target, library },
        results,
    };
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, serde_json::to_vec_pretty(&report)?)?;
    anyhow::ensure!(
        report.results.iter().all(|result| {
            matches!(result.status, Status::Pass | Status::UnsupportedCapability)
        }),
        "one or more reference cases failed; report written to {}",
        output_path.display()
    );
    Ok(())
}

fn write_missing_reference_report(
    root: &Path,
    selected: &[&Case],
    output_path: &Path,
    profile: &str,
    manifest_digest: &str,
    environment: ReferenceEnvironment,
    detail: &str,
) -> anyhow::Result<()> {
    let results = selected
        .iter()
        .map(|case| {
            let source_digest = fs::read(root.join(&case.source))
                .map(|contents| digest(&contents))
                .unwrap_or_default();
            CaseResult {
                case_id: case.id.clone(),
                stage: case.stage,
                compiler: environment.compiler.clone(),
                source_digest,
                commands: environment.commands.clone(),
                exit_status: None,
                observation: None,
                resolved_policy: None,
                artifacts: None,
                status: Status::MissingInfrastructure,
                detail: detail.into(),
            }
        })
        .collect();
    let report = Report {
        schema_version: 1,
        manifest_digest: manifest_digest.into(),
        profile: profile.into(),
        generated_at_unix_seconds: timestamp()?,
        host: environment.host,
        results,
    };
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, serde_json::to_vec_pretty(&report)?)?;
    anyhow::bail!(
        "{detail}; missing-infrastructure report written to {}",
        output_path.display()
    )
}

fn run_reference_case(
    root: &Path,
    case: &Case,
    compiler_path: &Path,
    compiler_version: &str,
    identity_commands: &[Vec<String>],
) -> anyhow::Result<CaseResult> {
    let source = root.join(&case.source);
    let source_contents = fs::read(&source).with_context(|| source.display().to_string())?;
    let source_digest = digest(&source_contents);
    let compiler = CompilerIdentity {
        version: compiler_version.into(),
        executable: compiler_path.display().to_string(),
    };
    if matches!(case.probe, Probe::ManifestOnly) {
        return Ok(CaseResult {
            case_id: case.id.clone(),
            stage: case.stage,
            compiler,
            source_digest,
            commands: identity_commands.to_vec(),
            exit_status: None,
            observation: None,
            resolved_policy: None,
            artifacts: None,
            status: Status::UnsupportedCapability,
            detail: format!(
                "manifest-only case requires {} from {}",
                case.target_requirements.join(", "),
                case.oracle.reference
            ),
        });
    }

    let directory = tir_adt::TempDir::with_prefix("tir-fp-check-")?;
    let copied_source = directory.path().join("probe.c");
    fs::write(&copied_source, &source_contents)?;
    let output_path = match case.probe {
        Probe::Assembly => directory.path().join("probe.s"),
        Probe::CompileDiagnostic => directory.path().join("probe.o"),
        Probe::Execute => directory.path().join("probe"),
        Probe::ManifestOnly => unreachable!(),
    };
    let mut compile = vec![
        compiler_path.display().to_string(),
        format!("-std={}", case.language_mode),
    ];
    compile.extend(case.compiler_args.iter().cloned());
    if matches!(case.probe, Probe::Assembly) {
        compile.push("-S".into());
    } else if matches!(case.probe, Probe::CompileDiagnostic) {
        compile.push("-c".into());
    }
    compile.push(copied_source.display().to_string());
    compile.extend(["-o".into(), output_path.display().to_string()]);
    if matches!(case.probe, Probe::Execute)
        && case
            .target_requirements
            .iter()
            .any(|requirement| requirement == "libm")
    {
        compile.push("-lm".into());
    }
    let mut commands = identity_commands.to_vec();
    commands.push(compile.clone());
    let compiled = command_output(&compile)?;
    if matches!(case.probe, Probe::CompileDiagnostic) {
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&compiled.stdout),
            String::from_utf8_lossy(&compiled.stderr)
        );
        let observation = Observation::Diagnostic { message };
        let expectation = case
            .reference_expectation
            .as_ref()
            .unwrap_or(&case.expectation);
        let comparison = expectation.compare(Some(&observation));
        let (status, detail) = if compiled.status.success() {
            (
                Status::Fail,
                "expected compilation to fail with a diagnostic".into(),
            )
        } else {
            match comparison {
                Ok(()) => (Status::Pass, case.oracle_detail()),
                Err(detail) => (Status::Fail, detail),
            }
        };
        let artifacts = if status == Status::Fail {
            Some(directory.keep().display().to_string())
        } else {
            None
        };
        return Ok(CaseResult {
            case_id: case.id.clone(),
            stage: case.stage,
            compiler,
            source_digest,
            commands,
            exit_status: compiled.status.code(),
            observation: Some(observation),
            resolved_policy: None,
            artifacts,
            status,
            detail,
        });
    }
    if !compiled.status.success() {
        let detail = process_failure("compiler", &compiled);
        return Ok(fail_case(
            case,
            compiler,
            source_digest,
            commands,
            compiled.status.code(),
            directory,
            detail,
        ));
    }

    match case.probe {
        Probe::Execute => {
            let mut run = vec![output_path.display().to_string()];
            run.extend(case.run_args.iter().cloned());
            run.extend(case.runtime_input_bits.iter().cloned());
            commands.push(run.clone());
            let executed = command_output(&run)?;
            if !executed.status.success() {
                let detail = process_failure("probe", &executed);
                return Ok(fail_case(
                    case,
                    compiler,
                    source_digest,
                    commands,
                    executed.status.code(),
                    directory,
                    detail,
                ));
            }
            let observation: Observation = match serde_json::from_slice(&executed.stdout) {
                Ok(observation) => observation,
                Err(error) => {
                    return Ok(fail_case(
                        case,
                        compiler,
                        source_digest,
                        commands,
                        executed.status.code(),
                        directory,
                        format!("invalid observation JSON: {error}"),
                    ));
                }
            };
            let expectation = case
                .reference_expectation
                .as_ref()
                .unwrap_or(&case.expectation);
            let (status, detail) = match expectation.compare(Some(&observation)) {
                Ok(()) => (Status::Pass, case.oracle_detail()),
                Err(detail) => (Status::Fail, detail),
            };
            let artifacts = if status == Status::Fail {
                Some(directory.keep().display().to_string())
            } else {
                None
            };
            Ok(CaseResult {
                case_id: case.id.clone(),
                stage: case.stage,
                compiler,
                source_digest,
                commands,
                exit_status: executed.status.code(),
                observation: Some(observation),
                resolved_policy: None,
                artifacts,
                status,
                detail,
            })
        }
        Probe::Assembly => {
            let assembly = fs::read_to_string(&output_path)?;
            let observation = Observation::CodeShape {
                instructions: assembly
                    .lines()
                    .map(str::trim)
                    .filter(|line| {
                        !line.is_empty()
                            && !line.starts_with('.')
                            && !line.starts_with('#')
                            && !line.ends_with(':')
                    })
                    .map(str::to_string)
                    .collect(),
            };
            let expectation = case
                .reference_expectation
                .as_ref()
                .unwrap_or(&case.expectation);
            let (status, detail) = match expectation.compare(Some(&observation)) {
                Ok(()) => (Status::Pass, case.oracle_detail()),
                Err(detail) => (Status::Fail, detail),
            };
            let artifacts = if status == Status::Fail {
                Some(directory.keep().display().to_string())
            } else {
                None
            };
            Ok(CaseResult {
                case_id: case.id.clone(),
                stage: case.stage,
                compiler,
                source_digest,
                commands,
                exit_status: compiled.status.code(),
                observation: Some(observation),
                resolved_policy: None,
                artifacts,
                status,
                detail,
            })
        }
        Probe::CompileDiagnostic => unreachable!(),
        Probe::ManifestOnly => unreachable!(),
    }
}

fn fail_case(
    case: &Case,
    compiler: CompilerIdentity,
    source_digest: String,
    commands: Vec<Vec<String>>,
    exit_status: Option<i32>,
    directory: tir_adt::TempDir,
    detail: String,
) -> CaseResult {
    CaseResult {
        case_id: case.id.clone(),
        stage: case.stage,
        compiler,
        source_digest,
        commands,
        exit_status,
        observation: None,
        resolved_policy: None,
        artifacts: Some(directory.keep().display().to_string()),
        status: Status::Fail,
        detail,
    }
}

fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let manifest: Manifest =
        toml::from_str(&fs::read_to_string(path).with_context(|| path.display().to_string())?)
            .with_context(|| path.display().to_string())?;
    anyhow::ensure!(
        manifest.schema_version == 1,
        "unsupported manifest schema version"
    );
    let ids = manifest
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(ids.len() == manifest.cases.len(), "duplicate case ID");
    let inventory_ids = manifest
        .inventory
        .iter()
        .map(|item| item.id.as_str())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        inventory_ids.len() == manifest.inventory.len(),
        "duplicate coverage inventory ID"
    );
    anyhow::ensure!(
        ids.is_disjoint(&inventory_ids),
        "case and coverage inventory IDs overlap"
    );
    anyhow::ensure!(
        manifest.inventory.iter().all(|item| {
            item.stage == Stage::Release
                && item.status == model::CoverageStatus::Unimplemented
                && !item.area.is_empty()
                && !item.requirements.is_empty()
        }),
        "invalid coverage inventory entry"
    );
    Ok(manifest)
}

fn resolve_executable(path: &Path) -> anyhow::Result<PathBuf> {
    if path.components().count() > 1 {
        return path
            .canonicalize()
            .with_context(|| path.display().to_string());
    }
    let search = std::env::var_os("PATH").context("PATH is not set")?;
    std::env::split_paths(&search)
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
        .context("GCC executable was not found")?
        .canonicalize()
        .context("resolving GCC executable")
}

fn stdout(argv: &[String]) -> anyhow::Result<String> {
    let output = command_output(argv)?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        process_failure(&argv[0], &output)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn command_output(argv: &[String]) -> anyhow::Result<Output> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("starting {}", argv[0]))
}

fn process_failure(name: &str, output: &Output) -> String {
    format!(
        "{name} failed with {}: {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn digest(contents: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.input(contents);
    format!("sha256:{:x}", digest.result())
}

fn timestamp() -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn report(path: &Path) -> anyhow::Result<()> {
    let contents = fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let report: Report =
        serde_json::from_str(&contents).with_context(|| path.display().to_string())?;
    anyhow::ensure!(
        report.schema_version == 1,
        "unsupported report schema version"
    );

    let count = |status| {
        report
            .results
            .iter()
            .filter(|result| result.status == status)
            .count()
    };
    let passed = count(Status::Pass);
    let failed = count(Status::Fail);
    let unsupported = count(Status::UnsupportedCapability);
    let missing = count(Status::MissingInfrastructure);
    println!(
        "profile={} target={} library={} pass={} fail={} unsupported={} missing_infrastructure={}",
        report.profile,
        report.host.target,
        report.host.library,
        passed,
        failed,
        unsupported,
        missing
    );
    for result in report
        .results
        .iter()
        .filter(|result| result.status != Status::Pass)
    {
        println!("{} {:?}: {}", result.case_id, result.status, result.detail);
    }
    anyhow::ensure!(
        failed == 0 && unsupported == 0 && missing == 0 && !report.results.is_empty(),
        "report contains non-passing or no results"
    );
    Ok(())
}
