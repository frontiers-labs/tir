use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;

use crate::compiler::{Cli, check_modules, compile_command};
use crate::error::TMDLError;
#[cfg(feature = "json")]
use crate::json::PackageDocument;
use crate::module_names::Names;
use crate::module_tree::{ModuleSource, ModuleTree};
use crate::{Action, Compiler, OutputKind};

const ISA_DEFS: &str = "isa_defs";

#[derive(Parser)]
#[command(about = "Compile TMDL files and build TMDL packages")]
struct PackageCli {
    #[command(subcommand)]
    command: PackageCommand,
}

#[derive(Subcommand)]
enum PackageCommand {
    /// Compile one or more TMDL files.
    Compile(Cli),
    /// Build a TMDL module tree.
    Build(BuildArgs),
}

#[derive(Args)]
#[command(group(ArgGroup::new("source").required(true)))]
struct BuildArgs {
    /// Cargo workspace member with an isa_defs/mod.tmdl file.
    #[arg(short, long, group = "source")]
    package: Option<String>,
    /// Directory containing the root mod.tmdl.
    #[arg(long, group = "source")]
    path: Option<PathBuf>,
    /// Artifacts to build.
    #[arg(long)]
    output: BuildOutput,
    /// Directory for generated artifacts.
    #[arg(long)]
    output_path: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum BuildOutput {
    Compiler,
    Docs,
    Json,
}

#[derive(Deserialize)]
struct Workspace {
    packages: Vec<Member>,
    workspace_members: Vec<String>,
}

#[derive(Deserialize)]
struct Member {
    id: String,
    name: String,
    manifest_path: PathBuf,
    metadata: serde_json::Value,
}

#[derive(Default, Deserialize)]
struct Metadata {
    tmdl: Option<Config>,
}

#[derive(Deserialize)]
struct Config {
    dialect: String,
}

#[derive(Deserialize)]
struct Manifest {
    package: ManifestPackage,
}

#[derive(Deserialize)]
struct ManifestPackage {
    metadata: Option<Metadata>,
}

fn workspace_member(name: &str) -> Result<Member, TMDLError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let result = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()?;
    if !result.status.success() {
        return Err(TMDLError::Codegen(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        )));
    }
    let workspace: Workspace = serde_json::from_slice(&result.stdout)?;
    workspace
        .packages
        .into_iter()
        .find(|member| member.name == name && workspace.workspace_members.contains(&member.id))
        .ok_or_else(|| TMDLError::Codegen(format!("no Cargo workspace member named `{name}`")))
}

fn compiler_dialect(metadata: Metadata) -> Result<String, TMDLError> {
    let Config { dialect } = metadata.tmdl.ok_or_else(|| {
        TMDLError::Codegen("compiler output requires [package.metadata.tmdl] dialect".into())
    })?;
    let mut bytes = dialect.bytes();
    if !bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        || !bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || dialect == "_"
    {
        return Err(TMDLError::Codegen(format!(
            "invalid TMDL dialect `{dialect}`"
        )));
    }
    if dialect.eq_ignore_ascii_case("model_check_sources") {
        return Err(TMDLError::Codegen(
            "dialect name `model_check_sources` collides with the generated source table".into(),
        ));
    }
    Ok(dialect)
}

/// Run the package-aware TMDL command-line interface.
pub fn package_main() -> Result<(), Box<dyn std::error::Error>> {
    match PackageCli::parse().command {
        PackageCommand::Compile(args) => compile_command(args, rust_writer),
        PackageCommand::Build(args) => Ok(build(args)?),
    }
}

