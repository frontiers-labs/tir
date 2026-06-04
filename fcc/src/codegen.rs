//! Lowers the C [`crate::ast`] to TIR using the `builtin` and `ptr` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any mem2reg pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops. Only `int` (lowered to `i32`) and `void` are supported.

use std::collections::HashMap;

use tir::builtin::{IntegerType, UnitType, ops as b};
use tir::ptr::{PtrType, ops as p};
use tir::{Context, IRBuilder, IRFormatter, Operand, Operation, TypeId, ValueId};

use crate::ast::*;

/// A local variable: the pointer to its stack slot and the slot's element type.
#[derive(Clone, Copy)]
struct Slot {
    ptr: ValueId,
    elem: TypeId,
}

struct FnCodegen<'a> {
    context: &'a Context,
    builder: IRBuilder,
    locals: HashMap<String, Slot>,
}

/// Lower a translation unit into a `builtin.module` and return the printed IR.
pub fn codegen(unit: &TranslationUnit) -> Result<String, String> {
    let context = Context::with_default_dialects();

    let module = b::module(&context, None).build();
    let mut module_builder = IRBuilder::new(module.body());

    for func in &unit.functions {
        let func_op = lower_function(&context, func)?;
        module_builder.insert(func_op);
    }
    module_builder.insert(b::module_end(&context).build());

    let mut out = String::new();
    let mut fmt = IRFormatter::new(&mut out);
    module
        .print(&mut fmt)
        .map_err(|e| format!("failed to print IR: {e}"))?;
    Ok(out)
}

fn lower_ctype(context: &Context, ty: &CType) -> TypeId {
    match ty {
        CType::Int => IntegerType::new(context, 32),
        CType::Void => UnitType::new(context),
    }
}

fn lower_function(context: &Context, func: &Function) -> Result<impl Operation, String> {
    let ret_ty = lower_ctype(context, &func.ret);

    // Entry block arguments carry the incoming parameter values.
    let mut param_values = Vec::new();
    for param in &func.params {
        let ty = lower_ctype(context, &param.ty);
        param_values.push(context.create_value(ty, None));
    }
    let param_ids: Vec<ValueId> = param_values.iter().map(|v| v.id()).collect();

    let region = context.create_region();
    let block = context.create_block(param_values);
    region.add_block(block.id());

    let func_op = b::func(context, func.name.as_str(), ret_ty, Some(region.id())).build();

    let mut cg = FnCodegen {
        context,
        builder: IRBuilder::new(func_op.body()),
        locals: HashMap::new(),
    };

    // Spill each parameter into its own stack slot, mirroring -O0 codegen.
    for (param, value) in func.params.iter().zip(param_ids) {
        let elem = lower_ctype(context, &param.ty);
        let slot = cg.alloca(elem);
        cg.builder
            .insert(p::store(context, value, slot.ptr).build());
        cg.locals.insert(param.name.clone(), slot);
    }

    for stmt in &func.body {
        cg.lower_stmt(stmt)?;
    }

    Ok(func_op)
}

impl FnCodegen<'_> {
    fn alloca(&mut self, elem: TypeId) -> Slot {
        let ptr_ty = PtrType::typed(self.context, elem);
        let op = self.builder.insert(p::alloca(self.context, ptr_ty).build());
        Slot {
            ptr: op.result(),
            elem,
        }
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::Decl { name, ty, init } => {
                let elem = lower_ctype(self.context, ty);
                let slot = self.alloca(elem);
                if let Some(expr) = init {
                    let value = self.lower_expr(expr)?;
                    self.builder
                        .insert(p::store(self.context, value, slot.ptr).build());
                }
                self.locals.insert(name.clone(), slot);
                Ok(())
            }
            Stmt::Assign { name, value } => {
                let slot = *self
                    .locals
                    .get(name)
                    .ok_or_else(|| format!("assignment to unknown variable '{name}'"))?;
                let v = self.lower_expr(value)?;
                self.builder
                    .insert(p::store(self.context, v, slot.ptr).build());
                Ok(())
            }
            Stmt::Return(expr) => {
                let operand = match expr {
                    Some(e) => Operand::from(self.lower_expr(e)?),
                    None => Operand::none(),
                };
                self.builder
                    .insert(b::r#return(self.context, operand).build());
                Ok(())
            }
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<ValueId, String> {
        let i32_ty = IntegerType::new(self.context, 32);
        match expr {
            Expr::Int(n) => {
                let op = self
                    .builder
                    .insert(b::constant(self.context, *n, i32_ty).build());
                Ok(op.result())
            }
            Expr::Var(name) => {
                let slot = *self
                    .locals
                    .get(name)
                    .ok_or_else(|| format!("use of unknown variable '{name}'"))?;
                let op = self
                    .builder
                    .insert(p::load(self.context, slot.ptr, slot.elem).build());
                Ok(op.result())
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.lower_expr(lhs)?;
                let r = self.lower_expr(rhs)?;
                let result = match op {
                    BinOp::Add => self
                        .builder
                        .insert(b::addi(self.context, l, r, i32_ty).build())
                        .result(),
                    BinOp::Sub => self
                        .builder
                        .insert(b::subi(self.context, l, r, i32_ty).build())
                        .result(),
                    BinOp::Mul => self
                        .builder
                        .insert(b::muli(self.context, l, r, i32_ty).build())
                        .result(),
                };
                Ok(result)
            }
        }
    }
}
