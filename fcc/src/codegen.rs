//! Lowers the C [`crate::ast`] to TIR using the `builtin`, `ptr`, `scf` and
//! `cir` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any mem2reg pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops. Control flow stays structured: `if` lowers to `scf.if`, loops
//! to the `cir` loop ops, and `break`/`continue` to the matching `cir` ops
//! naming the enclosing loop's token. Only `int` (lowered to `i32`) and `void`
//! are supported.

use std::collections::HashMap;

use tir::builtin::{IntegerType, ModuleOp, TokenType, UnitType, ops as b};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::scf::ops as scf;
use tir::{
    Block, Context, IRBuilder, Operand, Operation, RegionId, Terminator, TypeId, Value, ValueId,
};

use crate::ast::*;
use crate::cir::ops as c;
use crate::diagnostics::{
    Diagnostic, EmptyTranslationUnit, UndeclaredIdentifier, UnsupportedConstruct,
};

/// Which terminator closes a region whose body falls off its end: a loop
/// `body`/`step` re-enters the loop via `cir.yield`, an `scf.if` arm merges
/// back via `scf.yield`.
#[derive(Clone, Copy)]
enum Fallthrough {
    LoopBack,
    IfMerge,
}

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
    /// Tokens of the loops enclosing the statement being lowered, innermost
    /// last. `break`/`continue` name the top one.
    loop_tokens: Vec<ValueId>,
}

/// Lower a translation unit into a `builtin.module` in `context`.
pub fn codegen(context: &Context, ast: &Ast) -> Result<ModuleOp, Diagnostic> {
    let module = b::module(context, None).build();
    let mut module_builder = IRBuilder::new(module.body());

    let root = ast.root().ok_or_else(EmptyTranslationUnit::new)?;
    for func in ast.children(root) {
        let func_op = lower_function(context, ast, func)?;
        module_builder.insert(func_op);
    }
    module_builder.insert(b::module_end(context).build());
    Ok(module)
}

/// Use of a name with no declaration in scope, spanned at the offending node.
fn undeclared(ast: &Ast, node: NodeId, name: &str) -> Diagnostic {
    UndeclaredIdentifier::new(ast.get_node(node).span, name).into()
}

/// A construct the parser accepts but codegen does not lower yet.
fn unsupported(ast: &Ast, node: NodeId, what: String) -> Diagnostic {
    UnsupportedConstruct::new(ast.get_node(node).span, what).into()
}

fn lower_ctype(context: &Context, ty: &CType) -> TypeId {
    match ty {
        CType::Int => IntegerType::new(context, 32),
        CType::Void => UnitType::new(context),
    }
}

