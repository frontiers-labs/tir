//! Trace-driven timing: replay the dynamic instruction stream recorded by the
//! functional [`crate::Executor`] (the oracle) against a TMDL-generated
//! [`MachineModel`] and assign cycles with the shared [`crate::scoreboard`]
//! engine. Nothing is executed here — the trace already encodes every taken
//! branch and resolved address, so loops and control flow come for free, and
//! branch outcomes can be scored against a [`BranchPredictor`].

use std::collections::HashMap;

use tir::backend::liveness::execution_regs;
use tir::backend::sched::{InstrSchedClass, MachineModel};
use tir::backend::{ControlFlow, MachineInstruction, RegSlot, reg_slot};
use tir::{Context, OpId};

use crate::MemAccess;
use crate::memsys::MemorySystem;
use crate::predictor::BranchPredictor;
use crate::scoreboard::{
    self, BranchOutcome, EventHandler, FusionOperandFact, FusionOperandValue, Prf, ScoreboardInstr,
    phys_regs,
};

pub use crate::scoreboard::{TimingConfig, TimingResult};

/// Replay `trace` (a `(op, pc)` stream) against `model` and return the cycle
/// count. `predictor` supplies branch-direction guesses; mispredictions stall
/// the front end by `config.mispredict_penalty` cycles. `prf` enables
/// register-file pressure on a renaming core. `handler` receives the pipeline
/// events for report rendering.
///
/// `sched_trace`, when supplied, must contain one entry-state scheduling class
/// per trace entry, captured for `model`. `None` uses static instruction classes.
///
/// Only [`ControlFlow::Conditional`] instructions are predictor-scored: an
/// unconditional transfer's target is known at decode, so it flows through the
/// scoreboard as an ordinary instruction with its scheduled cost.
/// `register_widths` carries configured target widths for fusion guards.
/// `encoded_trace`, when supplied, carries bytes captured at fetch; unavailable
/// bytes stay `None` so encoding guards remain unknown.
/// `mem_trace`, when supplied, holds the data-memory accesses per trace entry
/// (same length as `trace`) recorded by the executor; together with `mem` (the
/// hierarchy) it makes load/store latency state-dependent. Both `None` keeps the
/// fixed-latency behavior.
#[allow(clippy::too_many_arguments)]
pub fn simulate(
    model: &MachineModel,
    context: &Context,
    trace: &[(OpId, u64)],
    sched_trace: Option<&[InstrSchedClass]>,
    config: &TimingConfig,
    predictor: &mut dyn BranchPredictor,
    prf: Option<&Prf>,
    register_widths: &[(&'static str, u32)],
    encoded_trace: Option<&[Option<Vec<u8>>]>,
    mem_trace: Option<&[Vec<MemAccess>]>,
    mem: Option<&mut MemorySystem>,
    handler: Option<&mut dyn EventHandler>,
) -> TimingResult {
    if let Some(classes) = sched_trace {
        assert_eq!(
            classes.len(),
            trace.len(),
            "schedule trace must match instruction trace"
        );
    }
    if let Some(bytes) = encoded_trace {
        assert_eq!(
            bytes.len(),
            trace.len(),
            "encoded trace must match instruction trace"
        );
    }
    // Pre-resolve each trace entry to its scheduling class, registers, and
    // (for conditional branches) PC and width — branch outcomes need the next
    // entry's PC, so they are filled in a second pass below.
    struct Pre {
        pc: u64,
        width: u64,
        control_flow: ControlFlow,
    }
    let mut pre: Vec<Pre> = Vec::with_capacity(trace.len());
    let mut slots: Vec<ScoreboardInstr> = Vec::with_capacity(trace.len());
    for (i, (id, pc)) in trace.iter().enumerate() {
        let op = context.get_op(*id);
        let mi = op.clone().as_interface::<dyn MachineInstruction>();
        let (op_name, class, width, control_flow) = match &mi {
            Some(mi) => {
                let info = mi.info();
                let class = sched_trace.map_or_else(|| info.sched_on(model), |classes| classes[i]);
                (
                    info.name,
                    class,
                    u64::from(mi.width_bytes()),
                    info.control_flow,
                )
            }
            None => ("", InstrSchedClass::DEFAULT, 4, ControlFlow::None),
        };
        let regs = execution_regs(&op);
        let fusion_boundary = if i == 0 {
            true
        } else {
            let previous = &pre[i - 1];
            let fallthrough = previous.pc.wrapping_add(previous.width);
            *pc != fallthrough || previous.control_flow != ControlFlow::None
        };
        pre.push(Pre {
            pc: *pc,
            width,
            control_flow,
        });
        let fusion_operands = if model.fusions.is_empty() {
            Vec::new()
        } else {
            mi.as_ref()
                .map(|mi| fusion_operands(&op, mi.info(), prf, register_widths))
                .unwrap_or_default()
        };
        slots.push(ScoreboardInstr {
            text: String::new(),
            op_name: op_name.to_string(),
            class,
            defs: phys_regs(&regs.phys_defs, prf),
            uses: phys_regs(&regs.phys_uses, prf),
            or_updates: phys_regs(
                mi.as_ref()
                    .map(|mi| mi.info().implicit_or_updates)
                    .unwrap_or_default(),
                prf,
            ),
            fusion_operands,
            fusion_boundary,
            layout_known: true,
            encoded_bytes: encoded_trace.and_then(|bytes| bytes[i].clone()),
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
        if pre[i].control_flow != ControlFlow::Conditional {
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

/// Read the named register and immediate operands used by target fusion rules.
/// Register names are normalized through the model's physical register file.
pub fn fusion_operands(
    op: &tir::OpHandle,
    info: &tir::backend::InstrInfo,
    prf: Option<&Prf>,
    register_widths: &[(&'static str, u32)],
) -> Vec<FusionOperandFact> {
    let mut facts = Vec::new();
    for port in info.regs {
        let Some(RegSlot::Phys((class, index))) = reg_slot(op, port.name) else {
            continue;
        };
        let file = prf
            .and_then(|p| p.class_to_file.get(class.name()))
            .map(String::as_str)
            .unwrap_or_else(|| class.file())
            .to_string();
        let class_width = register_widths
            .iter()
            .find(|(name, _)| *name == class.name())
            .map(|(_, width)| u64::from(*width));
        let file_width = prf.and_then(|prf| {
            register_widths
                .iter()
                .filter(|(name, _)| {
                    prf.class_to_file
                        .get(*name)
                        .is_some_and(|register_file| register_file == &file)
                })
                .map(|(_, width)| u64::from(*width))
                .max()
        });
        let bit_range = class_width
            .zip(file_width)
            .and_then(|(class_width, file_width)| {
                let start = u64::from(index)
                    .checked_mul(file_width)?
                    .checked_add(u64::from(class.info().view.bit_offset))?;
                let end = class_width
                    .checked_mul(u64::from(class.info().group_width.max(1)))?
                    .checked_add(start)?;
                Some((start, end))
            });
        facts.push(FusionOperandFact {
            name: port.name.to_string(),
            width_bits: class_width.and_then(|width| u16::try_from(width).ok()),
            value: FusionOperandValue::Register {
                file,
                index,
                bit_range,
            },
        });
    }
    for attribute in op.attributes() {
        let value = match attribute.value {
            tir::attributes::AttributeValue::Int(value) => i128::from(value),
            tir::attributes::AttributeValue::UInt(value) => i128::from(value),
            _ => continue,
        };
        let name = op.context.resolve(attribute.name);
        let width_bits = info.encode.and_then(|encoding| {
            let mut widths = encoding.shapes.iter().filter_map(|shape| {
                shape
                    .fields
                    .iter()
                    .find(|field| field.attr == name)
                    .map(|field| field.runs.iter().map(|run| run.width).sum::<u16>())
            });
            let width = widths.next()?;
            widths.all(|candidate| candidate == width).then_some(width)
        });
        facts.push(FusionOperandFact {
            name,
            width_bits,
            value: FusionOperandValue::Immediate(value),
        });
    }
    facts
}
