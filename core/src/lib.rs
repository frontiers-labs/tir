extern crate self as tir;

// Re-exported so the `register_pass!` macro can reference linkme from
// downstream crates without each of them depending on it directly.
pub use linkme;

pub mod analysis;
pub mod attributes;
mod block;
mod builder;
mod context;
mod diagnostics;
mod dialect;
mod dialects;
mod error;
pub mod graph;
mod interfaces;
mod ir_formatter;
mod operand;
mod operation;
mod pass;
pub mod passes;
pub mod pbqp;
mod region;
pub mod sem_expr;
mod ty;
pub mod utils;
mod value;

pub mod helpers {
    pub use tir_macros::{SimpleNode, dialect, operation};
}
pub mod parse;

pub use block::{Block, BlockId};
pub use builder::{IRBuilder, InsertionPoint};
pub use context::{Context, ContextIterator, ContextRef, GetFromContext};
pub use diagnostics::{print_error_range, print_parse_error};
pub use dialect::{Dialect, OperationParser};
pub use error::Error;
pub use interfaces::{
    Commutative, MemoryRead, MemoryWrite, PromotableAllocation, SameOperandType, Terminator,
};
pub use ir_formatter::IRFormatter;
pub use operand::Operand;
pub use operation::{
    ErasedOpInterface, ImplementsOpInterface, OpDefVerifiable, OpId, OpInstance,
    OpInterfaceConverter, Operation, Verifiable, downcast_op_interface, erase_op_interface,
    op_interface_converter,
};
pub use pass::{
    OperationRef, PASSES, Pass, PassError, PassInfo, PassManager, PassTarget, Rewriter, build_pass,
    parse_pipeline, registered_passes,
};
pub use region::{Region, RegionId};
pub use ty::{Any, Type, TypeConstraint, TypeId, TypeParser};
pub use value::{Use, UseSite, Value, ValueId};

pub use dialects::builtin;
pub use dialects::builtin::Integer;
pub use dialects::ptr;
pub use dialects::scf;

pub use tir_macros::{dialect, operation};
