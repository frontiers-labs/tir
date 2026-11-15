//! `cargo xtask lint`: every static check CI runs besides rustfmt.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
    fs,
    io::ErrorKind,
    ops::Range,
    path::{Component, Path, PathBuf},
};

use anyhow::{ensure, Context};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use xshell::{cmd, Shell};

#[derive(Args)]
pub struct Options {
    /// Install missing tools for the selected lints without running checks.
    #[arg(long)]
    install: bool,
    /// Run only these lints. Accepts comma-separated or repeated values.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "LINT")]
    only: Vec<Lint>,
    /// Skip these lints, including those selected with --only.
    #[arg(long, value_enum, value_delimiter = ',', value_name = "LINT")]
    exclude: Vec<Lint>,
    /// Report only findings on committed lines changed between COMMIT and HEAD.
    #[arg(long, value_name = "COMMIT", conflicts_with = "install")]
    diff: Option<String>,
    /// Fail when a lint reports warnings, including missing optional prerequisites.
    #[arg(long)]
    deny_warnings: bool,
    /// Check this checkout instead of the source checkout used to build xtask.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Lint {
    Scripts,
    Dependencies,
    Dupehound,
    AstGrep,
    Clippy,
    Rustdoc,
    Jevlint,
}

struct ChangedLines(BTreeMap<PathBuf, Vec<Range<u32>>>);

impl ChangedLines {
    fn between(root: &Path, commit: &str) -> anyhow::Result<Self> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let revision = format!("{commit}^{{commit}}");
        let base = cmd!(sh, "git rev-parse --verify --end-of-options {revision}")
            .read()
            .with_context(|| format!("invalid diff commit `{commit}`"))?;
        let output = cmd!(
            sh,
            "git diff --name-status -z --find-renames --diff-filter=AMR {base} HEAD --"
        )
        .output()?;
        let mut names = std::str::from_utf8(&output.stdout)?.split_terminator('\0');
        let mut changed = BTreeMap::new();
        while let Some(status) = names.next() {
            let first = names.next().context("missing Git diff path")?;
            let paths = match status.as_bytes().first() {
                Some(b'A' | b'M') => vec![first],
                Some(b'R') => vec![
                    first,
                    names.next().context("missing Git rename destination")?,
                ],
                _ => anyhow::bail!("unexpected Git diff status `{status}`"),
            };
            let path = PathBuf::from(*paths.last().context("missing Git diff path")?);
            let output = cmd!(
                sh,
                "git --literal-pathspecs diff --unified=0 --inter-hunk-context=0 --no-ext-diff --no-textconv --no-color --find-renames {base} HEAD -- {paths...}"
            )
            .output()?;
            let mut ranges = Vec::new();
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Some(hunk) = line.strip_prefix("@@ ") else {
                    continue;
                };
                let added = hunk
                    .split_whitespace()
                    .nth(1)
                    .and_then(|range| range.strip_prefix('+'))
                    .context("invalid Git diff hunk")?;
                let (start, count) = added.split_once(',').unwrap_or((added, "1"));
                let start: u32 = start.parse()?;
                let count: u32 = count.parse()?;
                if count != 0 {
                    let end = start.checked_add(count).context("Git diff line overflow")?;
                    ranges.push(start..end);
                }
            }
            changed.insert(path, ranges);
        }
        Ok(Self(changed))
    }

    fn contains(&self, root: &Path, finding: &LintDiagnostic) -> bool {
        if finding.file.is_empty() {
            return true;
        }
        let path = Path::new(&finding.file);
        let path: PathBuf = path
            .strip_prefix(root)
            .unwrap_or(path)
            .components()
            .filter(|component| !matches!(component, Component::CurDir))
            .collect();
        self.0.get(&path).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|range| range.start <= finding.end_line && finding.line < range.end)
        })
    }
}

pub trait LintRule {
    fn name(&self) -> &'static str;
    fn tools(&self) -> &'static [Tool] {
        &[]
    }
    fn optional(&self) -> bool {
        false
    }
    fn required_env(&self) -> &'static [&'static str] {
        &[]
    }
    fn install(&self, root: &Path) -> anyhow::Result<()> {
        for tool in self.tools() {
            tool.install(root)?;
        }
        Ok(())
    }
    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>>;
}

pub struct Tool {
    pub probe: &'static [&'static str],
    pub version: Option<&'static str>,
    pub install: &'static [&'static str],
}

