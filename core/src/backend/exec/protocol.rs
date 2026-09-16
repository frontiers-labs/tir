use crate::backend::SimTrap;
use crate::sem::{AtomicRmwOp, MemOrdering};
use crate::utils::RawBits;

/// Identity of one effect within one instruction evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId {
    pub instruction: u64,
    pub sequence: u64,
}

/// An external operation requested by instruction semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEffect {
    Read {
        address: u64,
        size: usize,
    },
    Write {
        address: u64,
        bytes: Vec<u8>,
    },
    LoadReserved {
        address: u64,
        size: usize,
        ordering: MemOrdering,
    },
    StoreConditional {
        address: u64,
        size: usize,
        value: u64,
        ordering: MemOrdering,
    },
    AtomicRmw {
        op: AtomicRmwOp,
        address: u64,
        size: usize,
        value: u64,
        ordering: MemOrdering,
    },
    Fence {
        pred: u32,
        succ: u32,
        kind: u32,
    },
    Exception {
        cause: u64,
    },
}

/// A request remains pending until a response with its identity is supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRequest {
    pub id: RequestId,
    pub pc: u64,
    pub effect: MemoryEffect,
}

/// The result of an accepted external operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseValue {
    Word(u64),
    Bytes(RawBits),
    Done,
}

#[derive(Debug, Clone)]
pub struct EffectResponse {
    pub id: RequestId,
    pub result: Result<ResponseValue, SimTrap>,
}

/// Evaluation stopped at an external effect or reached its commit boundary.
#[derive(Debug, Clone)]
pub enum FrameYield {
    Effect(EffectRequest),
    Complete,
}

impl MemoryEffect {
    pub(super) fn accepts(&self, response: &ResponseValue) -> bool {
        match (self, response) {
            (Self::Read { size, .. }, ResponseValue::Bytes(bits)) => {
                *size > 8 && bits.bytes().len() == *size
            }
            (Self::Read { size, .. }, ResponseValue::Word(_)) => *size <= 8,
            (Self::LoadReserved { .. } | Self::AtomicRmw { .. }, ResponseValue::Word(_)) => true,
            (Self::StoreConditional { .. }, ResponseValue::Word(value)) => *value <= 1,
            (
                Self::Write { .. } | Self::Fence { .. } | Self::Exception { .. },
                ResponseValue::Done,
            ) => true,
            _ => false,
        }
    }
}
