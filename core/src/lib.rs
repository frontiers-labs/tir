extern crate self as tir;

// Re-exported so the `register_pass!` macro can reference linkme from
// downstream crates without each of them depending on it directly.
pub use linkme;

/// Declares an entity's identity: a `u32` handle into the context's slab of
/// that entity.
macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        pub struct $name(u32);

        impl $name {
            pub(crate) fn new(id: u32) -> Self {
                Self(id)
            }

            /// Raw integer id, for stable identification across an FFI boundary.
            pub fn number(self) -> u32 {
                self.0
            }

            /// Reconstruct an id from its raw integer, the inverse of
            /// [`Self::number`].
            pub fn from_number(id: u32) -> Self {
                Self(id)
            }

            pub(crate) fn index(self) -> usize {
                self.0 as usize
            }

            /// The hive handle backing this id.
            pub(crate) fn raw(self) -> u32 {
                self.0
            }
        }
    };
}

pub mod analysis;
pub mod attributes;
pub mod backend;
pub mod binding;
mod block;
mod clone;
pub use clone::{clone_op, clone_region_with_mapping};
mod context;
mod diagnostics;
mod dialect;
mod dialects;
mod edits;
mod error;
pub mod graph;
mod interfaces;
pub mod interp;
mod ir_formatter;
mod layout;
pub mod memstats;
mod operand;
mod operation;
mod overlay;
mod pass;
pub mod passes;
mod print;
mod region;
pub mod region_format;
pub(crate) mod run;
pub mod schema;
mod scoped_attr;
pub mod sem;
mod store;
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
pub use context::{Context, ContextIterator, GetFromContext, Parent, StagedRegion};
pub use diagnostics::{print_error_range, print_parse_error};
pub use dialect::{Dialect, OperationParser};
pub use error::Error;
pub use interfaces::{
    Apply, Binding, BranchGuard, BranchTerminator, Callable, Commutative, ConstantFold,
    ConstantLike, CountedLoop, ExitScope, ExitScopeKind, ExitTarget, Gamma, Global,
    IntegerArithmetic, MemoryRead, MemoryState, MemoryWrite, NonLocalExit, OpCost,
    PromotableAllocation, Pure, SameOperandAndResultType, Speculatable, Symbol, Terminator, Theta,
    Visibility,
};
pub use interp::{Interp, InterpError, Memory as InterpMemory, Value as InterpValue};
pub use ir_formatter::IRFormatter;
pub use layout::{DATA_LAYOUT, DataLayout, Endianness, data_layout_spec};
pub use operand::Operand;
pub use operation::{
    DialectName, ErasedOpInterface, ImplementsOpInterface, NewOp, NewOpParts, OpDefSpec,
    OpDefVerifiable, OpHandle, OpId, OpInstance, OpInterfaceConverter, Operation, OperationName,
    RegionIds, ValueIds, Verifiable, downcast_op_interface, erase_op_interface,
    op_interface_converter, verify_op_tree, verify_opdef_attributes, verify_opdef_operands,
};
pub use overlay::{Frozen, OverlayCensus};
pub use pass::{
    OperationRef, PASSES, Pass, PassError, PassInfo, PassManager, PassTarget, build_pass,
    parse_pipeline, registered_passes, report_pass_timing,
};
pub use print::print_ir;
pub use region::{Region, RegionBody, RegionHandle, RegionId, RegionKind};
pub use schema::{
    AttrSchema, FieldSchema, OP_SCHEMAS, OpSchema, TYPE_SCHEMAS, TypeArg, TypeParam, TypeParamKind,
    TypeSchema, build_type, schema_json, type_schema_json,
};
pub use scoped_attr::{AttributeDict, scoped_dict};
pub use symbol_table::{SymbolEntry, SymbolTable};
pub use target_env::{TARGET_ENV, TargetEnv, target_env_spec};
pub use tir_adt::Sym;
pub use ty::{Any, Type, TypeConstraint, TypeId, TypeParser};
pub use value::{Use, Value, ValueId};

pub use dialects::builtin;
pub use dialects::builtin::Integer;
pub use dialects::cfg;
pub use dialects::func;
pub use dialects::ptr;
pub use dialects::scf;
pub use dialects::state;
pub use dialects::vector;

pub use tir_macros::{TirType, dialect, operation};
