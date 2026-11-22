#[cfg(feature = "interp")]
mod interp_build;

use tir_cdsl::OutputKind::{OpDef, Producers, Program};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "interp")]
    interp_build::compile()?;
    tir_cdsl::compile([
        ("core::ir::*", Program("ir_program.rs")),
        ("core::std::ir::interfaces::*", OpDef("interfaces.rs")),
        ("core::dialects::son::*", OpDef("son_ops.rs")),
        ("core::dialects::builtin::*", OpDef("builtin.rs")),
        ("core::dialects::fp::*", OpDef("fp.rs")),
        ("core::dialects::asm::*", OpDef("asm_ops.rs")),
        ("core::dialects::cfg::*", OpDef("cfg_ops.rs")),
        ("core::dialects::func::*", OpDef("func_ops.rs")),
        ("core::dialects::layout::*", OpDef("layout_ops.rs")),
        ("core::dialects::ptr::*", OpDef("ptr_ops.rs")),
        ("core::dialects::scf::*", OpDef("scf_ops.rs")),
        ("core::dialects::state::*", OpDef("state_ops.rs")),
        ("core::dialects::type_def::*", OpDef("type_def_ops.rs")),
        ("core::dialects::vector::*", OpDef("vector_ops.rs")),
        ("core::eqsat::*", Program("eqsat.rs")),
        ("core::analysis::cfg::*", Producers("cfg_analysis.rs")),
        ("core::passes::restructure::*", Program("restructure.rs")),
    ])?;
    Ok(())
}
