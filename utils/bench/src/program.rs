//! Untimed preparation and reproducible input identity for process benchmarks.
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Run preparation or verification, retaining the command and stderr on failure.
pub fn checked(command: &mut Command, timeout: Duration) -> Result<Output> {
    let output = crate::process::capture(command, timeout)
        .with_context(|| format!("running {command:?}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{command:?} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

/// Hash length-delimited parameters and declared input contents in caller order.
/// Paths are relative identities, so moving a checkout does not change its work.
pub fn digest_inputs(
    root: &Path,
    files: &[std::path::PathBuf],
    parameters: &[String],
) -> Result<String> {
    let mut digest = Sha256::new();
    for value in parameters {
        field(&mut digest, value.as_bytes());
    }
    for file in files {
        field(
            &mut digest,
            file.strip_prefix(root)
                .unwrap_or(file)
                .to_string_lossy()
                .as_bytes(),
        );
        field(
            &mut digest,
            &std::fs::read(file).with_context(|| format!("reading {}", file.display()))?,
        );
    }
    Ok(format!("{:x}", digest.result()))
}

fn field(digest: &mut Sha256, value: &[u8]) {
    digest.input((value.len() as u64).to_le_bytes());
    digest.input(value);
}

/// Resolve a process executable through PATH and hash its bytes for provenance.
pub fn executable_identity(program: &Path) -> Result<(std::path::PathBuf, String)> {
    let path = if program.components().count() > 1 || program.is_absolute() {
        program.canonicalize()?
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(program))
            .find(|path| path.is_file())
            .with_context(|| format!("executable {} is absent from PATH", program.display()))?
            .canonicalize()?
    };
    let hash = format!("{:x}", Sha256::digest(&std::fs::read(&path)?));
    Ok((path, hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_identity_tracks_headers_parameters_and_relative_names() -> Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        for directory in [first.path(), second.path()] {
            std::fs::write(directory.join("input.c"), "#include \"input.h\"\n")?;
            std::fs::write(directory.join("input.h"), "#define N 42\n")?;
        }
        let files = |root: &Path| vec![root.join("input.c"), root.join("input.h")];
        let original = digest_inputs(first.path(), &files(first.path()), &["1000".into()])?;
        assert_eq!(
            original,
            digest_inputs(second.path(), &files(second.path()), &["1000".into()])?
        );
        assert_ne!(
            original,
            digest_inputs(first.path(), &files(first.path()), &["1001".into()])?
        );
        std::fs::write(first.path().join("input.h"), "#define N 43\n")?;
        assert_ne!(
            original,
            digest_inputs(first.path(), &files(first.path()), &["1000".into()])?
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn checked_captures_stdout_and_bounds_preparation() {
        let output = checked(
            Command::new("/bin/sh").args(["-c", "printf prepared"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.stdout, b"prepared");
        let start = std::time::Instant::now();
        let error = checked(
            Command::new("/bin/sh").args(["-c", "sleep 30 & wait"]),
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn failed_preparation_is_an_error() {
        assert!(
            checked(
                Command::new("false").arg("preparation"),
                Duration::from_secs(5)
            )
            .is_err()
        );
    }
}
