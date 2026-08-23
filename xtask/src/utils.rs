use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xshell::{cmd, Shell};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const PROGRESS_CASE_INTERVAL: usize = 250;
const PROGRESS_TIME_INTERVAL: Duration = Duration::from_secs(30);

pub fn project_root() -> PathBuf {
    let dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_owned());
    PathBuf::from(dir).parent().unwrap().to_owned()
}

pub fn git_checkout(sh: &Shell, url: &str, tag: &str, dest: &str) -> anyhow::Result<()> {
    let root = project_root();
    let target_dir = root.join("target");
    let dest_dir = target_dir.join(dest);

    if std::env::var("TIR_SKIP_SAIL_FETCH").ok().as_deref() == Some("1") {
        return Ok(());
    }

    if let Some(parent) = dest_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if let Some(parent) = dest_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = cmd!(sh, "git clone --depth 1 --branch {tag} {url} {dest_dir}").run();

    Ok(())
}

/// Download `url` to `dest` unless it already exists. Downloads go through a
/// `.part` file so an interrupted run never leaves a truncated artifact.
pub fn download_file(sh: &Shell, url: &str, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = dest.with_extension("part");
    cmd!(sh, "curl -fsSL --retry 3 -o {part} {url}").run()?;
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Runs `task` over `items` on every available core, reporting progress under
/// `label`, and returns the results sorted by their reported path.
pub fn run_parallel<T: Send + Sync, R: Send>(
    label: &str,
    items: Vec<T>,
    task: impl Fn(usize, &T) -> (String, R) + Sync,
) -> Vec<(String, R)> {
    let items = Arc::new(items);
    let next = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(Vec::with_capacity(items.len())));
    let workers = std::thread::available_parallelism().map_or(1, usize::from);
    let task = &task;
    let (completed_sender, completed_receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let items = Arc::clone(&items);
            let next = Arc::clone(&next);
            let results = Arc::clone(&results);
            let completed_sender = completed_sender.clone();
            scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(index) else {
                    break;
                };
                let result = task(index, item);
                results.lock().unwrap().push(result);
                let _ = completed_sender.send(());
            });
        }
        drop(completed_sender);

        let mut completed = 0;
        while completed < items.len() {
            match completed_receiver.recv_timeout(PROGRESS_TIME_INTERVAL) {
                Ok(()) => {
                    completed += 1;
                    if !should_report_progress(completed, items.len()) {
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            println!("{label} progress: {completed}/{} cases", items.len());
        }
    });
    let mut results = Arc::into_inner(results).unwrap().into_inner().unwrap();
    results.sort_by(|left, right| left.0.cmp(&right.0));
    results
}

fn should_report_progress(completed: usize, total: usize) -> bool {
    completed == total || completed.is_multiple_of(PROGRESS_CASE_INTERVAL)
}

/// Runs `command` to completion, killing its whole process group once
/// `timeout` elapses. Returns whether it exited successfully in time.
pub fn run_with_timeout(command: &mut std::process::Command, timeout: Duration) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::process::Command;
    use std::time::Duration;

    #[cfg(unix)]
    use super::run_with_timeout;
    use super::should_report_progress;

    #[cfg(unix)]
    #[test]
    fn timed_command_runs_in_its_own_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", r#"test "$(ps -o pgid= -p $$ | tr -d ' ')" = "$$""#]);
        assert!(run_with_timeout(&mut command, Duration::from_secs(1)));
    }

    #[test]
    fn progress_is_reported_at_intervals_and_completion() {
        assert!(!should_report_progress(249, 1_000));
        assert!(should_report_progress(250, 1_000));
        assert!(!should_report_progress(999, 1_000));
        assert!(should_report_progress(1_000, 1_000));
    }
}
