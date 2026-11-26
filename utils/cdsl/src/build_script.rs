//! Cargo `build.rs` API for compiling CDSL v3 packages.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    env, fs, io,
    path::{Path, PathBuf},
};

use crate::{CheckedProgram, Error, check, compiler_sources, elaborate};

/// A generated file requested by a Cargo build script.
pub enum OutputKind<'a> {
    /// Rust definitions for the selected module's exports.
    OpDef(&'a str),
    /// Rust API and embedded TEGG bytecode for the selected program.
    Program(&'a str),
    /// Rust interface for a catalog owned by a workspace package.
    Catalog(&'a str),
    /// Unified CDSL v3 Rust output (ops + analyses + passes + bytecode).
    Rust(&'a str),
    /// Lean 4 proof skeleton output from unerased kernel AST.
    Lean(&'a str),
}

struct OutputFile<'a> {
    destination: &'a str,
    bytes: Vec<u8>,
}

/// Unified v3 build.rs entrypoint: compile a package root file (relative to
/// `CARGO_MANIFEST_DIR`) and write the generated Rust file into `OUT_DIR` (or
/// an absolute path).
pub fn compile_package(root_rel: impl AsRef<Path>, out_rel: impl AsRef<Path>) -> Result<(), Error> {
    let manifest = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .ok_or_else(|| Error("missing CARGO_MANIFEST_DIR".into()))?,
    );
    let root = manifest.join(root_rel);
    if let Some(defs) = root.parent()
        && defs.is_dir()
    {
        println!("cargo:rerun-if-changed={}", defs.display());
    }
    let mut checked = check_package(&root)?;
    let rust = checked.emit_rust()?;
    let out_path = out_rel.as_ref();
    let dest = if out_path.is_absolute() {
        out_path.to_path_buf()
    } else {
        PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| Error("missing OUT_DIR".into()))?)
            .join(out_path)
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| path_error(parent, e))?;
    }
    fs::write(&dest, rust).map_err(|e| path_error(&dest, e))
}

/// Preserved `build.rs` batch compilation API supporting module globs (`pkg::mod::*`).
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

    let mut local_checked: Option<CheckedProgram> = None;
    let mut files = Vec::new();

    for (filter, kind) in outputs {
        let invalid_filter = || {
            Error(format!(
                "invalid filter {filter:?}; expected a module glob in {package} or {}",
                manifest.display()
            ))
        };
        let mut segments = filter
            .strip_suffix("::*")
            .ok_or_else(invalid_filter)?
            .split("::");
        let prefix = segments.next().ok_or_else(invalid_filter)?;
        let module: Vec<String> = segments.map(str::to_owned).collect();
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

        let module_prefix = module.join("::");
        let prefix_opt = if module_prefix.is_empty() {
            Some("")
        } else {
            Some(module_prefix.as_str())
        };
        let (destination, bytes) = match kind {
            OutputKind::Catalog(dest) => {
                let target_root = if local {
                    root.clone()
                } else {
                    compiler_sources::CargoMetadata::read(None)?.workspace_defs(prefix, true)?
                };
                if let Some(d) = target_root.parent() {
                    println!("cargo:rerun-if-changed={}", d.display());
                }
                let mut checked = check_package(&target_root)?;
                let rust = checked.emit_for_mode(prefix_opt, crate::erase::EraseMode::Catalog)?;
                (dest, rust.into_bytes())
            }
            OutputKind::Lean(dest) => {
                if local_checked.is_none() {
                    local_checked = Some(check_package(&root)?);
                }
                let lean = local_checked.as_ref().unwrap().emit_lean();
                (dest, lean.into_bytes())
            }
            OutputKind::OpDef(dest) => {
                if local_checked.is_none() {
                    local_checked = Some(check_package(&root)?);
                }
                let rust = local_checked
                    .as_mut()
                    .unwrap()
                    .emit_for_mode(prefix_opt, crate::erase::EraseMode::OpDef)?;
                (dest, rust.into_bytes())
            }
            OutputKind::Program(dest) => {
                if local_checked.is_none() {
                    local_checked = Some(check_package(&root)?);
                }
                let rust = local_checked
                    .as_mut()
                    .unwrap()
                    .emit_for_mode(prefix_opt, crate::erase::EraseMode::Program)?;
                (dest, rust.into_bytes())
            }
            OutputKind::Rust(dest) => {
                if local_checked.is_none() {
                    local_checked = Some(check_package(&root)?);
                }
                let rust = local_checked
                    .as_mut()
                    .unwrap()
                    .emit_for_mode(prefix_opt, crate::erase::EraseMode::Standalone)?;
                (dest, rust.into_bytes())
            }
        };
        files.push(OutputFile { destination, bytes });
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

fn check_package(root: &Path) -> Result<CheckedProgram, Error> {
    let (interner, decls, inputs) = elaborate::elaborate_file(root)?;
    for input in inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let signature = check::check_program(interner, decls)?;
    Ok(CheckedProgram { signature })
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
