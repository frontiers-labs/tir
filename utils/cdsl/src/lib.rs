//! CDSL v3 compiler: minimal formally verifiable kernel with Rocq-style
//! extraction to Rust, TEGG v15 bytecode, and Lean 4.
//!
//! `kernel_core` and `tegg_core` are extracted to Lean 4 via Hax and verified
//! against the standalone `CdslV3Spec` specifications.

extern crate proc_macro;

pub mod ast;
mod build_script;
pub mod check;
mod compiler_sources;
pub mod core;
pub mod elaborate;
pub mod emit_lean;
pub mod emit_opdef;
pub mod emit_program;
pub mod emit_rust;
pub mod erase;
mod error;
pub mod format;
mod layout;
pub mod lexer;
pub mod nbe;
pub mod parser;
mod parser_syntax;
mod stdlib;
pub mod syntax;
pub mod tegg;

pub use tir_cdsl_core::{kernel_core, tegg_core};

use std::path::Path;

pub use build_script::{OutputKind, compile, compile_package};
pub use check::Signature;
pub use erase::MlProgram;
pub use error::Error;

/// A compiled and typechecked CDSL v3 program ready for extraction to Rust, TEGG, `.egg`, or Lean.
#[derive(Clone, Debug)]
pub struct CheckedProgram {
    pub signature: Signature,
}

impl CheckedProgram {
    /// Elaborate and typecheck a CDSL root file (recursively loading `mod foo;` files).
    pub fn from_file(path: &Path) -> Result<Self, Error> {
        let (interner, decls, _) = elaborate::elaborate_file(path)?;
        let signature = check::check_program(interner, decls)?;
        Ok(Self { signature })
    }

    /// Elaborate and typecheck standalone CDSL source text.
    pub fn from_source(source: &str) -> Result<Self, Error> {
        let (interner, decls) = elaborate::elaborate_source(source, "stdin.cdsl")?;
        let signature = check::check_program(interner, decls)?;
        Ok(Self { signature })
    }

    /// Elaborate and typecheck a single-file zero-copy parsed AST directly.
    pub fn from_ast(ast: &ast::Ast<'_>) -> Result<Self, Error> {
        let (interner, decls) = elaborate::elaborate_ast(ast)?;
        let signature = check::check_program(interner, decls)?;
        Ok(Self { signature })
    }

    /// Erase to MiniML (dropping `Prop` and staging `egg_program` / `@memo`).
    pub fn erase(&mut self) -> Result<MlProgram, Error> {
        erase::erase_program(&mut self.signature)
    }

    /// Erase to MiniML with module-prefix and target mode filtering.
    pub fn erase_for_mode(
        &mut self,
        module_prefix: Option<&str>,
        mode: erase::EraseMode,
    ) -> Result<MlProgram, Error> {
        erase::erase_program_mode(&mut self.signature, module_prefix, mode)
    }

    /// Emit Rust source code via Rocq-style MiniML extraction.
    pub fn emit_rust(&mut self) -> Result<String, Error> {
        let ml = self.erase()?;
        Ok(emit_rust::emit_program(&self.signature, &ml))
    }

    /// Emit Rust source code for a specific `EraseMode` and module prefix.
    pub fn emit_for_mode(
        &mut self,
        module_prefix: Option<&str>,
        mode: erase::EraseMode,
    ) -> Result<String, Error> {
        let ml = self.erase_for_mode(module_prefix, mode)?;
        Ok(match mode {
            erase::EraseMode::Standalone => {
                emit_rust::emit_program_in_module(&self.signature, &ml, module_prefix)
            }
            erase::EraseMode::OpDef => {
                emit_opdef::emit_opdef_module(&self.signature, &ml, module_prefix)
            }
            erase::EraseMode::Program => {
                emit_program::emit_program_module(&self.signature, &ml, module_prefix, false)
            }
            erase::EraseMode::Catalog => {
                emit_program::emit_program_module(&self.signature, &ml, module_prefix, true)
            }
        })
    }

