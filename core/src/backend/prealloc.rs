//! Pre-allocation lowerings: target-independent IR rewrites that must run after
//! instruction selection and before register allocation. Each is a separate
//! pass so a fix in one cannot perturb the others, and each is exercisable in
//! isolation; [`regalloc_stage_for`] composes them with the allocation pass.

use std::collections::HashMap;

use tir::attributes::{AttributeRole, AttributeValue, RegisterAttr};
use tir::{
    AnalysisManager, Context, OperationRef, Pass, PassError, PassTarget, PreservedAnalyses,
    Rewriter, ValueId,
};

use crate::backend::abi::{
    GroupRollback, align_argument_group, reserve_indirect_result_argument, value_kind,
};
use crate::backend::liveness::PhysReg;
use crate::backend::regalloc::{
    RegClassId, RegisterAllocationPass, RegisterInfo, RoleClass, TargetRegAlloc, op_ref_in,
    rename_attr, role_of, symbol_body_blocks, vreg_class_in,
};
use crate::backend::{SymbolOp, VirtualBranchOp, VirtualReturnOp};

/// Compose the register-allocation stage for a target: the pre-allocation
/// lowerings (in execution order) followed by the allocation pass. `make_target`
/// is called once per pass because each pass owns its target hooks.
pub fn regalloc_stage_for(
    make_target: impl Fn() -> Box<dyn TargetRegAlloc>,
    abi: &'static crate::backend::abi::AbiInfo,
) -> Vec<Box<dyn tir::Pass>> {
    vec![
        Box::new(TiedOperandLoweringPass::new(make_target())),
        Box::new(BlockArgLoweringPass::new(make_target(), abi)),
        Box::new(AbiPrecolorPass::new(make_target(), abi)),
        Box::new(RegisterAllocationPass::with_abi(make_target(), abi)),
    ]
}

/// Lower tied (two-address) operands ahead of allocation. Instruction
/// selection emits `op {dst = %r (ReadWrite), dst_tied = %x, ...}` for an
/// instruction whose behavior reads its destination (e.g. the x86
/// `dst = dst + src`): the op defines `%r` but must read `%x` through the same
/// register. Insert `copy %r <- %x` ahead of the op and drop the marker
/// attribute, leaving a plain read-modify-write of `%r` that liveness and
/// coloring already model.
pub struct TiedOperandLoweringPass {
    target: Box<dyn TargetRegAlloc>,
}

impl TiedOperandLoweringPass {
    pub fn new(target: Box<dyn TargetRegAlloc>) -> Self {
        Self { target }
    }
}

impl Pass for TiedOperandLoweringPass {
    fn name(&self) -> &'static str {
        "tied-operand-lowering"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        for block_id in symbol_body_blocks(context, op) {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                let mut ties = Vec::new();
                for attr in &op.attributes {
                    let Some(base) = attr.name.strip_suffix("_tied") else {
                        continue;
                    };
                    if role_of(&op, base) != AttributeRole::ReadWrite {
                        continue;
                    }
                    let AttributeValue::Register(RegisterAttr::Virtual { id: src, .. }) =
                        &attr.value
                    else {
                        continue;
                    };
                    let Some(AttributeValue::Register(RegisterAttr::Virtual { id: dst, class })) =
                        op.attributes
                            .iter()
                            .find(|a| a.name == base)
                            .map(|a| &a.value)
                    else {
                        continue;
                    };
                    let class = class.ok_or_else(|| {
                        PassError::InvalidRuleSet(format!(
                            "tied operand {} has no register class",
                            attr.name
                        ))
                    })?;
                    ties.push((attr.name.clone(), *dst, *src, class));
                }
                if ties.is_empty() {
                    continue;
                }
                let op_ref = op_ref_in(context, block_id, op_id);
                for (_, dst, src, class) in &ties {
                    let copy = self.target.emit_copy(context, *class, *dst, *src);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                }
                let mut attrs = op.attributes.clone();
                attrs.retain(|a| !ties.iter().any(|(name, ..)| a.name == *name));
                context.set_op_attributes(op_id, attrs);
            }
        }
        Ok(PreservedAnalyses::none())
    }
}