impl Tool {
    fn available(&self, root: &Path) -> bool {
        let Ok(sh) = Shell::new() else { return false };
        sh.change_dir(root);
        let program = self.probe[0];
        let args = &self.probe[1..];
        cmd!(sh, "{program} {args...}")
            .ignore_status()
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && self.version.is_none_or(|version| {
                        String::from_utf8_lossy(&output.stdout)
                            .split_whitespace()
                            .any(|word| word == version)
                    })
            })
    }

    fn check(&self, root: &Path) -> anyhow::Result<()> {
        ensure!(
            self.available(root),
            "{}{} is unavailable; run `{}` or `cargo xtask lint --install`",
            self.probe[0],
            self.version
                .map_or(String::new(), |version| format!(" {version}")),
            self.install.join(" "),
        );
        Ok(())
    }

    fn install(&self, root: &Path) -> anyhow::Result<()> {
        if !self.available(root) {
            let sh = Shell::new()?;
            sh.change_dir(root);
            let program = self.install[0];
            let args = &self.install[1..];
            cmd!(sh, "{program} {args...}").run()?;
        }
        self.check(root)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Error => "error",
            Self::Warning => "warning",
        })
    }
}

pub struct LintDiagnostic {
    pub severity: Severity,
    pub line: u32,
    pub end_line: u32,
    pub file: String,
    pub message: String,
}

fn escape_annotation(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

impl Display for LintDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let severity = self.severity;
        if self.file.is_empty() {
            if std::env::var_os("CI").is_some() {
                write!(f, "::{severity}::{}", escape_annotation(&self.message))
            } else {
                write!(f, "{severity}: {}", self.message)
            }
        } else if std::env::var_os("CI").is_some() {
            let file = escape_annotation(&self.file)
                .replace(':', "%3A")
                .replace(',', "%2C");
            write!(
                f,
                "::{severity} file={file},line={}::{}",
                self.line,
                escape_annotation(&self.message)
            )
        } else {
            write!(
                f,
                "{}:{} - {severity}: {}",
                self.file, self.line, self.message
            )
        }
    }
}

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

struct Dependencies;

impl LintRule for Dependencies {
    fn name(&self) -> &'static str {
        "production dependencies"
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        let output = cmd!(sh, "cargo metadata --no-deps --format-version 1")
            .ignore_status()
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
            .filter(|dependency| {
                dependency.kind.as_deref() != Some("dev") && dependency.path.is_none()
            })
            .map(|dependency| dependency.name)
            .collect();
        let path = "utils/scripts/production-dependencies.txt";
        let text = fs::read_to_string(root.join(path))?;
        let allowed: BTreeSet<_> = text.split_whitespace().map(str::to_owned).collect();
        let mut errors = Vec::new();
        for dependency in actual.difference(&allowed) {
            errors.push(LintDiagnostic {
                severity: Severity::Error,
                file: String::new(),
                line: 0,
                end_line: 0,
                message: format!(
                    "new production dependency `{dependency}`; implement it in-tree instead"
                ),
            });
        }
        for dependency in allowed.difference(&actual) {
            errors.push(LintDiagnostic { severity: Severity::Error,
                file: String::new(),
                line: 0,
                end_line: 0,
                message: format!("`{dependency}` is no longer a production dependency; remove it from production-dependencies.txt"),
            });
        }
        Ok(errors)
    }
}

struct Clippy;
struct Rustdoc;

impl LintRule for Clippy {
    fn name(&self) -> &'static str {
        "clippy"
    }

    fn tools(&self) -> &'static [Tool] {
        &[Tool {
            probe: &["cargo", "clippy", "--version"],
            version: None,
            install: &["rustup", "component", "add", "clippy"],
        }]
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        cargo_diagnostics(cmd!(
            sh,
            "cargo clippy --workspace --all-targets --no-deps --message-format=json -- -D warnings"
        ))
    }
}

impl LintRule for Rustdoc {
    fn name(&self) -> &'static str {
        "rustdoc"
    }

    fn tools(&self) -> &'static [Tool] {
        &[Tool {
            probe: &["rustdoc", "--version"],
            version: None,
            install: &["rustup", "component", "add", "rustc"],
        }]
    }

    fn run(&self, root: &Path) -> anyhow::Result<Vec<LintDiagnostic>> {
        let sh = Shell::new()?;
        sh.change_dir(root);
        // Skip rewriting the search index shared by all crates under one lock.
        cargo_diagnostics(
            cmd!(sh, "cargo doc --workspace --no-deps --message-format=json").env(
                "RUSTDOCFLAGS",
                "-D warnings -Zunstable-options --merge=none",
            ),
        )
    }
}

#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    message: Option<Diagnostic>,
}

#[derive(Deserialize)]
struct Diagnostic {
    message: String,
    rendered: Option<String>,
    level: String,
    spans: Vec<Span>,
    children: Vec<Diagnostic>,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    line_start: u32,
    is_primary: bool,
}