    /// Emit human-readable `.egg` dump of all staged TEGG programs.
    pub fn emit_egg(&mut self) -> Result<String, Error> {
        let ml = self.erase()?;
        let mut out = String::new();
        for (id, _, egg) in &ml.bytecodes {
            out.push_str(&format!(";; === {id} ===\n{egg}\n"));
        }
        Ok(out)
    }

    /// Emit raw TEGG v15 binary bytes of the first staged program.
    pub fn emit_tegg(&mut self) -> Result<Vec<u8>, Error> {
        let ml = self.erase()?;
        ml.bytecodes
            .into_iter()
            .next()
            .map(|(_, bytes, _)| bytes)
            .ok_or_else(|| Error("no TEGG bytecode blob emitted".into()))
    }

    /// Emit Lean 4 proof skeleton from the UNERASED kernel AST.
    pub fn emit_lean(&self) -> String {
        emit_lean::emit_lean(&self.signature)
    }

    /// Render a concise summary of checked kernel declarations.
    pub fn render_typed(&self) -> String {
        let mut out = String::new();
        for d in &self.signature.decl_order {
            match d {
                core::Decl::Ind(i) => {
                    out.push_str(&format!(
                        "inductive {}\n",
                        self.signature.interner.resolve(i.iname)
                    ));
                }
                core::Decl::Type(n, _, _) => {
                    out.push_str(&format!("type {}\n", self.signature.interner.resolve(*n)));
                }
                core::Decl::Def(n, Some(ty), _) => {
                    out.push_str(&format!(
                        "def {} : {}\n",
                        self.signature.interner.resolve(*n),
                        core::pp_term(&self.signature.interner, ty)
                    ));
                }
                core::Decl::Def(n, None, _) => {
                    out.push_str(&format!("def {}\n", self.signature.interner.resolve(*n)));
                }
                core::Decl::Axiom(n, ty) => {
                    out.push_str(&format!(
                        "axiom {} : {}\n",
                        self.signature.interner.resolve(*n),
                        core::pp_term(&self.signature.interner, ty)
                    ));
                }
                core::Decl::Extract(_, n, _) => {
                    out.push_str(&format!(
                        "extract {}\n",
                        self.signature.interner.resolve(*n)
                    ));
                }
            }
        }
        out
    }

    /// Compile standalone CDSL op definitions directly to Rust source (used by `tmdl` and `build.rs` hooks).
    pub fn emit_rust_source(source: &str, _package: &str) -> Result<String, Error> {
        let mut checked = Self::from_source(source)?;
        checked.emit_for_mode(Some(""), erase::EraseMode::OpDef)
    }
}

/// Compile a CDSL root file through the lexer, parser, elaborator, and kernel checker.
pub fn compile_file(file: &Path) -> Result<CheckedProgram, Error> {
    CheckedProgram::from_file(file)
}

/// Compile standalone CDSL source text through the shared parser, elaborator, and kernel checker.
pub fn compile_source(source: &str) -> Result<CheckedProgram, Error> {
    CheckedProgram::from_source(source)
}

/// Compile inline Rust proc-macro tokens through the same lexer, parser, elaborator, and checker.
pub fn compile_tokens(input: proc_macro::TokenStream) -> Result<CheckedProgram, Error> {
    let source = lexer::source_from_proc_macro(input);
    CheckedProgram::from_source(&source)
}

/// Resolve and compile a workspace Cargo package's `defs/lib.cdsl` by package name.
pub fn resolve_crate(name: &str) -> Result<CheckedProgram, Error> {
    let path = compiler_sources::CargoMetadata::read(None)?.workspace_defs(name, false)?;
    CheckedProgram::from_file(&path)
}

/// Resolve and compile a workspace Cargo package's `defs/lib.cdsl` by package name or directory name.
pub fn resolve_program_package(prefix: &str) -> Result<CheckedProgram, Error> {
    let path = compiler_sources::CargoMetadata::read(None)?.workspace_defs(prefix, true)?;
    CheckedProgram::from_file(&path)
}
