//! Cooperative benchmark locking and read-only host policy checks.
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
    pub governors: BTreeMap<usize, String>,
    pub strict: bool,
    pub hostname: String,
    pub allowed_cpus: Vec<usize>,
    pub applied_cpus: Vec<usize>,
    /// Stable sysfs/proc configuration, keyed by the source file.
    pub configuration: BTreeMap<String, String>,
    /// Thermal readings and hardware throttling counters at acquisition.
    pub observations: BTreeMap<String, String>,
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
    /// Fail if another cooperating runner holds `lock_path`. Strict mode requires
    /// one pinned CPU, performance governor and disabled boost, without changing host policy.
    #[cfg(target_os = "linux")]
    pub fn acquire(lock_path: &Path, cpu: Option<usize>, strict: bool) -> Result<Self> {
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
        ensure!(
            !strict || cpu.is_some(),
            "strict mode requires an explicit CPU"
        );
        let mut governors = BTreeMap::new();
        for index in 0..libc::CPU_SETSIZE as usize {
            if unsafe { libc::CPU_ISSET(index, &original_affinity) } {
                let path = format!("/sys/devices/system/cpu/cpu{index}/cpufreq/scaling_governor");
                if let Ok(value) = std::fs::read_to_string(path) {
                    governors.insert(index, value.trim().to_owned());
                }
            }
        }
        if let Some(cpu) = cpu.filter(|_| strict) {
            ensure!(
                governors
                    .get(&cpu)
                    .is_some_and(|value| value == "performance"),
                "strict mode requires a readable performance governor for CPU {cpu}"
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
        let configuration = read_configuration(&applied_cpus);
        if strict {
            validate_boost_disabled(&configuration)?;
        }
        let observations = read_observations(&applied_cpus);
        let metadata = EnvironmentMetadata {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            cpu_model,
            cpu,
            governors,
            strict,
            hostname: read_value("/proc/sys/kernel/hostname").unwrap_or_default(),
            allowed_cpus,
            applied_cpus,
            configuration,
            observations,
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

    /// Reject changes to CPU policy, affinity, ASLR/NUMA policy, or throttling counters.
    /// Temperatures are observations and are deliberately not compared for equality.
    #[cfg(target_os = "linux")]
    pub fn check_stable(&self) -> Result<()> {
        ensure!(
            current_cpus()? == self.metadata.applied_cpus,
            "CPU affinity changed during benchmark"
        );
        ensure!(
            read_configuration(&self.metadata.applied_cpus) == self.metadata.configuration,
            "host CPU, boost, ASLR, or NUMA configuration changed during benchmark"
        );
        let observations = read_observations(&self.metadata.applied_cpus);
        let throttle = |values: &BTreeMap<String, String>| {
            values
                .iter()
                .filter(|(key, _)| key.contains("/thermal_throttle/"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        ensure!(
            throttle(&observations) == throttle(&self.metadata.observations),
            "CPU throttling counters changed during benchmark"
        );
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn check_stable(&self) -> Result<()> {
        anyhow::bail!("benchmark environment controls are supported only on Linux")
    }

    #[cfg(not(target_os = "linux"))]
    pub fn acquire(_: &Path, _: Option<usize>, _: bool) -> Result<Self> {
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

fn read_configuration(cpus: &[usize]) -> BTreeMap<String, String> {
    let mut paths = vec![
        "/sys/devices/system/cpu/cpufreq/boost".to_owned(),
        "/sys/devices/system/cpu/intel_pstate/no_turbo".to_owned(),
        "/proc/sys/kernel/randomize_va_space".to_owned(),
        "/proc/sys/kernel/numa_balancing".to_owned(),
        "/sys/devices/system/node/online".to_owned(),
    ];
    for cpu in cpus {
        for suffix in [
            "cpufreq/scaling_governor",
            "cpufreq/scaling_driver",
            "cpufreq/scaling_min_freq",
            "cpufreq/scaling_max_freq",
            "cpufreq/boost",
            "topology/thread_siblings_list",
            "topology/physical_package_id",
        ] {
            paths.push(format!("/sys/devices/system/cpu/cpu{cpu}/{suffix}"));
        }
    }
    let mut values: BTreeMap<_, _> = paths
        .into_iter()
        .filter_map(|path| read_value(&path).map(|value| (path, value)))
        .collect();
    if let Some(status) = read_value("/proc/self/status")
        && let Some(value) = status
            .lines()
            .find_map(|line| line.strip_prefix("Mems_allowed_list:"))
    {
        values.insert(
            "/proc/self/status:Mems_allowed_list".to_owned(),
            value.trim().to_owned(),
        );
    }
    values
}

fn validate_boost_disabled(configuration: &BTreeMap<String, String>) -> Result<()> {
    let policies: Vec<_> = configuration
        .iter()
        .filter(|(key, _)| key.ends_with("/boost") || key.ends_with("/no_turbo"))
        .collect();
    ensure!(
        !policies.is_empty(),
        "strict mode cannot verify boost policy on this host"
    );
    ensure!(
        policies.iter().all(
            |(key, value)| value.as_str() == if key.ends_with("/no_turbo") { "1" } else { "0" }
        ),
        "strict mode requires CPU boost/turbo disabled"
    );
    Ok(())
}

fn read_observations(cpus: &[usize]) -> BTreeMap<String, String> {
    let mut paths = Vec::new();
    for cpu in cpus {
        if let Ok(entries) =
            std::fs::read_dir(format!("/sys/devices/system/cpu/cpu{cpu}/thermal_throttle"))
        {
            paths.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with("_throttle_count"))
            }));
        }
    }
    if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
        paths.extend(
            entries
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("thermal_zone")
                })
                .map(|entry| entry.path().join("temp")),
        );
    }
    if let Ok(devices) = std::fs::read_dir("/sys/class/hwmon") {
        for device in devices.flatten() {
            if let Ok(entries) = std::fs::read_dir(device.path()) {
                paths.extend(
                    entries
                        .flatten()
                        .filter(|entry| {
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
                            name.starts_with("temp") && name.ends_with("_input")
                        })
                        .map(|entry| entry.path()),
                );
            }
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let path = path.to_string_lossy().into_owned();
            read_value(&path).map(|value| (path, value))
        })
        .collect()
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
            EnvironmentGuard::acquire(&dir.path().join("lock"), Some(before[0]), false).unwrap();
        assert_eq!(current_cpus().unwrap(), vec![before[0]]);
        guard.check_stable().unwrap();
        guard
            .metadata
            .configuration
            .insert("test-policy".to_owned(), "changed".to_owned());
        assert!(guard.check_stable().is_err());
        drop(guard);
        assert_eq!(current_cpus().unwrap(), before);
    }

    #[test]
    fn strict_boost_requires_known_disabled_policy() {
        assert!(validate_boost_disabled(&BTreeMap::new()).is_err());
        let mut policy = BTreeMap::from([("cpu/boost".to_owned(), "0".to_owned())]);
        assert!(validate_boost_disabled(&policy).is_ok());
        policy.insert("cpu/no_turbo".to_owned(), "0".to_owned());
        assert!(validate_boost_disabled(&policy).is_err());
    }

    #[test]
    fn lock_excludes_other_runners_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.lock");
        let guard = EnvironmentGuard::acquire(&path, None, false).unwrap();
        assert!(EnvironmentGuard::acquire(&path, None, false).is_err());
        drop(guard);
        assert!(EnvironmentGuard::acquire(&path, None, false).is_ok());
    }
}
