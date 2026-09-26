use std::collections::{HashMap, HashSet};

use crate::attributes::AttributeValue;
use crate::backend::liveness::PhysReg;
use crate::backend::regalloc::RegClassId;
use crate::{Context, PassError, TypeId, ValueId};

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

/// Classifies a value for ABI register assignment. A value already living in a
/// register is classified by the file it lives in — what the register holds is
/// what the calling convention places — and everything else by its type.
pub(crate) fn value_kind(context: &Context, abi: &AbiInfo, value: ValueId) -> ValueKind {
    let ty = context.get_value(value).ty();
    match crate::backend::type_class(context, ty) {
        Some(class) => class_kind(abi, class),
        None => type_kind(context, ty),
    }
}

/// The ABI class a register class belongs to: the argument or return sequence
/// drawing from the same register file.
fn class_kind(abi: &AbiInfo, class: crate::backend::regalloc::RegClassId) -> ValueKind {
    abi.args
        .iter()
        .chain(abi.rets.iter())
        .find(|sequence| {
            sequence
                .regs
                .first()
                .is_some_and(|register| register.0.file() == class.file())
        })
        .map(|sequence| sequence.kind)
        .unwrap_or(ValueKind::Int)
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

/// One member of an atomic argument group: the ABI sequence it draws from and,
/// on the callee side, the register class its pin is named in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArgumentMember {
    pub kind: ValueKind,
    pub class: Option<RegClassId>,
    pub stack_slots: usize,
}

/// An argument placed as a unit, with the alignment its source type demanded.
#[derive(Debug, Clone)]
pub(crate) struct ArgumentGroup {
    pub members: Vec<ArgumentMember>,
    pub alignment: u64,
    pub force_stack: bool,
}

/// Where [`place_arguments`] put one group member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgumentSlot {
    Register(PhysReg),
    Stack(usize),
}

/// Place every argument group in registers where the convention has room for
/// the whole group, and on the stack otherwise. Groups are atomic — all members
/// land in registers or all on the stack — and the ABI's rollback policy decides
/// whether a spilled group also exhausts the remaining argument registers.
/// Returns one slot per member, in order, and the number of stack slots used.
pub(crate) fn place_arguments(
    abi: &AbiInfo,
    groups: &[ArgumentGroup],
    has_result_address: bool,
) -> (Vec<ArgumentSlot>, usize) {
    let mut next_slot = HashMap::new();
    if has_result_address {
        reserve_indirect_result_argument(abi, &mut next_slot);
    }
    let mut slots = Vec::new();
    let mut stack_slots: usize = 0;
    for group in groups {
        let mut trial_slots = next_slot.clone();
        align_argument_group(
            abi,
            group.alignment,
            group.members.iter().map(|member| member.kind),
            &mut trial_slots,
        );
        let direct = (!group.force_stack
            && abi.argument_group_fits_register_limit(group.members.len()))
        .then(|| {
            group
                .members
                .iter()
                .map(|member| {
                    next_argument_register(abi, member.class, member.kind, &mut trial_slots)
                })
                .collect::<Option<Vec<_>>>()
        })
        .flatten();
        if let Some(registers) = direct {
            next_slot = trial_slots;
            slots.extend(registers.into_iter().map(ArgumentSlot::Register));
            continue;
        }
        let stack_alignment = group
            .alignment
            .min(u64::from(abi.stack.align))
            .div_ceil(u64::from(abi.stack.slot_size))
            .max(1) as usize;
        stack_slots = stack_slots.div_ceil(stack_alignment) * stack_alignment;
        for member in &group.members {
            if !group.force_stack && abi.argument_group_rollback() == GroupRollback::Exhaust {
                exhaust_argument_registers(abi, member.kind, &mut next_slot);
            }
            stack_slots = stack_slots.div_ceil(member.stack_slots) * member.stack_slots;
            slots.push(ArgumentSlot::Stack(stack_slots));
            stack_slots += member.stack_slots;
        }
    }
    (slots, stack_slots)
}

/// The attribute an argument group is carried in: a bare member list, or a
/// dictionary naming the alignment its source type demanded.
pub(crate) fn encode_argument_group(
    members: Vec<AttributeValue>,
    alignment: u64,
    force_stack: bool,
) -> AttributeValue {
    if alignment == 1 && !force_stack {
        return AttributeValue::Array(members.into());
    }
    let mut group = std::collections::BTreeMap::from([
        ("alignment".to_string(), AttributeValue::UInt(alignment)),
        ("members".to_string(), AttributeValue::Array(members.into())),
    ]);
    if force_stack {
        group.insert("stack".to_string(), AttributeValue::Bool(true));
    }
    AttributeValue::Dict(Box::new(group))
}

#[derive(Clone, Copy)]
pub(crate) struct ArgumentGroupAttribute<'a> {
    pub members: &'a [AttributeValue],
    pub alignment: u64,
    pub force_stack: bool,
}

