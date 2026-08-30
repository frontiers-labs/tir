//! Differential execution harness: compile one program under several
//! pipelines and compilers, run the binaries, compare observable behavior.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long one compiled program may run before it is killed.
pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one compiler invocation may run before it is killed.
pub(super) const COMPILE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Variant {
    pub name: String,
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Fcc(FccVariant),
    Gcc,
    Clang,
}

/// What makes one fcc variant different from the default: a mid-end pipeline
/// override, a backend oracle, or both. Every one of them is a
/// semantics-preserving change, so a divergence names a defect rather than a
/// difference of opinion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FccVariant {
    /// Replaces fcc's default mid-end pipeline.
    pub pipeline: Option<String>,
    /// `--shuffle-machine-order`: every machine block re-linearized by another
    /// topological order of its dependence graph, after selection and again
    /// after allocation.
    pub shuffle_machine_order: bool,
}

/// How the oracle is named in a variant's name and in a record's identity,
/// ahead of any pipeline.
const SHUFFLE_MACHINE_ORDER: &str = "shuffle-machine-order";

impl FccVariant {
    pub fn pipeline(pipeline: &str) -> Self {
        Self {
            pipeline: Some(pipeline.to_string()),
            shuffle_machine_order: false,
        }
    }

    pub fn shuffle_machine_order() -> Self {
        Self {
            pipeline: None,
            shuffle_machine_order: true,
        }
    }

    /// The one line a record names this variant by, and the suffix of the
    /// variant's own name. `None` for the default, which has nothing to name.
    pub fn tag(&self) -> Option<String> {
        match (&self.pipeline, self.shuffle_machine_order) {
            (None, false) => None,
            (Some(pipeline), false) => Some(pipeline.clone()),
            (None, true) => Some(SHUFFLE_MACHINE_ORDER.to_string()),
            (Some(pipeline), true) => Some(format!("{SHUFFLE_MACHINE_ORDER} {pipeline}")),
        }
    }

    /// The variant a tag denotes, so a filed record replays as what it was
    /// filed against.
    pub fn from_tag(tag: &str) -> Self {
        match tag.strip_prefix(SHUFFLE_MACHINE_ORDER) {
            Some(rest) => Self {
                pipeline: Some(rest.trim())
                    .filter(|rest| !rest.is_empty())
                    .map(str::to_string),
                shuffle_machine_order: true,
            },
            None => Self::pipeline(tag),
        }
    }

    /// The `fcc compile` arguments this variant adds.
    pub fn args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(pipeline) = &self.pipeline {
            args.push("--pipeline".to_string());
            args.push(pipeline.clone());
        }
        if self.shuffle_machine_order {
            args.push("--shuffle-machine-order".to_string());
        }
        args
    }
}

impl Variant {
    pub fn fcc(spec: FccVariant) -> Self {
        let name = spec
            .tag()
            .map_or_else(|| "fcc-default".to_string(), |tag| format!("fcc:{tag}"));
        Self {
            name,
            backend: Backend::Fcc(spec),
        }
    }

    pub fn gcc() -> Self {
        Self {
            name: "gcc".into(),
            backend: Backend::Gcc,
        }
    }

    pub fn clang() -> Self {
        Self {
            name: "clang".into(),
            backend: Backend::Clang,
        }
    }

    fn compiler(&self) -> &'static str {
        match self.backend {
            Backend::Fcc(_) => "fcc",
            Backend::Gcc => "gcc",
            Backend::Clang => "clang",
        }
    }
}

/// Observable behavior of one compiled binary.
#[derive(PartialEq, Eq, Clone)]
pub struct Behavior {
    stdout: Vec<u8>,
    exit_code: Option<i32>,
}

impl Behavior {
    pub fn describe(&self) -> String {
        let text = String::from_utf8_lossy(&self.stdout);
        format!("exit={:?}, stdout={text:?}", self.exit_code)
    }
}

pub enum Outcome {
    /// Every variant agrees.
    Agree,
    /// `variant` disagreed with `reference`.
    Diverged {
        variant: String,
        expected: Behavior,
        actual: Behavior,
    },
    /// A variant could not be compiled or run; reported but not a divergence.
    Errored { variant: String, message: String },
}

