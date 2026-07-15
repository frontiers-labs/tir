use std::collections::{HashMap, HashSet};
use std::io::Write;

use quote::{format_ident, quote};

use crate::Type;
use crate::ast;
use crate::error::TMDLError;
use crate::sem_expr_state;
use crate::utils::{
    get_encoding_arms, parse_literal_value, resolve_effective_asm_for_instruction,
    resolve_isa_param_values, resolve_operand_widths, resolve_operands_for_instruction,
    resolve_params_for_instruction,
};

pub struct GeneratedRust {
    pub root: String,
    pub modules: Vec<(String, String)>,
}

pub fn generate_rust<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    text_only: bool,
    custom_assembly: bool,
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    let generated =
        generate_rust_modules(dialect, files, item_cache, text_only, custom_assembly, &[])?;
    output.write_all(generated.root.as_bytes())?;
    Ok(())
}

pub fn generate_rust_modules<'a>(
    dialect: &str,
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    text_only: bool,
    custom_assembly: bool,
    split_inputs: &[String],
) -> Result<GeneratedRust, TMDLError> {
    let features = emit_features(files)?;
    let register_traits = emit_register_trait_helpers(files)?;
    let registers = emit_register_parsers_and_printers(files)?;
    let register_info = emit_register_info(files)?;
    let machine_models = emit_machine_models(files, item_cache)?;
    let instruction_cost = emit_instruction_cost(files, item_cache)?;
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
        InstructionOptions {
            dialect,
            text_only,
            custom_assembly,
            include_global_rules: true,
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
            InstructionOptions {
                dialect,
                text_only,
                custom_assembly,
                include_global_rules: false,
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

    let final_rust = quote! {
        #features
        #register_traits

        #registers

        #register_info

        #machine_models

        #instruction_cost

        #instruction_section
        #module_aggregation
    };

    Ok(GeneratedRust {
        root: format_rust(final_rust),
        modules,
    })
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
    let syntax = text_only.then(|| {
        quote! {
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
    let binary = (!text_only).then(|| quote! {
        fn get_instruction_encoders() -> std::collections::HashMap<String, tir::backend::binary::InstructionEncoder> {
            let mut map = std::collections::HashMap::new();
            #(map.extend(#modules::get_instruction_encoders());)*
            map
        }

        fn get_instruction_patchers() -> std::collections::HashMap<String, tir::backend::binary::InstructionPatcher> {
            let mut map = std::collections::HashMap::new();
            #(map.extend(#modules::get_instruction_patchers());)*
            map
        }
    });

    quote! {
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

        fn get_instruction_printers() -> std::collections::HashMap<String, tir::backend::AsmInstructionPrinter> {
            let mut map = std::collections::HashMap::new();
            #(map.extend(#modules::get_instruction_printers());)*
            map
        }

        #syntax
        #binary

        pub fn decode_instruction(context: &tir::Context, word: u32) -> Option<tir::OpId> {
            #(if let Some(op) = #modules::decode_instruction(context, word) {
                return Some(op);
            })*
            None
        }

        pub fn get_isel_rules(
            context: &tir::Context,
            features: &[Feature],
        ) -> Vec<tir::backend::isel::Rule> {
            let mut rules = Vec::new();
            #(rules.extend(#modules::get_isel_rules(context, features));)*
            rules
        }
    }
}

pub fn generate_operation_list(
    files: &[ast::File],
    mut output: Box<dyn Write>,
) -> Result<(), TMDLError> {
    writeln!(output, "[")?;
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let name = format_ident!("{}Op", &inst.name);
        writeln!(output, "    {name},")?;
    }
    writeln!(output, "]")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Top-level emitters
// ---------------------------------------------------------------------------

include!("features.rs");
include!("instructions.rs");
include!("registers.rs");
include!("scheduling.rs");
include!("register_traits.rs");
include!("flag_analysis.rs");
include!("flag_emission.rs");
include!("instruction_analysis.rs");
include!("assembly.rs");
include!("behavior.rs");
include!("encoding.rs");
