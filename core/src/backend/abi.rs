use crate::analysis::defuse::PhysReg;
use crate::{Context, TypeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    Int,
    Float,
    Vector,
}

/// Classifies an IR type for ABI register assignment.
pub fn type_kind(context: &Context, ty: TypeId) -> ValueKind {
    if crate::builtin::FloatType::view(context, ty).is_some() {
        ValueKind::Float
    } else if crate::vector::VectorType::view(context, ty).is_some() {
        ValueKind::Vector
    } else {
        ValueKind::Int
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    Chain(ValueKind),
    Stack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveStyle {
    FrameSlots,
    PushPop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierKind {
    Riscv,
    Aapcs64,
    Sysv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackLayout {
    pub align: u32,
    pub slot_size: u32,
    pub red_zone: u32,
    pub grows_down: bool,
    pub save_style: SaveStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassSeq {
    pub kind: ValueKind,
    pub regs: &'static [PhysReg],
    pub overflow: Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgumentGroupAlignment {
    pub kind: ValueKind,
    pub minimum_source_alignment: u64,
    pub register_multiple: usize,
}

impl ArgumentGroupAlignment {
    pub fn align_slot(self, kind: ValueKind, source_alignment: u64, slot: usize) -> usize {
        if kind != self.kind || source_alignment < self.minimum_source_alignment {
            return slot;
        }
        slot.div_ceil(self.register_multiple) * self.register_multiple
    }
}

/// Register-state handling when an atomic argument group cannot use registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRollback {
    Exhaust,
    Preserve,
}

/// Allocation constraints shared by every member of one atomic argument group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgumentGroupPolicy {
    pub register_limit: Option<usize>,
    pub rollback: GroupRollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiInfo {
    pub name: &'static str,
    pub stack: StackLayout,
    pub sp: PhysReg,
    pub ra: Option<PhysReg>,
    pub fp: Option<PhysReg>,
    pub indirect_result: Option<PhysReg>,
    pub argument_group_alignment: Option<ArgumentGroupAlignment>,
    pub argument_group_policy: Option<ArgumentGroupPolicy>,
    pub args: &'static [PassSeq],
    pub rets: &'static [PassSeq],
    pub callee_saved: &'static [PhysReg],
    pub caller_saved: &'static [PhysReg],
    pub reserved: &'static [PhysReg],
    pub classifier: ClassifierKind,
}

impl AbiInfo {
    pub fn indirect_result_argument_slots(&self) -> Option<(ValueKind, usize)> {
        let register = self.indirect_result?;
        self.args.iter().find_map(|sequence| {
            sequence
                .regs
                .iter()
                .position(|candidate| *candidate == register)
                .map(|slot| (sequence.kind, slot + 1))
        })
    }
}
