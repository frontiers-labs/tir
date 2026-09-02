//! Pre-allocation lowerings: target-independent IR rewrites that must run after
//! instruction selection and before register allocation. Each is a separate
//! pass so a fix in one cannot perturb the others, and each is exercisable in
//! isolation; [`regalloc_stage_for`] composes them with the allocation pass.

use std::collections::HashMap;

use tir::attributes::AttributeValue;
use tir::{AnalysisManager, Context, OperationRef, Pass, PassError, PassTarget, Rewriter, ValueId};

use crate::backend::abi::{
    GroupRollback, align_argument_group, reserve_indirect_result_argument, value_kind,
};
use crate::backend::liveness::PhysReg;
use crate::backend::regalloc::{
    RegClassId, RegisterAllocationPass, RegisterInfo, TargetRegAlloc, op_ref_in, symbol_body_blocks,
};
use crate::backend::registers::{RegAssignment, RegSlot, fresh_reg, value_class};
use crate::backend::{SymbolOp, VirtualBranchOp, VirtualReturnOp, reg_slots};

/// A fresh value of `class`: the type a machine instruction reads it through.
/// Require the slot of `op` holding `value` to be `register`. A copy whose
/// slots do not name the value cannot carry the constraint, and the ABI
/// placement would be silently lost.
fn pin_on(
    context: &Context,
    op: tir::OpId,
    value: ValueId,
    register: PhysReg,
) -> Result<(), PassError> {
    let op = context.get_op(op);
    let slot = reg_slots(&op)
        .into_iter()
        .find(|slot| slot.slot == RegSlot::Value(value))
        .ok_or_else(|| {
            PassError::InvalidRuleSet(format!(
                "{} has no register slot holding %{}",
                op.name().as_str(),
                value.number()
            ))
        })?;
    crate::backend::pin_slot(context, &op, slot.port.name, register);
    Ok(())
}

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

/// Lower tied (two-address) operands ahead of allocation. An instruction whose
/// behavior reads its destination (the x86 `dst = dst + src`) declares its
/// source slot tied to its destination slot: it defines `%r` but must read `%x`
/// through the same register. Insert `copy %r <- %x` ahead of the op and point
/// the tied slot at `%r`, leaving a plain read-modify-write of `%r` that
/// liveness and coloring already model. The copy carries
/// [`COALESCABLE_COPY_ATTR`]: the tie is a request for one register at both
/// ends, so allocation reads it as an affinity and erases the copy where it is
/// granted.
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
    ) -> Result<(), PassError> {
        for block_id in symbol_body_blocks(context, op) {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                let slots = reg_slots(&op);
                let mut ties = Vec::new();
                for slot in &slots {
                    let (Some(tied_to), RegSlot::Value(src), Some(position)) =
                        (slot.port.tied_to, slot.slot, slot.position)
                    else {
                        continue;
                    };
                    let Some(destination) = slots.iter().find(|s| s.port.name == tied_to) else {
                        return Err(PassError::InvalidRuleSet(format!(
                            "tied operand '{}' names no destination",
                            slot.port.name
                        )));
                    };
                    let RegSlot::Value(dst) = destination.slot else {
                        continue;
                    };
                    let class = value_class(context, dst).ok_or_else(|| {
                        PassError::InvalidRuleSet(format!(
                            "tied operand '{}' has no register class",
                            slot.port.name
                        ))
                    })?;
                    ties.push((position, dst, src, class));
                }
                if ties.is_empty() {
                    continue;
                }
                let op_ref = op_ref_in(context, block_id, op_id);
                for &(position, dst, src, class) in &ties {
                    let copy = self.target.emit_copy(
                        context,
                        class,
                        RegSlot::Value(dst),
                        RegSlot::Value(src),
                    );
                    let copy_id = copy.id();
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                    mark_op(
                        context,
                        copy_id,
                        COALESCABLE_COPY_ATTR,
                        AttributeValue::Bool(true),
                    );
                    context.set_op_operand(op_id, position, dst);
                }
            }
        }
        Ok(())
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
/// swaps two parameters). Each copy carries [`COALESCABLE_COPY_ATTR`]: parameter and
/// forwarded value want one register, and a copy whose ends coalesce is a
/// self-move. Sequentialization stays correct because only a copy the allocator
/// gave one register at both ends is erased, and a value still needed as a
/// source interferes with the destination that would overwrite it.
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
    ) -> Result<(), PassError> {
        let blocks = symbol_body_blocks(context, op);
        let info = self.target.register_info();
        for &block_id in &blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                if !op.is::<VirtualBranchOp>() || op.operands().is_empty() {
                    continue;
                }
                let args: Vec<ValueId> = op.operands().to_vec();
                let Some(AttributeValue::Block(dest)) = op.attr("dest") else {
                    return Err(PassError::InvalidRuleSet(
                        "vbr with block arguments is missing its 'dest' target".to_string(),
                    ));
                };
                let params: Vec<ValueId> = context
                    .get_block(dest)
                    .arguments()
                    .iter()
                    .map(|v| v.id())
                    .collect();
                if params.len() != args.len() {
                    return Err(PassError::InvalidRuleSet(format!(
                        "branch forwards {} argument(s) to a block with {} parameter(s)",
                        args.len(),
                        params.len()
                    )));
                }

                // Pair each parameter with its forwarded value and the register
                // class to copy it in. A copy moves the whole value, so the class
                // follows the parameter's type — the only thing that says how
                // much there is to move. An instruction naming either endpoint
                // may pin a narrower class (x86 reads the low byte of a 16-bit
                // value through `GPR8`), and copying through that class would
                // leave the rest of the parameter holding whatever the
                // destination carried before; such a class only decides the copy
                // when the type names no file at all.
                let state = tir::builtin::StateType::new(context);
                let mut pairs: Vec<(ValueId, ValueId, RegClassId)> = Vec::new();
                for (&param, &arg) in params.iter().zip(args.iter()) {
                    if param == arg {
                        continue;
                    }
                    // A `!state` port is memory order, not a register: nothing
                    // moves across the edge, and the parameter simply names the
                    // chain the join is entered on.
                    if context.get_value(param).ty() == state {
                        continue;
                    }
                    let class = info
                        .default_class(self.abi, value_kind(context, self.abi, param))
                        .or_else(|| value_class(context, arg))
                        .or_else(|| value_class(context, param))
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(format!(
                                "block argument %{} has no register class",
                                arg.number()
                            ))
                        })?;
                    // A forwarded value no instruction read — a constant or
                    // allocation a target pass materializes later — meets its
                    // first machine instruction here: the copy.
                    crate::backend::retype_untyped(context, arg, class);
                    crate::backend::retype_untyped(context, param, class);
                    pairs.push((param, arg, class));
                }

                let op_ref = op_ref_in(context, block_id, op_id);
                for (dst, src, class) in sequence_parallel_copies(context, pairs) {
                    let copy = self.target.emit_copy(
                        context,
                        class,
                        RegSlot::Value(dst),
                        RegSlot::Value(src),
                    );
                    let copy_id = copy.id();
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                    mark_op(
                        context,
                        copy_id,
                        COALESCABLE_COPY_ATTR,
                        AttributeValue::Bool(true),
                    );
                }
                context.set_op_operands(op_id, Vec::new());
            }
        }
        Ok(())
    }
}

