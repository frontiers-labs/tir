use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use clap::Args;
use serde::Deserialize;
use xshell::{cmd, Shell};

#[derive(Args)]
pub struct Options {
    /// Limit formal proofs to this Cargo workspace package.
    #[arg(short, long)]
    pub package: Option<String>,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    target_directory: PathBuf,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    scenario: BTreeMap<String, Scenario>,
    #[serde(rename = "project-files")]
    project_files: Option<bool>,
}

#[derive(Deserialize)]
struct Scenario {
    backend: String,
    package: Option<String>,
    #[serde(rename = "output-dir")]
    output_dir: Option<PathBuf>,
    #[serde(rename = "project-files")]
    project_files: Option<bool>,
}

struct PackageProofs<'a> {
    package: &'a Package,
    directory: &'a Path,
    scenarios: Vec<(String, Scenario)>,
    project_files: Option<bool>,
    kani_enabled: bool,
}

#[derive(Deserialize)]
struct KaniReport {
    verification_results: KaniResults,
}

#[derive(Deserialize)]
struct KaniResults {
    results: Vec<KaniHarness>,
}

#[derive(Deserialize)]
struct KaniHarness {
    harness_id: String,
    checks: Vec<KaniCheck>,
}

#[derive(Deserialize)]
struct KaniCheck {
    category: String,
    status: String,
    description: String,
}