/// Lower forwarded block arguments on unconditional branches to explicit
/// copies ahead of allocation (SSA destruction for block parameters). A `vbr`
/// carries the values forwarded to its destination's block parameters as SSA
/// operands (`dest_args`); nothing otherwise connects a predecessor's forwarded
/// value to the successor's parameter register. For each edge, insert
/// `copy param <- arg` before the branch and clear `dest_args`, leaving the
/// parameter defined in every predecessor. The copies of one edge form a
/// parallel copy (all sources read before any destination is written), so they
/// are sequentialized with a fresh temporary per cycle (e.g. a loop edge that
/// swaps two parameters).
pub struct BlockArgLoweringPass {
    target: Box<dyn TargetRegAlloc>,
    abi: &'static crate::backend::abi::AbiInfo,
}

impl BlockArgLoweringPass {
    pub fn new(
        target: Box<dyn TargetRegAlloc>,
        abi: &'static crate::backend::abi::AbiInfo,
    ) -> Self {
        Self { target, abi }
    }
}

impl Pass for BlockArgLoweringPass {
    fn name(&self) -> &'static str {
        "block-arg-lowering"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        let blocks = symbol_body_blocks(context, op);
        let info = self.target.register_info();
        let default_class = info.default_integer_class(self.abi);
        for &block_id in &blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                if !op.is::<VirtualBranchOp>() || op.operands.is_empty() {
                    continue;
                }
                let args: Vec<u32> = op.operands.iter().map(|v| v.number()).collect();
                let Some(dest) = op.attributes.iter().find_map(|a| match &a.value {
                    AttributeValue::Block(b) if a.name == "dest" => Some(*b),
                    _ => None,
                }) else {
                    return Err(PassError::InvalidRuleSet(
                        "vbr with block arguments is missing its 'dest' target".to_string(),
                    ));
                };
                let params: Vec<u32> = context
                    .get_block(dest)
                    .arguments()
                    .iter()
                    .map(|v| v.id().number())
                    .collect();
                if params.len() != args.len() {
                    return Err(PassError::InvalidRuleSet(format!(
                        "branch forwards {} argument(s) to a block with {} parameter(s)",
                        args.len(),
                        params.len()
                    )));
                }

                // Pair each parameter with its forwarded value and the register
                // class to copy it in (from either endpoint's uses, else the
                // default integer class).
                let mut pairs: Vec<(u32, u32, RegClassId)> = Vec::new();
                for (&param, &arg) in params.iter().zip(args.iter()) {
                    if param == arg {
                        continue;
                    }
                    let class = vreg_class_in(context, &blocks, arg)
                        .or_else(|| vreg_class_in(context, &blocks, param))
                        .or(default_class)
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(format!(
                                "block argument vreg {arg} has no register class"
                            ))
                        })?;
                    pairs.push((param, arg, class));
                }

                let op_ref = op_ref_in(context, block_id, op_id);
                for (dst, src, class) in sequence_parallel_copies(context, pairs) {
                    let copy = self.target.emit_copy(context, class, dst, src);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                }
                context.set_op_operands(op_id, Vec::new());
            }
        }
        Ok(PreservedAnalyses::none())
    }
}

/// Marks a copy this stage inserted to bridge an ABI boundary (argument or
/// return). Register allocation reads the endpoints as a coalescing affinity,
/// erases the copy when both land in one register, and strips the mark.
pub(crate) const ABI_COPY_ATTR: &str = "abi_copy";

/// Marks the placeholder load of an incoming stack argument and carries its
/// stack slot index. Register allocation patches the real offset in once the
/// frame layout is known.
pub(crate) const ABI_STACK_INDEX_ATTR: &str = "abi_stack_index";

