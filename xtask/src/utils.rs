use std::path::{Path, PathBuf};
use xshell::{cmd, Shell};

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
