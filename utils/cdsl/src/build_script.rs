use std::{
    collections::{BTreeMap, btree_map::Entry},
    env, fs, io,
    path::{Path, PathBuf},
};

use crate::{Error, compiler};

/// A generated file requested by a Cargo build script.
pub enum OutputKind<'a> {
    /// Rust definitions for the selected module's exports.
    OpDef(&'a str),
    /// Rust API and private bytecode for the selected program.
    Program(&'a str),
    /// Rust API and dependency metadata for restartable analysis producers.
    Producers(&'a str),
    /// Rust interface for a catalog owned by a workspace package.
    Catalog(&'a str),
}

struct OutputFile<'a> {
    destination: &'a str,
    bytes: Vec<u8>,
}

/// Check CDSL sources for the current Cargo package and generate build outputs.
///
/// Select exports or rules with `package::module::*` or select the root with
/// `package::*`. The package prefix can be the Cargo package name or its
/// directory name. Relative destinations are resolved under `OUT_DIR`.
/// All selections and destinations are checked before any output file is written.
pub fn compile<'a>(
    outputs: impl IntoIterator<Item = (&'a str, OutputKind<'a>)>,
) -> Result<(), Error> {
    let outputs = outputs.into_iter().collect::<Vec<_>>();
    if outputs.is_empty() {
        return Ok(());
    }

    let manifest = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| Error("missing CARGO_MANIFEST_DIR".into()))?,
    );
    let package =
        env::var("CARGO_PKG_NAME").map_err(|error| Error(format!("CARGO_PKG_NAME: {error}")))?;
    let out_dir = env::var_os("OUT_DIR").map(PathBuf::from);
    let defs = manifest.join("defs");
    if defs.is_dir() {
        println!("cargo:rerun-if-changed={}", defs.display());
    }
    let root = defs.join("lib.cdsl");
    let checked = outputs
        .iter()
        .any(|(_, kind)| !matches!(kind, OutputKind::Catalog(_)))
        .then(|| check_package(&root))
        .transpose()?;

    let mut files = Vec::new();
    for (filter, kind) in outputs {
        let language = match kind {
            OutputKind::OpDef(_) => "Rust",
            OutputKind::Program(_) => "program",
            OutputKind::Producers(_) => "producers",
            OutputKind::Catalog(_) => "catalog",
        };
        let invalid_filter = || {
            Error(format!(
                "invalid {language} filter {filter:?}; expected a module glob in {package} or {}",
                manifest.display()
            ))
        };
        let mut segments = filter
            .strip_suffix("::*")
            .ok_or_else(invalid_filter)?
            .split("::");
        let prefix = segments.next().ok_or_else(invalid_filter)?;
        let module = segments.map(str::to_owned).collect::<Vec<_>>();
        let local = prefix == package || manifest.file_name().is_some_and(|name| name == prefix);
        if (!local && !matches!(kind, OutputKind::Catalog(_)))
            || module.iter().any(|name| {
                name.is_empty()
                    || name.contains(['*', '/', '\\'])
                    || matches!(name.as_str(), "." | "..")
            })
        {
            return Err(invalid_filter());
        }

        match kind {
            OutputKind::Catalog(destination) => {
                let root = if local {
                    root.clone()
                } else {
                    compiler::resolve_package_defs(prefix, true)?
                };
                if let Some(defs) = root.parent() {
                    println!("cargo:rerun-if-changed={}", defs.display());
                }
                let checked = check_package(&root)?;
                let rust = checked.render_catalog(&module)?;
                files.push(OutputFile {
                    destination,
                    bytes: rust.into_bytes(),
                });
            }
            OutputKind::OpDef(destination)
            | OutputKind::Program(destination)
            | OutputKind::Producers(destination) => {
                let checked = checked
                    .as_ref()
                    .ok_or_else(|| Error("missing checked package".into()))?;
                let rust = match kind {
                    OutputKind::Program(_) => checked.render_program(&module),
                    OutputKind::Producers(_) => checked.render_producers(&module),
                    _ => checked.artifact().render_rust_module(&module),
                }
                .map_err(|error| Error(format!("{language} filter {filter:?}: {error}")))?;
                files.push(OutputFile {
                    destination,
                    bytes: rust.into_bytes(),
                });
            }
        }
    }

    let mut pending = Vec::new();
    for OutputFile { destination, bytes } in files {
        let path = Path::new(destination);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            out_dir
                .as_deref()
                .ok_or_else(|| Error("missing OUT_DIR".into()))?
                .join(path)
        };
        let parent = path
            .parent()
            .ok_or_else(|| Error(format!("invalid Rust output {}", path.display())))?;
        fs::create_dir_all(parent).map_err(|error| path_error(parent, error))?;
        pending.push((path, bytes));
    }
    let mut destinations = BTreeMap::new();
    for (path, bytes) in pending {
        let destination = canonical_destination(&path)?;
        match destinations.entry(destination) {
            Entry::Vacant(entry) => {
                entry.insert(bytes);
            }
            Entry::Occupied(entry) => {
                return Err(Error(format!(
                    "duplicate Rust output {} resolves to {}",
                    path.display(),
                    entry.key().display()
                )));
            }
        }
    }
    for (path, bytes) in destinations {
        fs::write(&path, bytes).map_err(|error| path_error(&path, error))?;
    }
    Ok(())
}

fn check_package(root: &Path) -> Result<crate::CheckedProgram, Error> {
    let (checked, inputs) = compiler::check_package(root)?;
    for input in inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    Ok(checked)
}

fn canonical_destination(path: &Path) -> Result<PathBuf, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let file = fs::canonicalize(path).map_err(|error| path_error(path, error))?;
            if !file.is_file() {
                return Err(Error(format!(
                    "Rust output {} is not a file",
                    path.display()
                )));
            }
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(Error(format!("invalid Rust output {}", path.display())));
            };
            let parent = fs::canonicalize(parent).map_err(|error| path_error(parent, error))?;
            Ok(parent.join(name))
        }
        Err(error) => Err(path_error(path, error)),
    }
}

fn path_error(path: &Path, error: io::Error) -> Error {
    Error(format!("{}: {error}", path.display()))
}