fn cargo_diagnostics(command: xshell::Cmd<'_>) -> anyhow::Result<Vec<LintDiagnostic>> {
    let output = command.ignore_status().output()?;
    let mut errors = Vec::new();
    for line in std::str::from_utf8(&output.stdout)?.lines() {
        // Build scripts may print ordinary text among Cargo's JSON messages.
        if !line.starts_with('{') {
            continue;
        }
        let event: CargoMessage =
            serde_json::from_str(line).context("invalid cargo diagnostic output")?;
        if event.reason != "compiler-message" {
            continue;
        }
        let Some(diagnostic) = event.message else {
            continue;
        };
        if !matches!(diagnostic.level.as_str(), "error" | "warning") {
            continue;
        }
        let message = diagnostic.rendered.unwrap_or_else(|| {
            let mut message = diagnostic.message;
            for child in diagnostic.children {
                message.push_str(&format!("\n{}: {}", child.level, child.message));
            }
            message
        });
        if let Some(span) = diagnostic
            .spans
            .iter()
            .find(|span| span.is_primary)
            .or_else(|| diagnostic.spans.first())
        {
            errors.push(LintDiagnostic {
                severity: Severity::Error,
                file: span.file_name.clone(),
                line: span.line_start,
                end_line: span.line_start,
                message,
            });
        } else {
            errors.push(LintDiagnostic {
                severity: Severity::Error,
                file: String::new(),
                line: 0,
                end_line: 0,
                message,
            });
        }
    }
    ensure!(
        output.status.success() || !errors.is_empty(),
        "cargo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(errors)
}

pub fn install_git_hooks(root: &Path) -> anyhow::Result<()> {
    let sh = Shell::new()?;
    sh.change_dir(root);
    let output = cmd!(sh, "git rev-parse --git-path hooks/pre-commit")
        .ignore_status()
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

pub fn run(root: &Path, options: Options) -> anyhow::Result<()> {
    let root = options
        .root
        .as_deref()
        .unwrap_or(root)
        .canonicalize()
        .context("could not resolve lint root")?;
    let root = root.as_path();
    let changed = options
        .diff
        .as_deref()
        .map(|commit| ChangedLines::between(root, commit))
        .transpose()?;
    let rules: &[(Lint, &dyn LintRule)] = &[
        (Lint::Scripts, &crate::scripts::Scripts),
        (Lint::Dependencies, &Dependencies),
        (Lint::Dupehound, &crate::dupehound::Dupehound),
        (Lint::AstGrep, &crate::ast_grep::RuleTests),
        (Lint::AstGrep, &crate::ast_grep::AstGrep),
        (Lint::Clippy, &Clippy),
        (Lint::Rustdoc, &Rustdoc),
        (Lint::Jevlint, &crate::jevlint::Jevlint),
    ];
    let mut diagnostics = Vec::new();
    for (lint, rule) in rules {
        if (!options.only.is_empty() && !options.only.contains(lint))
            || options.exclude.contains(lint)
        {
            continue;
        }
        let result = if options.install {
            rule.install(root).map(|()| Vec::new())
        } else {
            let mut prerequisites: Vec<_> = rule
                .tools()
                .iter()
                .filter_map(|tool| tool.check(root).err().map(|error| format!("{error:#}")))
                .collect();
            for variable in rule.required_env() {
                if std::env::var_os(variable).is_none_or(|value| value.is_empty()) {
                    prerequisites.push(format!("{variable} is missing or empty"));
                }
            }
            if prerequisites.is_empty() {
                rule.run(root)
            } else {
                let severity = if rule.optional() {
                    Severity::Warning
                } else {
                    Severity::Error
                };
                Ok(prerequisites
                    .into_iter()
                    .map(|reason| LintDiagnostic {
                        severity,
                        file: String::new(),
                        line: 0,
                        end_line: 0,
                        message: format!("skipping {}: {reason}", rule.name()),
                    })
                    .collect())
            }
        };
        match result {
            Ok(findings) => diagnostics.extend(findings),
            Err(error) => diagnostics.push(LintDiagnostic {
                severity: Severity::Error,
                file: String::new(),
                line: 0,
                end_line: 0,
                message: format!("{}: {error:#}", rule.name()),
            }),
        }
    }
    if let Some(changed) = changed {
        diagnostics.retain(|finding| changed.contains(root, finding));
    }
    for diagnostic in &diagnostics {
        println!("{diagnostic}");
    }
    ensure!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error || options.deny_warnings),
        "lint {} failed",
        if options.install {
            "installation"
        } else {
            "checks"
        }
    );
    Ok(())
}
