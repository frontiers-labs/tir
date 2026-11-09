use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;

use quote::{format_ident, quote};
use tir_cdsl::output::{AttrSpec, Operation, StatePorts, ValueSpec, emit_operation, op_fn_ident};

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::sem_expr_state;
use crate::utils::{
    EncodingShape, OperandConstraint, behavior_memory_effects, behavior_uses_todo,
    get_encoding_shapes, inline_let_bindings, parse_literal_value,
    resolve_effective_asm_for_instruction, resolve_isa_param_values,
    resolve_operand_constraints_for_instruction, resolve_operand_widths,
    resolve_operands_for_instruction, resolve_params_for_instruction,
};

pub struct GeneratedRust {
    pub root: String,
    pub modules: Vec<(String, String)>,
    /// The serialized sem programs the generated code embeds; written to
    /// [`SEM_BLOB_FILE`] beside the Rust.
    pub sem_blob: Vec<u8>,
}

pub fn generate_rust_modules<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    text_only: bool,
    custom_assembly: bool,
    split_inputs: &[String],
) -> Result<GeneratedRust, TMDLError> {
    begin_sem_blob();
    let features = emit_features(files)?;
    let register_traits = emit_register_trait_helpers(files)?;
    let registers = emit_register_parsers_and_printers(files, custom_assembly)?;
    let register_info = emit_register_info(files, dialect)?;
    let abi_info = emit_abi_info(files, item_cache)?;
    let (machine_models, sched_tables) = emit_machine_models(files, item_cache)?;
    let split_files: Vec<&ast::File> = split_inputs
        .iter()
        .map(|input| {
            files
                .iter()
                .find(|file| file.file_name == *input)
                .ok_or_else(|| TMDLError::Codegen(format!("split input '{input}' is not an input")))
        })
        .collect::<Result<_, _>>()?;
    let has_split = !split_files.is_empty();
    let root_files: Vec<&ast::File> = files
        .iter()
        .filter(|file| {
            !split_files
                .iter()
                .any(|split| split.file_name == file.file_name)
        })
        .collect();

    let instructions = emit_instructions(
        files,
        &root_files,
        item_cache,
        &sched_tables,
        InstructionOptions {
            dialect,
            text_only,
            custom_assembly,
            module_fragment: has_split,
        },
    )?;

    let mut modules = Vec::new();
    let mut module_idents = Vec::new();
    let mut module_sections = Vec::new();
    if has_split {
        let root_ident = format_ident!("__root_instructions");
        let root_reexport = root_files
            .iter()
            .any(|file| file.instructions().next().is_some())
            .then(|| quote! { pub use #root_ident::*; });
        module_sections.push(quote! {
            mod #root_ident {
                use super::*;
                #instructions
            }
            #root_reexport
        });
        module_idents.push(root_ident);
    }
    for file in split_files {
        let stem = std::path::Path::new(&file.file_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                TMDLError::Codegen(format!(
                    "split input '{}' has no UTF-8 file stem",
                    file.file_name
                ))
            })?;
        let module_ident = rust_module_ident(stem)?;
        let file_name = format!("{stem}.rs");
        if modules.iter().any(|(name, _)| name == &file_name) {
            return Err(TMDLError::Codegen(format!(
                "multiple split inputs produce '{file_name}'"
            )));
        }
        let module_instructions = emit_instructions(
            files,
            &[file],
            item_cache,
            &sched_tables,
            InstructionOptions {
                dialect,
                text_only,
                custom_assembly,
                module_fragment: true,
            },
        )?;
        let child = format_rust(quote! {
            use super::*;
            #module_instructions
        });
        module_sections.push(quote! {
            mod #module_ident {
                include!(#file_name);
            }
            pub use #module_ident::*;
        });
        module_idents.push(module_ident);
        modules.push((file_name, child));
    }

    let module_aggregation =
        (!modules.is_empty()).then(|| emit_module_aggregation(&module_idents, text_only));
    let instruction_section = if modules.is_empty() {
        instructions
    } else {
        quote! { #(#module_sections)* }
    };

    let (sem_blob, sem_kinds) = finish_sem_blob();
    let sem_kind_entries = sem_kinds.iter().map(emit_expr_kind_ts);
    let sem_program_store = quote! {
        #[allow(dead_code)]
        static SEM_BLOB: &[u8] = include_bytes!(#SEM_BLOB_FILE);
        #[allow(dead_code)]
        static SEM_KINDS: &[tir::sem::SymKind] = &[#(#sem_kind_entries),*];
        #[allow(dead_code)]
        static EXEC_ENV: tir::backend::exec::ExecEnv = tir::backend::exec::ExecEnv {
            kinds: SEM_KINDS,
            blob: SEM_BLOB,
            is_hardwired_zero: register_has_trait_hardwired_zero,
        };
    };

    let final_rust = quote! {
        #sem_program_store

        #features
        #register_traits

        #registers

        #register_info

        #abi_info

        #machine_models

        #instruction_section
        #module_aggregation
    };

    Ok(GeneratedRust {
        root: format_rust(final_rust),
        modules,
        sem_blob,
    })
}

pub(crate) fn generated_lint_allow() -> proc_macro2::TokenStream {
    quote! { #[allow(clippy::cognitive_complexity, clippy::too_many_lines)] }
}

fn format_rust(tokens: proc_macro2::TokenStream) -> String {
    prettyplease::unparse(&syn::parse2(tokens).unwrap())
}

fn rust_module_ident(stem: &str) -> Result<proc_macro2::Ident, TMDLError> {
    let valid = !stem.is_empty()
        && stem
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && stem.chars().all(|c| c == '_' || c.is_ascii_alphanumeric());
    if !valid {
        return Err(TMDLError::Codegen(format!(
            "split input stem '{stem}' is not a Rust module name"
        )));
    }
    syn::parse_str::<proc_macro2::Ident>(stem)
        .or_else(|_| syn::parse_str::<proc_macro2::Ident>(&format!("r#{stem}")))
        .map_err(|_| TMDLError::Codegen(format!("invalid Rust module name '{stem}'")))
}

fn emit_module_aggregation(
    modules: &[proc_macro2::Ident],
    text_only: bool,
) -> proc_macro2::TokenStream {
    let lint_allow = generated_lint_allow();
    let syntax = text_only.then(|| {
        quote! {
            #lint_allow
            pub fn asm_syntax() -> &'static [tir::backend::asm_syntax::InstrSyntax] {
                static SYNTAX: std::sync::LazyLock<
                    Vec<tir::backend::asm_syntax::InstrSyntax>,
                > = std::sync::LazyLock::new(|| {
                    let mut entries = Vec::new();
                    #(entries.extend_from_slice(#modules::asm_syntax());)*
                    entries
                });
                &SYNTAX
            }
        }
    });
    quote! {
        #lint_allow
        fn get_instruction_parsers(
            features: &[Feature],
        ) -> (
            std::collections::HashMap<String, Vec<tir::backend::AsmInstructionParser>>,
            std::collections::HashSet<String>,
        ) {
            let mut map = std::collections::HashMap::new();
            let mut disabled = std::collections::HashSet::new();
            #(
                let (module_map, module_disabled) = #modules::get_instruction_parsers(features);
                for (mnemonic, parsers) in module_map {
                    map.entry(mnemonic).or_insert_with(Vec::new).extend(parsers);
                }
                disabled.extend(module_disabled);
            )*
            disabled.retain(|mnemonic| !map.contains_key(mnemonic));
            (map, disabled)
        }

        #syntax

        /// Every opcode this target describes, as the one record per opcode the
        /// backend interface reads.
        pub fn instruction_infos() -> &'static [&'static tir::backend::InstrInfo] {
            static INFOS: std::sync::LazyLock<Vec<&'static tir::backend::InstrInfo>> =
                std::sync::LazyLock::new(|| {
                    let mut infos = Vec::new();
                    #(infos.extend_from_slice(#modules::instruction_infos());)*
                    infos
                });
            &INFOS
        }

        #lint_allow
        pub fn decode_instruction(context: &tir::Context, word: u32) -> Option<tir::OpId> {
            #(if let Some(op) = #modules::decode_instruction(context, word) {
                return Some(op);
            })*
            None
        }
    }
}

