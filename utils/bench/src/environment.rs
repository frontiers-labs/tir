//! Cooperative benchmark locking, CPU affinity, and host identity.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentMetadata {
    pub os: String,
    pub architecture: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpu: Option<usize>,
    pub hostname: String,
    pub allowed_cpus: Vec<usize>,
    pub applied_cpus: Vec<usize>,
    pub subprocess_environment: BTreeMap<String, String>,
}

/// Holds a cooperative host lock and restores this thread's affinity on drop.
/// Child processes inherit the selected affinity. Existing threads are untouched.
pub struct EnvironmentGuard {
    pub metadata: EnvironmentMetadata,
    #[cfg(target_os = "linux")]
    lock: std::fs::File,
    #[cfg(target_os = "linux")]
    original_affinity: libc::cpu_set_t,
    #[cfg(target_os = "linux")]
    thread: libc::pthread_t,
    // Affinity belongs to the acquiring thread, so this guard cannot move between threads.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl EnvironmentGuard {
    /// Hold the cooperative lock and optionally pin this thread to an allowed CPU.
    #[cfg(target_os = "linux")]
    pub fn acquire(lock_path: &Path, cpu: Option<usize>) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("opening host benchmark lock {}", lock_path.display()))?;
        // flock is released when the file is closed, including every early error path.
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "host benchmark lock unavailable at {}: {}",
            lock_path.display(),
            std::io::Error::last_os_error()
        );
        let mut original_affinity = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
        ensure!(
            unsafe {
                libc::sched_getaffinity(
                    0,
                    std::mem::size_of::<libc::cpu_set_t>(),
                    &mut original_affinity,
                )
            } == 0,
            "reading CPU affinity: {}",
            std::io::Error::last_os_error()
        );
        if let Some(cpu) = cpu {
            ensure!(
                cpu < libc::CPU_SETSIZE as usize
                    && unsafe { libc::CPU_ISSET(cpu, &original_affinity) },
                "CPU {cpu} is outside the allowed affinity set"
            );
        }
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let cpu_model = cpuinfo
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                matches!(key.trim(), "model name" | "Hardware" | "uarch")
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let allowed_cpus = cpu_list(&original_affinity);
        let applied_cpus = cpu.map_or_else(|| allowed_cpus.clone(), |cpu| vec![cpu]);
        let metadata = EnvironmentMetadata {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            cpu_model,
            cpu,
            hostname: read_value("/proc/sys/kernel/hostname").unwrap_or_default(),
            allowed_cpus,
            applied_cpus,
            subprocess_environment: crate::process::default_environment()
                .into_iter()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        };
        if let Some(cpu) = cpu {
            let mut selected = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            unsafe {
                libc::CPU_SET(cpu, &mut selected);
            }
            ensure!(
                unsafe {
                    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &selected)
                } == 0,
                "setting CPU affinity: {}",
                std::io::Error::last_os_error()
            );
        }
        let guard = Self {
            metadata,
            lock,
            original_affinity,
            thread: unsafe { libc::pthread_self() },
            _thread_bound: std::marker::PhantomData,
        };
        ensure!(
            current_cpus()? == guard.metadata.applied_cpus,
            "CPU affinity readback differs from requested affinity"
        );
        Ok(guard)
    }

    /// Verify that the selected CPU affinity was retained.
    #[cfg(target_os = "linux")]
    pub fn check_stable(&self) -> Result<()> {
        ensure!(
            current_cpus()? == self.metadata.applied_cpus,
            "CPU affinity changed during benchmark"
        );
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn check_stable(&self) -> Result<()> {
        anyhow::bail!("benchmark environment controls are supported only on Linux")
    }

    #[cfg(not(target_os = "linux"))]
    pub fn acquire(_: &Path, _: Option<usize>) -> Result<Self> {
        anyhow::bail!("benchmark environment controls are supported only on Linux")
    }
}

fn read_value(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(target_os = "linux")]
fn cpu_list(set: &libc::cpu_set_t) -> Vec<usize> {
    (0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, set) })
        .collect()
}

#[cfg(target_os = "linux")]
fn current_cpus() -> Result<Vec<usize>> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    ensure!(
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) }
            == 0,
        "reading CPU affinity: {}",
        std::io::Error::last_os_error()
    );
    Ok(cpu_list(&set))
}

#[cfg(target_os = "linux")]
impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::pthread_setaffinity_np(
                self.thread,
                std::mem::size_of::<libc::cpu_set_t>(),
                &self.original_affinity,
            );
            libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn affinity_is_checked_and_restored() {
        let before = current_cpus().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut guard =
            EnvironmentGuard::acquire(&dir.path().join("lock"), Some(before[0])).unwrap();
        assert_eq!(current_cpus().unwrap(), vec![before[0]]);
        guard.check_stable().unwrap();
        guard.metadata.applied_cpus.clear();
        assert!(guard.check_stable().is_err());
        drop(guard);
        assert_eq!(current_cpus().unwrap(), before);
    }

    #[test]
    fn lock_excludes_other_runners_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        let guard = EnvironmentGuard::acquire(&path, None).unwrap();
        assert!(EnvironmentGuard::acquire(&path, None).is_err());
        drop(guard);
        assert!(EnvironmentGuard::acquire(&path, None).is_ok());
    }
}