/// Compute calling-convention pre-coloring at function entry and returns,
/// recording every constraint in the IR itself: register arguments pin their
/// `arg_regs` entry as [`RegisterAttr::FixedDef`], returns define their pinned
/// outgoing register through a copy marked [`ABI_COPY_ATTR`], and stack
/// arguments get a placeholder load marked [`ABI_STACK_INDEX_ATTR`]. Register
/// arguments are also copied from their pinned entry value into a free body
/// vreg: an ABI register is a point constraint at the function boundary, so it
/// must not pin an entire SSA live range across calls.
pub struct AbiPrecolorPass {
    target: Box<dyn TargetRegAlloc>,
    abi: &'static crate::backend::abi::AbiInfo,
}

impl AbiPrecolorPass {
    pub fn new(
        target: Box<dyn TargetRegAlloc>,
        abi: &'static crate::backend::abi::AbiInfo,
    ) -> Self {
        Self { target, abi }
    }
}

fn mark_op(context: &Context, op_id: tir::OpId, name: &str, value: AttributeValue) {
    let mut attrs = context.get_op(op_id).attributes.clone();
    attrs.push(tir::attributes::NamedAttribute {
        name: name.to_string(),
        value,
    });
    context.set_op_attributes(op_id, attrs);
}

impl Pass for AbiPrecolorPass {
    fn name(&self) -> &'static str {
        "abi-precolor"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        let info = self.target.register_info();
        let blocks = symbol_body_blocks(context, op);
        if blocks.is_empty() {
            return Ok(PreservedAnalyses::all());
        }

        // Argument vregs: the symbol's `arg_regs` attribute carries each argument's
        // register class (assigned by the target's function lowering, e.g. vectors
        // in `VR`, floats in `FPR32`/`FPR64`, everything else in `GPR`). Placement
        // is computed up front in one pure pass, then emitted.
        let mut arg_pins: HashMap<u32, PhysReg> = HashMap::new();
        if let Some(AttributeValue::Array(args)) = op
            .op()
            .attributes
            .iter()
            .find(|attribute| attribute.name == "arg_regs")
            .map(|attribute| &attribute.value)
        {
            let result_address =
                op.op()
                    .attributes
                    .iter()
                    .find_map(|attribute| match &attribute.value {
                        AttributeValue::Register(RegisterAttr::Virtual { id, .. })
                            if attribute.name == "result_address" =>
                        {
                            Some(*id)
                        }
                        _ => None,
                    });
            let plan = plan_arguments(context, &info, self.abi, args, result_address)?;
            let entry = blocks
                .first()
                .and_then(|block| context.get_block(*block).op_ids().first().copied())
                .map(|first| op_ref_in(context, blocks[0], first));
            let entry = || {
                entry.clone().ok_or_else(|| {
                    PassError::InvalidRuleSet(
                        "function with register arguments has no entry operation".to_string(),
                    )
                })
            };
            for &(incoming, pin) in &plan.pins {
                let ty = context.get_value(ValueId::from_number(incoming)).ty();
                let body = context.create_value(ty, None).id().number();
                for &block_id in &blocks {
                    for op_id in context.get_block(block_id).op_ids() {
                        rename_attr(context, op_id, incoming, body, RoleClass::Read);
                    }
                }
                context
                    .replace_value_uses(ValueId::from_number(incoming), ValueId::from_number(body));
                let copy = self.target.emit_copy(context, pin.0, body, incoming);
                let copy_id = copy.id();
                rewriter.insert_op_before(&entry()?, copy.as_ref())?;
                mark_op(context, copy_id, ABI_COPY_ATTR, AttributeValue::Bool(true));
                if let Some(previous) = arg_pins.insert(incoming, pin)
                    && previous != pin
                {
                    return Err(PassError::InvalidRuleSet(format!(
                        "argument vreg {incoming} pinned to conflicting registers {previous:?} and {pin:?}"
                    )));
                }
            }

            // Give stack arguments real definitions before allocation so ordinary
            // spilling can split them. Their final offsets are patched after the
            // frame and callee-save layout are known.
            for &(vreg, class, stack_index) in &plan.stack_args {
                let load = self
                    .target
                    .emit_spill_reload(context, vreg, class, &self.abi.sp, 0);
                let load_id = load.id();
                rewriter.insert_op_before(&entry()?, load.as_ref())?;
                mark_op(
                    context,
                    load_id,
                    ABI_STACK_INDEX_ATTR,
                    AttributeValue::UInt(stack_index as u64),
                );
            }
        }

