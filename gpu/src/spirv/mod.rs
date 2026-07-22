//! Human-readable SSA representation of core compute SPIR-V.

mod binary;
mod generated;
mod ops;
mod types;

pub use binary::{read_binary, write_binary};
pub use generated::*;
pub use ops::*;
pub use types::*;

use tir::helpers::dialect;

dialect! {
    SpirvDialect {
        name: "spirv",
        operation_file: concat!(env!("CARGO_MANIFEST_DIR"), "/src/spirv/generated_ops.rs"),
        types: [PointerType, RuntimeArrayType],
    }
}
