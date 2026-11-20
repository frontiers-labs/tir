pub use super::generated::{
    ModuleEndOp, ModuleEndOpBuilder, ModuleOp, ModuleOpBuilder, module, module_end,
};

use crate::Terminator;

impl Terminator for ModuleEndOp {}