pub fn generate_operation_list(
    files: &[ast::File],
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let item_cache: HashMap<&str, _> = files
        .iter()
        .flat_map(|file| file.items.iter().map(|item| (item.name(), item)))
        .collect();
    writeln!(output, "[")?;
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let params = resolve_params_for_instruction(inst, &item_cache);
        let name = ["OPNAME", "MNEMONIC"]
            .into_iter()
            .find_map(|key| params.get(key)?.1.as_ref().and_then(resolve_string))
            .ok_or_else(|| {
                TMDLError::Codegen(format!(
                    "Instruction '{}' must define OPNAME or MNEMONIC",
                    inst.name
                ))
            })?;
        let constructor = op_fn_ident(&name).to_string();
        let definition = constructor.trim_start_matches("r#").to_uppercase();
        writeln!(output, "    {definition},")?;
    }
    writeln!(output, "]")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level emitters
// ---------------------------------------------------------------------------

mod sem_blob;
pub use sem_blob::SEM_BLOB_FILE;
use sem_blob::*;
mod features;
use features::*;
mod instructions;
use instructions::*;
mod registers;
use registers::*;
mod abis;
use abis::*;
mod scheduling;
use scheduling::*;
mod latency;
use latency::*;
mod register_traits;
use register_traits::*;
mod fixed_register_emission;
use fixed_register_emission::*;
pub(crate) use fixed_register_emission::{
    collect_register_path_reads, collect_register_path_writes,
};
mod implicit_registers;
use implicit_registers::*;
mod instruction_analysis;
use instruction_analysis::*;
pub(crate) use instruction_analysis::{
    infer_defined_register_operands, infer_read_register_operands, resolve_asm_templates,
    resolve_string,
};
mod assembly;
use assembly::*;
mod behavior;
use behavior::*;
mod encoding;
use encoding::*;
