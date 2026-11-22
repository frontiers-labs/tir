//! File and embedded module sources share loading, parsing and source identities.
use crate::error::Error;
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) const CARGO_PACKAGE_ROOT_PREFIX: &str = "$cargo$";

#[derive(Deserialize)]
pub(super) struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    workspace_root: PathBuf,
    resolve: Option<CargoResolve>,
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

#[derive(Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoNode>,
}

#[derive(Deserialize)]
struct CargoNode {
    id: String,
    deps: Vec<CargoDependency>,
}

#[derive(Deserialize)]
struct CargoDependency {
    name: String,
    pkg: String,
    dep_kinds: Vec<CargoDependencyKind>,
}

#[derive(Deserialize)]
struct CargoDependencyKind {
    kind: Option<String>,
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

    pub fn dependency_defs(&self, owner: &Path, name: &str) -> Option<(String, PathBuf)> {
        let owner = self.packages.iter().find(|p| p.manifest_path == owner)?;
        let node = self
            .resolve
            .as_ref()?
            .nodes
            .iter()
            .find(|n| n.id == owner.id)?;
        let edge = node.deps.iter().find(|edge| {
            edge.name == name && edge.dep_kinds.iter().any(|kind| kind.kind.is_none())
        })?;
        let package = self.packages.iter().find(|p| p.id == edge.pkg)?;
        let defs = package.directory().join("defs/lib.cdsl");
        defs.is_file()
            .then(|| (format!("{CARGO_PACKAGE_ROOT_PREFIX}{}", package.id), defs))
    }

    pub fn workspace_inputs(&self) -> impl Iterator<Item = PathBuf> + '_ {
        ["Cargo.toml", "Cargo.lock"]
            .into_iter()
            .map(|name| self.workspace_root.join(name))
            .filter(|file| file.is_file())
    }
}

const SOURCES: &[(&str, &str)] = &[
    ("std.cdsl", include_str!("../stdlib/std.cdsl")),
    ("std/table.cdsl", include_str!("../stdlib/std/table.cdsl")),
    (
        "std/schedule.cdsl",
        include_str!("../stdlib/std/schedule.cdsl"),
    ),
    (
        "std/rewrite.cdsl",
        include_str!("../stdlib/std/rewrite.cdsl"),
    ),
    ("std/option.cdsl", include_str!("../stdlib/std/option.cdsl")),
    ("std/bits.cdsl", include_str!("../stdlib/std/bits.cdsl")),
    ("std/int.cdsl", include_str!("../stdlib/std/int.cdsl")),
    ("std/bigint.cdsl", include_str!("../stdlib/std/bigint.cdsl")),
    ("std/ir.cdsl", include_str!("../stdlib/std/ir.cdsl")),
    (
        "std/ir/interfaces.cdsl",
        include_str!("../stdlib/std/ir/interfaces.cdsl"),
    ),
    ("std/nat.cdsl", include_str!("../stdlib/std/nat.cdsl")),
    ("std/list.cdsl", include_str!("../stdlib/std/list.cdsl")),
    (
        "std/equality.cdsl",
        include_str!("../stdlib/std/equality.cdsl"),
    ),
];

pub(super) enum Source {
    File { root: PathBuf, file: PathBuf },
    Embedded(PathBuf),
}

impl Source {
    pub fn embedded_std() -> Self {
        Self::Embedded("std.cdsl".into())
    }

    pub fn read(&self) -> Result<(String, PathBuf, PathBuf), Error> {
        match self {
            Self::File { root, file } => {
                let key = file
                    .canonicalize()
                    .map_err(|e| Error(format!("{}: {e}", file.display())))?;
                let text = fs::read_to_string(file)
                    .map_err(|e| Error(format!("{}: {e}", file.display())))?;
                Ok((text, file.strip_prefix(root).unwrap_or(file).into(), key))
            }
            Self::Embedded(file) => {
                let text = SOURCES
                    .iter()
                    .find(|(name, _)| Path::new(name) == file)
                    .ok_or_else(|| Error(format!("missing embedded module {}", file.display())))?
                    .1;
                Ok((text.into(), file.clone(), file.clone()))
            }
        }
    }

    pub fn child(&self, name: &str) -> Result<Self, Error> {
        let file = match self {
            Self::File { file, .. } | Self::Embedded(file) => file,
        };
        let parent = file.parent().unwrap_or(Path::new(""));
        let directory = if file
            .file_name()
            .is_some_and(|n| n == "lib.cdsl" || n == "mod.cdsl")
        {
            parent.to_path_buf()
        } else {
            parent.join(file.file_stem().unwrap_or_default())
        };
        let sibling = directory.join(format!("{name}.cdsl"));
        match self {
            Self::Embedded(_) => Ok(Self::Embedded(sibling)),
            Self::File { root, .. } => {
                let child = directory.join(name).join("mod.cdsl");
                let file = match (child.is_file(), sibling.is_file()) {
                    (true, false) => child,
                    (false, true) => sibling,
                    (true, true) => {
                        return Err(Error(format!(
                            "ambiguous module {name}: {} and {}",
                            child.display(),
                            sibling.display()
                        )));
                    }
                    (false, false) => {
                        return Err(Error(format!(
                            "missing module {name} declared in {}",
                            file.display()
                        )));
                    }
                };
                Ok(Self::File {
                    root: root.clone(),
                    file,
                })
            }
        }
    }
}
