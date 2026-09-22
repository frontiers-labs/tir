//! What every backend must answer the same way, asked once per backend.

use tir::backend::abi::{PassSeq, ValueKind};
use tir::backend::liveness::PhysReg;
use tir::backend::sched::InstrSchedClass;
use tir::backend::{InstrInfo, MemoryEffects};

/// The sequence `sequences` passes values of `kind` in.
pub fn pass_seq(sequences: &[PassSeq], kind: ValueKind) -> &PassSeq {
    sequences
        .iter()
        .find(|sequence| sequence.kind == kind)
        .unwrap_or_else(|| panic!("the abi passes no {kind:?} values"))
}

/// The register numbers of `regs`, in order.
pub fn numbers(regs: &[PhysReg]) -> Vec<u16> {
    regs.iter().map(|register| register.1).collect()
}

/// The one per-opcode record `backend` describes `name` with.
pub fn info(backend: &str, infos: &[&'static InstrInfo], name: &str) -> &'static InstrInfo {
    infos
        .iter()
        .copied()
        .find(|info| info.name == name)
        .unwrap_or_else(|| panic!("{backend} declares no instruction '{name}'"))
}

/// The facts one opcode's record must carry on its own.
struct OpcodeFacts {
    backend: &'static str,
    infos: &'static [&'static InstrInfo],
    opcode: &'static str,
    mnemonic: &'static str,
    width_bytes: (u8, u8),
    /// One schedule per machine model the backend describes.
    sched_len: usize,
}

#[test]
fn instruction_info_carries_every_per_opcode_fact() {
    // One record per opcode, keyed by op name: an opcode prints, encodes and
    // schedules through the fields of its own `InstrInfo`, with no side table
    // keyed by its name. x86-64's `add32` and `add` share a mnemonic but not a
    // record, so neither can reach the other's facts.
    for facts in [
        OpcodeFacts {
            backend: "arm64",
            infos: tir_arm64::instruction_infos(),
            opcode: "add",
            mnemonic: "add",
            width_bytes: (4, 4),
            sched_len: tir_arm64::machines(tir_arm64::Feature::ALL).len(),
        },
        OpcodeFacts {
            backend: "riscv",
            infos: tir_riscv::instruction_infos(),
            opcode: "add",
            mnemonic: "add",
            width_bytes: (4, 4),
            sched_len: tir_riscv::machines(tir_riscv::Feature::ALL).len(),
        },
        OpcodeFacts {
            backend: "x86-64",
            infos: tir_x86_64::instruction_infos(),
            opcode: "add32",
            mnemonic: "add",
            // Its two shapes are the REX-free and REX encodings of the same
            // operation.
            width_bytes: (2, 3),
            sched_len: 1,
        },
    ] {
        let record = info(facts.backend, facts.infos, facts.opcode);
        let at = facts.backend;
        assert_eq!(record.mnemonic, facts.mnemonic, "{at}");
        assert_eq!(record.width_bytes, facts.width_bytes, "{at}");
        assert!(record.asm.is_some(), "{at}");
        assert!(record.encode.is_some(), "{at}");
        assert_eq!(record.sched.len(), facts.sched_len, "{at}");
        assert_ne!(record.sched[0], InstrSchedClass::DEFAULT, "{at}");
        assert_eq!(record.effects, MemoryEffects::NONE, "{at}");
    }
}
