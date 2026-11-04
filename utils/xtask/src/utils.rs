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
    PathBuf::from(dir).ancestors().nth(2).unwrap().to_owned()
}

/// Every `.c` file under `directory`, recursively. A path that is not a
/// directory contributes nothing.
pub fn collect_c_files(directory: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_c_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "c") {
            files.push(path);
        }
    }
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

/// Runs `command` in a process group of its own, polling until it exits or
/// `timeout` elapses, in which case it is killed and `None` comes back. The
/// output carries whatever pipes the caller configured on `command`.
pub fn run_with_deadline(
    command: &mut std::process::Command,
    timeout: Duration,
) -> Result<Option<std::process::Output>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => {
                return child
                    .wait_with_output()
                    .map(Some)
                    .map_err(|e| format!("collect output: {e}"));
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Runs `command` to completion, killing it once `timeout` elapses. Returns
/// whether it exited successfully in time.
pub fn run_with_timeout(command: &mut std::process::Command, timeout: Duration) -> bool {
    matches!(run_with_deadline(command, timeout), Ok(Some(output)) if output.status.success())
}