        // Return operands take consecutive registers within each ABI value class.
        // Each value is copied into a fresh vreg defined in its return register,
        // recorded as a `FixedDef` on the copy.
        for &block_id in &blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let body_op = context.get_op(op_id);
                if !body_op.is::<VirtualReturnOp>() {
                    continue;
                }
                let mut next_slot = HashMap::new();
                for (operand_index, value) in body_op.operands.iter().copied().enumerate() {
                    let vreg = value.number();
                    let class = vreg_class_in(context, &blocks, vreg);
                    let kind = value_kind(context, value);
                    let slot = next_slot.entry(kind).or_insert(0usize);
                    let Some(sequence) =
                        self.abi.rets.iter().find(|sequence| sequence.kind == kind)
                    else {
                        continue;
                    };
                    let Some(&register) = sequence.regs.get(*slot) else {
                        continue;
                    };
                    *slot += 1;
                    let rc = match class {
                        Some(class) if class.file() == register.0.file() => class,
                        _ => register.0,
                    };
                    let ret_pin = (rc, register.1);

                    let ty = context.get_value(value).ty();
                    let outgoing = context.create_value(ty, None).id().number();
                    let copy = self.target.emit_copy(context, rc, outgoing, vreg);
                    let copy_id = copy.id();
                    let op_ref = op_ref_in(context, block_id, op_id);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                    context.set_op_operand(op_id, operand_index, ValueId::from_number(outgoing));
                    let mut attrs = context.get_op(copy_id).attributes.clone();
                    for attr in &mut attrs {
                        if let AttributeValue::Register(RegisterAttr::Virtual { id, .. }) =
                            &attr.value
                            && *id == outgoing
                        {
                            attr.value = AttributeValue::Register(RegisterAttr::FixedDef {
                                id: outgoing,
                                class: rc,
                                index: ret_pin.1,
                            });
                        }
                    }
                    attrs.push(tir::attributes::NamedAttribute {
                        name: ABI_COPY_ATTR.to_string(),
                        value: AttributeValue::Bool(true),
                    });
                    context.set_op_attributes(copy_id, attrs);
                }
            }
        }

        // Record the argument pins where the allocation pass finds them: the
        // pinned `arg_regs` (and `result_address`) entries become `FixedDef`.
        if !arg_pins.is_empty() {
            let mut attrs = op.op().attributes.clone();
            for attr in &mut attrs {
                if matches!(attr.name.as_str(), "arg_regs" | "result_address") {
                    pin_fixed_defs(&mut attr.value, &arg_pins);
                }
            }
            context.set_op_attributes(op.op().id, attrs);
        }

        Ok(PreservedAnalyses::none())
    }
}

/// Rewrite every pinned `Virtual` register entry under an ABI attribute to a
/// `FixedDef` naming its calling-convention register, recursing into argument
/// groups (arrays and dicts).
fn pin_fixed_defs(value: &mut AttributeValue, pins: &HashMap<u32, PhysReg>) {
    match value {
        AttributeValue::Register(RegisterAttr::Virtual { id, .. }) => {
            if let Some(&(class, index)) = pins.get(id) {
                let id = *id;
                *value = AttributeValue::Register(RegisterAttr::FixedDef { id, class, index });
            }
        }
        AttributeValue::Array(items) => {
            for item in items {
                pin_fixed_defs(item, pins);
            }
        }
        AttributeValue::Dict(map) => {
            for item in map.values_mut() {
                pin_fixed_defs(item, pins);
            }
        }
        _ => {}
    }
}

/// Where one argument goes, computed by [`plan_arguments`]: pinned to a
/// calling-convention register, or passed on the stack at `stack_index`.
#[derive(Debug, Default, PartialEq, Eq)]
struct ArgumentPlan {
    pins: Vec<(u32, PhysReg)>,
    stack_args: Vec<(u32, RegClassId, usize)>,
}

