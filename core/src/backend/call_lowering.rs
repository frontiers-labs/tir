use std::collections::{HashMap, HashSet};

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::builtin::{CallOp, IndirectCallOp, UnitType};
use tir::{Context, Operation, OperationRef, PassError, Rewriter, ValueId};

use crate::backend::abi::{AbiInfo, Overflow, ValueKind};
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
}

impl CallLowering {
    pub fn new(abi: &'static AbiInfo, emitter: Box<dyn CallEmitter>) -> Self {
        Self { abi, emitter }
    }

    pub fn lower(
        &self,
        context: &Context,
        op: &OperationRef,
        rewriter: &mut Rewriter,
    ) -> Result<bool, PassError> {
        let (callee, args, result) = if let Some(call) = op.as_op::<CallOp>() {
            (Callee::Direct(call.callee()), call.args(), call.result())
        } else if let Some(call) = op.as_op::<IndirectCallOp>() {
            (Callee::Indirect(call.callee()), call.args(), call.result())
        } else {
            return Ok(false);
        };

        let mut next_slot = HashMap::new();
        let mut argument_locations = Vec::with_capacity(args.len());
        let mut stack_args = 0u32;
        for &arg in &args {
            let kind = value_kind(context, arg);
            if let Some(register) = next_register(self.abi, kind, &mut next_slot) {
                argument_locations.push(ArgumentLocation::Register(register));
            } else {
                let class = stack_class(self.abi, kind).ok_or_else(|| {
                    PassError::InvalidRuleSet("ABI has no argument sequence".to_string())
                })?;
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
        let mut fresh_args = Vec::with_capacity(args.len());
        for (&arg, location) in args.iter().zip(&argument_locations) {
            fresh_args.push(detach(rewriter, arg, location.class())?);
        }

        for prefix in self.emitter.call_prefix(context, self.abi, outgoing_size) {
            rewriter.insert_op_before(op, prefix.as_ref())?;
        }
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

        let clobbers = AttributeValue::Array(
            self.abi
                .caller_saved
                .iter()
                .copied()
                .map(physical_reg)
                .collect(),
        );
        let call: Box<dyn Operation> = match callee {
            Callee::Direct(name) => Box::new(
                super::VirtualCallOpBuilder::new(context)
                    .attr("callee", AttributeValue::Str(name))
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
        Ok(true)
    }
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

fn value_kind(context: &Context, value: ValueId) -> ValueKind {
    let ty = context.get_value(value).ty();
    let data = context.get_type_data(ty);
    let data = data.as_ref() as &dyn std::any::Any;
    if data.downcast_ref::<tir::builtin::FloatType>().is_some() {
        ValueKind::Float
    } else if data.downcast_ref::<tir::vector::VectorType>().is_some() {
        ValueKind::Vector
    } else {
        ValueKind::Int
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
        match sequence.overflow {
            Overflow::Chain(next) => kind = next,
            Overflow::Stack => return sequence.regs.first().map(|register| register.0),
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
