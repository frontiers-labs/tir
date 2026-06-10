//! Trace-driven timing: replay the dynamic instruction stream recorded by the
//! functional [`crate::Executor`] (the oracle) against a TMDL-generated
//! [`MachineModel`] and assign cycles with the shared [`crate::scoreboard`]
//! engine. Nothing is executed here — the trace already encodes every taken
//! branch and resolved address, so loops and control flow come for free, and
//! branch outcomes can be scored against a [`BranchPredictor`].

use std::collections::HashMap;

use tir::{Context, OpId};
use tir_be_common::liveness::op_regs;
use tir_be_common::sched::{InstrSchedClass, MachineModel};
use tir_be_common::{ControlFlow, MachineInstruction};

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
pub fn simulate(
    model: &MachineModel,
    context: &Context,
    trace: &[(OpId, u64)],
    config: &TimingConfig,
    predictor: &mut dyn BranchPredictor,
    prf: Option<&Prf>,
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
    for (id, pc) in trace {
        let op = context.get_op(*id);
        let mi = op.clone().as_interface::<dyn MachineInstruction>();
        let (class, width, is_branch) = match &mi {
            Some(mi) => (
                model.sched_class(mi.mnemonic()),
                u64::from(mi.width_bytes()),
                mi.control_flow() == ControlFlow::Conditional,
            ),
            None => (InstrSchedClass::DEFAULT, 4, false),
        };
        let regs = op_regs(&op);
        pre.push(Pre {
            pc: *pc,
            width,
            is_branch,
        });
        slots.push(ScoreboardInstr {
            text: String::new(),
            class,
            defs: phys_regs(&regs.defs),
            uses: phys_regs(&regs.uses),
            branch: None,
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

    scoreboard::run(model, &slots, 1, config, Some(predictor), prf, handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Executor, ProgramImage, TraceOptions};
    use tir_be_common::AsmDialect;
    use tir_riscv::RiscvDialect;

    use crate::predictor::AlwaysNotTaken;

    /// Control flow is derived from the behavior's `PC::pc` writes at
    /// TMDL-compile time: a guarded write is a conditional branch, an
    /// unguarded one an unconditional transfer, none a sequential instruction.
    /// PC *reads* (auipc) must not count.
    #[test]
    fn control_flow_derived_from_pc_writes() {
        use tir::Operation;
        use tir::attributes::{AttributeValue, RegisterAttr};
        use tir_be_common::ControlFlow;

        let context = tir::Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();
        context.register_dialect::<arm64::Arm64Dialect>();

        let gpr = |index: u16| {
            AttributeValue::Register(RegisterAttr::Physical {
                class: "GPR".to_string(),
                index,
            })
        };
        let imm = AttributeValue::Int(0);
        let cf = |op: &dyn Operation| {
            context
                .get_op(op.id())
                .as_interface::<dyn MachineInstruction>()
                .expect("machine instruction")
                .control_flow()
        };

        let beq = tir_riscv::BranchEqOpBuilder::new(&context)
            .attr("rs1", gpr(1))
            .attr("rs2", gpr(2))
            .attr("imm", imm.clone())
            .build();
        assert_eq!(cf(&beq), ControlFlow::Conditional);
        // jal writes PC unconditionally (and the link register).
        let jal = tir_riscv::JumpAndLinkOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("imm", imm.clone())
            .build();
        assert_eq!(cf(&jal), ControlFlow::Unconditional);
        let jalr = tir_riscv::JumpAndLinkRegOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("rs1", gpr(2))
            .attr("imm", imm.clone())
            .build();
        assert_eq!(cf(&jalr), ControlFlow::Unconditional);
        let add = tir_riscv::AddOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("rs1", gpr(2))
            .attr("rs2", gpr(3))
            .build();
        assert_eq!(cf(&add), ControlFlow::None);
        // auipc reads PC but never writes it.
        let auipc = tir_riscv::AddUpperImmToPCOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("imm", imm.clone())
            .build();
        assert_eq!(cf(&auipc), ControlFlow::None);

        let b_eq = arm64::BranchEqOpBuilder::new(&context)
            .attr("imm", imm.clone())
            .build();
        assert_eq!(cf(&b_eq), ControlFlow::Conditional);
        let ret = arm64::ReturnOpBuilder::new(&context)
            .attr("rn", gpr(30))
            .build();
        assert_eq!(cf(&ret), ControlFlow::Unconditional);
        let bl = arm64::BranchLinkOpBuilder::new(&context)
            .attr("imm", imm)
            .build();
        assert_eq!(cf(&bl), ControlFlow::Unconditional);
    }

    /// Run `asm` functionally, recording the dynamic trace, then time it.
    fn time_asm(asm: &str, model: &MachineModel, config: &TimingConfig) -> TimingResult {
        let context = tir::Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();
        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program = ProgramImage::from_module(&context, module, 0x8000_0000, Some("first"))
            .expect("program builder");
        let until_pc = *program.symbols.get("done").unwrap();

        let mut exec = Executor::new(4096);
        exec.enable_trace_recording();
        exec.load(program).unwrap();
        exec.run_with_trace(
            until_pc,
            10_000,
            TraceOptions::default(),
            &mut std::io::sink(),
        )
        .unwrap();

        simulate(
            model,
            &context,
            exec.trace(),
            config,
            &mut AlwaysNotTaken,
            None,
            None,
        )
    }

    /// Five independent ALU ops: an out-of-order core overlaps them (wide issue),
    /// an in-order core retires them one per cycle. The engine must reflect that.
    #[test]
    fn ooo_overlaps_independent_work() {
        // The asm parser emits symbols in reverse source order, so the `done`
        // sentinel is declared first to land *after* `first` in memory (giving
        // `first` a fallthrough to stop on).
        let asm = "
            .global done
            done:
              add x0, x0, x0
            .global first
            first:
              add a0, a1, a2
              add a3, a4, a5
              add a6, a7, t0
              add t1, t2, t3
              add t4, t5, t6
        ";

        let in_order_model = tir_riscv::in_order_core_model();
        let ooo_model = tir_riscv::out_of_order_core_model();

        let io = time_asm(
            asm,
            &in_order_model,
            &TimingConfig::for_model(&in_order_model),
        );
        let oo = time_asm(asm, &ooo_model, &TimingConfig::for_model(&ooo_model));

        assert_eq!(io.instructions, 5);
        assert_eq!(oo.instructions, 5);
        // The out-of-order core finishes the independent chain in fewer cycles and
        // sustains higher IPC.
        assert!(
            oo.cycles < io.cycles,
            "ooo {} should beat in-order {}",
            oo.cycles,
            io.cycles
        );
        assert!(oo.ipc() > io.ipc());
    }

    /// The predictor changes the cycle count: a *taken backward* branch (loop
    /// back-edge) is mispredicted by always-not-taken (paying the refetch penalty)
    /// but predicted correctly by BTFN. We parse a real branch op for its registers
    /// and width, then drive a synthetic `(op, pc)` trace whose addresses describe
    /// the back-edge — independent of the functional executor's branch handling.
    #[test]
    fn predictor_changes_mispredicts_on_backward_branch() {
        use crate::predictor::BackwardTaken;
        use tir::OpId;

        let context = tir::Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();
        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm = "
            .global blk
            blk:
              beq a0, a0, 0
              add a1, a2, a3
        ";
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("blk")).unwrap();
        let ops: Vec<OpId> = program
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter().copied())
            .collect();
        assert_eq!(ops.len(), 2);

        // Branch at 0x100 whose successor executes at 0x080: a taken back-edge.
        let trace = vec![(ops[0], 0x100u64), (ops[1], 0x080u64)];
        let model = tir_riscv::out_of_order_core_model();
        let config = TimingConfig::for_model(&model);

        let ant = simulate(
            &model,
            &context,
            &trace,
            &config,
            &mut AlwaysNotTaken,
            None,
            None,
        );
        let btfn = simulate(
            &model,
            &context,
            &trace,
            &config,
            &mut BackwardTaken,
            None,
            None,
        );

        assert_eq!(
            ant.mispredicts, 1,
            "not-taken mispredicts the taken back-edge"
        );
        assert_eq!(btfn.mispredicts, 0, "btfn predicts the back-edge taken");
        assert!(
            ant.cycles > btfn.cycles,
            "misprediction penalty should cost cycles: ant {} vs btfn {}",
            ant.cycles,
            btfn.cycles
        );
    }

    /// End-to-end: a real backward-branch loop runs functionally (3 iterations),
    /// and the recorded trace shows the loop predictor's advantage — always-not-taken
    /// mispredicts every taken back-edge, BTFN only the loop exit.
    #[test]
    fn loop_branch_prediction_end_to_end() {
        use crate::predictor::BackwardTaken;
        use tir_be_common::MachineContext;

        // Sentinel/exit blocks precede the entry so the reverse-ordered layout puts
        // `first` at the base; `bne …, -4` is a single-instruction block, so its PC
        // is exact and it branches back to the decrement.
        let asm = "
            .global done
            done:
              add x0, x0, x0
            .global exitblk
            exitblk:
              add x0, x0, x0
            .global br
            br:
              bne a0, zero, -4
            .global dec
            dec:
              addi a0, a0, -1
            .global first
            first:
              addi a0, zero, 3
        ";
        let context = tir::Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();
        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let module = dialect.get_asm_parser().parse_asm(&context, asm).unwrap();
        let program =
            ProgramImage::from_module(&context, module, 0x8000_0000, Some("first")).unwrap();
        let until_pc = *program.symbols.get("done").unwrap();

        let mut exec = Executor::new(4096);
        exec.enable_trace_recording();
        exec.load(program).unwrap();
        exec.run_with_trace(
            until_pc,
            10_000,
            TraceOptions::default(),
            &mut std::io::sink(),
        )
        .unwrap();

        // The loop ran to completion: counter 3 → 0.
        assert_eq!(
            MachineContext::read_register(&exec, "GPR", 10)
                .unwrap()
                .to_u64(),
            0
        );
        let trace = exec.trace().to_vec();

        let model = tir_riscv::out_of_order_core_model();
        let config = TimingConfig::for_model(&model);
        let ant = simulate(
            &model,
            &context,
            &trace,
            &config,
            &mut AlwaysNotTaken,
            None,
            None,
        );
        let btfn = simulate(
            &model,
            &context,
            &trace,
            &config,
            &mut BackwardTaken,
            None,
            None,
        );

        assert_eq!(
            ant.mispredicts, 2,
            "not-taken mispredicts both taken back-edges"
        );
        assert_eq!(btfn.mispredicts, 1, "btfn only mispredicts the loop exit");
        assert!(
            btfn.cycles < ant.cycles,
            "btfn {} should beat ant {}",
            btfn.cycles,
            ant.cycles
        );
    }

    /// A dependent chain serializes on both cores regardless of issue width.
    #[test]
    fn dependent_chain_serializes() {
        let asm = "
            .global done
            done:
              add x0, x0, x0
            .global first
            first:
              add a0, a0, a1
              add a0, a0, a1
              add a0, a0, a1
              add a0, a0, a1
        ";
        let ooo_model = tir_riscv::out_of_order_core_model();
        let oo = time_asm(asm, &ooo_model, &TimingConfig::for_model(&ooo_model));
        // Four dependent adds (override latency 2 each) cannot overlap: at least
        // 4 * 2 cycles of dependency latency.
        assert_eq!(oo.instructions, 4);
        assert!(oo.cycles >= 8, "dependent chain too short: {}", oo.cycles);
    }
}
