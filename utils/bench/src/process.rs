//! Bounded subprocess execution with retained output and per-process accounting.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

/// A command executed directly, without shell interpolation.
#[derive(Clone, Debug)]
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub directory: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

/// Measurements of one command. Native RSS comes from a fresh GNU time supervisor.
/// Native wall and CPU include the supervisor's startup and accounting overhead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSample {
    pub wall_ns: f64,
    pub user_ns: f64,
    pub system_ns: f64,
    pub peak_process_rss_bytes: u64,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

impl Command {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            directory: PathBuf::from("."),
            env: BTreeMap::new(),
        }
    }
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = directory.into();
        self
    }
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
    pub fn run(&self, output_dir: &Path, timeout: Duration) -> Result<ProcessSample> {
        ensure!(!timeout.is_zero(), "process timeout must be positive");
        std::fs::create_dir_all(output_dir)?;
        let output_dir = output_dir.canonicalize()?;
        let rss_path = output_dir.join("peak-rss-kib");
        ensure!(
            Path::new("/usr/bin/time").is_file(),
            "native process RSS accounting requires GNU time at /usr/bin/time; install the time package"
        );
        let mut command = std::process::Command::new("/usr/bin/time");
        // A direct fork/exec inherits the harness's resident size into ru_maxrss.
        // GNU time execs into a small supervisor before spawning the actual target.
        command
            .args(["--quiet", "--format=%M", "--output"])
            .arg(&rss_path)
            .arg("--")
            .arg(&self.program)
            .args(&self.args)
            .current_dir(&self.directory)
            .env_clear()
            .envs(default_environment())
            .envs(&self.env);
        let mut sample = execute_native(&mut command, &output_dir, timeout)?;
        let kib = std::fs::read_to_string(&rss_path)
            .context("reading GNU time peak RSS")?
            .trim()
            .parse::<u64>()
            .context("invalid GNU time peak RSS; /usr/bin/time must be GNU time")?;
        sample.peak_process_rss_bytes = kib
            .checked_mul(1024)
            .context("GNU time peak RSS overflow")?;
        Ok(sample)
    }
}

/// Base subprocess environment. Explicit `Command::env` values override it.
/// Compiler diagnostics and other unlisted inherited variables are removed.
pub fn default_environment() -> BTreeMap<OsString, OsString> {
    let mut environment = BTreeMap::new();
    for name in [
        "PATH",
        "HOME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
    ] {
        if let Some(value) = std::env::var_os(name) {
            environment.insert(name.into(), value);
        }
    }
    for (name, value) in [
        ("LC_ALL", "C"),
        ("LANG", "C"),
        ("TZ", "UTC"),
        ("OMP_NUM_THREADS", "1"),
        ("OPENBLAS_NUM_THREADS", "1"),
        ("MKL_NUM_THREADS", "1"),
        ("VECLIB_MAXIMUM_THREADS", "1"),
        ("NUMEXPR_NUM_THREADS", "1"),
        ("RAYON_NUM_THREADS", "1"),
    ] {
        environment.insert(name.into(), value.into());
    }
    environment
}

/// Capture an untimed command under the same process-group deadline as measurements.
/// The caller's environment is preserved, including Git authentication settings.
pub fn capture(
    command: &mut std::process::Command,
    timeout: Duration,
) -> Result<std::process::Output> {
    ensure!(!timeout.is_zero(), "process timeout must be positive");
    let directory = tir_adt::TempDir::with_prefix("tir-bench-command-")?;
    let result = execute_accounted(command, directory.path(), timeout);
    let (sample, status) = match result {
        Ok(output) => output,
        Err(error) => {
            let path = directory.keep();
            return Err(error)
                .with_context(|| format!("{command:?}; retained logs: {}", path.display()));
        }
    };
    Ok(std::process::Output {
        status,
        stdout: std::fs::read(sample.stdout)?,
        stderr: std::fs::read(sample.stderr)?,
    })
}

fn execute_native(
    command: &mut std::process::Command,
    output_dir: &Path,
    timeout: Duration,
) -> Result<ProcessSample> {
    let (sample, status) = execute_accounted(command, output_dir, timeout)?;
    ensure!(
        status.success(),
        "process failed ({status}); logs: {}",
        output_dir.display()
    );
    Ok(sample)
}