/// Compute the placement of every function argument in one pure pass, before
/// any IR mutation: register pins in emission order (the indirect result
/// address first) and stack arguments with their slot indices. Argument groups
/// are atomic — all members land in registers or all on the stack — and the
/// ABI's rollback policy decides whether a spilled group also exhausts the
/// remaining argument registers.
fn plan_arguments(
    context: &Context,
    info: &RegisterInfo,
    abi: &crate::backend::abi::AbiInfo,
    args: &[AttributeValue],
    result_address: Option<u32>,
) -> Result<ArgumentPlan, PassError> {
    let mut plan = ArgumentPlan::default();
    let mut next_slot: HashMap<crate::backend::abi::ValueKind, usize> = HashMap::new();
    let mut next_stack_slot = 0;

    if let Some(id) = result_address {
        let register = abi.indirect_result.ok_or_else(|| {
            PassError::InvalidRuleSet("ABI has no result-address register".to_string())
        })?;
        plan.pins.push((id, register));
        reserve_indirect_result_argument(abi, &mut next_slot);
    }

    for attribute in args {
        let group = match attribute {
            AttributeValue::Array(group) => Some((group, 1)),
            AttributeValue::Dict(group) => {
                let members = match group.get("members") {
                    Some(AttributeValue::Array(members)) => members,
                    _ => {
                        return Err(PassError::InvalidRuleSet(
                            "ABI argument group has no members".to_string(),
                        ));
                    }
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
                Some((members, alignment))
            }
            _ => None,
        };
        let members = if let Some((group, _)) = group {
            group
                .iter()
                .map(|member| {
                    let AttributeValue::Register(RegisterAttr::Virtual { id, class }) = member
                    else {
                        return Err(PassError::InvalidRuleSet(
                            "ABI argument group contains a non-register".to_string(),
                        ));
                    };
                    let class = class
                        .or_else(|| info.default_integer_class(abi))
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(
                                "ABI argument group has no register class".to_string(),
                            )
                        })?;
                    Ok((*id, class, value_kind(context, ValueId::from_number(*id))))
                })
                .collect::<Result<Vec<_>, PassError>>()?
        } else {
            let AttributeValue::Register(RegisterAttr::Virtual { id, class }) = attribute else {
                continue;
            };
            let Some(class) = class.or_else(|| info.default_integer_class(abi)) else {
                continue;
            };
            vec![(*id, class, value_kind(context, ValueId::from_number(*id)))]
        };

        if let Some((_, alignment)) = group {
            // A group is placed atomically: trial-assign every member, commit
            // only if all fit.
            let mut trial_slots = next_slot.clone();
            align_argument_group(
                abi,
                alignment,
                members.iter().map(|&(_, _, kind)| kind),
                &mut trial_slots,
            );
            let pins = if abi.argument_group_fits_register_limit(members.len()) {
                members
                    .iter()
                    .map(|&(_, class, kind)| next_abi_register(abi, class, kind, &mut trial_slots))
                    .collect::<Option<Vec<_>>>()
            } else {
                None
            };
            if let Some(pins) = pins {
                next_slot = trial_slots;
                plan.pins
                    .extend(members.iter().map(|&(id, _, _)| id).zip(pins));
            } else {
                for (id, class, kind) in members {
                    if abi.argument_group_rollback() == GroupRollback::Exhaust {
                        crate::backend::abi::exhaust_argument_registers(abi, kind, &mut next_slot);
                    }
                    plan.stack_args.push((id, class, next_stack_slot));
                    next_stack_slot += 1;
                }
            }
            continue;
        }

        let (id, class, kind) = members[0];
        match next_abi_register(abi, class, kind, &mut next_slot) {
            Some(pin) => plan.pins.push((id, pin)),
            None => {
                plan.stack_args.push((id, class, next_stack_slot));
                next_stack_slot += 1;
            }
        }
    }
    Ok(plan)
}

