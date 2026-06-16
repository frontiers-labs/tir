//! Lowers the C [`crate::ast`] to TIR using the `builtin` and `ptr` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any mem2reg pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops. Only `int` (lowered to `i32`) and `void` are supported.

use std::collections::HashMap;
use std::sync::Arc;

use tir::builtin::{IntegerType, ModuleOp, UnitType, ops as b};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::{
    Block, BlockId, Context, IRBuilder, Operand, Operation, Region, Terminator, TypeId, ValueId,
};

use crate::ast::*;
use crate::diagnostics::{
    Diagnostic, EmptyTranslationUnit, UndeclaredIdentifier, UnsupportedConstruct,
};

/// A local variable: the pointer to its stack slot and the slot's element type.
#[derive(Clone, Copy)]
struct Slot {
    ptr: ValueId,
    elem: TypeId,
}

struct FnCodegen<'a> {
    context: &'a Context,
    ast: &'a Ast,
    /// The function body region; control flow appends fresh blocks to it.
    region: Arc<Region>,
    builder: IRBuilder,
    /// The block the builder currently inserts into.
    cur_block: Arc<Block>,
    locals: HashMap<String, Slot>,
    /// Scratch holding the lowered SSA value of each node in the expression
    /// subtree currently being lowered, indexed by `node.index() - base`. Reused
    /// across expressions to avoid reallocating.
    values: Vec<ValueId>,
    /// `(continue target, break target)` for each enclosing loop.
    loops: Vec<(BlockId, BlockId)>,
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

