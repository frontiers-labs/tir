//! The shared backend pass pipeline, used by `tir mc` and `fcc`.
//!
//! Ordering matters: `vcond_br` is lowered to a real conditional branch plus
//! `vbr` *before* register allocation because its condition is an SSA value
//! the allocator must color, while `vret`/`vbr` are finalized *after* it
//! because the allocator consumes their typed operands.

use tir::Operation as _;
use tir::{
    AnalysisManager, Context, IntegerArithmetic, OperationRef, Pass, PassError, PassManager,
    PassTarget, Rewriter,
    builtin::{IntegerType, ModuleOp},
    func::FuncOp,
};

use crate::backend::TargetMachine;
use crate::backend::lower::OpLoweringPass;
use crate::passes::{
    CheckUniqueSymbolsPass, DeadCodeEliminationPass, EraseStatePass, LowerMemoryIntrinsicsPass,
    LowerPtrDisjointPass, MaterializeSymbolAddressesPass, RestructurePass,
};

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
    ) -> Result<(), PassError> {
        if op.as_interface::<dyn IntegerArithmetic>().is_none() {
            return Ok(());
        }
        for &value in op.op().operands().iter().chain(op.op().results().iter()) {
            self.check_value(context, value)?;
        }
        Ok(())
    }
}

/// Build the lowering pipeline for `target`: instruction selection, pre-RA
/// lowerings, register allocation, and post-RA finalization. Rooted at a module,
/// it lowers every function in it.
pub fn build_pipeline(
    target: &dyn TargetMachine,
    context: &Context,
    stop: StopAfter,
) -> PassManager {
    let mut pm = module_prologue();
    add_function_passes(&mut pm, target, context, stop);
    pm
}

/// The passes that must see the whole module: they run once, ahead of any
/// function's lowering.
fn module_prologue() -> PassManager {
    let mut pm = PassManager::new();
    // Memory state is the mid-end's order and codegen takes the implicit one
    // back, so the chains are dropped at the boundary — here, because a function
    // is lowered and erased one at a time and by then the λ its calls name may
    // already be a machine symbol, which is not IR erasure can be checked
    // against. It goes ahead of the intrinsic lowering, which expands a `memcpy`
    // into accesses no chain would reach.
    {
        let functions = pm.nest::<FuncOp>();
        functions.add_pass(EraseStatePass::new());
        // A no-alias fact is mid-end vocabulary too: no machine has an
        // instruction for it, so it becomes the range check it stands for.
        functions.add_pass(LowerPtrDisjointPass::new());
    }
    // Object symbols are unique by name, so overloads must already be mangled.
    pm.add_pass(CheckUniqueSymbolsPass::new());
    pm.add_pass(LowerMemoryIntrinsicsPass::new());
    // Machine code cannot reach a value defined outside the function it runs in,
    // and functions are lowered one at a time: uses of module-level λ and δ
    // values become symbol addresses while the whole module is still here.
    pm.add_pass(MaterializeSymbolAddressesPass::new());
    pm
}

/// Everything in [`build_pipeline`] that only ever looks at one function, so it
/// can be rooted at a single one.
fn add_function_passes(
    pm: &mut PassManager,
    target: &dyn TargetMachine,
    context: &Context,
    stop: StopAfter,
) {
    pm.add_pass(TargetIntegerLegalizer::new(target));
    let function_pipeline = pm.nest::<FuncOp>();
    // Selection takes structured regions: a function that still holds a raw CFG
    // (a `.tir` input, or JIT text) is raised here. One that is already a single
    // structured block is left alone.
    function_pipeline.add_pass(RestructurePass::new());
    function_pipeline.add_pass(target.isel_pass(context));
    // Remove pure instructions left dead by selection (e.g. a value recomputed in
    // a consumer's block by cross-block fusion). Runs while results are still
    // virtual registers, so it must precede register allocation.
    pm.add_pass(DeadCodeEliminationPass::new());
    if stop == StopAfter::ISel {
        return;
    }

    let pre_ra = target.pre_ra_lowerings();
    if !pre_ra.is_empty() {
        pm.add_pass(OpLoweringPass::new("pre-ra-lowering", pre_ra));
    }
    for pass in target.machine_passes() {
        pm.add_boxed_pass(pass);
    }
    for pass in target.regalloc_stage() {
        pm.add_boxed_pass(pass);
    }
    if stop == StopAfter::RegAlloc {
        return;
    }

    let finalize = target.finalize_lowerings();
    if !finalize.is_empty() {
        pm.add_pass(OpLoweringPass::new("finalize-lowering", finalize));
    }
}

/// Lower `module` to machine code and hand it to `emit` one symbol at a time.
///
/// Each function is run through the whole backend on its own, emitted, and then
/// erased, so the machine IR alive at any moment is one function's rather than
/// the whole module's. Everything else in the module body — data sections,
/// external declarations — is handed to `emit` where it stands.
pub fn lower_and_emit(
    target: &dyn TargetMachine,
    context: &Context,
    module: &ModuleOp,
    mut emit: impl FnMut(&Context, &tir::OpHandle) -> Result<(), String>,
) -> Result<(), String> {
    let failed = |error: PassError| format!("backend pipeline failed: {error}");
    let body = module.body().id();
    module_prologue()
        .run_on_op_ref(
            context,
            OperationRef::new(context.get_op(module.id()), None, None),
            &AnalysisManager::new(),
        )
        .map_err(failed)?;

    let mut pipeline = PassManager::new();
    add_function_passes(&mut pipeline, target, context, StopAfter::Finalize);
    let mut rewriter = Rewriter::new(context.clone());
    let mut index = 0;
    while let Some(&op_id) = context.get_block(body).op_ids().get(index) {
        let block = context.get_block(body);
        let op = context.get_op(op_id);
        if !op.is::<FuncOp>() {
            emit(context, &op)?;
            index += 1;
            continue;
        }
        // A fresh cache per function: an analysis of a function that has been
        // emitted describes IR that no longer exists.
        let root = OperationRef::new(op, Some(block), Some(index));
        let symbol = pipeline
            .run_on_op_ref(context, root, &AnalysisManager::new())
            .map_err(failed)?;
        emit(context, symbol.op())?;
        // The bytes exist, so the machine IR behind them goes back to the arenas
        // and the next function is lowered in the space it left.
        rewriter.erase_op(&symbol).map_err(failed)?;
    }
    tir::memstats::summary();
    Ok(())
}
