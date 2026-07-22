use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Read, Write};

use clap::Args;
use tir::{Context, IRFormatter, Operation, builtin::ModuleOp};

use crate::common::read_input;

#[derive(Args)]
pub struct ExportArgs {
    /// Output SPIR-V binary file, or `-` for stdout.
    #[arg(short = 'o', default_value = "-")]
    output: OsString,
    /// Input TIR file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

#[derive(Args)]
pub struct ImportArgs {
    /// Output TIR file, or `-` for stdout.
    #[arg(short = 'o', default_value = "-")]
    output: OsString,
    /// Input SPIR-V binary file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

pub fn export(args: ExportArgs) -> Result<(), Box<dyn Error>> {
    let input = read_input(args.input.as_ref())?;
    let context = Context::with_default_dialects();
    context.register_dialect::<tir_gpu::spirv::SpirvDialect>();
    let module = tir::parse::ir::parse_ir::<ModuleOp>(&context, &input)
        .map_err(|(span, err)| format!("failed to parse input at byte {}: {err:?}", span.0))?;
    let binary = tir_gpu::spirv::write_binary(&context, &module)
        .map_err(|error| format!("SPIR-V export failed: {error}"))?;
    write_bytes(args.output.as_os_str(), &binary)?;
    Ok(())
}

pub fn import(args: ImportArgs) -> Result<(), Box<dyn Error>> {
    let binary = read_bytes(args.input.as_ref())?;
    let context = Context::with_default_dialects();
    context.register_dialect::<tir_gpu::spirv::SpirvDialect>();
    let module = tir_gpu::spirv::read_binary(&context, &binary)
        .map_err(|error| format!("SPIR-V import failed: {error}"))?;
    let mut rendered = String::new();
    module.print(&mut IRFormatter::new(&mut rendered))?;
    crate::common::write_output(args.output.as_os_str(), &rendered)?;
    Ok(())
}

fn read_bytes(path: Option<&OsString>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match path {
        Some(path) if path != "-" => std::fs::File::open(path)?.read_to_end(&mut bytes)?,
        _ => io::stdin().read_to_end(&mut bytes)?,
    };
    Ok(bytes)
}

fn write_bytes(path: &std::ffi::OsStr, bytes: &[u8]) -> io::Result<()> {
    if path == "-" {
        io::stdout().write_all(bytes)
    } else {
        std::fs::write(path, bytes)
    }
}
