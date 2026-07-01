//! Lower the [`ast`] into TIR, targeting the `builtin` and `ptr` dialects.
//! Instructions the parser flagged as [`ast::Inst::Unsupported`] — and any
//! construct with no TIR equivalent today — produce an error rather than a
//! silent drop.

use std::collections::HashMap;
use std::sync::Arc;

use tir::builtin::{self, IntegerType, UnitType, ops as bops};
use tir::ptr::{PtrType, ops as pops};
use tir::{Block, Context, IRBuilder, Operand, TypeId, ValueId};

use crate::ast::{self, BinOp, CastOp, Inst, Type};
use crate::error::Error;

pub fn import(context: &Context, module: &ast::Module) -> Result<builtin::ModuleOp, Error> {
    let m = bops::module(context, None).build();
    let mut builder = IRBuilder::new(m.body());
    for func in &module.functions {
        builder.insert(lower_function(context, func)?);
    }
    builder.insert(bops::module_end(context).build());
    Ok(m)
}

fn lower_type(context: &Context, ty: &Type) -> TypeId {
    match ty {
        Type::Int(width) => IntegerType::new(context, *width),
        Type::Void => UnitType::new(context),
        Type::Ptr(None) => PtrType::opaque(context),
        Type::Ptr(Some(pointee)) => PtrType::typed(context, lower_type(context, pointee)),
    }
}

fn lower_function(context: &Context, func: &ast::Function) -> Result<builtin::FuncOp, Error> {
    let region = context.create_region();
    let mut values: HashMap<String, ValueId> = HashMap::new();

    // Parameters become entry-block arguments.
    let mut entry_args = Vec::new();
    for param in &func.params {
        let value = context.create_value(lower_type(context, &param.ty), None);
        values.insert(param.name.clone(), value.id());
        entry_args.push(value);
    }

    // Pre-create every block so branches can resolve targets by label.
    let mut blocks: Vec<Arc<Block>> = Vec::new();
    let mut by_label: HashMap<String, Arc<Block>> = HashMap::new();
    for (i, block) in func.blocks.iter().enumerate() {
        let args = if i == 0 {
            std::mem::take(&mut entry_args)
        } else {
            Vec::new()
        };
        let created = context.create_block(args);
        region.add_block(created.id());
        if let Some(label) = &block.label {
            by_label.insert(label.clone(), created.clone());
        }
        blocks.push(created);
    }

    let ret_ty = lower_type(context, &func.ret);
    let op = bops::func(context, func.name.as_str(), ret_ty, Some(region.id())).build();

    for (block, created) in func.blocks.iter().zip(blocks.iter()) {
        let mut builder = IRBuilder::new(created.clone());
        for inst in &block.insts {
            lower_inst(context, inst, &mut builder, &mut values, &by_label)?;
        }
    }

    Ok(op)
}

