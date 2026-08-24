//! Differential execution harness: compile one program under several
//! pipelines and compilers, run the binaries, compare observable behavior.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long one compiled program may run before it is killed.
const RUN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one compiler invocation may run before it is killed.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Variant {
    pub name: String,
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    /// `fcc` with an optional mid-end pipeline override.
    Fcc {
        pipeline: Option<String>,
    },
    Gcc,
    Clang,
}

impl Variant {
    pub fn fcc(pipeline: Option<String>) -> Self {
        let name = pipeline
            .as_deref()
            .map_or_else(|| "fcc-default".to_string(), |p| format!("fcc:{p}"));
        Self {
            name,
            backend: Backend::Fcc { pipeline },
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
            Backend::Fcc { .. } => "fcc",
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
        Backend::Fcc { pipeline } => {
            let object = work_dir.join(format!("{stem}-{tag}.o"));
            let mut compile = Command::new(fcc);
            compile
                .args(["compile", "--stage", "obj", "--march", "x86_64"])
                .arg("-o")
                .arg(&object);
            if let Some(pipeline) = pipeline {
                compile.arg("--pipeline").arg(pipeline);
            }
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
fn timed_output(
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