/// Compile `source` under every variant, run each binary, and compare against
/// the first variant that produced a result. Artifacts land in `work_dir`.
pub fn run_variants(
    fcc: &Path,
    source: &Path,
    variants: &[Variant],
    work_dir: &Path,
) -> Vec<(String, Outcome)> {
    let mut results = Vec::new();
    let mut reference: Option<Behavior> = None;
    for variant in variants {
        let outcome = match compile_and_run(fcc, source, variant, work_dir) {
            Ok(behavior) => match &reference {
                None => {
                    reference = Some(behavior);
                    Outcome::Agree
                }
                Some(expected) if *expected == behavior => Outcome::Agree,
                Some(expected) => Outcome::Diverged {
                    variant: variant.name.clone(),
                    expected: expected.clone(),
                    actual: behavior,
                },
            },
            Err(message) => Outcome::Errored {
                variant: variant.name.clone(),
                message,
            },
        };
        results.push((variant.name.clone(), outcome));
    }
    results
}

fn compile_and_run(
    fcc: &Path,
    source: &Path,
    variant: &Variant,
    work_dir: &Path,
) -> Result<Behavior, String> {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("prog")
        .to_string();
    let tag = variant.name.replace(['/', ':', ',', '(', ')', '='], "_");
    let executable = work_dir.join(format!("{stem}-{tag}.bin"));

    let mut command = match &variant.backend {
        Backend::Fcc(spec) => {
            let object = work_dir.join(format!("{stem}-{tag}.o"));
            let mut compile = Command::new(fcc);
            compile
                .args(["compile", "--stage", "obj", "--march", "x86_64"])
                .arg("-o")
                .arg(&object)
                .args(spec.args());
            compile.arg(source);
            run_command(&mut compile, "fcc")?;

            let mut link = Command::new("cc");
            link.arg(&object).arg("-o").arg(&executable);
            run_command(&mut link, "cc")?;
            return run_program(&executable);
        }
        Backend::Gcc => {
            let mut command = Command::new(variant.compiler());
            command.arg("-O1").arg("-o").arg(&executable).arg(source);
            command
        }
        Backend::Clang => {
            let mut command = Command::new(variant.compiler());
            command.arg("-O1").arg("-o").arg(&executable).arg(source);
            command
        }
    };
    run_command(&mut command, variant.compiler())?;
    run_program(&executable)
}

fn run_command(command: &mut Command, what: &str) -> Result<(), String> {
    let output = timed_output(command, COMPILE_TIMEOUT)
        .map_err(|e| format!("{what}: {e}"))?
        .ok_or_else(|| format!("{what} timed out after {COMPILE_TIMEOUT:?}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} failed ({}):\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Run a command to completion, or `None` if it exceeded `timeout`.
pub(super) fn timed_output(
    command: &mut Command,
    timeout: Duration,
) -> Result<Option<std::process::Output>, String> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => {
                return Ok(Some(
                    child
                        .wait_with_output()
                        .map_err(|e| format!("collect output: {e}"))?,
                ));
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                return Ok(None);
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

fn run_program(executable: &Path) -> Result<Behavior, String> {
    use std::io::Read;

    let mut child = Command::new(executable)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", executable.display()))?;
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let mut stdout = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    pipe.read_to_end(&mut stdout)
                        .map_err(|e| format!("read stdout: {e}"))?;
                }
                return Ok(Behavior {
                    stdout,
                    exit_code: status.code(),
                });
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                return Err(format!(
                    "{} timed out after {RUN_TIMEOUT:?}",
                    executable.display()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// The first line where two outputs differ, for failure reports.
pub fn first_difference(expected: &Behavior, actual: &Behavior) -> Option<String> {
    let expected = String::from_utf8_lossy(&expected.stdout);
    let actual = String::from_utf8_lossy(&actual.stdout);
    for (index, pair) in expected.lines().zip(actual.lines()).enumerate() {
        let (e, a) = pair;
        if e != a {
            return Some(format!("line {}: expected {e:?}, got {a:?}", index + 1));
        }
    }
    let expected_lines = expected.lines().count();
    let actual_lines = actual.lines().count();
    if expected_lines != actual_lines {
        return Some(format!(
            "expected {expected_lines} output lines, got {actual_lines}"
        ));
    }
    if expected == actual {
        return None;
    }
    Some("outputs differ within the last line".into())
}
