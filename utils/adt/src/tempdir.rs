use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh directory under the system temporary directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> io::Result<Self> {
        Self::with_prefix("tir-")
    }

    pub fn with_prefix(prefix: &str) -> io::Result<Self> {
        // The counter separates directories within a process, the PID
        // separates concurrent processes, and the clock covers PID reuse after
        // a run that leaked its directory.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        loop {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let name = format!(
                "{prefix}{}-{}-{nanos}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            let mut builder = std::fs::DirBuilder::new();
            // Other users must not read or plant files in a shared `/tmp`.
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Returns the path and leaves the directory in place.
    pub fn keep(self) -> PathBuf {
        std::mem::take(&mut std::mem::ManuallyDrop::new(self).0)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