pub fn run(root: &Path, options: Options) -> Result<()> {
    let sh = Shell::new()?;
    sh.change_dir(root);
    let output = cmd!(sh, "cargo metadata --no-deps --format-version 1")
        .read()
        .context("failed to run cargo metadata")?;
    let metadata: Metadata = serde_json::from_str(&output)?;
    let packages: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .collect();
    if let Some(name) = &options.package {
        ensure!(
            packages.iter().any(|package| package.name == *name),
            "no workspace package named `{name}`"
        );
    }
    let workspace_config = read_config(&root.join("hax.toml"))?;
    let workspace_project_files = workspace_config
        .as_ref()
        .and_then(|config| config.project_files);
    let mut proofs = Vec::new();
    for package in packages {
        let directory = package
            .manifest_path
            .parent()
            .context("package manifest has no parent directory")?;
        let config = if directory == root {
            None
        } else {
            read_config(&directory.join("hax.toml"))?
        };
        let (scenarios, project_files) = config
            .map(|config| {
                (
                    config.scenario.into_iter().collect::<Vec<_>>(),
                    config.project_files,
                )
            })
            .unwrap_or_default();
        let kani_enabled = package.metadata.get("kani").is_some();
        proofs.push(PackageProofs {
            package,
            directory,
            scenarios,
            project_files,
            kani_enabled,
        });
    }
    // Hax member scenarios shadow same-named root scenarios across the workspace,
    // including members outside a requested package filter.
    let member_scenarios: BTreeSet<_> = proofs
        .iter()
        .flat_map(|proof| proof.scenarios.iter().map(|(name, _)| name.clone()))
        .collect();
    for (name, scenario) in workspace_config
        .into_iter()
        .flat_map(|config| config.scenario)
    {
        if member_scenarios.contains(&name) {
            continue;
        }
        let selected = match &scenario.package {
            Some(package) => proofs
                .iter_mut()
                .find(|proof| proof.package.name == *package)
                .with_context(|| {
                    format!("workspace scenario `{name}` selects unknown package `{package}`")
                })?,
            None => {
                ensure!(
                    proofs.len() == 1,
                    "workspace scenario `{name}` requires a package in a multi-member workspace"
                );
                &mut proofs[0]
            }
        };
        selected.scenarios.push((name, scenario));
    }
    for proof in &mut proofs {
        proof
            .scenarios
            .retain(|(_, scenario)| scenario.backend == "lean");
    }
    proofs.retain(|proof| {
        options
            .package
            .as_ref()
            .is_none_or(|name| proof.package.name == *name)
    });
    if let Some(name) = &options.package {
        ensure!(
            proofs
                .iter()
                .any(|proof| !proof.scenarios.is_empty() || proof.kani_enabled),
            "package `{name}` has no configured Lean or Kani proofs"
        );
    }
    proofs.retain(|proof| !proof.scenarios.is_empty() || proof.kani_enabled);
    ensure!(
        !proofs.is_empty(),
        "no configured Lean or Kani proofs in the workspace"
    );

    let lean_enabled = proofs.iter().any(|proof| !proof.scenarios.is_empty());
    if lean_enabled {
        cmd!(sh, "cargo hax --version")
            .output()
            .context("formal-test requires Hax; install cargo-hax and put it in PATH")?;
        // Elan need not have a global default: each scenario supplies lean-toolchain.
        for executable in ["lean", "lake"] {
            let in_path = std::env::var_os("PATH").is_some_and(|paths| {
                std::env::split_paths(&paths).any(|directory| {
                    fs::metadata(directory.join(executable)).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
            });
            ensure!(
                in_path,
                "formal-test requires Lean; put lean and lake in PATH"
            );
        }
    }
    if proofs.iter().any(|proof| proof.kani_enabled) {
        cmd!(sh, "cargo kani --version")
            .output()
            .context("formal-test requires Kani; install cargo-kani and put it in PATH")?;
    }

    for proof in proofs {
        let _package_directory = sh.push_dir(proof.directory);
        let package_name = &proof.package.name;
        if !proof.scenarios.is_empty() {
            let scenario_names = proof.scenarios.iter().map(|(name, _)| name);
            let validation = cmd!(
                sh,
                "cargo hax extract --dry-run -p {package_name} {scenario_names...}"
            )
            .ignore_status()
            .output()
            .context("failed to validate Hax scenarios")?;
            ensure!(
                validation.status.success(),
                "invalid Hax scenarios for package `{package_name}`: {}",
                String::from_utf8_lossy(&validation.stderr)
            );
        }
        for (name, scenario) in proof.scenarios {
            let proof_directory = checked_output_directory(
                root,
                &proof.directory.join(
                    scenario
                        .output_dir
                        .unwrap_or_else(|| PathBuf::from("proofs").join(&name).join("lean")),
                ),
            )?;
            if scenario
                .project_files
                .or(proof.project_files)
                .or(workspace_project_files)
                .unwrap_or(true)
            {
                for generated in ["lakefile.toml", "lean-toolchain", "lake-manifest.json"] {
                    let path = proof_directory.join(generated);
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("failed to remove {}", path.display()))
                        }
                    }
                }
            }
            cmd!(sh, "cargo hax extract -p {package_name} {name}")
                .run()
                .with_context(|| format!("Hax extraction failed for scenario `{name}`"))?;
            let _proof_directory = sh.push_dir(&proof_directory);
            cmd!(sh, "lake build")
                .run()
                .with_context(|| format!("Lean proofs failed for scenario `{name}`"))?;
        }
        if proof.kani_enabled {
            let report_directory = sh.create_dir(metadata.target_directory.join("formal-test"))?;
            let report_path = report_directory.join(format!("{package_name}.json"));
            sh.remove_path(&report_path)?;
            cmd!(
                sh,
                "cargo kani -p {package_name} -Z unstable-options --export-json {report_path}"
            )
            .run()
            .with_context(|| format!("Kani proofs failed for package `{package_name}`"))?;
            let report: KaniReport = serde_json::from_str(&sh.read_file(&report_path)?)
                .with_context(|| {
                    format!("failed to parse Kani report {}", report_path.display())
                })?;
            ensure!(
                !report.verification_results.results.is_empty(),
                "no Kani proof harnesses ran for package `{package_name}`; see {}",
                report_path.display()
            );
            for harness in report.verification_results.results {
                for check in harness.checks {
                    ensure!(
                        check.category != "cover" || check.status == "Satisfied",
                        "unreached Kani cover in `{}`: {} ({}); see {}",
                        harness.harness_id,
                        check.description,
                        check.status,
                        report_path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

fn read_config(path: &Path) -> Result<Option<Config>> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .map(Some)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn checked_output_directory(root: &Path, directory: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in directory.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(component.as_os_str()),
        }
    }
    let mut ancestor = normalized.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("invalid Hax output directory")?;
    }
    ensure!(
        ancestor.canonicalize()?.starts_with(root.canonicalize()?),
        "Hax output directory escapes the workspace: {}",
        directory.display()
    );
    Ok(normalized)
}