#[cfg(target_os = "linux")]
fn execute_accounted(
    command: &mut std::process::Command,
    output_dir: &Path,
    timeout: Duration,
) -> Result<(ProcessSample, std::process::ExitStatus)> {
    use std::{
        os::unix::process::{CommandExt, ExitStatusExt},
        process::Stdio,
        time::Instant,
    };
    let stdout = output_dir.join("stdout");
    let stderr = output_dir.join("stderr");
    command
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout)?)
        .stderr(std::fs::File::create(&stderr)?)
        .process_group(0);
    let start = Instant::now();
    let child = command.spawn().context("spawning benchmark process")?;
    let pid = child.id() as libc::pid_t;
    let (cancel, cancelled) = std::sync::mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if cancelled
            .recv_timeout(timeout.saturating_sub(start.elapsed()))
            .is_err()
        {
            // The child starts a fresh process group, so descendants receive the timeout too.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            true
        } else {
            false
        }
    });
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
    let wait_result = loop {
        // Leave the PID allocated until the watchdog stops; otherwise a timeout
        // could signal a newly reused process-group ID after wait4 reaps the child.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            break Err(error);
        }
    };
    let wall_ns = start.elapsed().as_nanos() as f64;
    let _ = cancel.send(());
    let timed_out = watchdog
        .join()
        .map_err(|_| anyhow::anyhow!("process watchdog panicked"))?;
    wait_result.context("waiting for benchmark process")?;
    let mut status = 0;
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    loop {
        let result = unsafe { libc::wait4(pid, &mut status, 0, usage.as_mut_ptr()) };
        if result == pid {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("reaping benchmark process");
        }
    }
    ensure!(
        !timed_out,
        "process timed out after {timeout:?}; logs: {}",
        output_dir.display()
    );
    // wait4 initialized rusage on the successful path above.
    let usage = unsafe { usage.assume_init() };
    let ns = |t: libc::timeval| t.tv_sec as f64 * 1e9 + t.tv_usec as f64 * 1e3;
    Ok((
        ProcessSample {
            wall_ns,
            user_ns: ns(usage.ru_utime),
            system_ns: ns(usage.ru_stime),
            peak_process_rss_bytes: (usage.ru_maxrss.max(0) as u64) * 1024,
            stdout,
            stderr,
        },
        std::process::ExitStatus::from_raw(status),
    ))
}

#[cfg(not(target_os = "linux"))]
fn execute_accounted(
    _: &mut std::process::Command,
    _: &Path,
    _: Duration,
) -> Result<(ProcessSample, std::process::ExitStatus)> {
    anyhow::bail!("native process accounting is supported only on Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_environment_allows_explicit_overrides() {
        let dir = tir_adt::TempDir::new().unwrap();
        let sample = Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s %s %s' \"$LC_ALL\" \"$TZ\" \"$OMP_NUM_THREADS\"",
            ])
            .env("OMP_NUM_THREADS", "2")
            .run(dir.path(), Duration::from_secs(5))
            .unwrap();
        assert_eq!(std::fs::read_to_string(sample.stdout).unwrap(), "C UTC 2");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failure_retains_output() {
        let dir = tir_adt::TempDir::new().unwrap();
        assert!(
            Command::new("/bin/sh")
                .args(["-c", "echo failure >&2; exit 7"])
                .run(dir.path(), Duration::from_secs(5))
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("stderr")).unwrap(),
            "failure\n"
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_kills_process_group() {
        let dir = tir_adt::TempDir::new().unwrap();
        let start = std::time::Instant::now();
        let error = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & wait"])
            .run(dir.path(), Duration::from_millis(40))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn target_rss_excludes_live_harness_allocations() {
        let dir = tir_adt::TempDir::new().unwrap();
        let small = vec![42_u8; 16 * 1024 * 1024];
        std::hint::black_box(&small);
        let first = Command::new("/bin/true")
            .run(&dir.path().join("small"), Duration::from_secs(5))
            .unwrap();
        let large = vec![43_u8; 64 * 1024 * 1024];
        std::hint::black_box(&large);
        let second = Command::new("/bin/true")
            .run(&dir.path().join("large"), Duration::from_secs(5))
            .unwrap();
        std::hint::black_box((&small, &large));
        assert!(second.peak_process_rss_bytes < large.len() as u64 / 2);
        assert!(
            first
                .peak_process_rss_bytes
                .abs_diff(second.peak_process_rss_bytes)
                < small.len() as u64
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn accounts_for_process_memory() {
        let dir = tir_adt::TempDir::new().unwrap();
        let sample = Command::new("/bin/sh")
            .args(["-c", "printf captured"])
            .run(dir.path(), Duration::from_secs(5))
            .unwrap();
        assert!(sample.wall_ns > 0.0);
        assert!(sample.peak_process_rss_bytes > 0);
        assert_eq!(std::fs::read_to_string(sample.stdout).unwrap(), "captured");
    }
}