/// Marks a copy whose two ends want the same register — a two-address tie, an
/// ABI boundary or a block-argument edge here, a fixed-register instruction
/// operand in [`crate::backend::regalloc`]. Register allocation reads the
/// endpoints as a coalescing affinity, erases the copy when both land in one
/// register, and strips the mark.
pub(crate) const COALESCABLE_COPY_ATTR: &str = "coalescable_copy";

/// Marks the placeholder load of an incoming stack argument and carries its
/// stack slot index. Register allocation patches the real offset in once the
/// frame layout is known.
pub(crate) const ABI_STACK_INDEX_ATTR: &str = "abi_stack_index";

/// Compute calling-convention pre-coloring at function entry and returns,
/// recording every constraint in the IR itself: register arguments pin their
/// `arg_regs` value in the symbol's [`PINS_ATTR`] map, returns define their
/// pinned outgoing register through a copy marked [`COALESCABLE_COPY_ATTR`],
/// and stack arguments get a placeholder load marked [`ABI_STACK_INDEX_ATTR`].
/// Register arguments are also copied from their pinned entry value into a free
/// body value: an ABI register is a point constraint at the function boundary,
/// so it must not pin an entire live range across calls.
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

pub(crate) fn mark_op(context: &Context, op_id: tir::OpId, name: &str, value: AttributeValue) {
    let mut attrs = context.get_op(op_id).attributes().to_vec();
    attrs.push(context.named_attribute(name, value));
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
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let blocks = symbol_body_blocks(context, op);
        if blocks.is_empty() {
            return Ok(());
        }

        // Argument values: the symbol's `arg_regs` attribute names them, and
        // each carries the class its function lowering gave it (vectors in
        // `VR`, floats in `FPR32`/`FPR64`, everything else in `GPR`). Placement
        // is computed up front in one pure pass, then emitted.
        let mut arg_pins = RegAssignment::default();
        if let Some(AttributeValue::Array(args)) = op.op().attr("arg_regs") {
            let result_address = match op.op().attr("result_address") {
                Some(AttributeValue::Value(value)) => Some(value),
                _ => None,
            };
            let plan = plan_arguments(context, &info, self.abi, &args, result_address)?;
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
                let body = fresh_reg(context, pin.0);
                // Rename first: the copy is the one op that must keep reading
                // the pinned value, so it is built after the rename is done.
                context.replace_value_uses(incoming, body);
                let copy = self.target.emit_copy(
                    context,
                    pin.0,
                    RegSlot::Value(body),
                    RegSlot::Value(incoming),
                );
                let copy_id = copy.id();
                rewriter.insert_op_before(&entry()?, copy.as_ref())?;
                mark_op(
                    context,
                    copy_id,
                    COALESCABLE_COPY_ATTR,
                    AttributeValue::Bool(true),
                );
                if let Some(previous) = arg_pins.get(incoming)
                    && previous != pin
                {
                    return Err(PassError::InvalidRuleSet(format!(
                        "argument %{} is pinned to conflicting registers {previous:?} and {pin:?}",
                        incoming.number()
                    )));
                }
                arg_pins.insert(incoming, pin);
            }

            // Give stack arguments real definitions before allocation so ordinary
            // spilling can split them. Their final offsets are patched after the
            // frame and callee-save layout are known.
            for &(value, class, stack_index) in &plan.stack_args {
                let load = self
                    .target
                    .emit_spill_reload(context, value, class, &self.abi.sp, 0);
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
        // Each value is copied into a fresh value pinned to its return register.
        for &block_id in &blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let body_op = context.get_op(op_id);
                if !body_op.is::<VirtualReturnOp>() {
                    continue;
                }
                let mut next_slot = HashMap::new();
                for (operand_index, value) in body_op.operands().iter().copied().enumerate() {
                    let class = value_class(context, value);
                    let kind = value_kind(context, self.abi, value);
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

                    let outgoing = fresh_reg(context, rc);
                    let copy = self.target.emit_copy(
                        context,
                        rc,
                        RegSlot::Value(outgoing),
                        RegSlot::Value(value),
                    );
                    let copy_id = copy.id();
                    let op_ref = op_ref_in(context, block_id, op_id);
                    rewriter.insert_op_before(&op_ref, copy.as_ref())?;
                    context.set_op_operand(op_id, operand_index, outgoing);
                    pin_on(context, copy_id, outgoing, (rc, register.1))?;
                    mark_op(
                        context,
                        copy_id,
                        COALESCABLE_COPY_ATTR,
                        AttributeValue::Bool(true),
                    );
                }
            }
        }

        // Record the argument pins where the allocation pass finds them.
        if !arg_pins.is_empty() {
            mark_op(
                context,
                op.op().id,
                crate::backend::ARG_PINS_ATTR,
                arg_pins.to_attribute(),
            );
        }

        Ok(())
    }
}