fn lower_inst(
    context: &Context,
    inst: &Inst,
    builder: &mut IRBuilder,
    values: &mut HashMap<String, ValueId>,
    by_label: &HashMap<String, Arc<Block>>,
) -> Result<(), Error> {
    // Resolve an operand to a value, materialising a `builtin.constant` for
    // inline integer literals (TIR has no inline constants).
    macro_rules! val {
        ($op:expr, $ty:expr) => {
            match $op {
                ast::Operand::Ref(name) => *values
                    .get(name)
                    .ok_or_else(|| Error::UndefinedValue(name.clone()))?,
                ast::Operand::ConstInt(v) => {
                    let c = bops::constant(context, *v, lower_type(context, $ty)).build();
                    let id = c.result();
                    builder.insert(c);
                    id
                }
            }
        };
    }

    match inst {
        Inst::Binary {
            result,
            op,
            ty,
            lhs,
            rhs,
        } => {
            let t = lower_type(context, ty);
            let l = val!(lhs, ty);
            let r = val!(rhs, ty);
            macro_rules! bin {
                ($f:path) => {{
                    let o = $f(context, l, r, t).build();
                    let id = o.result();
                    builder.insert(o);
                    id
                }};
            }
            let id = match op {
                BinOp::Add => bin!(bops::addi),
                BinOp::Sub => bin!(bops::subi),
                BinOp::Mul => bin!(bops::muli),
                BinOp::And => bin!(bops::andi),
                BinOp::Or => bin!(bops::ori),
                BinOp::Xor => bin!(bops::xori),
                BinOp::Shl => bin!(bops::shli),
                BinOp::LShr => bin!(bops::shrui),
                BinOp::AShr => bin!(bops::shrsi),
            };
            values.insert(result.clone(), id);
        }
        Inst::ICmp {
            result,
            pred,
            ty,
            lhs,
            rhs,
        } => {
            let l = val!(lhs, ty);
            let r = val!(rhs, ty);
            let i1 = IntegerType::new(context, 1);
            let o = bops::cmpi(context, l, r, pred.as_str(), i1).build();
            values.insert(result.clone(), o.result());
            builder.insert(o);
        }
        Inst::Cast {
            result,
            op,
            from,
            value,
            to,
        } => {
            let input = val!(value, from);
            let to_ty = lower_type(context, to);
            let id = match op {
                CastOp::SExt => {
                    let o = bops::extsi(context, input, to_ty).build();
                    let id = o.result();
                    builder.insert(o);
                    id
                }
                CastOp::ZExt => {
                    let o = bops::extui(context, input, to_ty).build();
                    let id = o.result();
                    builder.insert(o);
                    id
                }
                CastOp::Trunc => {
                    let o = bops::trunci(context, input, to_ty).build();
                    let id = o.result();
                    builder.insert(o);
                    id
                }
            };
            values.insert(result.clone(), id);
        }
        Inst::Alloca { result, ty } => {
            let ptr_ty = PtrType::typed(context, lower_type(context, ty));
            let o = pops::alloca(context, ptr_ty).build();
            values.insert(result.clone(), o.result());
            builder.insert(o);
        }
        Inst::Load { result, ty, ptr } => {
            let p = val!(ptr, &Type::Ptr(None));
            let o = pops::load(context, p, lower_type(context, ty)).build();
            values.insert(result.clone(), o.result());
            builder.insert(o);
        }
        Inst::Store { ty, value, ptr } => {
            let v = val!(value, ty);
            let p = val!(ptr, &Type::Ptr(None));
            builder.insert(pops::store(context, v, p).build());
        }
        Inst::Br { dest } => {
            let target = by_label
                .get(dest)
                .ok_or_else(|| Error::UndefinedBlock(dest.clone()))?
                .id();
            builder.insert(bops::br(context, vec![], target).build());
        }
        Inst::CondBr {
            cond,
            if_true,
            if_false,
        } => {
            let c = val!(cond, &Type::Int(1));
            let t = by_label
                .get(if_true)
                .ok_or_else(|| Error::UndefinedBlock(if_true.clone()))?
                .id();
            let f = by_label
                .get(if_false)
                .ok_or_else(|| Error::UndefinedBlock(if_false.clone()))?
                .id();
            builder.insert(bops::cond_br(context, c, vec![], vec![], t, f).build());
        }
        Inst::Ret { value } => match value {
            None => {
                builder.insert(bops::r#return(context, Operand::none()).build());
            }
            Some((ty, op)) => {
                let v = val!(op, ty);
                builder.insert(bops::r#return(context, v).build());
            }
        },
        Inst::Call {
            result,
            ret,
            callee,
            args,
        } => {
            let mut arg_ids = Vec::with_capacity(args.len());
            for (ty, op) in args {
                arg_ids.push(val!(op, ty));
            }
            let ret_ty = lower_type(context, ret);
            let o = bops::call(context, arg_ids, callee.as_str(), ret_ty).build();
            if let Some(name) = result {
                values.insert(name.clone(), o.result());
            }
            builder.insert(o);
        }
        Inst::Unsupported(opcode) => {
            return Err(Error::Unsupported(opcode.clone()));
        }
    }
    Ok(())
}