fn next_abi_register(
    abi: &crate::backend::abi::AbiInfo,
    class: RegClassId,
    mut kind: crate::backend::abi::ValueKind,
    next_slot: &mut HashMap<crate::backend::abi::ValueKind, usize>,
) -> Option<PhysReg> {
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(kind) {
            return None;
        }
        let sequence = match abi.args.iter().find(|sequence| sequence.kind == kind) {
            Some(sequence) => sequence,
            None if kind != crate::backend::abi::ValueKind::Int => {
                kind = crate::backend::abi::ValueKind::Int;
                continue;
            }
            None => return None,
        };
        let slot = next_slot.entry(kind).or_insert(0);
        let register = if class.group_width > 1
            && sequence
                .regs
                .first()
                .is_some_and(|register| register.0.file() == class.file())
        {
            let first = sequence.regs.first().unwrap();
            let last = sequence.regs.last().unwrap();
            let index = first.1 + (*slot as u16 * class.group_width);
            (index <= last.1).then_some((class, index))
        } else {
            sequence.regs.get(*slot).copied()
        };
        if let Some(register) = register {
            *slot += 1;
            return Some(if register.0.file() == class.file() {
                (class, register.1)
            } else {
                register
            });
        }
        match sequence.overflow {
            crate::backend::abi::Overflow::Chain(next) => kind = next,
            crate::backend::abi::Overflow::Stack => return None,
        }
    }
}