/// Where one argument goes, computed by [`plan_arguments`]: pinned to a
/// calling-convention register, or passed on the stack at `stack_index`.
#[derive(Debug, Default, PartialEq, Eq)]
struct ArgumentPlan {
    pins: Vec<(ValueId, PhysReg)>,
    stack_args: Vec<(ValueId, RegClassId, usize)>,
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
    result_address: Option<ValueId>,
) -> Result<ArgumentPlan, PassError> {
    let mut plan = ArgumentPlan::default();
    let mut next_slot: HashMap<crate::backend::abi::ValueKind, usize> = HashMap::new();
    let mut next_stack_slot = 0;

    if let Some(value) = result_address {
        let register = abi.indirect_result.ok_or_else(|| {
            PassError::InvalidRuleSet("ABI has no result-address register".to_string())
        })?;
        plan.pins.push((value, register));
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
                    let AttributeValue::Value(value) = member else {
                        return Err(PassError::InvalidRuleSet(
                            "ABI argument group contains a non-register".to_string(),
                        ));
                    };
                    let class = value_class(context, *value)
                        .or_else(|| info.default_integer_class(abi))
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(
                                "ABI argument group has no register class".to_string(),
                            )
                        })?;
                    Ok((*value, class, value_kind(context, abi, *value)))
                })
                .collect::<Result<Vec<_>, PassError>>()?
        } else {
            let AttributeValue::Value(value) = attribute else {
                continue;
            };
            let Some(class) =
                value_class(context, *value).or_else(|| info.default_integer_class(abi))
            else {
                continue;
            };
            vec![(*value, class, value_kind(context, abi, *value))]
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
                    .extend(members.iter().map(|&(value, _, _)| value).zip(pins));
            } else {
                for (value, class, kind) in members {
                    if abi.argument_group_rollback() == GroupRollback::Exhaust {
                        crate::backend::abi::exhaust_argument_registers(abi, kind, &mut next_slot);
                    }
                    plan.stack_args.push((value, class, next_stack_slot));
                    next_stack_slot += 1;
                }
            }
            continue;
        }

        let (value, class, kind) = members[0];
        match next_abi_register(abi, class, kind, &mut next_slot) {
            Some(pin) => plan.pins.push((value, pin)),
            None => {
                plan.stack_args.push((value, class, next_stack_slot));
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
    mut copies: Vec<(ValueId, ValueId, RegClassId)>,
) -> Vec<(ValueId, ValueId, RegClassId)> {
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
            let temp = fresh_reg(context, class);
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
