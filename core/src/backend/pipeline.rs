//! The shared backend pass pipeline, used by `tir mc` and `fcc`.
//!
//! Ordering matters: `vcond_br` is lowered to a real conditional branch plus
//! `vbr` *before* register allocation because its condition is an SSA value
//! the allocator must color, while `vret`/`vbr` are finalized *after* it
//! because the allocator identifies `vret` to precolor return values.

use tir::{
    AnalysisManager, Context, IntegerArithmetic, OperationRef, Pass, PassError, PassManager,
    PassTarget, PreservedAnalyses, Rewriter,
    builtin::{FuncOp, IntegerType},
};

use crate::backend::TargetMachine;
use crate::backend::lower::OpLoweringPass;
use crate::passes::DeadCodeEliminationPass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAfter {
    ISel,
    RegAlloc,
    Finalize,
}

struct TargetIntegerLegalizer {
    max_width: u32,
}

impl TargetIntegerLegalizer {
    fn new(target: &dyn TargetMachine) -> Self {
        let info = target.register_info();
        let class = target
            .abis()
            .first()
            .and_then(|abi| info.default_integer_class(abi))
            .or_else(|| {
                info.classes
                    .first()
                    .map(crate::backend::regalloc::RegClassId::new)
            })
            .expect("target must define an integer register class");
        let max_width = target
            .register_widths()
            .into_iter()
            .find_map(|(name, width)| (name == class.name()).then_some(width))
            .expect("target must define integer register width");
        Self { max_width }
    }

    fn check_value(&self, context: &Context, value: tir::ValueId) -> Result<(), PassError> {
        let ty = context.get_value(value).ty();
        let data = context.get_type_data(ty);
        let any = data.as_ref() as &dyn std::any::Any;
        if let Some(int) = any.downcast_ref::<IntegerType>()
            && int.width() > self.max_width
        {
            return Err(PassError::InvalidRuleSet(format!(
                "integer type i{} exceeds target register width i{}",
                int.width(),
                self.max_width
            )));
        }
        Ok(())
    }
}

impl Pass for TargetIntegerLegalizer {
    fn name(&self) -> &'static str {
        "target-integer-legalizer"
    }

    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        if op.as_interface::<dyn IntegerArithmetic>().is_none() {
            return Ok(PreservedAnalyses::all());
        }
        for &value in op.op().operands.iter().chain(op.op().results.iter()) {
            self.check_value(context, value)?;
        }
        Ok(PreservedAnalyses::all())
    }
}

/// Build the lowering pipeline for `target`: instruction selection, pre-RA
/// lowerings, register allocation, and post-RA finalization.
pub fn build_pipeline(
    target: &dyn TargetMachine,
    context: &Context,
    stop: StopAfter,
) -> PassManager {
    let mut pm = PassManager::new();
    pm.add_pass(TargetIntegerLegalizer::new(target));
    pm.nest::<FuncOp>().add_pass(target.isel_pass(context));
    // Remove pure instructions left dead by selection (e.g. a value recomputed in
    // a consumer's block by cross-block fusion). Runs while results are still
    // virtual registers, so it must precede register allocation.
    pm.add_pass(DeadCodeEliminationPass::new());
    if stop == StopAfter::ISel {
        return pm;
    }

    let pre_ra = target.pre_ra_lowerings();
    if !pre_ra.is_empty() {
        pm.add_pass(OpLoweringPass::new("pre-ra-lowering", pre_ra));
    }
    for pass in target.machine_passes() {
        pm.add_boxed_pass(pass);
    }
    pm.add_pass(target.regalloc_pass());
    if stop == StopAfter::RegAlloc {
        return pm;
    }

    let finalize = target.finalize_lowerings();
    if !finalize.is_empty() {
        pm.add_pass(OpLoweringPass::new("finalize-lowering", finalize));
    }
    pm
}
