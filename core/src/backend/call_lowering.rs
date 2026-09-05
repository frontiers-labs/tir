use std::collections::{HashMap, HashSet};

use tir::Symbol;
use tir::attributes::AttributeValue;
use tir::builtin::PtrToFnOp;
use tir::builtin::{
    MakeTupleOp, MakeTupleOpBuilder, TupleGetOp, TupleGetOpBuilder, TupleType, UnitType,
};
use tir::func::{CallOp, ReturnOp};
use tir::{Context, OpId, Operand, Operation, OperationRef, PassError, Rewriter, ValueId};

use crate::backend::abi::{
    AbiInfo, GroupRollback, Overflow, ValueKind, align_argument_group, exhaust_argument_registers,
    next_argument_register, next_return_register, reserve_indirect_result_argument, type_kind,
    value_kind,
};
use crate::backend::liveness::PhysReg;
use crate::backend::regalloc::RegClassId;
use crate::backend::registers::RegSlot;
use crate::backend::registers::fresh_reg;

pub trait CallEmitter: Send + Sync {
    /// A register-to-register move. Either end is a value the copy defines or
    /// reads, or a physical register the calling convention names.
    fn copy(&self, context: &Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation>;

    fn stack_arg_store(
        &self,
        _context: &Context,
        _abi: &AbiInfo,
        _value: ValueId,
        _class: RegClassId,
        _offset: i64,
    ) -> Result<Box<dyn Operation>, PassError> {
        Err(PassError::InvalidRuleSet(
            "stack-passed call arguments are not supported by this target".to_string(),
        ))
    }

    fn call_prefix(
        &self,
        _context: &Context,
        _abi: &AbiInfo,
        _outgoing_size: u32,
        _vector_register_args: u8,
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    fn call_suffix(
        &self,
        _context: &Context,
        _abi: &AbiInfo,
        _outgoing_size: u32,
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }
}

pub struct CallLowering {
    abi: &'static AbiInfo,
    emitter: Box<dyn CallEmitter>,
    prepared_functions: HashSet<OpId>,
    tuple_argument_elements: HashMap<(OpId, usize), Vec<ValueId>>,
}

impl CallLowering {
    pub fn new(abi: &'static AbiInfo, emitter: Box<dyn CallEmitter>) -> Self {
        Self {
            abi,
            emitter,
            prepared_functions: HashSet::new(),
            tuple_argument_elements: HashMap::new(),
        }
    }

    /// Drop the scratch of the function just finished. Both maps are keyed by
    /// op id and describe one function, so they must not outlive it: a later
    /// function's op reusing an id would otherwise read the old one's answer.
    pub fn reset(&mut self) {
        self.prepared_functions.clear();
        self.tuple_argument_elements.clear();
    }

    pub fn prepare_function(
        &mut self,
        context: &Context,
        function: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        if !self.prepared_functions.insert(function.op().id) {
            return Ok(());
        }
        for region_id in function.op().regions() {
            let region = context.get_region(region_id);
            for block in region.iter(context.clone()) {
                for op_id in block.op_ids() {
                    let instance = context.get_op(op_id);
                    if let Some(ret) = instance.clone().as_op::<ReturnOp>()
                        && let Some(value) = ret.returned_value()
                    {
                        let ty = context.get_value(value).ty();
                        let data = context.get_type_data(ty);
                        if let Some(tuple) =
                            (data.as_ref() as &dyn std::any::Any).downcast_ref::<TupleType>()
                        {
                            let assembled =
                                context
                                    .get_value(value)
                                    .defining_op()
                                    .is_some_and(|definition| {
                                        context.get_op(definition).is::<MakeTupleOp>()
                                    });
                            if !assembled {
                                let return_ref = OperationRef::new(instance.clone());
                                let elements = insert_tuple_extractions(
                                    context,
                                    rewriter,
                                    &return_ref,
                                    value,
                                    tuple,
                                )?;
                                let make_tuple = MakeTupleOpBuilder::new(context)
                                    .elements(elements)
                                    .result_type(ty)
                                    .build();
                                let tuple_value = make_tuple.result();
                                rewriter.insert_op_before(&return_ref, &make_tuple)?;
                                let replacement =
                                    tir::func::ops::r#return(context, Operand::from(tuple_value))
                                        .build();
                                rewriter.replace_op(&return_ref, &replacement)?;
                            }
                        }
                        continue;
                    }
                    let Some(call) = instance.clone().as_op::<CallOp>() else {
                        continue;
                    };
                    let args = call.args();
                    let call_ref = OperationRef::new(instance);
                    for (argument_index, argument) in args.into_iter().enumerate() {
                        let ty = context.get_type_data(context.get_value(argument).ty());
                        let Some(tuple) =
                            (ty.as_ref() as &dyn std::any::Any).downcast_ref::<TupleType>()
                        else {
                            continue;
                        };
                        let assembled =
                            context
                                .get_value(argument)
                                .defining_op()
                                .is_some_and(|definition| {
                                    context.get_op(definition).is::<MakeTupleOp>()
                                });
                        if assembled {
                            continue;
                        }
                        let elements = insert_tuple_extractions(
                            context, rewriter, &call_ref, argument, tuple,
                        )?;
                        self.tuple_argument_elements
                            .insert((op_id, argument_index), elements);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn lower(
        &mut self,
        context: &Context,
        op: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<bool, PassError> {
        let Some(call) = op.as_op::<CallOp>() else {
            return Ok(false);
        };
        let (callee, mut args, result, has_result_address, mut argument_alignments) = (
            resolve_callee(context, &call),
            call.args(),
            call.result(),
            call.has_result_address(),
            call.argument_alignments(),
        );
        if argument_alignments.is_empty() {
            argument_alignments.resize(args.len(), 1);
        } else if argument_alignments.len() != args.len() {
            return Err(PassError::InvalidRuleSet(
                "call argument alignment count does not match its arguments".to_string(),
            ));
        }
        let result_address = if has_result_address {
            if args.is_empty() {
                return Err(PassError::InvalidRuleSet(
                    "result-address call has no destination argument".to_string(),
                ));
            }
            argument_alignments.remove(0);
            Some(args.remove(0))
        } else {
            None
        };
        let argument_offset = usize::from(result_address.is_some());

        let (lowered_arguments, tuple_arguments) =
            self.flatten_arguments(context, op, args, argument_alignments, argument_offset)?;

        let (argument_values, argument_locations, outgoing_size) =
            self.assign_argument_locations(context, lowered_arguments, result_address.is_some())?;

        let indirect_class = self
            .abi
            .args
            .iter()
            .find(|sequence| sequence.kind == ValueKind::Int)
            .and_then(|sequence| sequence.regs.first())
            .map(|register| register.0)
            .ok_or_else(|| {
                PassError::InvalidRuleSet("ABI has no integer argument registers".to_string())
            })?;

        // An argument value must not be pinned for its whole live range, so the
        // call reads a copy of it made right here.
        let detach = |rewriter: &mut Rewriter, value: ValueId, class| {
            crate::backend::retype_untyped(context, value, class);
            let fresh = fresh_reg(context, class);
            let copy = self
                .emitter
                .copy(context, RegSlot::Value(fresh), RegSlot::Value(value));
            rewriter.insert_op_before(op, copy.as_ref()).map(|()| fresh)
        };

        let fresh_callee = match callee {
            Callee::Direct(_) => None,
            Callee::Indirect(value) => Some(detach(rewriter, value, indirect_class)?),
        };
        let fresh_result_address = result_address
            .map(|value| {
                let register = self.abi.indirect_result.ok_or_else(|| {
                    PassError::InvalidRuleSet("ABI has no result-address register".to_string())
                })?;
                detach(rewriter, value, register.0).map(|fresh| (fresh, register))
            })
            .transpose()?;
        let mut fresh_args = Vec::with_capacity(argument_values.len());
        for (&arg, location) in argument_values.iter().zip(&argument_locations) {
            fresh_args.push(detach(rewriter, arg, location.class())?);
        }

        let vector_register_args = argument_values
            .iter()
            .zip(&argument_locations)
            .filter(|&(value, location)| {
                matches!(location, ArgumentLocation::Register(_))
                    && value_kind(context, self.abi, *value) == ValueKind::Float
            })
            .count() as u8;
        for prefix in
            self.emitter
                .call_prefix(context, self.abi, outgoing_size, vector_register_args)
        {
            rewriter.insert_op_before(op, prefix.as_ref())?;
        }

        let saved_ra = if let Some(ra) = self.abi.ra {
            let saved = fresh_reg(context, ra.0);
            let copy = self
                .emitter
                .copy(context, RegSlot::Value(saved), RegSlot::Phys(ra));
            rewriter.insert_op_before(op, copy.as_ref())?;
            Some((saved, ra))
        } else {
            None
        };

        // The memory the call observes. An argument the convention places on the
        // stack is written into memory the callee reads, so those stores go on
        // this very chain: the call takes what the last of them left, and
        // nothing may put the call ahead of one.
        let mut observed = call.state_operand();
        for (&fresh, location) in fresh_args.iter().zip(&argument_locations) {
            match *location {
                ArgumentLocation::Register(register) => {
                    let copy =
                        self.emitter
                            .copy(context, RegSlot::Phys(register), RegSlot::Value(fresh));
                    rewriter.insert_op_before(op, copy.as_ref())?;
                }
                ArgumentLocation::Stack { class, offset } => {
                    let store = self
                        .emitter
                        .stack_arg_store(context, self.abi, fresh, class, offset)?;
                    rewriter.insert_op_before(op, store.as_ref())?;
                    observed = observed
                        .map(|state| tir::dependency::put_on_chain(context, store.as_ref(), state));
                }
            }
        }
        if let Some((fresh, register)) = fresh_result_address {
            let copy = self
                .emitter
                .copy(context, RegSlot::Phys(register), RegSlot::Value(fresh));
            rewriter.insert_op_before(op, copy.as_ref())?;
        }

        // What the call reads: the registers the convention placed the arguments
        // in, the result address where there is one, and the stack pointer it
        // pushes the return address on. None of them is an operand — a
        // placement is not a value — so without saying so nothing keeps the copy
        // that made it alive to the call, and nothing keeps the frame's own
        // adjustment on the right side of it.
        let uses = AttributeValue::Array(
            argument_locations
                .iter()
                .filter_map(|location| match location {
                    ArgumentLocation::Register(register) => Some(*register),
                    ArgumentLocation::Stack { .. } => None,
                })
                .chain(fresh_result_address.map(|(_, register)| register))
                .chain(std::iter::once(self.abi.sp))
                .map(crate::backend::phys_attr)
                .collect::<Vec<_>>()
                .into(),
        );
        let clobbers = AttributeValue::Array(
            self.abi
                .caller_saved
                .iter()
                .copied()
                .map(crate::backend::phys_attr)
                .collect::<Vec<_>>()
                .into(),
        );
        // The call runs a function, so the memory it observes and the one it
        // leaves behind are the chain the mid-end put it on: the virtual call
        // takes both ports over from `func.call`.
        let published = call.state_result();
        let call: Box<dyn Operation> = match callee {
            Callee::Direct(name) => {
                let mut builder = super::VirtualCallOpBuilder::new(context)
                    .attr("callee", AttributeValue::Str(name.into()))
                    .outgoing_stack_size(u64::from(outgoing_size))
                    .attr("clobbers", clobbers)
                    .attr("uses", uses);
                if let Some(observed) = observed {
                    builder = builder.dep_operand(observed).dep_result();
                }
                Box::new(builder.build())
            }
            Callee::Indirect(_) => {
                let mut builder = super::VirtualIndirectCallOpBuilder::new(context)
                    .callee(fresh_callee.expect("indirect callee was detached"))
                    .outgoing_stack_size(u64::from(outgoing_size))
                    .attr("clobbers", clobbers)
                    .attr("uses", uses);
                if let Some(observed) = observed {
                    builder = builder.dep_operand(observed).dep_result();
                }
                Box::new(builder.build())
            }
        };
        if let (Some(published), Some(new)) = (
            published,
            context.get_op(call.id()).dep_results().first().copied(),
        ) {
            context.replace_value_uses(published, new);
        }
        rewriter.insert_op_before(op, call.as_ref())?;
        for suffix in self.emitter.call_suffix(context, self.abi, outgoing_size) {
            rewriter.insert_op_before(op, suffix.as_ref())?;
        }

        let restore = saved_ra.map(|(saved, ra)| {
            self.emitter
                .copy(context, RegSlot::Phys(ra), RegSlot::Value(saved))
        });
        if let Some(restore) = &restore {
            rewriter.insert_op_before(op, restore.as_ref())?;
        }

        if context.get_value(result).ty() == UnitType::new(context) {
            rewriter.erase_op(op)?;
            erase_dead_tuple_arguments(context, rewriter, &tuple_arguments)?;
            return Ok(true);
        }

        let result_type = context.get_type_data(context.get_value(result).ty());
        if let Some(tuple) =
            (result_type.as_ref() as &dyn std::any::Any).downcast_ref::<TupleType>()
        {
            self.lower_tuple_result(context, op, rewriter, result, tuple, &tuple_arguments)?;
            return Ok(true);
        }

        let kind = value_kind(context, self.abi, result);
        let return_reg = next_return_register(self.abi, kind, &mut HashMap::new())
            .ok_or_else(|| PassError::InvalidRuleSet("ABI has no return register".to_string()))?;
        // The call op is erased below and takes its result with it, so the copy
        // defines a register value of its own. The rewiring is explicit: the
        // call publishes a state as well as a value, so the shapes of the two
        // ops do not line up for [`Rewriter::replace_op`] to do it.
        let returned = fresh_reg(context, return_reg.0);
        let copy = self
            .emitter
            .copy(context, RegSlot::Value(returned), RegSlot::Phys(return_reg));
        context.replace_value_uses(result, returned);
        rewriter.replace_op(op, copy.as_ref())?;
        erase_dead_tuple_arguments(context, rewriter, &tuple_arguments)?;
        Ok(true)
    }

    /// Expand every tuple argument into the scalars the convention passes,
    /// and name the constructions that lowering may leave dead.
    fn flatten_arguments(
        &self,
        context: &Context,
        op: &OperationRef,
        args: Vec<ValueId>,
        argument_alignments: Vec<u64>,
        argument_offset: usize,
    ) -> Result<(ArgumentGroups, Vec<(OpId, ValueId)>), PassError> {
        let mut tuple_arguments = Vec::new();
        let mut lowered_arguments = Vec::with_capacity(args.len());
        for (argument_index, (arg, alignment)) in
            args.into_iter().zip(argument_alignments).enumerate()
        {
            // The elements the preparation pass recorded describe the argument
            // on their own, so a tuple whose producing call has already been
            // lowered away need not be read back.
            if let Some(elements) = self
                .tuple_argument_elements
                .get(&(op.op().id, argument_index + argument_offset))
            {
                lowered_arguments.push((elements.clone(), alignment));
                continue;
            }
            let ty = context.get_type_data(context.get_value(arg).ty());
            if (ty.as_ref() as &dyn std::any::Any)
                .downcast_ref::<TupleType>()
                .is_none()
            {
                lowered_arguments.push((vec![arg], alignment));
                continue;
            }
            let defining_op = context.get_value(arg).defining_op().ok_or_else(|| {
                PassError::InvalidRuleSet("tuple call argument has no scalar elements".to_string())
            })?;
            let tuple = context
                .get_op(defining_op)
                .clone()
                .as_op::<MakeTupleOp>()
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(
                        "tuple call argument has no scalar elements".to_string(),
                    )
                })?;
            lowered_arguments.push((tuple.operands().to_vec(), alignment));
            tuple_arguments.push((defining_op, arg));
        }
        Ok((lowered_arguments, tuple_arguments))
    }

    /// Place every argument group in registers where the convention has room
    /// for the whole group, and on the outgoing stack otherwise.
    fn assign_argument_locations(
        &self,
        context: &Context,
        lowered_arguments: ArgumentGroups,
        has_result_address: bool,
    ) -> Result<(Vec<ValueId>, Vec<ArgumentLocation>, u32), PassError> {
        let mut next_slot = HashMap::new();
        if has_result_address {
            reserve_indirect_result_argument(self.abi, &mut next_slot);
        }
        let mut argument_values = Vec::new();
        let mut argument_locations = Vec::new();
        let mut stack_args = 0u32;
        for (values, alignment) in lowered_arguments {
            let mut trial_slots = next_slot.clone();
            align_argument_group(
                self.abi,
                alignment,
                values
                    .iter()
                    .map(|&value| value_kind(context, self.abi, value)),
                &mut trial_slots,
            );
            let direct = if self.abi.argument_group_fits_register_limit(values.len()) {
                values
                    .iter()
                    .map(|&value| {
                        next_argument_register(
                            self.abi,
                            None,
                            value_kind(context, self.abi, value),
                            &mut trial_slots,
                        )
                    })
                    .collect::<Option<Vec<_>>>()
            } else {
                None
            };
            if let Some(registers) = direct {
                next_slot = trial_slots;
                argument_values.extend(values);
                argument_locations.extend(registers.into_iter().map(ArgumentLocation::Register));
                continue;
            }

            for &value in &values {
                if self.abi.argument_group_rollback() == GroupRollback::Exhaust {
                    exhaust_argument_registers(
                        self.abi,
                        value_kind(context, self.abi, value),
                        &mut next_slot,
                    );
                }
                let class = stack_class(self.abi, value_kind(context, self.abi, value))
                    .ok_or_else(|| {
                        PassError::InvalidRuleSet("ABI has no argument sequence".to_string())
                    })?;
                argument_values.push(value);
                argument_locations.push(ArgumentLocation::Stack {
                    class,
                    offset: i64::from(stack_args * self.abi.stack.slot_size),
                });
                stack_args += 1;
            }
        }
        let outgoing_size = if stack_args == 0 {
            0
        } else {
            let bytes = stack_args * self.abi.stack.slot_size;
            bytes.div_ceil(self.abi.stack.align) * self.abi.stack.align
        };
        Ok((argument_values, argument_locations, outgoing_size))
    }

    /// Rewrite every extraction of a tuple result into a copy out of the
    /// register the convention returned that element in.
    fn lower_tuple_result(
        &self,
        context: &Context,
        op: &OperationRef,
        rewriter: &mut Rewriter,
        result: ValueId,
        tuple: &TupleType,
        tuple_arguments: &[(OpId, ValueId)],
    ) -> Result<(), PassError> {
        let registers = tuple_return_registers(context, self.abi, tuple)?;
        let mut extracts = Vec::new();
        for user in context.users_of(result) {
            if self
                .tuple_argument_elements
                .keys()
                .any(|(call, _)| *call == user)
            {
                continue;
            }
            let instance = context.get_op(user);
            if instance.operands().first() != Some(&result) {
                return Err(PassError::InvalidRuleSet(
                    "tuple call result has a non-extraction use".to_string(),
                ));
            }
            let extract = instance.clone().as_op::<TupleGetOp>().ok_or_else(|| {
                PassError::InvalidRuleSet("tuple call result has a non-extraction use".to_string())
            })?;
            let register = registers.get(extract.index()).copied().ok_or_else(|| {
                PassError::InvalidRuleSet("tuple extraction index is out of bounds".to_string())
            })?;
            extracts.push((
                extract.index(),
                extract.result(),
                register,
                OperationRef::new(instance),
            ));
        }
        extracts.sort_by_key(|(index, result, ..)| (*index, result.number()));

        for &(_, extracted, register, _) in &extracts {
            // The copy takes over the extraction's value: it is the one the
            // rest of the function — and this pass's own record of the
            // tuple's elements — already names.
            context.retype_value(
                extracted,
                crate::backend::RegClassType::new(context, register.0),
            );
            let copy =
                self.emitter
                    .copy(context, RegSlot::Value(extracted), RegSlot::Phys(register));
            rewriter.insert_op_before(op, copy.as_ref())?;
        }
        for (_, _, _, extract) in extracts {
            rewriter.erase_op_keeping_results(&extract)?;
        }
        rewriter.erase_op(op)?;
        erase_dead_tuple_arguments(context, rewriter, tuple_arguments)?;
        Ok(())
    }
}

/// The scalars each call argument lowers to, with the alignment of the group
/// they came from.
type ArgumentGroups = Vec<(Vec<ValueId>, u64)>;

fn insert_tuple_extractions(
    context: &Context,
    rewriter: &mut Rewriter,
    before: &OperationRef,
    tuple_value: ValueId,
    tuple: &TupleType,
) -> Result<Vec<ValueId>, PassError> {
    let mut elements = Vec::new();
    for (index, element_ty) in tuple.elements(context).into_iter().enumerate() {
        let extract = TupleGetOpBuilder::new(context)
            .tuple(tuple_value)
            .attr("index", AttributeValue::UInt(index as u64))
            .result_type(element_ty)
            .build();
        elements.push(extract.result());
        rewriter.insert_op_before(before, &extract)?;
    }
    Ok(elements)
}

fn erase_dead_tuple_arguments(
    context: &Context,
    rewriter: &mut Rewriter,
    tuple_arguments: &[(OpId, ValueId)],
) -> Result<(), PassError> {
    for &(tuple, value) in tuple_arguments {
        if !context.has_operation(tuple) || context.is_used(value) {
            continue;
        }
        rewriter.erase_op(&OperationRef::new(context.get_op(tuple)))?;
    }
    Ok(())
}

fn tuple_return_registers(
    context: &Context,
    abi: &AbiInfo,
    tuple: &TupleType,
) -> Result<Vec<PhysReg>, PassError> {
    let mut next_slot = HashMap::new();
    tuple
        .elements(context)
        .into_iter()
        .enumerate()
        .map(|(index, ty)| {
            next_return_register(abi, type_kind(context, ty), &mut next_slot).ok_or_else(|| {
                PassError::InvalidRuleSet(format!(
                    "ABI has no return register for tuple element {index}"
                ))
            })
        })
        .collect()
}

enum Callee {
    Direct(String),
    Indirect(ValueId),
}

#[derive(Clone, Copy)]
enum ArgumentLocation {
    Register(PhysReg),
    Stack {
        class: crate::backend::regalloc::RegClassId,
        offset: i64,
    },
}

impl ArgumentLocation {
    fn class(self) -> crate::backend::regalloc::RegClassId {
        match self {
            ArgumentLocation::Register(register) => register.0,
            ArgumentLocation::Stack { class, .. } => class,
        }
    }
}

fn stack_class(abi: &AbiInfo, mut kind: ValueKind) -> Option<crate::backend::regalloc::RegClassId> {
    let mut visited = HashSet::new();
    let mut value_class = None;
    loop {
        if !visited.insert(kind) {
            return None;
        }
        let sequence = match abi.args.iter().find(|sequence| sequence.kind == kind) {
            Some(sequence) => sequence,
            None if kind != ValueKind::Int => {
                kind = ValueKind::Int;
                continue;
            }
            None => return None,
        };
        value_class.get_or_insert(sequence.regs.first()?.0);
        match sequence.overflow {
            Overflow::Chain(next) => kind = next,
            Overflow::Stack => return value_class,
        }
    }
}

/// A fresh value of `class`, the type a machine instruction reads it through.
/// Where a call's target comes from: a named symbol when the callee traces back
/// to a λ node, otherwise the machine value the address was recovered from.
fn resolve_callee(context: &Context, call: &CallOp) -> Callee {
    if let Some(symbol) = call.callee_symbol() {
        return Callee::Direct(symbol);
    }
    let callee = call.callee();
    let definition = context
        .get_value(callee)
        .defining_op()
        .filter(|&definition| context.has_operation(definition));
    let Some(definition) = definition else {
        return Callee::Indirect(callee);
    };
    let instance = context.get_op(definition);
    if let Some(symbol) = instance.clone().as_interface::<dyn Symbol>() {
        return Callee::Direct(symbol.symbol_name());
    }
    match instance.as_op::<PtrToFnOp>() {
        Some(recovered) => Callee::Indirect(recovered.operands()[0]),
        None => Callee::Indirect(callee),
    }
}