/// Whether `kind` names an expression node (one that produces a value), as
/// opposed to a statement or structural node.
fn is_expr_kind(kind: AstKind) -> bool {
    use AstKind::*;
    matches!(
        kind,
        Add | Sub
            | Mul
            | Div
            | Mod
            | Lt
            | Gt
            | Le
            | Ge
            | Eq
            | Ne
            | LogAnd
            | LogOr
            | Neg
            | Not
            | Call
            | Var
            | Int
    )
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

    let entry = func_op.body();
    let mut cg = FnCodegen {
        context,
        ast,
        region,
        builder: IRBuilder::new(entry.clone()),
        cur_block: entry,
        locals: HashMap::new(),
        values: Vec::new(),
        loops: Vec::new(),
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

    /// Append a fresh empty block to the function region.
    fn new_block(&mut self) -> Arc<Block> {
        let block = self.context.create_block(vec![]);
        self.region.add_block(block.id());
        block
    }

    /// Point the builder at `block`, making it the current insertion target.
    fn switch_to(&mut self, block: Arc<Block>) {
        self.builder = IRBuilder::new(block.clone());
        self.cur_block = block;
    }

    /// Whether the current block already ends in a terminator.
    fn terminated(&self) -> bool {
        self.cur_block.op_ids().last().is_some_and(|id| {
            self.context
                .get_op(*id)
                .as_interface::<dyn Terminator>()
                .is_some()
        })
    }

    /// Emit an unconditional branch to `dest`, unless the block already ends in a
    /// terminator (the body fell through a `return`/`break`/`continue`).
    fn branch_to(&mut self, dest: BlockId) {
        if !self.terminated() {
            self.builder
                .insert(b::br(self.context, vec![], dest).build());
        }
    }

    /// Lower an expression used as a boolean test into an `i1`. A relational or
    /// equality operator becomes a single `cmpi`; any other expression is
    /// compared against zero (C truthiness).
    fn lower_condition(&mut self, node: NodeId) -> Result<ValueId, Diagnostic> {
        let ast = self.ast;
        let i1 = IntegerType::new(self.context, 1);
        let predicate = match ast.get_node(node).kind {
            AstKind::Lt => "slt",
            AstKind::Gt => "sgt",
            AstKind::Le => "sle",
            AstKind::Ge => "sge",
            AstKind::Eq => "eq",
            AstKind::Ne => "ne",
            _ => {
                let value = self.lower_expr(node)?;
                let i32_ty = IntegerType::new(self.context, 32);
                let zero = self
                    .builder
                    .insert(b::constant(self.context, 0, i32_ty).build())
                    .result();
                return Ok(self
                    .builder
                    .insert(b::cmpi(self.context, value, zero, "ne", i1).build())
                    .result());
            }
        };
        let mut children = ast.children(node);
        let lhs = children.next().unwrap();
        let rhs = children.next().unwrap();
        let l = self.lower_expr(lhs)?;
        let r = self.lower_expr(rhs)?;
        Ok(self
            .builder
            .insert(b::cmpi(self.context, l, r, predicate, i1).build())
            .result())
    }

    fn lower_if(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(stmt);
        let cond = children.next().unwrap();
        let then = children.next().unwrap();
        let els = children.next();

        let cv = self.lower_condition(cond)?;
        let then_blk = self.new_block();
        let else_blk = els.map(|_| self.new_block());
        let join_blk = self.new_block();
        let false_dest = else_blk.as_ref().unwrap_or(&join_blk).id();
        self.builder.insert(
            b::cond_br(self.context, cv, vec![], vec![], then_blk.id(), false_dest).build(),
        );

        self.switch_to(then_blk);
        self.lower_stmt(then)?;
        self.branch_to(join_blk.id());

        if let (Some(els), Some(else_blk)) = (els, else_blk) {
            self.switch_to(else_blk);
            self.lower_stmt(els)?;
            self.branch_to(join_blk.id());
        }

        self.switch_to(join_blk);
        Ok(())
    }

    fn lower_while(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(stmt);
        let cond = children.next().unwrap();
        let body = children.next().unwrap();

        let cond_blk = self.new_block();
        let body_blk = self.new_block();
        let join_blk = self.new_block();

        self.branch_to(cond_blk.id());
        self.switch_to(cond_blk.clone());
        let cv = self.lower_condition(cond)?;
        self.builder.insert(
            b::cond_br(
                self.context,
                cv,
                vec![],
                vec![],
                body_blk.id(),
                join_blk.id(),
            )
            .build(),
        );

        self.switch_to(body_blk);
        self.loops.push((cond_blk.id(), join_blk.id()));
        self.lower_stmt(body)?;
        self.loops.pop();
        self.branch_to(cond_blk.id());

        self.switch_to(join_blk);
        Ok(())
    }

    fn lower_do_while(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(stmt);
        let body = children.next().unwrap();
        let cond = children.next().unwrap();

        let body_blk = self.new_block();
        let cond_blk = self.new_block();
        let join_blk = self.new_block();

        self.branch_to(body_blk.id());
        self.switch_to(body_blk.clone());
        self.loops.push((cond_blk.id(), join_blk.id()));
        self.lower_stmt(body)?;
        self.loops.pop();
        self.branch_to(cond_blk.id());

        self.switch_to(cond_blk);
        let cv = self.lower_condition(cond)?;
        self.builder.insert(
            b::cond_br(
                self.context,
                cv,
                vec![],
                vec![],
                body_blk.id(),
                join_blk.id(),
            )
            .build(),
        );

        self.switch_to(join_blk);
        Ok(())
    }

    fn lower_for(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        let mut children = ast.children(stmt);
        let init = children.next().unwrap();
        let cond = children.next().unwrap();
        let step = children.next().unwrap();
        let body = children.next().unwrap();

        self.lower_stmt(init)?;

        let cond_blk = self.new_block();
        let body_blk = self.new_block();
        let step_blk = self.new_block();
        let join_blk = self.new_block();

        self.branch_to(cond_blk.id());
        self.switch_to(cond_blk.clone());
        if matches!(ast.get_node(cond).kind, AstKind::Empty) {
            self.branch_to(body_blk.id());
        } else {
            let cv = self.lower_condition(cond)?;
            self.builder.insert(
                b::cond_br(
                    self.context,
                    cv,
                    vec![],
                    vec![],
                    body_blk.id(),
                    join_blk.id(),
                )
                .build(),
            );
        }

        self.switch_to(body_blk);
        self.loops.push((step_blk.id(), join_blk.id()));
        self.lower_stmt(body)?;
        self.loops.pop();
        self.branch_to(step_blk.id());

        self.switch_to(step_blk);
        self.lower_stmt(step)?;
        self.branch_to(cond_blk.id());

        self.switch_to(join_blk);
        Ok(())
    }

    fn lower_stmt(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        // Statements following a `return`/`break`/`continue` are unreachable; the
        // current block is already closed, so skip them rather than appending past
        // a terminator.
        if self.terminated() {
            return Ok(());
        }
        let ast = self.ast;
        match ast.get_node(stmt).kind {
            AstKind::Block => {
                for child in ast.children(stmt) {
                    self.lower_stmt(child)?;
                }
                Ok(())
            }
            AstKind::ExprStmt => {
                let expr = ast.children(stmt).next().unwrap();
                self.lower_expr(expr)?;
                Ok(())
            }
            AstKind::If => self.lower_if(stmt),
            AstKind::While => self.lower_while(stmt),
            AstKind::DoWhile => self.lower_do_while(stmt),
            AstKind::For => self.lower_for(stmt),
            AstKind::Break => {
                let &(_, brk) = self
                    .loops
                    .last()
                    .ok_or_else(|| unsupported(ast, stmt, "break outside loop".to_string()))?;
                self.builder
                    .insert(b::br(self.context, vec![], brk).build());
                Ok(())
            }
            AstKind::Continue => {
                let &(cont, _) = self
                    .loops
                    .last()
                    .ok_or_else(|| unsupported(ast, stmt, "continue outside loop".to_string()))?;
                self.builder
                    .insert(b::br(self.context, vec![], cont).build());
                Ok(())
            }
            AstKind::Empty => Ok(()),
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
            // A bare expression in statement position (e.g. a `for` step clause
            // that is not an assignment): evaluate it for its side effects.
            kind if is_expr_kind(kind) => {
                self.lower_expr(stmt)?;
                Ok(())
            }
            kind => Err(unsupported(ast, stmt, format!("statement {kind:?}"))),
        }
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
                AstKind::Neg => {
                    let child = ast.children(node).next().unwrap();
                    let x = self.values[child.index() - base];
                    let zero = self
                        .builder
                        .insert(b::constant(self.context, 0, i32_ty).build())
                        .result();
                    self.builder
                        .insert(b::subi(self.context, zero, x, i32_ty).build())
                        .result()
                }
                // The remaining operators (division, logical, calls) are parsed
                // but not yet lowered; stub them out for now.
                kind => {
                    return Err(unsupported(ast, node, format!("expression {kind:?}")));
                }
            };
            self.values.push(value);
        }

        Ok(*self.values.last().unwrap())
    }
}
