//! Definition data and Rust output shared by CDSL and TMDL.
pub mod binds;
mod contract;
pub use contract::{
    CheckedOperation, check_operation, compile_operations, complete_operations, is_operation,
};
mod operation;
mod utils;
pub use operation::{
    AttrSpec, Operation, Region, RustFormat, Sem, StatePorts, ValueSpec, emit_operation,
};
pub use utils::op_fn_ident;

pub mod type_definitions;
