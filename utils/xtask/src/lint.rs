//! `cargo xtask lint`: every static check CI runs besides rustfmt.
//!

use std::{collections::BTreeSet, fs, io::ErrorKind, path::Path, process::Command};

use anyhow::ensure;
use serde::Deserialize;
use xshell::{cmd, Shell};

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
    path: Option<String>,
}

fn dependencies(root: &Path) -> anyhow::Result<()> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let actual: BTreeSet<_> = metadata
        .packages
        .into_iter()
        .flat_map(|package| package.dependencies)
        .filter(|dependency| dependency.kind.as_deref() != Some("dev") && dependency.path.is_none())
        .map(|dependency| dependency.name)
        .collect();
    let text = fs::read_to_string(root.join("utils/scripts/production-dependencies.txt"))?;
    let allowed: BTreeSet<_> = text.split_whitespace().map(str::to_owned).collect();
    for dependency in actual.difference(&allowed) {
        eprintln!("error: new production dependency `{dependency}`; implement it in-tree instead");
    }
    for dependency in allowed.difference(&actual) {
        eprintln!("error: `{dependency}` is no longer a production dependency; remove it from production-dependencies.txt");
    }
    ensure!(actual == allowed, "production dependency check failed");
    Ok(())
}

pub fn install_git_hooks(root: &Path) -> anyhow::Result<()> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--git-path", "hooks/pre-commit"])
        .output()?;
    ensure!(
        output.status.success(),
        "could not find Git hooks directory"
    );
    let path = root.join(std::str::from_utf8(&output.stdout)?.trim());
    let content = "#!/bin/sh\nexec cargo xtask lint\n";
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(".tir-pre-commit-{}.tmp", std::process::id()));
    match fs::remove_file(&temporary) {
        Ok(()) => (),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    fs::write(&temporary, content)?;
    match fs::hard_link(&temporary, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary)?;
            ensure!(
                fs::read_to_string(&path)? == content,
                "{} already exists",
                path.display()
            );
        }
        Err(error) => {
            fs::remove_file(&temporary)?;
            return Err(error.into());
        }
    }
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    println!("Installed {}", path.display());
    Ok(())
}

pub fn run(sh: &Shell, root: &Path) -> anyhow::Result<()> {
    sh.change_dir(root);
    crate::scripts::run(root)?;
    dependencies(root)?;
    let duplicates = crate::dupehound::run(root);
    let ast_grep = crate::ast_grep::run(root);
    let cargo = cmd!(
        sh,
        "cargo clippy --workspace --all-targets --no-deps -- -D warnings"
    )
    .run()
    .and_then(|()| {
        // Only the diagnostics matter here. Without merging, each crate skips
        // rewriting the search index shared by all of them under one lock.
        cmd!(sh, "cargo doc --workspace --no-deps")
            .env(
                "RUSTDOCFLAGS",
                "-D warnings -Zunstable-options --merge=none",
            )
            .run()
    });
    ast_grep?;
    cargo?;
    duplicates?;
    Ok(())
}
