use std::{fs::File, io::Read, path::Path, process::Command};

use anyhow::{ensure, Context, Result};

fn script(path: &Path) -> Result<bool> {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "py" | "pyw" | "sh" | "bash" | "bats"
            )
        })
    {
        return Ok(true);
    }
    let mut first = [0; 256];
    let count = File::open(path)?.read(&mut first)?;
    let line = String::from_utf8_lossy(&first[..count]);
    let Some(shebang) = line.lines().next().and_then(|line| line.strip_prefix("#!")) else {
        return Ok(false);
    };
    let mut words = shebang.split_whitespace();
    let Some(command) = words.next() else {
        return Ok(false);
    };
    let mut name = command.rsplit('/').next().unwrap_or(command);
    if name == "env" {
        name = words
            .find(|word| !word.starts_with('-') && !word.contains('='))
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
    }
    Ok(matches!(name, "bash" | "sh")
        || (name == "uv" && words.any(|word| word == "--script"))
        || name.strip_prefix("python").is_some_and(|version| {
            version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        }))
}

fn allowed(path: &Path) -> bool {
    [".agents", ".codex", ".claude", "utils/scripts"]
        .iter()
        .any(|dir| path.starts_with(dir))
        || [
            "utils/python/tir/__init__.py",
            "utils/python/tir/_capi.py",
            "utils/python/tests/test_tir.py",
        ]
        .iter()
        .any(|file| path == Path::new(file))
}

pub fn run(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let paths = std::str::from_utf8(&output.stdout).context("non-UTF-8 repository path")?;
    let mut failed = false;
    for name in paths.split('\0').filter(|name| !name.is_empty()) {
        let path = Path::new(name);
        let full = root.join(path);
        if allowed(path) || !full.is_file() {
            continue;
        }
        if script(&full).with_context(|| format!("checking {name}"))? {
            eprintln!("error: shell and Python scripts must be in .agents, .codex, .claude, or utils/scripts: {name}");
            failed = true;
        }
    }
    ensure!(!failed, "script location check failed");
    Ok(())
}
