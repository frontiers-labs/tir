//! Cargo workspace metadata resolution for CDSL package selectors.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use serde::Deserialize;

use crate::error::Error;

#[derive(Deserialize)]
pub(crate) struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    workspace_root: PathBuf,
}

#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
}

impl CargoPackage {
    fn directory(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new("."))
    }
}

impl CargoMetadata {
    pub fn read(manifest: Option<&Path>) -> Result<Self, Error> {
        let mut cargo = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cargo.args(["metadata", "--format-version", "1"]);
        if let Some(manifest) = manifest {
            cargo.arg("--manifest-path").arg(manifest);
        } else {
            cargo.arg("--no-deps");
        }
        let output = cargo
            .output()
            .map_err(|error| Error(format!("running cargo metadata: {error}")))?;
        if !output.status.success() {
            return Err(Error(format!(
                "cargo metadata: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| Error(format!("invalid cargo metadata: {error}")))
    }

    pub fn workspace_defs(&self, name: &str, allow_directory: bool) -> Result<PathBuf, Error> {
        let members = || {
            self.packages
                .iter()
                .filter(|package| self.workspace_members.contains(&package.id))
        };
        let package = members().find(|package| package.name == name).or_else(|| {
            allow_directory
                .then(|| members().find(|p| p.directory().file_name().is_some_and(|s| s == name)))
                .flatten()
        });
        package
            .map(|package| package.directory().join("defs/lib.cdsl"))
            .ok_or_else(|| {
                Error(format!(
                    "Cargo package {name} does not exist in {}",
                    self.workspace_root.display()
                ))
            })
    }
}
