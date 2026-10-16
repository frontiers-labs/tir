extern crate self as tir;

// Re-exported so the `register_pass!` macro can reference linkme from
// downstream crates without each of them depending on it directly.
pub use linkme;

/// Declares an entity's identity: the `u32` position of its row in the
/// context's storage.
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
mod idmap;
mod interfaces;
pub mod interp;
mod intrinsic;
mod ir_formatter;
pub mod layout;
pub mod memstats;
mod operand;
mod operation;
mod pass;
pub mod passes;
mod print;
mod region;
pub mod region_format;
pub(crate) mod rewrite;
pub mod schema;
mod scoped_attr;
pub mod sem;
mod state_verification;
mod store;
pub mod symbol_table;
mod target_env;
mod ty;
mod typecheck;
pub mod utils;
mod value;
pub mod variants;

pub mod helpers {
    pub use tir_macros::{dialect, operation};
}
pub mod parse;

pub use analysis::{Analysis, AnalysisManager};
pub use block::{BlockHandle, BlockId};
pub use context::{Census, Context, ContextIterator, FunctionScope, GetFromContext, Names, Parent};
pub use diagnostics::{print_error_range, print_parse_error};
pub use dialect::{Dialect, OperationParser};
pub use error::Error;
pub use idmap::{IdMap, IdSet, RowId};
pub use interfaces::{
    Additive, Apply, Binding, BranchGuard, BranchTerminator, Callable, CaseGuard, Commutative,
    Congruent, ConstantFold, ConstantLike, CountedLoop, ExitScope, ExitScopeKind, ExitTarget,
    Gamma, Global, HasResourceSemantics, IntegerArithmetic, MemoryRead, MemoryWrite, NonLocalExit,
    OpCost, PromotableAllocation, Pure, RegionBinding, ResourceAccess, ResourceEffect,
    ResourceEffects, ResourceField, ResourceSemantics, SameOperandAndResultType, Speculatable,
    Subtractive, Symbol, Terminator, Theta, Visibility,
};
pub use interp::{
    ExecutionState, FloatEnvironment, Interp, InterpError, Memory as InterpMemory,
    Value as InterpValue,
};
pub use intrinsic::Intrinsic;
pub use ir_formatter::IRFormatter;
pub use layout::{DataLayout, Endianness, LAYOUT_SPEC, data_layout_spec};
pub use operand::Operand;
pub use operation::{
    DialectName, ErasedOpInterface, ImplementsOpInterface, NewOp, NewOpParts, OpDefSpec,
    OpDefVerifiable, OpHandle, OpId, OpInterfaceConverter, Operation, OperationName, RegionIds,
    ValueIds, Verifiable, downcast_op_interface, erase_op_interface, op_interface_converter,
    verify_op_tree, verify_opdef_attributes, verify_opdef_operands,
};
pub use pass::{
    OperationRef, PASSES, Pass, PassError, PassInfo, PassManager, PassTarget, parse_pipeline,
    registered_passes, report_pass_timing,
};
pub use print::print_ir;
pub use region::{RegionHandle, RegionId, RegionKind};
pub use schema::{
    AttrSchema, FieldSchema, OP_SCHEMAS, OpSchema, build_type, schema_json, type_schema_json,
};
pub use scoped_attr::{AttributeDict, scoped_dict};
pub use symbol_table::{SymbolEntry, SymbolTable};
pub use target_env::{TARGET_ENV, TargetEnv, target_env_spec};
pub use tir_adt::Sym;
pub use tir_adt::{Dag, NodeId};
pub use ty::{
    Ctor, CtorId, Kind, Name, Param, Spellings, Terms, TypeArg, TypeCtor, TypeId, TypeParser,
    TypePrinter, TypeSpelling, TypeTerm,
};
pub use typecheck::{check_types, scheme_of};
pub use value::{Use, Value, ValueId};

pub use dialects::builtin;
pub use dialects::cfg;
pub use dialects::fp;
pub use dialects::func;
pub use dialects::ptr;
pub use dialects::scf;
pub use dialects::state;
pub use dialects::vector;

pub use tir_macros::{dialect, operation};