fn build(args: BuildArgs) -> Result<(), TMDLError> {
    let (root, metadata, name) = match (args.package, args.path) {
        (Some(name), None) => {
            let member = workspace_member(&name)?;
            let parent = member.manifest_path.parent().ok_or_else(|| {
                TMDLError::Codegen(format!("missing Cargo package directory for `{name}`"))
            })?;
            let metadata = if member.metadata.is_null() {
                Metadata::default()
            } else {
                serde_json::from_value(member.metadata)?
            };
            (parent.join(ISA_DEFS), metadata, name)
        }
        (None, Some(path)) => {
            if matches!(args.output, BuildOutput::Compiler) {
                return Err(TMDLError::Codegen(
                    "compiler output requires --package and its TMDL dialect metadata".into(),
                ));
            }
            let absolute = path.canonicalize()?;
            let name = absolute
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("TMDL"))
                .to_string_lossy()
                .into_owned();
            (path, Metadata::default(), name)
        }
        _ => unreachable!("the CLI requires one source selector"),
    };
    let dialect = match args.output {
        BuildOutput::Compiler => compiler_dialect(metadata)?,
        BuildOutput::Docs | BuildOutput::Json => {
            metadata.tmdl.map_or(name, |config| config.dialect)
        }
    };

    let tree = ModuleTree::load(&ModuleSource::Directory(root))?;
    match args.output {
        BuildOutput::Compiler => emit_backend(&tree, &dialect, &args.output_path),
        BuildOutput::Docs => {
            let files = check_modules(&tree, Names::Documentation, false)?;
            crate::markdown::generate_markdown_book(
                &dialect,
                &files,
                &args.output_path,
                Some(&tree),
            )
        }
        BuildOutput::Json => emit_json(&tree, &args.output_path),
    }
}

/// Generate native backend artifacts in Cargo's OUT_DIR from isa_defs/mod.tmdl.
pub fn build_backend_package() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is required")?,
    );
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is required")?);
    let manifest_path = directory.join("Cargo.toml");
    let root = directory.join(ISA_DEFS);
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rerun-if-changed={}", root.display());
    let manifest: Manifest = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
    let dialect = compiler_dialect(manifest.package.metadata.unwrap_or_default())?;
    let tree = ModuleTree::load(&ModuleSource::Directory(root))?;
    for module in tree.modules() {
        println!("cargo:rerun-if-changed={}", module.source_key.display());
    }
    emit_backend(&tree, &dialect, &output)?;
    Ok(())
}

fn rust_writer(source: &str, package: &str) -> Result<String, Box<dyn std::error::Error>> {
    Ok(tir_cdsl::CheckedProgram::emit_rust_source(source, package)?)
}

fn emit_backend(tree: &ModuleTree, dialect: &str, output: &Path) -> Result<(), TMDLError> {
    let files = check_modules(tree, Names::Backend, false)?;
    fs::create_dir_all(output)?;
    for (action, suffix) in [
        (Action::EmitRust, ".rs"),
        (Action::EmitOperationList, "_ops.rs"),
    ] {
        let path = output.join(format!("{dialect}{suffix}"));
        Compiler::builder()
            .action(action)
            .dialect(Some(dialect.to_string()))
            .output(OutputKind::File(path.to_string_lossy().into_owned()))
            .build()
            .emit_program(Some(&rust_writer), &files, None)?;
    }
    let mut sources = String::from("const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[\n");
    for module in tree.modules() {
        let name = module.source_path.to_string_lossy();
        let path = module.source_key.to_string_lossy();
        sources.push_str(&format!("    ({name:?}, include_str!({path:?})),\n"));
    }
    sources.push_str("];\n");
    fs::write(output.join("model_check_sources.rs"), sources)?;
    Ok(())
}

#[cfg(feature = "json")]
fn emit_json(tree: &ModuleTree, output: &Path) -> Result<(), TMDLError> {
    use std::io::Write;

    let files = check_modules(tree, Names::Canonical, false)?;
    let document = PackageDocument::from_ast(&files, tree);
    fs::create_dir_all(output)?;
    let mut json = fs::File::create(output.join("isa.json"))?;
    serde_json::to_writer_pretty(&mut json, &document)?;
    writeln!(json)?;
    let mut schema = fs::File::create(output.join("isa.schema.json"))?;
    serde_json::to_writer_pretty(&mut schema, &PackageDocument::schema())?;
    writeln!(schema)?;
    Ok(())
}

#[cfg(not(feature = "json"))]
fn emit_json(_tree: &ModuleTree, _output: &Path) -> Result<(), TMDLError> {
    Err(TMDLError::Codegen(
        "tmdl was built without the `json` feature".into(),
    ))
}
