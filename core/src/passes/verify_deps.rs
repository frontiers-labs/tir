//! The memory-order invariant of an unordered body.
//!
//! The conversion from ordered blocks constructs the dependency chains; no
//! pass draws them again. What every later pass has to keep is checked here:
//! every operation touching memory names the state it observes, every one
//! changing memory leaves a state behind, and every such operation is demanded
//! from the body's results, since under demand evaluation an effect nothing
//! demands never runs.

use std::collections::HashSet;

use crate::analysis::AnalysisManager;
use crate::func::{CallOp, FuncOp, ReturnOp};
use crate::ptr::MemcpyOp;
use crate::{
    Context, MemoryRead, MemoryWrite, OpHandle, OpId, OperationRef, Pass, PassError, PassTarget,
    PromotableAllocation, RegionKind, Rewriter, ValueId,
};

/// What one operation does to memory, before the objects it names are read.
enum Kind {
    /// Opens the memory of a slot.
    Open,
    /// Observes the memory an address names and leaves it as it found it.
    Read,
    /// Leaves a memory at an address that the reads after it see.
    Write,
    /// Touches every object the outside can reach.
    Clobber,
    /// Hands every object the outside can reach to the function's caller.
    Export,
}

/// What `op` does to memory.
fn classify(op: &OpHandle) -> Option<Kind> {
    if op.has_interface::<dyn PromotableAllocation>() {
        return Some(Kind::Open);
    }
    // Both interfaces are asked before either answers: an operation declaring
    // the two writes the extent it reads, and is no observer.
    if op.has_interface::<dyn MemoryWrite>() {
        return Some(Kind::Write);
    }
    if op.has_interface::<dyn MemoryRead>() {
        return Some(Kind::Read);
    }
    if op.is::<MemcpyOp>() || op.is::<CallOp>() {
        return Some(Kind::Clobber);
    }
    op.is::<ReturnOp>().then_some(Kind::Export)
}

/// cone never runs, and the order it was meant to keep is gone with it.
pub fn verify_deps(context: &Context, function: &OpHandle) -> Result<(), crate::Error> {
    let name = function
        .clone()
        .as_interface::<dyn crate::Symbol>()
        .map(|symbol| symbol.symbol_name())
        .unwrap_or_default();
    let fail = |op: &OpHandle, what: &str| {
        Err(crate::Error::VerificationError(format!(
            "{}.{} in @{name} {what}",
            op.dialect(),
            op.name()
        )))
    };
    // Demand runs from the body's results: an op is demanded through an operand
    // of a demanded op or a result of a region of one, and an op in a region
    // nobody demands is demanded by nothing, however its own region reads it.
    let mut demanded: HashSet<OpId> = HashSet::new();
    let defining = |values: Vec<ValueId>| {
        values
            .into_iter()
            .filter_map(|value| context.get_value(value).defining_op())
            .collect::<Vec<_>>()
    };
    let mut worklist: Vec<OpId> = function
        .regions()
        .iter()
        .flat_map(|&region| defining(context.get_region(region).results()))
        .collect();
    while let Some(op) = worklist.pop() {
        if !demanded.insert(op) {
            continue;
        }
        let instance = context.get_op(op);
        worklist.extend(defining(instance.operands().to_vec()));
        for region in instance.regions() {
            worklist.extend(defining(context.get_region(region).results()));
        }
    }
    for region in function
        .regions()
        .iter()
        .flat_map(|&region| context.nested_regions(region))
    {
        let handle = context.get_region(region);
        if !handle.is_nodes() {
            continue;
        }
        for op_id in handle.op_ids() {
            let op = context.get_op(op_id);
            let changes = match classify(&op) {
                Some(Kind::Read) => false,
                Some(Kind::Write | Kind::Clobber) => true,
                _ => continue,
            };
            if op.dep_operands().is_empty() {
                return fail(&op, "names no dependency");
            }
            if changes && op.dep_results().is_empty() {
                return fail(&op, "leaves no dependency behind");
            }
            if !demanded.contains(&op_id) {
                return fail(&op, "is demanded by nothing");
            }
        }
    }
    Ok(())
}

/// [`verify_deps`] as a pass: the unordered pipeline's stand-in for
/// `thread-state`, which constructs nothing there and checks instead.
#[derive(Default)]
pub struct VerifyDepsPass;

impl VerifyDepsPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(VerifyDepsPass, "verify-deps");

impl Pass for VerifyDepsPass {
    fn name(&self) -> &'static str {
        "verify-deps"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn run(
        &mut self,
        operation: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        verify_deps(context, operation.op()).map_err(|error| PassError::InvalidIR {
            pass: "verify-deps",
            error,
        })
    }
}
