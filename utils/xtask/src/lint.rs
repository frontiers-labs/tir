//! `cargo xtask lint`: every static check CI runs besides rustfmt.
//!
//! ast-grep and dryer run through `uv` at the versions the scripts in
//! `.github/scripts` pin, so `uv` is the only tool to install.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::ensure;
use xshell::{cmd, Shell};

pub fn run(sh: &Shell, root: &Path) -> anyhow::Result<()> {
    sh.change_dir(root);
    // The scripts never wait on cargo's build directory lock, so they run
    // beside it.
    let scripts = [
        ".github/scripts/check-ast-grep.py",
        ".github/scripts/check-duplicates.py",
    ]
    .map(|script| {
        Command::new(root.join(script))
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    });
    let cargo = cmd!(
        sh,
        "cargo clippy --workspace --all-targets --no-deps -- -D warnings"
    )
    .run()
    .and_then(|()| {
        cmd!(sh, "cargo doc --workspace --no-deps")
            .env("RUSTDOCFLAGS", "-D warnings")
            .run()
    });
    let mut failed = Vec::new();
    for (name, script) in ["ast-grep", "duplicates"].into_iter().zip(scripts) {
        let output = script?.wait_with_output()?;
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        if !output.status.success() {
            failed.push(name);
        }
    }
    cargo?;
    ensure!(failed.is_empty(), "{} check failed", failed.join(" and "));
    Ok(())
}
