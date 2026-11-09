use crate::symbol_table::SymbolTable;
use crate::{Context, Error, OpHandle};

use crate as tir;

tir::helpers::op_def! {
    file: "defs/builtin.cdsl",
    hosts: {
        module_verifier => module_verify_impl,
        ops::Terminator => (crate::Terminator, &crate::TerminatorCallbacks::DEFAULT),
    },
}

fn module_verify_impl(operation: &OpHandle, context: &Context) -> Result<(), Error> {
    SymbolTable::build(context, operation.id).verify(context)
}