/// Sequentialize a parallel copy: a set of `dst <- src` moves (destinations
/// unique) whose sources are all read before any destination is written. Emit a
/// copy whose destination is not still needed as a source first; when only cycles
/// remain, save one destination into a fresh temporary and reroute the reads of it
/// through the temporary, which frees that destination and breaks the cycle. Each
/// tuple carries the register class to copy the destination in; a temporary
/// inherits the class and type of the value it saves.
fn sequence_parallel_copies(
    context: &Context,
    mut copies: Vec<(u32, u32, RegClassId)>,
) -> Vec<(u32, u32, RegClassId)> {
    let mut result = Vec::new();
    while !copies.is_empty() {
        if let Some(i) = copies
            .iter()
            .position(|(dst, _, _)| !copies.iter().any(|(_, src, _)| src == dst))
        {
            result.push(copies.remove(i));
        } else {
            // Only cycles remain: break one by saving its destination.
            let (dst, _, class) = copies[0];
            let ty = context.get_value(ValueId::from_number(dst)).ty();
            let temp = context.create_value(ty, None).id().number();
            result.push((temp, dst, class));
            for (_, src, _) in copies.iter_mut() {
                if *src == dst {
                    *src = temp;
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::regalloc::{RegClassInfo, RegisterInfo, RegisterView};
    use std::collections::HashMap;
    use tir::builtin::IntegerType;

    fn three_reg_info() -> RegisterInfo {
        RegisterInfo {
            classes: &[RegClassInfo {
                name: "R",
                file: "R",
                registers: &[0, 1, 2, 3],
                group_width: 1,
                view: RegisterView {
                    bit_offset: 0,
                    merge: false,
                },
            }],
        }
    }

    fn r_id() -> RegClassId {
        three_reg_info().class("R").unwrap()
    }

    use crate::backend::abi::{
        AbiInfo, ArgumentGroupAlignment, ArgumentGroupPolicy, ClassifierKind, GroupRollback,
        Overflow, PassSeq, SaveStyle, StackLayout, ValueKind,
    };
    use std::collections::BTreeMap;

    fn test_abi(arg_regs: &[u16]) -> &'static AbiInfo {
        let class = r_id();
        let caller_saved = Box::leak(
            (0..4)
                .map(|index| (class, index))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let int_args = Box::leak(
            arg_regs
                .iter()
                .map(|&index| (class, index))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        Box::leak(Box::new(AbiInfo {
            name: "test",
            stack: StackLayout {
                align: 8,
                slot_size: 8,
                red_zone: 0,
                grows_down: true,
                save_style: SaveStyle::FrameSlots,
            },
            sp: (class, 1000),
            ra: None,
            fp: None,
            indirect_result: None,
            argument_group_alignment: None,
            argument_group_policy: None,
            args: Box::leak(
                vec![PassSeq {
                    kind: ValueKind::Int,
                    regs: int_args,
                    overflow: Overflow::Stack,
                }]
                .into_boxed_slice(),
            ),
            rets: &[],
            callee_saved: &[],
            caller_saved,
            reserved: &[],
            classifier: ClassifierKind::Sysv,
        }))
    }

    fn int_vreg(context: &Context) -> (u32, AttributeValue) {
        let ty = IntegerType::new(context, 64);
        let id = context.create_value(ty, None).id().number();
        (
            id,
            AttributeValue::Register(RegisterAttr::Virtual {
                id,
                class: Some(r_id()),
            }),
        )
    }

    fn plan(
        context: &Context,
        abi: &'static AbiInfo,
        args: Vec<AttributeValue>,
        result_address: Option<u32>,
    ) -> ArgumentPlan {
        plan_arguments(context, &three_reg_info(), abi, &args, result_address)
            .expect("argument planning should succeed")
    }

    #[test]
    fn register_args_take_sequential_slots() {
        let context = Context::with_default_dialects();
        let (a, arg_a) = int_vreg(&context);
        let (b, arg_b) = int_vreg(&context);

        let plan = plan(&context, test_abi(&[0, 1]), vec![arg_a, arg_b], None);
        assert_eq!(plan.pins, vec![(a, (r_id(), 0)), (b, (r_id(), 1))]);
        assert!(plan.stack_args.is_empty());
    }

    #[test]
    fn group_in_registers_stays_atomic() {
        let context = Context::with_default_dialects();
        let (a, arg_a) = int_vreg(&context);
        let (b, arg_b) = int_vreg(&context);
        let group = AttributeValue::Array(vec![arg_a, arg_b]);

        let plan = plan(&context, test_abi(&[0, 1]), vec![group], None);
        assert_eq!(plan.pins, vec![(a, (r_id(), 0)), (b, (r_id(), 1))]);
        assert!(plan.stack_args.is_empty());
    }

    #[test]
    fn oversized_group_spills_atomically_and_exhausts() {
        let context = Context::with_default_dialects();
        let (c, arg_c) = int_vreg(&context);
        let (d0, arg_d0) = int_vreg(&context);
        let (d1, arg_d1) = int_vreg(&context);
        let (d2, arg_d2) = int_vreg(&context);
        let triple = AttributeValue::Array(vec![arg_d0, arg_d1, arg_d2]);

        // Two registers, a three-member group: every member goes to the stack,
        // and the default Exhaust policy burns the register sequence so the
        // trailing argument goes to the stack too.
        let plan = plan(&context, test_abi(&[0, 1]), vec![triple, arg_c], None);
        assert!(plan.pins.is_empty());
        assert_eq!(
            plan.stack_args,
            vec![
                (d0, r_id(), 0),
                (d1, r_id(), 1),
                (d2, r_id(), 2),
                (c, r_id(), 3),
            ]
        );
    }

    #[test]
    fn preserve_policy_leaves_registers_for_later_args() {
        let context = Context::with_default_dialects();
        let (a, arg_a) = int_vreg(&context);
        let (b, arg_b) = int_vreg(&context);
        let (c, arg_c) = int_vreg(&context);
        let (d, arg_d) = int_vreg(&context);
        let group = AttributeValue::Array(vec![arg_a, arg_b, arg_c]);

        let mut abi = *test_abi(&[0, 1]);
        abi.argument_group_policy = Some(ArgumentGroupPolicy {
            register_limit: None,
            rollback: GroupRollback::Preserve,
        });
        let abi: &'static AbiInfo = Box::leak(Box::new(abi));

        let plan = plan(&context, abi, vec![group, arg_d], None);
        assert_eq!(plan.pins, vec![(d, (r_id(), 0))]);
        assert_eq!(
            plan.stack_args,
            vec![(a, r_id(), 0), (b, r_id(), 1), (c, r_id(), 2)]
        );
    }

    #[test]
    fn aligned_group_starts_at_an_aligned_slot() {
        let context = Context::with_default_dialects();
        let (a, arg_a) = int_vreg(&context);
        let (b, arg_b) = int_vreg(&context);
        let (c, arg_c) = int_vreg(&context);
        let group = AttributeValue::Dict(BTreeMap::from([
            (
                "members".to_string(),
                AttributeValue::Array(vec![arg_b, arg_c]),
            ),
            ("alignment".to_string(), AttributeValue::UInt(16)),
        ]));

        let mut abi = *test_abi(&[0, 1, 2, 3]);
        abi.argument_group_alignment = Some(ArgumentGroupAlignment {
            kind: ValueKind::Int,
            minimum_source_alignment: 16,
            register_multiple: 2,
        });
        let abi: &'static AbiInfo = Box::leak(Box::new(abi));

        // After one plain argument takes slot 0, the 16-byte-aligned pair must
        // start at slot 2, not slot 1.
        let plan = plan(&context, abi, vec![arg_a, group], None);
        assert_eq!(
            plan.pins,
            vec![(a, (r_id(), 0)), (b, (r_id(), 2)), (c, (r_id(), 3))]
        );
        assert!(plan.stack_args.is_empty());
    }

    #[test]
    fn indirect_result_reserves_its_register() {
        let context = Context::with_default_dialects();
        let (a, arg_a) = int_vreg(&context);
        let (b, arg_b) = int_vreg(&context);

        let mut abi = *test_abi(&[0, 1]);
        abi.indirect_result = Some((r_id(), 0));
        let abi: &'static AbiInfo = Box::leak(Box::new(abi));

        let plan = plan(&context, abi, vec![arg_a, arg_b], Some(9));
        assert_eq!(plan.pins, vec![(9, (r_id(), 0)), (a, (r_id(), 1))]);
        assert_eq!(plan.stack_args, vec![(b, r_id(), 0)]);
    }

    // Sequentializing a parallel copy that swaps two registers (a loop edge
    // forwarding `^loop(%b, %a)` from within `^loop(%a, %b)`) needs one temporary
    // to break the cycle: naive `a<-b; b<-a` would lose a's value.
    #[test]
    fn parallel_copy_swap_uses_a_temporary() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None).id().number();
        let b = context.create_value(ty, None).id().number();

        let seq = sequence_parallel_copies(&context, vec![(a, b, r_id()), (b, a, r_id())]);
        assert_eq!(seq.len(), 3, "a swap needs a saving temporary");

        // Simulate: each register starts holding its own original value; applying
        // the emitted copies in order must swap a and b.
        let mut regs: HashMap<u32, u32> = HashMap::new();
        regs.insert(a, a);
        regs.insert(b, b);
        for (dst, src, _) in &seq {
            let v = *regs.get(src).expect("source read before it is written");
            regs.insert(*dst, v);
        }
        assert_eq!(regs[&a], b, "a receives b's original value");
        assert_eq!(regs[&b], a, "b receives a's original value");
    }

    // A non-cyclic parallel copy is ordered so a value is never overwritten before
    // it is read: `c<-a` must precede `a<-b`.
    #[test]
    fn parallel_copy_orders_reads_before_writes() {
        let context = Context::with_default_dialects();
        let ty = IntegerType::new(&context, 64);
        let a = context.create_value(ty, None).id().number();
        let b = context.create_value(ty, None).id().number();
        let c = context.create_value(ty, None).id().number();

        let seq = sequence_parallel_copies(&context, vec![(a, b, r_id()), (c, a, r_id())]);
        assert_eq!(seq.len(), 2, "no cycle, no temporary");
        let mut regs: HashMap<u32, u32> = HashMap::new();
        regs.insert(a, a);
        regs.insert(b, b);
        regs.insert(c, c);
        for (dst, src, _) in &seq {
            regs.insert(*dst, regs[src]);
        }
        assert_eq!(regs[&a], b);
        assert_eq!(regs[&c], a, "c must capture a's original value, not b's");
    }
}
