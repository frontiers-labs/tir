//! Lowers the C [`crate::ast`] to TIR using the `builtin` and `ptr` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any mem2reg pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops. Only `int` (lowered to `i32`) and `void` are supported.

use std::collections::HashMap;

use tir::builtin::{IntegerType, ModuleOp, UnitType, ops as b};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::{Context, IRBuilder, Operand, Operation, TypeId, ValueId};

use crate::ast::*;

/// A local variable: the pointer to its stack slot and the slot's element type.
#[derive(Clone, Copy)]
struct Slot {
    ptr: ValueId,
    elem: TypeId,
}

struct FnCodegen<'a> {
    context: &'a Context,
    ast: &'a Ast,
    builder: IRBuilder,
    locals: HashMap<String, Slot>,
    /// Scratch holding the lowered SSA value of each node in the expression
    /// subtree currently being lowered, indexed by `node.index() - base`. Reused
    /// across expressions to avoid reallocating.
    values: Vec<ValueId>,
}

/// Lower a translation unit into a `builtin.module` in `context`.
pub fn codegen(context: &Context, ast: &Ast) -> Result<ModuleOp, String> {
    let module = b::module(context, None).build();
    let mut module_builder = IRBuilder::new(module.body());

    let root = ast.root().ok_or("empty translation unit")?;
    for func in ast.children(root) {
        let func_op = lower_function(context, ast, func)?;
        module_builder.insert(func_op);
    }
    module_builder.insert(b::module_end(context).build());
    Ok(module)
}

fn lower_ctype(context: &Context, ty: &CType) -> TypeId {
    match ty {
        CType::Int => IntegerType::new(context, 32),
        CType::Void => UnitType::new(context),
    }
}

fn lower_function(context: &Context, ast: &Ast, func: NodeId) -> Result<impl Operation, String> {
    let AstLeaf::Function { name, ret } = ast.get_leaf_data(func).unwrap() else {
        unreachable!("function node carries a function payload");
    };
    let ret_ty = lower_ctype(context, ret);

    // Entry block arguments carry the incoming parameter values; parameters are
    // the function node's leading children.
    let mut param_values = Vec::new();
    for param in ast
        .children(func)
        .take_while(|&c| matches!(ast.get_node(c), AstKind::Param))
    {
        let AstLeaf::Param { ty, .. } = ast.get_leaf_data(param).unwrap() else {
            unreachable!("param node carries a param payload");
        };
        param_values.push(context.create_value(lower_ctype(context, ty), None));
    }
    let param_ids: Vec<ValueId> = param_values.iter().map(|v| v.id()).collect();

    let region = context.create_region();
    let block = context.create_block(param_values);
    region.add_block(block.id());

    let func_op = b::func(context, name.as_str(), ret_ty, Some(region.id())).build();

    let mut cg = FnCodegen {
        context,
        ast,
        builder: IRBuilder::new(func_op.body()),
        locals: HashMap::new(),
        values: Vec::new(),
    };
    cg.lower_body(func, &param_ids)?;

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

    /// Lower a function: spill parameters into stack slots, then lower each body
    /// statement in source order (statement order is a side-effect ordering, so it
    /// stays top-down; only the expressions within use the post-order iterator).
    fn lower_body(&mut self, func: NodeId, param_ids: &[ValueId]) -> Result<(), String> {
        let ast = self.ast;

        let mut idx = 0;
        for param in ast
            .children(func)
            .take_while(|&c| matches!(ast.get_node(c), AstKind::Param))
        {
            let AstLeaf::Param { name, ty } = ast.get_leaf_data(param).unwrap() else {
                unreachable!("param node carries a param payload");
            };
            let elem = lower_ctype(self.context, ty);
            let slot = self.alloca(elem);
            self.builder
                .insert(p::store(self.context, param_ids[idx], slot.ptr).build());
            idx += 1;
            self.locals.insert(name.clone(), slot);
        }

        for stmt in ast.children(func).skip(idx) {
            self.lower_stmt(stmt)?;
        }

        Ok(())
    }

    fn lower_stmt(&mut self, stmt: NodeId) -> Result<(), String> {
        let ast = self.ast;
        match ast.get_node(stmt) {
            AstKind::Decl => {
                let AstLeaf::Decl { name, ty } = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("decl node carries a decl payload");
                };
                let elem = lower_ctype(self.context, ty);
                let slot = self.alloca(elem);
                if let Some(init) = ast.children(stmt).next() {
                    let value = self.lower_expr(init)?;
                    self.builder
                        .insert(p::store(self.context, value, slot.ptr).build());
                }
                self.locals.insert(name.clone(), slot);
                Ok(())
            }
            AstKind::Assign => {
                let AstLeaf::Assign(name) = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("assign node carries an assign payload");
                };
                let slot = *self
                    .locals
                    .get(name)
                    .ok_or_else(|| format!("assignment to unknown variable '{name}'"))?;
                let value = ast.children(stmt).next().unwrap();
                let v = self.lower_expr(value)?;
                self.builder
                    .insert(p::store(self.context, v, slot.ptr).build());
                Ok(())
            }
            AstKind::Return => {
                let operand = match ast.children(stmt).next() {
                    Some(e) => Operand::from(self.lower_expr(e)?),
                    None => Operand::none(),
                };
                self.builder
                    .insert(b::r#return(self.context, operand).build());
                Ok(())
            }
            kind => unreachable!("not a statement: {kind:?}"),
        }
    }

    /// Lower an expression subtree in one post-order pass: operands precede their
    /// operator, so each node's value is ready when its parent is reached. The
    /// AST is a tree, so the subtree is a contiguous index range `[base, root]`;
    /// values are pushed in index order, letting children be read by offset
    /// without hashing.
    fn lower_expr(&mut self, root: NodeId) -> Result<ValueId, String> {
        let ast = self.ast;
        let i32_ty = IntegerType::new(self.context, 32);
        self.values.clear();
        let mut base = root.index();

        for node in ast.postorder_from(root) {
            if self.values.is_empty() {
                base = node.index();
            }
            debug_assert_eq!(
                node.index(),
                base + self.values.len(),
                "subtree not contiguous"
            );

            let value = match ast.get_node(node) {
                AstKind::Int => {
                    let AstLeaf::Int(n) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("int node carries an int payload");
                    };
                    self.builder
                        .insert(b::constant(self.context, *n, i32_ty).build())
                        .result()
                }
                AstKind::Var => {
                    let AstLeaf::Var(name) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("var node carries a var payload");
                    };
                    let slot = *self
                        .locals
                        .get(name)
                        .ok_or_else(|| format!("use of unknown variable '{name}'"))?;
                    self.builder
                        .insert(p::load(self.context, slot.ptr, slot.elem).build())
                        .result()
                }
                kind @ (AstKind::Add | AstKind::Sub | AstKind::Mul) => {
                    let kind = *kind;
                    let mut children = ast.children(node);
                    let l = self.values[children.next().unwrap().index() - base];
                    let r = self.values[children.next().unwrap().index() - base];
                    match kind {
                        AstKind::Add => self
                            .builder
                            .insert(b::addi(self.context, l, r, i32_ty).build())
                            .result(),
                        AstKind::Sub => self
                            .builder
                            .insert(b::subi(self.context, l, r, i32_ty).build())
                            .result(),
                        _ => self
                            .builder
                            .insert(b::muli(self.context, l, r, i32_ty).build())
                            .result(),
                    }
                }
                kind => unreachable!("not an expression: {kind:?}"),
            };
            self.values.push(value);
        }

        Ok(*self.values.last().unwrap())
    }
}
