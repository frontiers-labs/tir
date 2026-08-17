use std::collections::{HashMap, HashSet};

use tir::analysis::DefUse;
use tir::attributes::{AttributeValue, RegisterAttr};
use tir::builtin::{
    MakeTupleOp, MakeTupleOpBuilder, TupleGetOp, TupleGetOpBuilder, TupleType, UnitType,
};
use tir::func::{CallOp, IndirectCallOp, ReturnOp};
use tir::{Context, OpId, Operand, Operation, OperationRef, PassError, Rewriter, ValueId};

use crate::backend::abi::{
    AbiInfo, GroupRollback, Overflow, ValueKind, align_argument_group, exhaust_argument_registers,
    reserve_indirect_result_argument, type_kind, value_kind,
};
use crate::backend::liveness::PhysReg;

pub trait CallEmitter: Send + Sync {
    fn copy(
        &self,
        context: &Context,
        dst: AttributeValue,
        src: AttributeValue,
    ) -> Box<dyn Operation>;

    fn stack_arg_store(
        &self,
        _context: &Context,
        _abi: &AbiInfo,
        _value: AttributeValue,
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
                        && let Some(value) = ret.operands().first().copied()
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
                                let return_ref = OperationRef::new(
                                    instance.clone(),
                                    Some(context.get_block(block.id())),
                                    None,
                                );
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
                    let args = if let Some(call) = instance.clone().as_op::<CallOp>() {
                        call.args()
                    } else if let Some(call) = instance.clone().as_op::<IndirectCallOp>() {
                        call.args()
                    } else {
                        continue;
                    };
                    let call_ref =
                        OperationRef::new(instance, Some(context.get_block(block.id())), None);
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
        let (callee, mut args, result, has_result_address, mut argument_alignments) =
            if let Some(call) = op.as_op::<CallOp>() {
                (
                    Callee::Direct(call.callee()),
                    call.args(),
                    call.result(),
                    call.has_result_address(),
                    call.argument_alignments(),
                )
            } else if let Some(call) = op.as_op::<IndirectCallOp>() {
                (
                    Callee::Indirect(call.callee()),
                    call.args(),
                    call.result(),
                    call.has_result_address(),
                    call.argument_alignments(),
                )
            } else {
                return Ok(false);
            };
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

        let mut tuple_arguments = Vec::new();
        let mut lowered_arguments = Vec::with_capacity(args.len());
        for (argument_index, (arg, alignment)) in
            args.into_iter().zip(argument_alignments).enumerate()
        {
            let ty = context.get_type_data(context.get_value(arg).ty());
            if (ty.as_ref() as &dyn std::any::Any)
                .downcast_ref::<TupleType>()
                .is_none()
            {
                lowered_arguments.push((vec![arg], alignment));
                continue;
            }
            if let Some(elements) = self
                .tuple_argument_elements
                .get(&(op.op().id, argument_index + argument_offset))
            {
                lowered_arguments.push((elements.clone(), alignment));
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

        let mut next_slot = HashMap::new();
        if result_address.is_some() {
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
                values.iter().map(|&value| value_kind(context, value)),
                &mut trial_slots,
            );
            let direct = if self.abi.argument_group_fits_register_limit(values.len()) {
                values
                    .iter()
                    .map(|&value| {
                        next_register(self.abi, value_kind(context, value), &mut trial_slots)
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
                        value_kind(context, value),
                        &mut next_slot,
                    );
                }
                let class = stack_class(self.abi, value_kind(context, value)).ok_or_else(|| {
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

        let detach = |rewriter: &mut Rewriter, value: ValueId, class| {
            let ty = context.get_value(value).ty();
            let fresh = context.create_value(ty, None).id().number();
            let copy = self.emitter.copy(
                context,
                virtual_reg(fresh, class),
                virtual_reg(value.number(), class),
            );
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
                    && value_kind(context, *value) == ValueKind::Float
            })
            .count() as u8;
        for prefix in
            self.emitter
                .call_prefix(context, self.abi, outgoing_size, vector_register_args)
        {
            rewriter.insert_op_before(op, prefix.as_ref())?;
        }

        let saved_ra = if let Some(ra) = self.abi.ra {
            let ty = tir::builtin::IntegerType::new(context, self.abi.stack.slot_size * 8);
            let saved = context.create_value(ty, None).id().number();
            let copy = self
                .emitter
                .copy(context, virtual_reg(saved, ra.0), physical_reg(ra));
            rewriter.insert_op_before(op, copy.as_ref())?;
            Some((saved, ra))
        } else {
            None
        };

        for (&fresh, location) in fresh_args.iter().zip(&argument_locations) {
            match *location {
                ArgumentLocation::Register(register) => {
                    let copy = self.emitter.copy(
                        context,
                        physical_reg(register),
                        virtual_reg(fresh, register.0),
                    );
                    rewriter.insert_op_before(op, copy.as_ref())?;
                }
                ArgumentLocation::Stack { class, offset } => {
                    let store = self.emitter.stack_arg_store(
                        context,
                        self.abi,
                        virtual_reg(fresh, class),
                        offset,
                    )?;
                    rewriter.insert_op_before(op, store.as_ref())?;
                }
            }
        }
        if let Some((fresh, register)) = fresh_result_address {
            let copy = self.emitter.copy(
                context,
                physical_reg(register),
                virtual_reg(fresh, register.0),
            );
            rewriter.insert_op_before(op, copy.as_ref())?;
        }

        let clobbers = AttributeValue::Array(
            self.abi
                .caller_saved
                .iter()
                .copied()
                .map(physical_reg)
                .collect::<Vec<_>>()
                .into(),
        );
        let call: Box<dyn Operation> = match callee {
            Callee::Direct(name) => Box::new(
                super::VirtualCallOpBuilder::new(context)
                    .attr("callee", AttributeValue::Str(name.into()))
                    .outgoing_stack_size(u64::from(outgoing_size))
                    .attr("clobbers", clobbers)
                    .build(),
            ),
            Callee::Indirect(_) => Box::new(
                super::VirtualIndirectCallOpBuilder::new(context)
                    .attr(
                        "callee_reg",
                        virtual_reg(
                            fresh_callee.expect("indirect callee was detached"),
                            indirect_class,
                        ),
                    )
                    .outgoing_stack_size(u64::from(outgoing_size))
                    .attr("clobbers", clobbers)
                    .build(),
            ),
        };
        rewriter.insert_op_before(op, call.as_ref())?;
        for suffix in self.emitter.call_suffix(context, self.abi, outgoing_size) {
            rewriter.insert_op_before(op, suffix.as_ref())?;
        }

        let restore = saved_ra.map(|(saved, ra)| {
            self.emitter
                .copy(context, physical_reg(ra), virtual_reg(saved, ra.0))
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
            let registers = tuple_return_registers(context, self.abi, tuple)?;
            let mut extracts = Vec::new();
            for user in enclosing_def_use(context, op)?.users_of(result.number()) {
                if self
                    .tuple_argument_elements
                    .keys()
                    .any(|(call, _)| call == user)
                {
                    continue;
                }
                let instance = context.get_op(*user);
                if instance.operands().first() != Some(&result) {
                    return Err(PassError::InvalidRuleSet(
                        "tuple call result has a non-extraction use".to_string(),
                    ));
                }
                let extract = instance.clone().as_op::<TupleGetOp>().ok_or_else(|| {
                    PassError::InvalidRuleSet(
                        "tuple call result has a non-extraction use".to_string(),
                    )
                })?;
                let register = registers.get(extract.index()).copied().ok_or_else(|| {
                    PassError::InvalidRuleSet("tuple extraction index is out of bounds".to_string())
                })?;
                let block = context.parent_block(*user).ok_or_else(|| {
                    PassError::InvalidRuleSet("tuple extraction has no parent block".to_string())
                })?;
                extracts.push((
                    extract.index(),
                    extract.result(),
                    register,
                    OperationRef::new(instance, Some(context.get_block(block)), None),
                ));
            }
            extracts.sort_by_key(|(index, result, ..)| (*index, result.number()));

            for &(_, extracted, register, _) in &extracts {
                let copy = self.emitter.copy(
                    context,
                    virtual_reg(extracted.number(), register.0),
                    physical_reg(register),
                );
                rewriter.insert_op_before(op, copy.as_ref())?;
            }
            for (_, _, _, extract) in extracts {
                rewriter.erase_op(&extract)?;
            }
            rewriter.erase_op(op)?;
            erase_dead_tuple_arguments(context, rewriter, &tuple_arguments)?;
            return Ok(true);
        }

        let kind = value_kind(context, result);
        let return_reg = self
            .abi
            .rets
            .iter()
            .find(|sequence| sequence.kind == kind)
            .or_else(|| {
                (kind != ValueKind::Int).then(|| {
                    self.abi
                        .rets
                        .iter()
                        .find(|sequence| sequence.kind == ValueKind::Int)
                })?
            })
            .and_then(|sequence| sequence.regs.first())
            .copied()
            .ok_or_else(|| PassError::InvalidRuleSet("ABI has no return register".to_string()))?;
        let copy = self.emitter.copy(
            context,
            virtual_reg(result.number(), return_reg.0),
            physical_reg(return_reg),
        );
        rewriter.replace_op(op, copy.as_ref())?;
        erase_dead_tuple_arguments(context, rewriter, &tuple_arguments)?;
        Ok(true)
    }
}

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

/// The use index of the function `op` sits in, rebuilt on demand: the pass has no
/// analysis manager to cache it in, and rewrites it as it goes.
fn enclosing_def_use(context: &Context, op: &OperationRef) -> Result<DefUse, PassError> {
    let root = context.parent_op(op.op().id).ok_or_else(|| {
        PassError::InvalidRuleSet("call lowering needs an enclosing function".to_string())
    })?;
    Ok(DefUse::new(context, root))
}

fn erase_dead_tuple_arguments(
    context: &Context,
    rewriter: &mut Rewriter,
    tuple_arguments: &[(OpId, ValueId)],
) -> Result<(), PassError> {
    for &(tuple, value) in tuple_arguments {
        let block = context.parent_block(tuple).ok_or_else(|| {
            PassError::InvalidRuleSet("tuple construction has no parent block".to_string())
        })?;
        let root = context.parent_op(tuple).ok_or_else(|| {
            PassError::InvalidRuleSet("tuple construction has no enclosing op".to_string())
        })?;
        if DefUse::new(context, root).is_used(value.number()) {
            continue;
        }
        rewriter.erase_op(&OperationRef::new(
            context.get_op(tuple),
            Some(context.get_block(block)),
            None,
        ))?;
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
            let kind = type_kind(context, ty);
            let slot = next_slot.entry(kind).or_insert(0usize);
            let register = abi
                .rets
                .iter()
                .find(|sequence| sequence.kind == kind)
                .or_else(|| {
                    (kind != ValueKind::Int)
                        .then(|| {
                            abi.rets
                                .iter()
                                .find(|sequence| sequence.kind == ValueKind::Int)
                        })
                        .flatten()
                })
                .and_then(|sequence| sequence.regs.get(*slot))
                .copied()
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!(
                        "ABI has no return register for tuple element {index}"
                    ))
                })?;
            *slot += 1;
            Ok(register)
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

fn next_register(
    abi: &AbiInfo,
    mut kind: ValueKind,
    next_slot: &mut HashMap<ValueKind, usize>,
) -> Option<PhysReg> {
    let mut visited = HashSet::new();
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
        let slot = next_slot.entry(kind).or_insert(0);
        if let Some(&register) = sequence.regs.get(*slot) {
            *slot += 1;
            return Some(register);
        }
        match sequence.overflow {
            Overflow::Chain(next) => kind = next,
            Overflow::Stack => return None,
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

fn physical_reg(reg: PhysReg) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Physical {
        class: reg.0,
        index: reg.1,
    })
}

fn virtual_reg(id: u32, class: crate::backend::regalloc::RegClassId) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Virtual {
        id,
        class: Some(class),
    })
}
