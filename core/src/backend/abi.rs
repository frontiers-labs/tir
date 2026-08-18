use std::collections::{HashMap, HashSet};

use crate::backend::liveness::PhysReg;
use crate::{Context, TypeId, ValueId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    Int,
    Float,
    Vector,
}

/// Classifies an IR type for ABI register assignment.
pub fn type_kind(context: &Context, ty: TypeId) -> ValueKind {
    let data = context.get_type_data(ty);
    let data = data.as_ref() as &dyn std::any::Any;
    if data.downcast_ref::<crate::builtin::FloatType>().is_some() {
        ValueKind::Float
    } else if data.downcast_ref::<crate::vector::VectorType>().is_some() {
        ValueKind::Vector
    } else {
        ValueKind::Int
    }
}

pub(crate) fn value_kind(context: &Context, value: ValueId) -> ValueKind {
    type_kind(context, context.get_value(value).ty())
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

impl ArgumentGroupPolicy {
    pub fn fits_register_limit(self, members: usize) -> bool {
        self.register_limit.is_none_or(|limit| members <= limit)
    }
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
    pub(crate) fn argument_group_fits_register_limit(&self, members: usize) -> bool {
        self.argument_group_policy
            .is_none_or(|policy| policy.fits_register_limit(members))
    }

    pub(crate) fn argument_group_rollback(&self) -> GroupRollback {
        self.argument_group_policy
            .map_or(GroupRollback::Exhaust, |policy| policy.rollback)
    }

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

pub(crate) fn align_argument_group(
    abi: &AbiInfo,
    source_alignment: u64,
    kinds: impl IntoIterator<Item = ValueKind>,
    next_slot: &mut HashMap<ValueKind, usize>,
) {
    let Some(alignment) = abi.argument_group_alignment else {
        return;
    };
    if !kinds.into_iter().any(|kind| kind == alignment.kind) {
        return;
    }
    let slot = next_slot.entry(alignment.kind).or_default();
    *slot = alignment.align_slot(alignment.kind, source_alignment, *slot);
}

pub(crate) fn reserve_indirect_result_argument(
    abi: &AbiInfo,
    next_slot: &mut HashMap<ValueKind, usize>,
) {
    let Some((kind, slot)) = abi.indirect_result_argument_slots() else {
        return;
    };
    let next = next_slot.entry(kind).or_default();
    *next = (*next).max(slot);
}

pub(crate) fn exhaust_argument_registers(
    abi: &AbiInfo,
    mut kind: ValueKind,
    next_slot: &mut HashMap<ValueKind, usize>,
) {
    let mut visited = HashSet::new();
    while visited.insert(kind) {
        let sequence = match abi.args.iter().find(|sequence| sequence.kind == kind) {
            Some(sequence) => sequence,
            None if kind != ValueKind::Int => {
                kind = ValueKind::Int;
                continue;
            }
            None => return,
        };
        next_slot.insert(kind, sequence.regs.len());
        match sequence.overflow {
            Overflow::Chain(next) => kind = next,
            Overflow::Stack => return,
        }
    }
}