/// The members and placement of an argument group attribute, or `None` for an
/// attribute that does not carry a group.
pub(crate) fn decode_argument_group(
    attribute: &AttributeValue,
) -> Result<Option<ArgumentGroupAttribute<'_>>, PassError> {
    match attribute {
        AttributeValue::Array(members) => Ok(Some(ArgumentGroupAttribute {
            members,
            alignment: 1,
            force_stack: false,
        })),
        AttributeValue::Dict(group) => {
            let Some(AttributeValue::Array(members)) = group.get("members") else {
                return Err(PassError::InvalidRuleSet(
                    "ABI argument group has no members".to_string(),
                ));
            };
            let alignment = match group.get("alignment") {
                Some(AttributeValue::UInt(alignment)) => *alignment,
                Some(AttributeValue::Int(alignment)) if *alignment >= 0 => *alignment as u64,
                _ => {
                    return Err(PassError::InvalidRuleSet(
                        "ABI argument group has invalid alignment".to_string(),
                    ));
                }
            };
            let force_stack = match group.get("stack") {
                None => false,
                Some(AttributeValue::Bool(stack)) => *stack,
                _ => {
                    return Err(PassError::InvalidRuleSet(
                        "ABI argument group has invalid stack placement".to_string(),
                    ));
                }
            };
            Ok(Some(ArgumentGroupAttribute {
                members,
                alignment,
                force_stack,
            }))
        }
        _ => Ok(None),
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

/// The argument sequences a value of `kind` may draw from, in overflow order:
/// the sequence for its own kind — the integer one where the ABI does not
/// sequence that kind — followed by every sequence that one chains to.
pub(crate) fn argument_sequences(
    abi: &AbiInfo,
    kind: ValueKind,
) -> impl Iterator<Item = &'static PassSeq> {
    let mut next = Some(kind);
    let mut visited = HashSet::new();
    let args = abi.args;
    std::iter::from_fn(move || {
        loop {
            let kind = next?;
            if !visited.insert(kind) {
                return None;
            }
            let sequence = match args.iter().find(|sequence| sequence.kind == kind) {
                Some(sequence) => sequence,
                None if kind != ValueKind::Int => {
                    next = Some(ValueKind::Int);
                    continue;
                }
                None => return None,
            };
            next = match sequence.overflow {
                Overflow::Chain(chained) => Some(chained),
                Overflow::Stack => None,
            };
            return Some(sequence);
        }
    })
}

pub(crate) fn exhaust_argument_registers(
    abi: &AbiInfo,
    kind: ValueKind,
    next_slot: &mut HashMap<ValueKind, usize>,
) {
    for sequence in argument_sequences(abi, kind) {
        next_slot.insert(sequence.kind, sequence.regs.len());
    }
}

/// The next argument register for a value of `kind`, following the ABI's
/// overflow chain and falling back to the integer sequence for a kind the ABI
/// does not sequence. A `class` with a group width strides the sequence by
/// that width and names the register in its own class.
pub(crate) fn next_argument_register(
    abi: &AbiInfo,
    class: Option<crate::backend::regalloc::RegClassId>,
    kind: ValueKind,
    next_slot: &mut HashMap<ValueKind, usize>,
) -> Option<crate::backend::liveness::PhysReg> {
    let same_file = |class: crate::backend::regalloc::RegClassId, register: PhysReg| {
        register.0.file() == class.file()
    };
    for sequence in argument_sequences(abi, kind) {
        let slot = next_slot.entry(sequence.kind).or_insert(0);
        let register = match class {
            Some(class)
                if class.group_width > 1
                    && sequence
                        .regs
                        .first()
                        .is_some_and(|&first| same_file(class, first)) =>
            {
                let first = sequence.regs.first().unwrap();
                let last = sequence.regs.last().unwrap();
                let index = first.1 + (*slot as u16 * class.group_width);
                (index <= last.1).then_some((class, index))
            }
            _ => sequence.regs.get(*slot).copied(),
        };
        if let Some(register) = register {
            *slot += 1;
            return Some(match class {
                Some(class) if same_file(class, register) => (class, register.1),
                _ => register,
            });
        }
    }
    None
}

/// The next return register for a value of `kind`, falling back to the
/// integer sequence for a kind the ABI does not sequence.
pub(crate) fn next_return_register(
    abi: &AbiInfo,
    kind: ValueKind,
    next_slot: &mut HashMap<ValueKind, usize>,
) -> Option<PhysReg> {
    let sequence = abi
        .rets
        .iter()
        .find(|sequence| sequence.kind == kind)
        .or_else(|| {
            abi.rets
                .iter()
                .find(|sequence| sequence.kind == ValueKind::Int)
        })?;
    let slot = next_slot.entry(sequence.kind).or_insert(0);
    let register = *sequence.regs.get(*slot)?;
    *slot += 1;
    Some(register)
}
