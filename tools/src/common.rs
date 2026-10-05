use std::{
    error::Error,
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
};

use clap::{Args, ValueEnum};
use tir::backend::TargetMachine;
use tir::{Context, Operation, builtin::ModuleOp};

/// The target flags a code-generating tool takes besides `--march`, which is
/// the tool's own: only a tool that can read the target from its input makes it
/// optional. The display orders leave room for it in second place.
#[derive(Args)]
pub struct TargetArgs {
    /// Target CPU
    #[arg(long, display_order = 0)]
    mcpu: Option<String>,
    /// Target feature toggles (e.g. `+m,-zmmul`), applied on top of `--march`.
    #[arg(long, display_order = 2)]
    mattr: Option<String>,
    /// Target calling convention.
    #[arg(long, display_order = 3)]
    mabi: Option<String>,
}

impl TargetArgs {
    /// The target `march` names, as the rest of the flags configure it.
    pub fn select(&self, march: &str) -> Result<Box<dyn TargetMachine>, String> {
        tir::backend::select_target_with_abi(
            march,
            self.mcpu.as_deref(),
            self.mattr.as_deref(),
            self.mabi.as_deref(),
        )
    }
}

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum InputKind {
    #[default]
    Auto,
    Tir,
    Llvm,
    Assembly,
}

/// Parse the tool input into a module. Returns the module and whether it still
/// needs lowering (assembly is already in machine-IR form; TIR is not).
pub fn parse_module(
    target: &dyn TargetMachine,
    context: &Context,
    input_path: Option<&OsString>,
    kind: InputKind,
) -> Result<(ModuleOp, bool), Box<dyn Error>> {
    let input = read_input(input_path)?;

    match resolve_kind(input_path, kind) {
        InputKind::Assembly => {
            // A target with its own assembly syntax (e.g. PTX) parses text
            // directly; otherwise fall back to the shared flat assembler.
            if let Some(result) = target.parse_asm_text(context, &input) {
                return Ok((
                    result.map_err(|e| format!("failed to parse assembly: {e}"))?,
                    false,
                ));
            }
            Ok((
                target
                    .asm_parser(context)
                    .parse_asm(context, &input)
                    .map_err(|_| "failed to parse assembly input")?,
                false,
            ))
        }
        InputKind::Tir => {
            let module = parse_tir(context, &input)?;
            attach_data_layout(context, &module, target);
            Ok((module, true))
        }
        InputKind::Llvm => {
            let module = tir_llvm::import_str(context, &input, target.data_layout().as_ref())
                .map_err(|e| format!("llvm import failed: {e}"))?;
            let mut attributes = context.get_op(module.id()).attributes().to_vec();
            if let Some(value) = target.target_env() {
                attributes.push(context.named_attribute(tir::TARGET_ENV, value));
            }
            context.set_op_attributes(module.id(), attributes);
            Ok((
                context.get_op(module.id()).as_op::<ModuleOp>().unwrap(),
                true,
            ))
        }
        InputKind::Auto => unreachable!(),
    }
}

/// Give `module` the data layout of `target`, the one its code is lowered
/// against, so the verifier and every pass read that layout from the IR. Entries
/// the module declares itself override the target's key by key, which is how
/// the backend already combines the two.
pub fn attach_data_layout(context: &Context, module: &ModuleOp, target: &dyn TargetMachine) {
    let Some(layout) =
        tir::DataLayout::for_op_with_default(context, module.id(), target.data_layout().as_ref())
    else {
        return;
    };
    let name = context.intern(tir::DATA_LAYOUT);
    let mut attributes = context.get_op(module.id()).attributes().to_vec();
    match attributes
        .iter_mut()
        .find(|attribute| attribute.name == name)
    {
        Some(attribute) => attribute.value = layout.spec(),
        None => attributes.push(context.named_attribute(tir::DATA_LAYOUT, layout.spec())),
    }
    context.set_op_attributes(module.id(), attributes);
}

/// Give a TIR `module` the data layout of the target its `target_env` `arch`
/// names. A module that names no target keeps whatever layout it declares:
/// nothing else says how wide its pointers are.
pub fn attach_declared_data_layout(context: &Context, module: &ModuleOp) -> Result<(), String> {
    let Some(arch) =
        tir::TargetEnv::for_op(context, module.id()).and_then(|env| env.arch().map(str::to_string))
    else {
        return Ok(());
    };
    let target = tir::backend::select_target(&arch, None, None)?;
    attach_data_layout(context, module, target.as_ref());
    Ok(())
}

/// Whether the input holds TIR, LLVM IR or assembly, resolving [`InputKind::Auto`] by
/// file extension.
pub fn resolve_kind(input_path: Option<&OsString>, kind: InputKind) -> InputKind {
    if kind != InputKind::Auto {
        return kind;
    }
    if input_path
        .and_then(|path| path.to_str())
        .is_some_and(|path| path.ends_with(".ll"))
    {
        return InputKind::Llvm;
    }
    let is_assembly = input_path
        .and_then(|path| path.to_str())
        .is_some_and(|path| {
            [".S", ".s", ".asm", ".ptx"]
                .iter()
                .any(|extension| path.ends_with(extension))
        });
    if is_assembly {
        InputKind::Assembly
    } else {
        InputKind::Tir
    }
}

/// Parse TIR text into a module. Needs no target: the dialects the text uses
/// must already be registered.
pub fn parse_tir(context: &Context, input: &str) -> Result<ModuleOp, Box<dyn Error>> {
    tir::parse::ir::parse_ir::<ModuleOp>(context, input)
        .map_err(|(span, err)| format!("failed to parse input at byte {}: {err:?}", span.0).into())
}

pub fn read_input(path: Option<&OsString>) -> Result<String, io::Error> {
    let mut input = String::new();
    match path {
        Some(path) if path != "-" => File::open(path)?.read_to_string(&mut input)?,
        _ => io::stdin().read_to_string(&mut input)?,
    };
    Ok(input)
}

pub fn write_output(path: &std::ffi::OsStr, contents: &str) -> Result<(), io::Error> {
    if path == "-" {
        print!("{contents}");
        io::stdout().flush()
    } else {
        std::fs::write(path, contents)
    }
}