fn lower_function(
    context: &Context,
    ast: &Ast,
    func: NodeId,
) -> Result<impl Operation, Diagnostic> {
    let AstLeaf::Function { name, ret } = ast.get_leaf_data(func).unwrap() else {
        unreachable!("function node carries a function payload");
    };
    let ret_ty = lower_ctype(context, ret);

    // Entry block arguments carry the incoming parameter values; parameters are
    // the function node's leading children.
    let mut param_values = Vec::new();
    for param in ast
        .children(func)
        .take_while(|&c| matches!(ast.get_node(c).kind, AstKind::Param))
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
        loop_tokens: Vec::new(),
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
    fn lower_body(&mut self, func: NodeId, param_ids: &[ValueId]) -> Result<(), Diagnostic> {
        let ast = self.ast;

        let mut idx = 0;
        for param in ast
            .children(func)
            .take_while(|&c| matches!(ast.get_node(c).kind, AstKind::Param))
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

    fn lower_stmt(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        match ast.get_node(stmt).kind {
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
                    .ok_or_else(|| undeclared(ast, stmt, name))?;
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
            AstKind::Block => {
                for child in ast.children(stmt) {
                    self.lower_stmt(child)?;
                }
                Ok(())
            }
            AstKind::Empty => Ok(()),
            AstKind::ExprStmt => {
                let expr = ast.children(stmt).next().unwrap();
                self.lower_expr(expr)?;
                Ok(())
            }
            AstKind::If => self.lower_if(stmt),
            AstKind::While => self.lower_while(stmt),
            AstKind::DoWhile => self.lower_do_while(stmt),
            AstKind::For => self.lower_for(stmt),
            kind @ (AstKind::Break | AstKind::Continue) => {
                let token = *self
                    .loop_tokens
                    .last()
                    .ok_or_else(|| unsupported(ast, stmt, format!("{kind:?} outside a loop")))?;
                if kind == AstKind::Break {
                    self.builder.insert(c::r#break(self.context, token).build());
                } else {
                    self.builder
                        .insert(c::r#continue(self.context, token).build());
                }
                Ok(())
            }
            kind => Err(unsupported(ast, stmt, format!("statement {kind:?}"))),
        }
    }

    /// Run `f` with the builder temporarily inserting into `block`, restoring
    /// the previous insertion point afterwards.
    fn with_block<R>(&mut self, block: std::sync::Arc<Block>, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = std::mem::replace(&mut self.builder, IRBuilder::new(block));
        let result = f(self);
        self.builder = saved;
        result
    }

    /// True if `block` already ends in a terminator (e.g. a trailing `return`,
    /// `break` or `continue`), so no fall-through terminator is needed.
    fn is_terminated(&self, block: &Block) -> bool {
        block.op_ids().last().is_some_and(|id| {
            self.context
                .get_op(*id)
                .as_interface::<dyn Terminator>()
                .is_some()
        })
    }

    /// Build a fresh single-block region, lower `f` into it, and append the
    /// fall-through terminator unless `f` already terminated the block.
    fn region(
        &mut self,
        args: Vec<Value>,
        fallthrough: Fallthrough,
        f: impl FnOnce(&mut Self) -> Result<(), Diagnostic>,
    ) -> Result<RegionId, Diagnostic> {
        let region = self.context.create_region();
        let block = self.context.create_block(args);
        region.add_block(block.id());
        self.with_block(block.clone(), f)?;
        if !self.is_terminated(&block) {
            let mut builder = IRBuilder::new(block);
            match fallthrough {
                Fallthrough::LoopBack => {
                    builder.insert(c::r#yield(self.context).build());
                }
                Fallthrough::IfMerge => {
                    builder.insert(scf::r#yield(self.context, Operand::none()).build());
                }
            }
        }
        Ok(region.id())
    }

    /// Build a loop `cond` region: lower the condition to `i1` and end with
    /// `cir.condition`.
    fn cond_region(&mut self, cond: NodeId) -> Result<RegionId, Diagnostic> {
        let region = self.context.create_region();
        let block = self.context.create_block(vec![]);
        region.add_block(block.id());
        self.with_block(block, |cg| -> Result<(), Diagnostic> {
            let value = cg.lower_condition(cond)?;
            cg.builder.insert(c::condition(cg.context, value).build());
            Ok(())
        })?;
        Ok(region.id())
    }

    /// Create the `!token` value that becomes a loop body's entry argument and
    /// the loop's handle.
    fn loop_token(&self) -> Value {
        self.context
            .create_value(TokenType::new(self.context), None)
    }

    fn lower_if(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let mut children = self.ast.children(stmt);
        let cond = children.next().unwrap();
        let then = children.next().unwrap();
        let els = children.next();

        let condition = self.lower_condition(cond)?;
        let then_region = self.region(vec![], Fallthrough::IfMerge, |cg| cg.lower_stmt(then))?;
        let else_region = self.region(vec![], Fallthrough::IfMerge, |cg| match els {
            Some(e) => cg.lower_stmt(e),
            None => Ok(()),
        })?;
        self.builder.insert(
            scf::r#if(
                self.context,
                condition,
                Some(then_region),
                Some(else_region),
            )
            .build(),
        );
        Ok(())
    }

    fn lower_while(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let mut children = self.ast.children(stmt);
        let cond = children.next().unwrap();
        let body = children.next().unwrap();

        // Create the token before the regions so its value number precedes
        // every value defined inside them; the printer surfaces it in the op
        // header (ahead of the regions), and the parser, which numbers values
        // by textual position, only round-trips if that order is monotonic.
        let token = self.loop_token();
        let cond_region = self.cond_region(cond)?;
        let body_region = self.lower_loop_body(token, body)?;
        self.builder
            .insert(c::r#while(self.context, Some(cond_region), Some(body_region)).build());
        Ok(())
    }

    fn lower_do_while(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let mut children = self.ast.children(stmt);
        let body = children.next().unwrap();
        let cond = children.next().unwrap();

        // See `lower_while`: the token's number must precede the regions'.
        let token = self.loop_token();
        let body_region = self.lower_loop_body(token, body)?;
        let cond_region = self.cond_region(cond)?;
        self.builder
            .insert(c::r#do(self.context, Some(body_region), Some(cond_region)).build());
        Ok(())
    }

    fn lower_for(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let mut children = self.ast.children(stmt);
        let init = children.next().unwrap();
        let cond = children.next().unwrap();
        let step = children.next().unwrap();
        let body = children.next().unwrap();

        self.lower_stmt(init)?;
        // See `lower_while`: the token's number must precede the regions'.
        let token = self.loop_token();
        let cond_region = self.cond_region(cond)?;
        let body_region = self.lower_loop_body(token, body)?;
        let step_region = self.region(vec![], Fallthrough::LoopBack, |cg| cg.lower_stmt(step))?;
        self.builder.insert(
            c::r#for(
                self.context,
                Some(cond_region),
                Some(body_region),
                Some(step_region),
            )
            .build(),
        );
        Ok(())
    }

    /// Lower a loop body into a region whose entry block carries `token`, with
    /// that token pushed so nested `break`/`continue` resolve to this loop.
    fn lower_loop_body(&mut self, token: Value, body: NodeId) -> Result<RegionId, Diagnostic> {
        let token_id = token.id();
        self.loop_tokens.push(token_id);
        let region = self.region(vec![token], Fallthrough::LoopBack, |cg| cg.lower_stmt(body));
        self.loop_tokens.pop();
        region
    }

    /// Lower a controlling expression to an `i1`: a relational operator becomes
    /// the matching `cmpi`, an omitted `for` condition is constant true, and any
    /// other integer expression is compared against zero (C's "non-zero").
    fn lower_condition(&mut self, cond: NodeId) -> Result<ValueId, Diagnostic> {
        let i1 = IntegerType::new(self.context, 1);
        let kind = self.ast.get_node(cond).kind;

        if kind == AstKind::Empty {
            return Ok(self
                .builder
                .insert(b::constant(self.context, 1, i1).build())
                .result());
        }

        let predicate = match kind {
            AstKind::Lt => "slt",
            AstKind::Le => "sle",
            AstKind::Gt => "sgt",
            AstKind::Ge => "sge",
            AstKind::Eq => "eq",
            AstKind::Ne => "ne",
            _ => {
                let value = self.lower_expr(cond)?;
                let i32_ty = IntegerType::new(self.context, 32);
                let zero = self
                    .builder
                    .insert(b::constant(self.context, 0, i32_ty).build())
                    .result();
                let cmp = b::CmpIOpBuilder::new(self.context)
                    .lhs(value)
                    .rhs(zero)
                    .result_type(i1)
                    .predicate("ne")
                    .build();
                return Ok(self.builder.insert(cmp).result());
            }
        };

        let mut operands = self.ast.children(cond);
        let lhs = operands.next().unwrap();
        let rhs = operands.next().unwrap();
        let lhs = self.lower_expr(lhs)?;
        let rhs = self.lower_expr(rhs)?;
        let cmp = b::CmpIOpBuilder::new(self.context)
            .lhs(lhs)
            .rhs(rhs)
            .result_type(i1)
            .predicate(predicate)
            .build();
        Ok(self.builder.insert(cmp).result())
    }

    /// Lower an expression subtree in one post-order pass: operands precede their
    /// operator, so each node's value is ready when its parent is reached. The
    /// AST is a tree, so the subtree is a contiguous index range `[base, root]`;
    /// values are pushed in index order, letting children be read by offset
    /// without hashing.
    fn lower_expr(&mut self, root: NodeId) -> Result<ValueId, Diagnostic> {
        let ast = self.ast;
        let i32_ty = IntegerType::new(self.context, 32);
        self.values.clear();
        let mut base = root.index();

        for node in ast.postorder(root) {
            if self.values.is_empty() {
                base = node.index();
            }
            debug_assert_eq!(
                node.index(),
                base + self.values.len(),
                "subtree not contiguous"
            );

            let value = match ast.get_node(node).kind {
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
                        .ok_or_else(|| undeclared(ast, node, name))?;
                    self.builder
                        .insert(p::load(self.context, slot.ptr, slot.elem).build())
                        .result()
                }
                kind @ (AstKind::Add | AstKind::Sub | AstKind::Mul) => {
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
                // The richer operators (division, comparison, logical, unary,
                // calls) are parsed but not yet lowered; stub them out for now.
                kind => {
                    return Err(unsupported(ast, node, format!("expression {kind:?}")));
                }
            };
            self.values.push(value);
        }

        Ok(*self.values.last().unwrap())
    }
}
