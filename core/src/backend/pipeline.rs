use crate::backend::{TargetMachine, isel::InstructionSelectPass};
use tir::{Context, PassError, PassManager, builtin::ModuleOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAfter {
    ISel,
    RegAlloc,
    Finalize,
}

/// The oracles a differential-testing variant turns on. All of them are
/// semantics-preserving by construction, so a divergence under one is a fact the
/// form does not carry; none is on in production.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Oracles {
    /// Re-linearize every machine block by a seeded random topological order of
    /// its dependence graph, once after selection and once after allocation
    /// (see [`crate::backend::ShuffleMachineOrderPass`]).
    pub shuffle_machine_order: bool,
}

pub fn structure(_context: &Context, _root: tir::OpHandle) -> Result<(), PassError> {
    Ok(())
}

pub fn build_pipeline(
    _target: &dyn TargetMachine,
    _context: &Context,
    _stop: StopAfter,
    _oracles: Oracles,
) -> PassManager {
    PassManager::new()
}

pub fn lower_and_emit(
    _target: &dyn TargetMachine,
    _context: &Context,
    _module: &ModuleOp,
    _isel: InstructionSelectPass,
    _oracles: Oracles,
    _emit: impl FnMut(&Context, &tir::OpHandle) -> Result<(), String>,
) -> Result<(), String> {
    Err("backend lowering is unavailable during the IR migration".into())
}
