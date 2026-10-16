//! The shared backend pass pipeline, used by `tir mc` and `fcc`.
//!
//! Ordering matters: `vcond_br` is lowered to a real conditional branch plus
//! `vbr` *before* register allocation because its condition is an SSA value
//! the allocator must color, while `vret`/`vbr` are finalized *after* it
//! because the allocator consumes their typed operands.

use tir::Operation as _;
use tir::{
    AnalysisManager, Context, IntegerArithmetic, OperationRef, Pass, PassError, PassManager,
    PassTarget,
    builtin::{IntegerType, ModuleOp},
    func::FuncOp,
};

use crate::backend::isel::InstructionSelectPass;
use crate::backend::lower::OpLoweringPass;
use crate::backend::{MachineBlockLayoutPass, ShuffleMachineOrderPass, TargetMachine};
use crate::passes::{
    AffineSchedulePass, CheckUniqueSymbolsPass, DeadCodeEliminationPass, EraseLayoutPass,
    LowerIntrinsicsPass, LowerPtrDisjointPass, MaterializeSymbolAddressesPass, ResolveFpPass,
    RestructureNodesPass, VerifyDepsPass,
};

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
    /// (see [`ShuffleMachineOrderPass`]).
    pub shuffle_machine_order: bool,
}

#[derive(Clone)]
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
        if let Some(int) = IntegerType::view(context, ty)
            && int.width() > u64::from(self.max_width)
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

/// Turn every function under `root` that still holds a control-flow graph into
/// regions, memory order constructed on the way, and schedule its loops. A
/// caller whose input is a raw graph (IR text, LLVM IR, the C API) runs this
/// ahead of [`build_pipeline`]; a front end that builds regions itself does
/// not.
pub fn structure(context: &Context, root: tir::OpHandle) -> Result<(), PassError> {
    let mut pm = PassManager::new();
    let functions = pm.nest::<FuncOp>();
    // A no-alias fact is mid-end vocabulary: no machine has an instruction
    // for it, so it becomes the range check it stands for. The lowering reads
    // blocks, so it goes ahead of the conversion.
    functions.add_pass(LowerPtrDisjointPass::new());
    functions.add_pass(RestructureNodesPass::new());
    functions.add_pass(AffineSchedulePass::new());
    pm.run(context, root)
}

/// Build the lowering pipeline for `target`: instruction selection, pre-RA
/// lowerings, register allocation, and post-RA finalization. Rooted at a module,
/// it lowers every function in it.
pub fn build_pipeline(
    target: &dyn TargetMachine,
    context: &Context,
    stop: StopAfter,
    oracles: Oracles,
) -> PassManager {
    let mut pm = module_prologue(target);
    add_function_passes(&mut pm, target, target.isel_pass(context), stop, oracles);
    pm
}

/// The passes that must see the whole module: they run once, ahead of any
/// function's lowering.
fn module_prologue(target: &dyn TargetMachine) -> PassManager {
    let mut pm = PassManager::new();
    {
        let functions = pm.nest::<FuncOp>();
        // Selection has no instruction for a field address, a record copy or
        // a proved fact, whoever built the module. A function a front end
        // already erased holds none of them, and the pass reads no more of it.
        functions.add_pass(EraseLayoutPass::new());
        functions.add_pass(DeadCodeEliminationPass::new());
        // Selection takes an unordered body, and whoever hands the module over
        // builds one: a front end restructures its own control flow, and a
        // caller with a raw graph runs [`structure`] first. The chains are
        // checked here, while the module is whole: the check reads
        // a function's calls, and by the time it is lowered one at a time the
        // λ those name may already be a machine symbol.
        functions.add_pass(VerifyDepsPass::new());
    }
    // Object symbols are unique by name, so overloads must already be mangled.
    pm.add_pass(CheckUniqueSymbolsPass::new());
    pm.add_pass(LowerIntrinsicsPass::for_target(target));
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
    isel: InstructionSelectPass,
    stop: StopAfter,
    oracles: Oracles,
) {
    pm.add_pass(TargetIntegerLegalizer::new(target));
    let function_pipeline = pm.nest::<FuncOp>();
    let isel = std::sync::Arc::new(std::sync::Mutex::new(isel));
    function_pipeline.add_boxed_pass(Box::new(SettleSelectionRules(isel.clone())));
    function_pipeline.add_boxed_pass(Box::new(ResolveFpPass::shared(isel.clone())));
    function_pipeline.add_boxed_pass(Box::new(SharedInstructionSelect(isel)));
    // Remove pure instructions left dead by selection (e.g. a value recomputed in
    // a consumer's block by cross-block fusion). Runs while results are still
    // virtual registers, so it must precede register allocation.
    pm.add_pass(DeadCodeEliminationPass::new());
    if oracles.shuffle_machine_order {
        pm.add_pass(ShuffleMachineOrderPass::new());
    }
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
    // Post-allocation the map is what makes two values one register, so this is
    // the run that exercises the anti- and output edges derived from it.
    if oracles.shuffle_machine_order {
        pm.add_pass(ShuffleMachineOrderPass::new());
    }
    if stop == StopAfter::RegAlloc {
        return;
    }

    let finalize = target.finalize_lowerings();
    if !finalize.is_empty() {
        pm.add_pass(OpLoweringPass::new("finalize-lowering", finalize));
    }
    if let Some(format) = target.object_format() {
        pm.add_pass(MachineBlockLayoutPass::new(format));
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
    isel: InstructionSelectPass,
    oracles: Oracles,
    mut emit: impl FnMut(&Context, &tir::OpHandle) -> Result<(), String>,
) -> Result<(), String> {
    let failed = |error: PassError| format!("backend pipeline failed: {error}");
    let body = module.body().id();
    module_prologue(target)
        .run_on_op_ref(
            context,
            OperationRef::new(context.get_op(module.id())),
            &AnalysisManager::new(),
        )
        .map_err(failed)?;

    let mut pipeline = PassManager::new();
    add_function_passes(&mut pipeline, target, isel, StopAfter::Finalize, oracles);
    let mut index = 0;
    while let Some(&op_id) = context.get_block(body).op_ids().get(index) {
        let op = context.get_op(op_id);
        if !op.is::<FuncOp>() {
            emit(context, &op)?;
            index += 1;
            continue;
        }
        // A fresh cache per function: an analysis of a function that has been
        // emitted describes IR that no longer exists.
        let root = OperationRef::new(op);
        let symbol = pipeline
            .run_on_op_ref(context, root, &AnalysisManager::new())
            .map_err(failed)?;
        emit(context, symbol.op())?;
        // The bytes exist, so the machine IR behind them goes back to the arenas
        // and the next function is lowered in the space it left.
        context.erase_op(&symbol).map_err(failed)?;
        context.commit();
    }
    tir::memstats::summary();
    Ok(())
}

// Waits for the selection rules, so the passes that select are timed
// without their synthesis.
struct SettleSelectionRules(std::sync::Arc<std::sync::Mutex<InstructionSelectPass>>);

impl Pass for SettleSelectionRules {
    fn name(&self) -> &'static str {
        "instruction-select-synthesis"
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if !op.op().regions().is_empty() {
            self.0
                .lock()
                .expect("instruction select")
                .settle(context, op);
        }
        Ok(())
    }
}

struct SharedInstructionSelect(std::sync::Arc<std::sync::Mutex<InstructionSelectPass>>);

impl Pass for SharedInstructionSelect {
    fn name(&self) -> &'static str {
        "instruction-select"
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        self.0
            .lock()
            .expect("instruction select")
            .run(op, context, analyses)
    }
}
