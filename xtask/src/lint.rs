//! `cargo xtask lint`: every static check CI runs besides rustfmt.
//!
//! ast-grep and dryer run through `uvx` at the versions the scripts in
//! `.github/scripts` pin, so `uv` is the only tool to install.

use std::path::Path;

use xshell::{cmd, Shell};

pub fn run(sh: &Shell, root: &Path) -> anyhow::Result<()> {
    sh.change_dir(root);
    cmd!(sh, ".github/scripts/check-ast-grep.py").run()?;
    cmd!(sh, ".github/scripts/check-duplicates.py").run()?;
    cmd!(
        sh,
        "cargo clippy --workspace --all-targets --no-deps -- -D warnings"
    )
    .run()?;
    cmd!(sh, "cargo doc --workspace --no-deps")
        .env("RUSTDOCFLAGS", "-D warnings")
        .run()?;
    Ok(())
}
