use crate::Terminator;
use crate::operation;
use crate::symbol_table::SymbolTable;
use crate::{Context, Error, Operation};

use crate as tir;

operation! {
    ModuleOp {
        name: "module",
        dialect: "builtin",
        verifier: "true",
        regions: R {
            body: Region {
                single_block: true,
            }
        }
    }
}

impl tir::Verifiable for ModuleOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        SymbolTable::build(context, self.id()).verify(context)
    }
}

operation! {
    ModuleEndOp {
        name: "module_end",
        dialect: "builtin",
        interfaces: [Terminator],
    }
}

impl Terminator for ModuleEndOp {}
