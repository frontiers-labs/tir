//! Trace-driven timing: replay the dynamic instruction stream recorded by the
//! functional [`crate::Executor`] (the oracle) against a TMDL-generated
//! [`MachineModel`] and assign cycles with the shared [`crate::scoreboard`]
//! engine. Nothing is executed here — the trace already encodes every taken
//! branch and resolved address, so loops and control flow come for free, and
//! branch outcomes can be scored against a [`BranchPredictor`].

use std::collections::HashMap;

use tir::backend::liveness::execution_regs;
use tir::backend::sched::{InstrSchedClass, MachineModel};
use tir::backend::{ControlFlow, MachineInstruction};
use tir::{Context, OpId};

use crate::MemAccess;
use crate::memsys::MemorySystem;
use crate::predictor::BranchPredictor;
use crate::scoreboard::{self, BranchOutcome, EventHandler, Prf, ScoreboardInstr, phys_regs};

pub use crate::scoreboard::{TimingConfig, TimingResult};

/// Replay `trace` (a `(op, pc)` stream) against `model` and return the cycle
/// count. `predictor` supplies branch-direction guesses; mispredictions stall
/// the front end by `config.mispredict_penalty` cycles. `prf` enables
/// register-file pressure on a renaming core. `handler` receives the pipeline
/// events for report rendering.
///
/// Only [`ControlFlow::Conditional`] instructions are predictor-scored: an
/// unconditional transfer's target is known at decode, so it flows through the
/// scoreboard as an ordinary instruction with its scheduled cost.
/// `mem_trace`, when supplied, holds the data-memory accesses per trace entry
/// (same length as `trace`) recorded by the executor; together with `mem` (the
/// hierarchy) it makes load/store latency state-dependent. Both `None` keeps the
/// fixed-latency behavior.
#[allow(clippy::too_many_arguments)]
pub fn simulate(
    model: &MachineModel,
    context: &Context,
    trace: &[(OpId, u64)],
    config: &TimingConfig,
    predictor: &mut dyn BranchPredictor,
    prf: Option<&Prf>,
    mem_trace: Option<&[Vec<MemAccess>]>,
    mem: Option<&mut MemorySystem>,
    handler: Option<&mut dyn EventHandler>,
) -> TimingResult {
    // Pre-resolve each trace entry to its scheduling class, registers, and
    // (for conditional branches) PC and width — branch outcomes need the next
    // entry's PC, so they are filled in a second pass below.
    struct Pre {
        pc: u64,
        width: u64,
        is_branch: bool,
    }
    let mut pre = Vec::with_capacity(trace.len());
    let mut slots: Vec<ScoreboardInstr> = Vec::with_capacity(trace.len());
    for (i, (id, pc)) in trace.iter().enumerate() {
        let op = context.get_op(*id);
        let mi = op.clone().as_interface::<dyn MachineInstruction>();
        let (op_name, class, width, is_branch) = match &mi {
            Some(mi) => {
                let info = mi.info();
                (
                    info.name,
                    info.sched_on(model),
                    u64::from(info.width_bytes),
                    info.control_flow == ControlFlow::Conditional,
                )
            }
            None => ("", InstrSchedClass::DEFAULT, 4, false),
        };
        let regs = execution_regs(&op);
        pre.push(Pre {
            pc: *pc,
            width,
            is_branch,
        });
        slots.push(ScoreboardInstr {
            text: String::new(),
            op_name: op_name.to_string(),
            class,
            defs: phys_regs(&regs.phys_defs, prf),
            uses: phys_regs(&regs.phys_uses, prf),
            branch: None,
            pc: *pc,
            width_bytes: width.min(u64::from(u16::MAX)) as u16,
            mem: mem_trace.map(|mt| mt[i].clone()).unwrap_or_default(),
        });
    }

    // Resolve branch outcomes from consecutive PCs. Learned branch targets (a
    // minimal BTB) give a not-taken branch a target to predict against. The
    // final trace entry has no successor, so its outcome is unknowable and it
    // is not scored.
    let mut btb: HashMap<u64, u64> = HashMap::new();
    for i in 0..pre.len().saturating_sub(1) {
        if !pre[i].is_branch {
            continue;
        }
        let pc = pre[i].pc;
        let fallthrough = pc.wrapping_add(pre[i].width);
        let next_pc = pre[i + 1].pc;
        let taken = next_pc != fallthrough;
        let target = if taken {
            btb.insert(pc, next_pc);
            next_pc
        } else {
            btb.get(&pc).copied().unwrap_or(fallthrough)
        };
        slots[i].branch = Some(BranchOutcome { pc, target, taken });
    }

    scoreboard::run(model, &slots, 1, config, Some(predictor), prf, mem, handler)
}
