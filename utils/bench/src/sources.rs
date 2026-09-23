//! Pinned Git sources fetched before benchmark measurement.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;

use crate::program::checked;

/// A source checkout whose revision must be a complete commit ID.
pub struct GitSource {
    pub repository: &'static str,
    pub revision: &'static str,
    pub subdir: &'static str,
}

impl GitSource {
    /// Resolve a clean checkout, fetching only when offline mode is disabled.
    pub fn prepare(&self, cache: &Path, offline: bool, timeout: Duration) -> Result<PathBuf> {
        anyhow::ensure!(
            self.revision.len() == 40 && self.revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "source revision must be a full Git commit ID"
        );
        let git = || {
            let mut command = Command::new("git");
            if offline {
                // A partial clone may have the commit but still lack checkout blobs.
                command
                    .env("GIT_NO_LAZY_FETCH", "1")
                    .env("GIT_ALLOW_PROTOCOL", "");
            }
            command
        };
        let checkout = cache.join(self.revision);
        if !checkout.join(".git").is_dir() {
            anyhow::ensure!(
                !offline,
                "source {} is absent from offline cache {}",
                self.revision,
                cache.display()
            );
            std::fs::create_dir_all(&checkout)?;
            checked(git().arg("init").arg(&checkout), timeout)?;
        }
        let present = crate::process::capture(
            git()
                .arg("-C")
                .arg(&checkout)
                .args(["cat-file", "-e", self.revision]),
            timeout,
        )?
        .status
        .success();
        if !present {
            anyhow::ensure!(
                !offline,
                "revision {} is unavailable offline",
                self.revision
            );
            checked(
                git().arg("-C").arg(&checkout).args([
                    "fetch",
                    "--depth",
                    "1",
                    "--filter=blob:none",
                    self.repository,
                    self.revision,
                ]),
                timeout,
            )?;
        }
        if !self.subdir.is_empty() {
            checked(
                git()
                    .arg("-C")
                    .arg(&checkout)
                    .args(["sparse-checkout", "set", self.subdir]),
                timeout,
            )?;
        }
        checked(
            git()
                .arg("-C")
                .arg(&checkout)
                .args(["checkout", "--detach", self.revision]),
            timeout,
        )?;
        let dirty = checked(
            git()
                .arg("-C")
                .arg(&checkout)
                .args(["status", "--porcelain", "--untracked-files=all"]),
            timeout,
        )?;
        anyhow::ensure!(
            dirty.stdout.is_empty(),
            "pinned source cache {} is modified",
            checkout.display()
        );
        Ok(checkout.join(self.subdir).canonicalize()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_cache_miss_does_not_fetch_or_create_a_checkout() -> Result<()> {
        let cache = tempfile::tempdir()?;
        let source = GitSource {
            repository: "https://invalid.example/source",
            revision: "0000000000000000000000000000000000000000",
            subdir: "",
        };
        assert!(
            source
                .prepare(cache.path(), true, Duration::from_secs(5))
                .is_err()
        );
        assert_eq!(std::fs::read_dir(cache.path())?.count(), 0);
        Ok(())
    }
}
