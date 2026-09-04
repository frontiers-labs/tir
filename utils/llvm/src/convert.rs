//! Lower the [`ast`] into TIR, targeting the `builtin` and `ptr` dialects.
//! Instructions the parser flagged as [`ast::Inst::Unsupported`] — and any
//! construct with no TIR equivalent today — produce an error rather than a
//! silent drop.

use std::collections::HashMap;
use tir::BlockHandle;

use tir::builtin::{self, FnType, IntegerType, UnitType, ops as bops};
use tir::cfg::ops as cbops;
use tir::func::ops as func_ops;
use tir::ptr::{PtrType, ops as pops};
use tir::{Context, Operand, Symbol, TypeId, ValueId};

use crate::ast::{self, BinOp, CastOp, Inst, Type};
use crate::error::Error;

pub fn import(context: &Context, module: &ast::Module) -> Result<builtin::ModuleOp, Error> {
    let m = bops::module(context, None).build();
    let builder = m.body();
    let mut callees = Callees::default();
    for func in &module.functions {
        builder.append_op(lower_function(context, func, &mut callees)?);
    }
    let bindings = callees.bind(context, &m, &builder);
    builder.append_op(bops::module_end(context).build());
    for (&old, &new) in &bindings {
        context.replace_value_uses(old, new);
    }
    Ok(m)
}

/// The λ values calls name before their definitions are in the module. LLVM has
/// no overloads, so a name identifies a function outright.
#[derive(Default)]
struct Callees {
    placeholders: Vec<(String, ValueId)>,
    by_name: HashMap<String, ValueId>,
}

impl Callees {
    fn value(&mut self, context: &Context, name: &str, args: &[TypeId], ret: TypeId) -> ValueId {
        if let Some(value) = self.by_name.get(name) {
            return *value;
        }
        let value = context
            .create_value(FnType::new(context, args, ret), None)
            .id();
        self.by_name.insert(name.to_string(), value);
        self.placeholders.push((name.to_string(), value));
        value
    }

    /// Pair every placeholder with the λ it names, declaring the functions this
    /// module only calls: LLVM `declare` lines carry no body and are not parsed.
    fn bind(
        self,
        context: &Context,
        module: &builtin::ModuleOp,
        body: &BlockHandle,
    ) -> HashMap<ValueId, ValueId> {
        let mut defined = HashMap::new();
        for op in module.body().iter(context.clone()) {
            if let Some(func) = op.as_op::<tir::func::FuncOp>() {
                defined.insert(func.symbol_name(), func.fn_value());
            }
        }
        self.placeholders
            .into_iter()
            .map(|(name, placeholder)| {
                if let Some(value) = defined.get(&name) {
                    return (placeholder, *value);
                }
                let signature = context.get_type_data(context.get_value(placeholder).ty());
                let signature = (signature.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<FnType>()
                    .expect("a callee placeholder has a function type");
                let declaration = tir::func::declare_op(
                    context,
                    &name,
                    signature.ret(context),
                    &signature.params(context),
                );
                let value = declaration.fn_value();
                body.append_op(declaration);
                (placeholder, value)
            })
            .collect()
    }
}

fn lower_type(context: &Context, ty: &Type) -> TypeId {
    match ty {
        Type::Int(width) => IntegerType::new(context, *width),
        Type::Void => UnitType::new(context),
        Type::Ptr(None) => PtrType::opaque(context),
        Type::Ptr(Some(pointee)) => PtrType::typed(context, lower_type(context, pointee)),
    }
}

fn lower_function(
    context: &Context,
    func: &ast::Function,
    callees: &mut Callees,
) -> Result<tir::func::FuncOp, Error> {
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
    let mut blocks: Vec<BlockHandle> = Vec::new();
    let mut by_label: HashMap<String, BlockHandle> = HashMap::new();
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
    let parameters: Vec<_> = func
        .params
        .iter()
        .map(|param| lower_type(context, &param.ty))
        .collect();
    let op = func_ops::func(
        context,
        func.name.as_str(),
        ret_ty,
        FnType::new(context, &parameters, ret_ty),
        Some(region.id()),
    )
    .build();

    for (block, created) in func.blocks.iter().zip(blocks.iter()) {
        let builder = created.clone();
        for inst in &block.insts {
            lower_inst(context, inst, &builder, &mut values, &by_label, callees)?;
        }
    }

    Ok(op)
}

fn lower_inst(
    context: &Context,
    inst: &Inst,
    body: &BlockHandle,
    values: &mut HashMap<String, ValueId>,
    by_label: &HashMap<String, BlockHandle>,
    callees: &mut Callees,
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
                    body.append_op(c);
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
                    body.append_op(o);
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
            body.append_op(o);
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
                    body.append_op(o);
                    id
                }
                CastOp::ZExt => {
                    let o = bops::extui(context, input, to_ty).build();
                    let id = o.result();
                    body.append_op(o);
                    id
                }
                CastOp::Trunc => {
                    let o = bops::trunci(context, input, to_ty).build();
                    let id = o.result();
                    body.append_op(o);
                    id
                }
            };
            values.insert(result.clone(), id);
        }
        Inst::Alloca { result, ty } => {
            let ptr_ty = PtrType::typed(context, lower_type(context, ty));
            let bytes = match ty {
                Type::Int(width) => u64::from(width.div_ceil(8)),
                Type::Ptr(_) => 8,
                _ => 8,
            };
            let o = pops::alloca(context, bytes, bytes, ptr_ty).build();
            values.insert(result.clone(), o.result());
            body.append_op(o);
        }
        Inst::Load { result, ty, ptr } => {
            let p = val!(ptr, &Type::Ptr(None));
            let o = pops::load(context, p, lower_type(context, ty)).build();
            values.insert(result.clone(), o.result());
            body.append_op(o);
        }
        Inst::Store { ty, value, ptr } => {
            let v = val!(value, ty);
            let p = val!(ptr, &Type::Ptr(None));
            body.append_op(pops::store(context, v, p).build());
        }
        Inst::Br { dest } => {
            let target = by_label
                .get(dest)
                .ok_or_else(|| Error::UndefinedBlock(dest.clone()))?
                .id();
            body.append_op(cbops::br(context, vec![], target).build());
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
            body.append_op(cbops::cond_br(context, c, vec![], vec![], t, f).build());
        }
        Inst::Ret { value } => match value {
            None => {
                body.append_op(func_ops::r#return(context, Operand::none()).build());
            }
            Some((ty, op)) => {
                let v = val!(op, ty);
                body.append_op(func_ops::r#return(context, v).build());
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
            let arg_types: Vec<_> = arg_ids
                .iter()
                .map(|&arg| context.get_value(arg).ty())
                .collect();
            let callee = callees.value(context, callee.as_str(), &arg_types, ret_ty);
            let o = func_ops::call(context, callee, arg_ids, ret_ty).build();
            if let Some(name) = result {
                values.insert(name.clone(), o.result());
            }
            body.append_op(o);
        }
        Inst::Unsupported(opcode) => {
            return Err(Error::Unsupported(opcode.clone()));
        }
    }
    Ok(())
}
