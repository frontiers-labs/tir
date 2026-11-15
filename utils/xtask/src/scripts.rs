use std::{collections::BTreeSet, fs::File, io::Read, path::Path};

use anyhow::{ensure, Context, Result};
use xshell::{cmd, Shell};

use crate::lint::{LintDiagnostic, LintRule, Severity};

enum Script {
    Shell,
    Python,
}

fn script(path: &Path) -> Result<Option<Script>> {
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        match extension.to_ascii_lowercase().as_str() {
            "py" | "pyw" => return Ok(Some(Script::Python)),
            "sh" | "bash" | "bats" => return Ok(Some(Script::Shell)),
            _ => {}
        }
    }
    let mut first = [0; 256];
    let count = File::open(path)?.read(&mut first)?;
    let line = String::from_utf8_lossy(&first[..count]);
    let Some(shebang) = line.lines().next().and_then(|line| line.strip_prefix("#!")) else {
        return Ok(None);
    };
    let mut words = shebang.split_whitespace();
    let Some(command) = words.next() else {
        return Ok(None);
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
    Ok(if matches!(name, "bash" | "sh") {
        Some(Script::Shell)
    } else if (name == "uv" && words.any(|word| word == "--script"))
        || name.strip_prefix("python").is_some_and(|version| {
            version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
    {
        Some(Script::Python)
    } else {
        None
    })
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

pub struct Scripts;

impl LintRule for Scripts {
    fn name(&self) -> &'static str {
        "script locations"
    }

    fn run(&self, root: &Path) -> Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let output = cmd!(sh, "git ls-files --cached --others --exclude-standard -z")
            .ignore_status()
            .output()?;
        ensure!(
            output.status.success(),
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let paths = std::str::from_utf8(&output.stdout).context("non-UTF-8 repository path")?;
        let base = std::env::var("LINT_BASE").unwrap_or_else(|_| "master".to_owned());
        let previous = cmd!(sh, "git ls-tree --name-only --full-tree -rz {base}").read()?;
        let existing: BTreeSet<_> = previous.split_terminator('\0').collect();
        let mut errors = Vec::new();
        for name in paths.split('\0').filter(|name| !name.is_empty()) {
            let path = Path::new(name);
            let full = root.join(path);
            if !full.is_file() {
                continue;
            }
            let kind = script(&full).with_context(|| format!("checking {name}"))?;
            if matches!(kind, Some(Script::Python)) && !existing.contains(name) {
                errors.push(LintDiagnostic {
                    severity: Severity::Error,
                    file: String::new(),
                    line: 0,
                    end_line: 0,
                    message: format!("new Python files are prohibited; use Rust: {name}"),
                });
            } else if kind.is_some() && !allowed(path) {
                errors.push(LintDiagnostic { severity: Severity::Error, file: name.to_owned(), line: 1, end_line: 1, message: "shell and Python scripts must be in .agents, .codex, .claude, or utils/scripts".to_owned() });
            }
        }
        Ok(errors)
    }
}
