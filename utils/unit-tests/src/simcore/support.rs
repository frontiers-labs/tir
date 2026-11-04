//! Shared helpers for the simcore unit tests.

use tir::backend::sched::MachineModel;
use tir::backend::AsmDialect;
use tir::{Context, OpId};
use tir_riscv::RiscvDialect;
use tir_sim::predictor::BranchPredictor;
use tir_sim::timing::{simulate, TimingConfig, TimingResult};
use tir_sim::ProgramImage;

/// Assemble RISC-V `asm` and lay it out at 0x8000_0000, entered at `entry`.
pub fn riscv_program(context: &Context, asm: &str, entry: &str) -> ProgramImage {
    context.register_dialect::<AsmDialect>();
    context.register_dialect::<RiscvDialect>();
    let dialect = context.find_dialect::<RiscvDialect>().unwrap();
    let module = dialect.get_asm_parser().parse_asm(context, asm).unwrap();
    ProgramImage::from_module(context, module, 0x8000_0000, Some(entry)).unwrap()
}

/// Time `trace` under `predictor`, with no register file, memory trace, memory
/// system or event handler.
pub fn run_sim(
    model: &MachineModel,
    context: &Context,
    trace: &[(OpId, u64)],
    config: &TimingConfig,
    predictor: &mut dyn BranchPredictor,
) -> TimingResult {
    simulate(
        model,
        context,
        trace,
        None,
        config,
        predictor,
        None,
        &[],
        None,
        None,
        None,
        None,
    )
}
