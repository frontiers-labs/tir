//! Runtime cache geometry for working-set choices.

use std::{num::NonZeroUsize, sync::OnceLock};

/// Cache geometry shared by the CPUs reported by the operating system.
///
/// Capacities are conservative minima for data or unified caches at each level.
/// Unavailable capacities remain unknown; an unavailable line size uses 64 bytes.
#[derive(Clone, Copy, Debug)]
pub struct CacheInfo {
    /// Largest reported power-of-two coherency line size, in bytes.
    pub line_size: NonZeroUsize,
    /// Smallest reported L1 data-cache capacity, in bytes.
    pub l1_data: Option<NonZeroUsize>,
    /// Smallest reported L2 capacity, in bytes.
    pub l2: Option<NonZeroUsize>,
}

impl CacheInfo {
    /// Detects cache geometry once. Platforms without a detector use unknown
    /// capacities and a 64-byte line.
    pub fn detect() -> &'static Self {
        static CACHE: OnceLock<CacheInfo> = OnceLock::new();
        CACHE.get_or_init(|| {
            #[cfg(target_os = "linux")]
            {
                Self::from_sysfs(std::path::Path::new("/sys/devices/system/cpu"))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Self::fallback()
            }
        })
    }

    fn fallback() -> Self {
        Self {
            line_size: NonZeroUsize::new(64).unwrap(),
            l1_data: None,
            l2: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn from_sysfs(root: &std::path::Path) -> Self {
        use std::fs;
        let mut info = Self::fallback();
        let mut line = None;
        let Ok(cpus) = fs::read_dir(root) else {
            return info;
        };
        for cpu in cpus.flatten() {
            let name = cpu.file_name();
            let Some(name) = name.to_str().and_then(|name| name.strip_prefix("cpu")) else {
                continue;
            };
            if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let Ok(indices) = fs::read_dir(cpu.path().join("cache")) else {
                continue;
            };
            for index in indices.flatten() {
                let path = index.path();
                let Ok(kind) = fs::read_to_string(path.join("type")) else {
                    continue;
                };
                if !matches!(kind.trim(), "Data" | "Unified") {
                    continue;
                }
                if let Ok(raw) = fs::read_to_string(path.join("coherency_line_size"))
                    && let Ok(value) = raw.trim().parse::<usize>()
                    && value.is_power_of_two()
                    && value <= isize::MAX as usize
                {
                    line = Some(line.map_or(value, |previous: usize| previous.max(value)));
                }
                let capacity = match fs::read_to_string(path.join("size")) {
                    Ok(size) => parse_capacity(&size),
                    Err(_) => None,
                };
                let target = match fs::read_to_string(path.join("level")) {
                    Ok(level) => match level.trim() {
                        "1" => &mut info.l1_data,
                        "2" => &mut info.l2,
                        _ => continue,
                    },
                    Err(_) => continue,
                };
                if let Some(capacity) = capacity {
                    *target = Some(target.map_or(capacity, |old| old.min(capacity)));
                }
            }
        }
        if let Some(line) = line {
            info.line_size = NonZeroUsize::new(line).unwrap();
        }
        info
    }
}

#[cfg(target_os = "linux")]
fn parse_capacity(raw: &str) -> Option<NonZeroUsize> {
    let raw = raw.trim();
    let (number, scale) = if let Some(number) = raw.strip_suffix('K') {
        (number, 1024)
    } else if let Some(number) = raw.strip_suffix('M') {
        (number, 1024 * 1024)
    } else {
        (raw, 1)
    };
    NonZeroUsize::new(number.parse::<usize>().ok()?.checked_mul(scale)?)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::CacheInfo;
    use std::fs;

    #[test]
    fn sysfs_geometry_uses_conservative_data_caches_and_unknown_fallback() {
        let root = crate::TempDir::new().unwrap();
        let missing = CacheInfo::from_sysfs(&root.path().join("absent"));
        assert_eq!(missing.line_size.get(), 64);
        assert_eq!((missing.l1_data, missing.l2), (None, None));
        for (cpu, index, kind, level, size, line) in [
            (0, 0, "Data", "1", "48K", "64"),
            (1, 0, "Data", "1", "32K", "128"),
            (0, 1, "Instruction", "1", "16K", "256"),
            (0, 2, "Unified", "2", "1M", "64"),
            (1, 2, "Unified", "2", "512K", "96"),
        ] {
            let path = root.path().join(format!("cpu{cpu}/cache/index{index}"));
            fs::create_dir_all(&path).unwrap();
            for (field, value) in [
                ("type", kind),
                ("level", level),
                ("size", size),
                ("coherency_line_size", line),
            ] {
                fs::write(path.join(field), value).unwrap();
            }
        }
        let info = CacheInfo::from_sysfs(root.path());
        assert_eq!(info.line_size.get(), 128);
        assert_eq!(info.l1_data.unwrap().get(), 32 * 1024);
        assert_eq!(info.l2.unwrap().get(), 512 * 1024);
    }
}
