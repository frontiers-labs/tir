extern crate self as tir;

// Re-exported so the `register_pass!` macro can reference linkme from
// downstream crates without each of them depending on it directly.
pub use linkme;

pub mod analysis;
pub mod attributes;
pub mod backend;
mod block;
mod clone;
mod context;
mod diagnostics;
mod dialect;
mod dialects;
mod error;
pub mod graph;
mod interfaces;
mod ir_formatter;
mod layout;
pub mod memstats;
mod operand;
mod operation;
mod pass;
pub mod passes;
mod print;
mod region;
pub mod region_format;
pub mod schema;
mod scoped_attr;
pub mod sem;
pub mod symbol_table;
mod target_env;
mod ty;
pub mod utils;
mod value;

pub mod helpers {
    pub use tir_macros::{TirType, dialect, operation};
}
pub mod parse;

pub use analysis::{Analysis, AnalysisManager};
pub use block::{Block, BlockHandle, BlockId};
pub use context::{Context, ContextIterator, ContextRef, GetFromContext, StagedRegion};
pub use diagnostics::{print_error_range, print_parse_error};
pub use dialect::{Dialect, OperationParser};
pub use error::Error;
pub use interfaces::{
    BranchGuard, BranchTerminator, Commutative, Conditional, ConstantFold, ConstantLike,
    CountedLoop, EntryGuard, GuardOrdering, GuardedLoop, IntegerArithmetic, LoopLike, MemoryRead,
    MemoryWrite, OpCost, PromotableAllocation, SameOperandAndResultType, SameOperandType, Symbol,
    Terminator, TokenScope, Visibility,
};
pub use ir_formatter::IRFormatter;
pub use layout::{DATA_LAYOUT, DataLayout, Endianness, data_layout_spec};
pub use operand::Operand;
pub use operation::{
    DialectName, ErasedOpInterface, ImplementsOpInterface, OpDefSpec, OpDefVerifiable, OpHandle,
    OpId, OpInstance, OpInterfaceConverter, Operation, OperationName, RegionIds, ValueIds,
    Verifiable, downcast_op_interface, erase_op_interface, op_interface_converter, verify_op_tree,
    verify_opdef_attributes, verify_opdef_operands,
};
pub use pass::{
    OperationRef, PASSES, Pass, PassError, PassInfo, PassManager, PassTarget, Rewriter, build_pass,
    parse_pipeline, registered_passes,
};
pub use print::print_ir;
pub use region::{Region, RegionHandle, RegionId};
pub use schema::{
    AttrSchema, FieldSchema, OP_SCHEMAS, OpSchema, TYPE_SCHEMAS, TypeArg, TypeParam, TypeParamKind,
    TypeSchema, build_type, schema_json, type_schema_json,
};
pub use scoped_attr::{AttributeDict, scoped_dict};
pub use symbol_table::{SymbolEntry, SymbolTable};
pub use target_env::{TARGET_ENV, TargetEnv, target_env_spec};
pub use tir_adt::Sym;
pub use ty::{Any, Type, TypeConstraint, TypeId, TypeParser};
pub use value::{Value, ValueId};

pub use dialects::builtin;
pub use dialects::builtin::Integer;
pub use dialects::ptr;
pub use dialects::scf;
pub use dialects::state;
pub use dialects::vector;

pub use tir_macros::{TirType, dialect, operation};
