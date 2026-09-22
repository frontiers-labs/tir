//! Lowers statements and control flow inside functions.

use super::{
    AbiParameter, ExitTarget, FnCodegen, Slot, SwitchItem, abi_storage_layout,
    crosses_loop_boundary, lower_type, node_entity, node_type, source_type_layout, unsupported,
};
use crate::ast::{AstKind, AstLeaf};
use crate::cir;
use crate::diagnostics::Diagnostic;
use crate::sema::{QualType, TypeKind};
use tir::attributes::Predicate;
use tir::builtin::{FloatType, IntegerType, UnitType, ops as b};
use tir::cfg::ops as cb;
use tir::fp::ops as fp;
use tir::func::ops as func_ops;
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::{Operand, Operation, TypeId, ValueId};

impl FnCodegen<'_> {
    /// Lower a function: spill parameters into stack slots, then lower each body
    /// statement in source order (statement order is a side-effect ordering, so it
    /// stays top-down; only the expressions within use the post-order iterator).
    pub(super) fn lower_body(
        &mut self,
        func: NodeId,
        param_ids: &[ValueId],
        abi_params: &[AbiParameter],
    ) -> Result<(), Diagnostic> {
        let ast = self.ast;

        let params = ast
            .children(func)
            .take_while(|&c| matches!(ast.get_node(c).kind, AstKind::Param))
            .collect::<Vec<_>>();
        let mut abi_value = 0;
        for (&param, abi_param) in params.iter().zip(abi_params) {
            let AstLeaf::Param { .. } = ast.get_leaf_data(param).unwrap() else {
                unreachable!("param node carries a param payload");
            };
            let source_ty = node_type(self.typed, param);
            let elem = lower_type(self.context, self.typed, source_ty);
            let (size, align) = source_type_layout(self.typed, source_ty);
            if abi_param.indirect {
                self.locals.insert(
                    node_entity(self.typed, param),
                    Slot {
                        ptr: param_ids[abi_value],
                        elem,
                    },
                );
                abi_value += 1;
                continue;
            }
            let (abi_size, abi_align) =
                if matches!(self.typed.types().kind(source_ty), TypeKind::Record(_)) {
                    abi_storage_layout(self.context, &abi_param.pieces).unwrap_or((size, align))
                } else {
                    (size, align)
                };
            let slot = self.alloca(elem, size.max(abi_size), align.max(abi_align));
            let values = if abi_param.grouped {
                let tuple = param_ids[abi_value];
                abi_value += 1;
                abi_param
                    .pieces
                    .iter()
                    .enumerate()
                    .map(|(index, piece)| {
                        self.emit(
                            b::TupleGetOpBuilder::new(self.context)
                                .tuple(tuple)
                                .index(index as u64)
                                .result_type(piece.ty)
                                .build(),
                        )
                        .result()
                    })
                    .collect::<Vec<_>>()
            } else {
                let values = param_ids[abi_value..abi_value + abi_param.pieces.len()].to_vec();
                abi_value += abi_param.pieces.len();
                values
            };
            for (piece, value) in abi_param.pieces.iter().zip(values) {
                let address = self.offset_address(slot.ptr, piece.offset);
                self.emit(p::store(self.context, value, address).build());
            }
            self.locals.insert(node_entity(self.typed, param), slot);
        }

        let TypeKind::Function { ret: result, .. } =
            self.typed.types().kind(node_type(self.typed, func))
        else {
            unreachable!("function node has function type");
        };
        let result = *result;
        let returns_void = matches!(self.typed.types().kind(result), TypeKind::Void);

        let statements = ast.children(func).skip(params.len()).collect::<Vec<_>>();
        self.lower_statements(&statements, result, returns_void)
    }

    /// Lower a function body, structured loops apart, as a flat graph of blocks
    /// and branches: a `goto` is an edge like any other, and the `restructure`
    /// pass raises the whole body back to structured control flow. Every `return`
    /// stores its value and leaves through the one exit block.
    fn lower_statements(
        &mut self,
        statements: &[NodeId],
        result: QualType,
        returns_void: bool,
    ) -> Result<(), Diagnostic> {
        // A label can be jumped over the declaration it precedes, so every slot
        // is opened in the entry block, which dominates the whole body.
        for &statement in statements {
            self.hoist_declarations(statement);
        }
        self.structured_loops = statements
            .iter()
            .all(|&statement| !crosses_loop_boundary(self.ast, statement, false));
        if !returns_void {
            self.result_type = Some(result);
            self.open_return_value_slot(result);
        }
        let exit = self.context.create_block(vec![]);
        self.exit_block = Some(exit.clone());

        for &statement in statements {
            self.lower_stmt(statement)?;
        }
        self.store_main_exit_status(result);
        self.leave_block(&exit);

        self.context.get_region(self.region).add_block(exit.id());
        self.enter_block(exit);
        let operand = self.return_operand(result, returns_void);
        self.emit(func_ops::r#return(self.context, operand).build());
        self.terminated = true;
        Ok(())
    }

    fn hoist_declarations(&mut self, statement: NodeId) {
        let ast = self.ast;
        if ast.get_node(statement).kind == AstKind::Decl {
            let slot = self.declare_slot(statement);
            self.locals.insert(node_entity(self.typed, statement), slot);
        }
        for child in ast.children(statement) {
            self.hoist_declarations(child);
        }
    }

    fn declare_slot(&mut self, statement: NodeId) -> Slot {
        let source_ty = node_type(self.typed, statement);
        let elem = match self.typed.types().kind(source_ty) {
            TypeKind::Array(element, Some(_)) => {
                let element = *element;
                lower_type(self.context, self.typed, element)
            }
            _ => lower_type(self.context, self.typed, source_ty),
        };
        let (size, align) = source_type_layout(self.typed, source_ty);
        self.alloca(elem, size, align)
    }

    /// A fresh block of the function body, appended after the ones emitted so
    /// far.
    pub(super) fn new_block(&mut self) -> tir::BlockHandle {
        let block = self.context.create_block(vec![]);
        self.context.get_region(self.region).add_block(block.id());
        block
    }

    /// Continue emitting into `block`, which the branch that reaches it has
    /// already been emitted for.
    pub(super) fn enter_block(&mut self, block: tir::BlockHandle) {
        self.builder = block;
        self.terminated = false;
    }

    /// End the current block by falling through to `block`, unless it already
    /// left through a branch of its own.
    fn leave_block(&mut self, block: &tir::BlockHandle) {
        self.branch_to(block, vec![]);
    }

    /// End the current block by branching to `block` with `arguments`, unless
    /// it already left through a branch of its own.
    pub(super) fn branch_to(&mut self, block: &tir::BlockHandle, arguments: Vec<ValueId>) {
        if self.terminated {
            return;
        }
        self.emit(cb::br(self.context, arguments, block.id()).build());
        self.terminated = true;
    }

    pub(super) fn branch_on(
        &mut self,
        condition: ValueId,
        if_true: &tir::BlockHandle,
        if_false: &tir::BlockHandle,
    ) {
        self.emit(
            cb::cond_br(
                self.context,
                condition,
                vec![],
                vec![],
                if_true.id(),
                if_false.id(),
            )
            .build(),
        );
        self.terminated = true;
    }

    /// The block a label names, shared by the label itself and every `goto`
    /// that reaches it, whichever comes first in the source.
    fn label_block(&mut self, statement: NodeId) -> tir::BlockHandle {
        let Some(AstLeaf::Label(name)) = self.ast.get_leaf_data(statement) else {
            unreachable!("label and goto nodes carry a label payload");
        };
        if let Some(block) = self.label_blocks.get(name) {
            return block.clone();
        }
        let name = name.clone();
        let block = self.new_block();
        self.label_blocks.insert(name, block.clone());
        block
    }

    /// Lower one statement of a function that holds a label. Control flow
    /// becomes branches between blocks; everything else lowers exactly as it
    /// does in a structured function.
    fn lower_stmt(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        if self.terminated {
            // What follows a branch is unreachable, but it may still hold a
            // label control arrives at, so it is lowered into a block of its own.
            let unreachable = self.new_block();
            self.enter_block(unreachable);
        }
        match ast.get_node(stmt).kind {
            AstKind::Block | AstKind::DeclGroup => {
                for child in ast.children(stmt).collect::<Vec<_>>() {
                    self.lower_stmt(child)?;
                }
                Ok(())
            }
            AstKind::Label => {
                let block = self.label_block(stmt);
                self.leave_block(&block);
                self.enter_block(block);
                self.lower_stmt(ast.children(stmt).next().unwrap())
            }
            AstKind::Goto => {
                let block = self.label_block(stmt);
                self.leave_block(&block);
                Ok(())
            }
            AstKind::Return => {
                self.store_return_value(ast.children(stmt).next())?;
                let exit = self.exit_block.clone().unwrap();
                self.leave_block(&exit);
                Ok(())
            }
            AstKind::Break => {
                match self.break_targets.last().cloned().unwrap() {
                    ExitTarget::Block(block) => self.leave_block(&block),
                    ExitTarget::Loop => {
                        self.terminate_with(cir::ops::r#break(self.context).build())
                    }
                }
                Ok(())
            }
            AstKind::Continue => {
                match self.continue_targets.last().cloned().unwrap() {
                    ExitTarget::Block(block) => self.leave_block(&block),
                    ExitTarget::Loop => {
                        self.terminate_with(cir::ops::r#continue(self.context).build())
                    }
                }
                Ok(())
            }
            AstKind::If => {
                let mut children = ast.children(stmt);
                let condition = children.next().unwrap();
                let then_stmt = children.next().unwrap();
                let else_stmt = children.next();
                let condition = self.lower_condition(condition)?;
                let then_block = self.new_block();
                let else_block = self.new_block();
                let join = self.new_block();
                self.branch_on(condition, &then_block, &else_block);

                self.enter_block(then_block);
                self.lower_stmt(then_stmt)?;
                self.leave_block(&join);

                self.enter_block(else_block);
                if let Some(else_stmt) = else_stmt {
                    self.lower_stmt(else_stmt)?;
                }
                self.leave_block(&join);

                self.enter_block(join);
                Ok(())
            }
            AstKind::While => {
                let mut children = ast.children(stmt);
                let condition = children.next().unwrap();
                let body = children.next().unwrap();
                if !self.structured_loops {
                    return self.lower_flat_while(condition, body);
                }
                let condition_region = self.lower_condition_region(condition, false)?;
                let body_region = self.lower_body_region(body)?;
                let op = cir::WhileOpBuilder::new(self.context)
                    .condition_region(condition_region)
                    .body_region(body_region)
                    .build();
                self.emit(op);
                Ok(())
            }
            AstKind::DoWhile => {
                let mut children = ast.children(stmt);
                let body = children.next().unwrap();
                let condition = children.next().unwrap();
                if !self.structured_loops {
                    return self.lower_flat_do_while(body, condition);
                }
                let body_region = self.lower_body_region(body)?;
                let condition_region = self.lower_condition_region(condition, false)?;
                let op = cir::DoOpBuilder::new(self.context)
                    .body_region(body_region)
                    .condition_region(condition_region)
                    .build();
                self.emit(op);
                Ok(())
            }
            AstKind::For => {
                let children = ast.children(stmt).collect::<Vec<_>>();
                let [init, condition, step, body] = *children.as_slice() else {
                    unreachable!("for statement has four children");
                };
                // The init clause runs once, before the loop, so it stays inline.
                if ast.get_node(init).kind != AstKind::Empty {
                    self.lower_stmt(init)?;
                }
                if !self.structured_loops {
                    return self.lower_flat_for(condition, step, body);
                }
                let condition_region = self.lower_condition_region(condition, true)?;
                let body_region = self.lower_body_region(body)?;
                let step_region = self.lower_region(|cg| {
                    cg.lower_for_step(step)?;
                    cg.terminate_with(cir::ops::r#yield(cg.context).build());
                    Ok(())
                })?;
                let op = cir::ForOpBuilder::new(self.context)
                    .condition_region(condition_region)
                    .step_region(step_region)
                    .body_region(body_region)
                    .build();
                self.emit(op);
                Ok(())
            }
            AstKind::Switch => self.lower_switch(stmt),
            _ => self.lower_plain_stmt(stmt),
        }
    }

    fn lower_flat_while(&mut self, condition: NodeId, body: NodeId) -> Result<(), Diagnostic> {
        let header = self.new_block();
        let body_block = self.new_block();
        let exit = self.new_block();
        self.leave_block(&header);

        self.enter_block(header.clone());
        let value = self.lower_condition(condition)?;
        self.branch_on(value, &body_block, &exit);

        self.enter_block(body_block);
        self.lower_flat_loop_body(body, &exit, &header)?;
        self.leave_block(&header);

        self.enter_block(exit);
        Ok(())
    }

    fn lower_flat_do_while(&mut self, body: NodeId, condition: NodeId) -> Result<(), Diagnostic> {
        let body_block = self.new_block();
        let latch = self.new_block();
        let exit = self.new_block();
        self.leave_block(&body_block);

        self.enter_block(body_block.clone());
        self.lower_flat_loop_body(body, &exit, &latch)?;
        self.leave_block(&latch);

        self.enter_block(latch);
        let value = self.lower_condition(condition)?;
        self.branch_on(value, &body_block, &exit);

        self.enter_block(exit);
        Ok(())
    }

    fn lower_flat_for(
        &mut self,
        condition: NodeId,
        step: NodeId,
        body: NodeId,
    ) -> Result<(), Diagnostic> {
        let header = self.new_block();
        let body_block = self.new_block();
        let step_block = self.new_block();
        let exit = self.new_block();
        self.leave_block(&header);

        self.enter_block(header.clone());
        let value = self.lower_for_condition(condition)?;
        self.branch_on(value, &body_block, &exit);

        self.enter_block(body_block);
        self.lower_flat_loop_body(body, &exit, &step_block)?;
        self.leave_block(&step_block);

        self.enter_block(step_block);
        self.lower_for_step(step)?;
        self.leave_block(&header);

        self.enter_block(exit);
        Ok(())
    }

    fn lower_flat_loop_body(
        &mut self,
        body: NodeId,
        exit: &tir::BlockHandle,
        next: &tir::BlockHandle,
    ) -> Result<(), Diagnostic> {
        self.break_targets.push(ExitTarget::Block(exit.clone()));
        self.continue_targets.push(ExitTarget::Block(next.clone()));
        let lowered = self.lower_stmt(body);
        self.continue_targets.pop();
        self.break_targets.pop();
        lowered
    }

    /// Lower a loop's condition into a region of its own, leaving through
    /// `cir.condition`. An omitted `for` condition is the constant true C gives it.
    fn lower_condition_region(
        &mut self,
        condition: NodeId,
        omittable: bool,
    ) -> Result<tir::RegionId, Diagnostic> {
        self.lower_region(|cg| {
            let value = if omittable {
                cg.lower_for_condition(condition)?
            } else {
                cg.lower_condition(condition)?
            };
            cg.terminate_with(cir::ops::condition(cg.context, value).build());
            Ok(())
        })
    }

    /// Lower a loop body into a region of its own, where `break` and `continue`
    /// name the loop op rather than a block.
    fn lower_body_region(&mut self, body: NodeId) -> Result<tir::RegionId, Diagnostic> {
        self.lower_region(|cg| {
            cg.break_targets.push(ExitTarget::Loop);
            cg.continue_targets.push(ExitTarget::Loop);
            let lowered = cg.lower_stmt(body);
            cg.continue_targets.pop();
            cg.break_targets.pop();
            lowered?;
            cg.terminate_with(cir::ops::r#yield(cg.context).build());
            Ok(())
        })
    }

    /// Emit `lower` into a fresh region, restoring the enclosing one afterwards.
    fn lower_region(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Result<(), Diagnostic>,
    ) -> Result<tir::RegionId, Diagnostic> {
        let region = self.context.create_region();
        let entry = self.context.create_block(vec![]);
        region.add_block(entry.id());
        let enclosing = (
            std::mem::replace(&mut self.region, region.id()),
            std::mem::replace(&mut self.builder, entry),
            std::mem::replace(&mut self.terminated, false),
        );
        let lowered = lower(self);
        (self.region, self.builder, self.terminated) = enclosing;
        lowered.map(|()| region.id())
    }

    /// End the block being emitted with `terminator`, unless it already left.
    fn terminate_with(&mut self, terminator: impl Operation) {
        if self.terminated {
            return;
        }
        self.emit(terminator);
        self.terminated = true;
    }

    /// Lower a `switch` as the comparison chain it is: the controlling value is
    /// tested against each case in turn, the arms fall through to one another in
    /// source order, and an unmatched value reaches the default arm or leaves.
    fn lower_switch(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let mut children = self.ast.children(stmt);
        let value = self.lower_expr(children.next().unwrap())?;
        let value_ty = self.context.get_value(value).ty();
        let body = children.next().unwrap();
        let mut items = Vec::new();
        self.flatten_switch_items(body, &mut items)?;

        let exit = self.new_block();
        let arms = items
            .iter()
            .map(|item| match item {
                SwitchItem::Statement(_) => None,
                _ => Some(self.new_block()),
            })
            .collect::<Vec<_>>();
        let default = items.iter().zip(&arms).find_map(|(item, arm)| match item {
            SwitchItem::Default => arm.clone(),
            _ => None,
        });

        for (item, arm) in items.iter().zip(&arms) {
            let (SwitchItem::Case(case), Some(arm)) = (item, arm) else {
                continue;
            };
            let case = self
                .emit(b::constant(self.context, *case, value_ty).build())
                .result();
            let matches = self
                .emit(
                    b::CmpIOpBuilder::new(self.context)
                        .lhs(value)
                        .rhs(case)
                        .predicate(Predicate::Eq)
                        .result_type(IntegerType::new(self.context, 1))
                        .build(),
                )
                .result();
            let next = self.new_block();
            self.branch_on(matches, arm, &next);
            self.enter_block(next);
        }
        self.leave_block(default.as_ref().unwrap_or(&exit));

        self.break_targets.push(ExitTarget::Block(exit.clone()));
        let lowered = self.lower_switch_arms(&items, &arms);
        self.break_targets.pop();
        lowered?;
        self.leave_block(&exit);
        self.enter_block(exit);
        Ok(())
    }

    /// Lower the arms of a `switch` in source order, each one falling through to
    /// the next as C requires of an arm that does not `break`.
    fn lower_switch_arms(
        &mut self,
        items: &[SwitchItem],
        arms: &[Option<tir::BlockHandle>],
    ) -> Result<(), Diagnostic> {
        for (item, arm) in items.iter().zip(arms) {
            match (item, arm) {
                (SwitchItem::Statement(statement), _) => self.lower_stmt(*statement)?,
                (_, Some(arm)) => {
                    self.leave_block(arm);
                    self.enter_block(arm.clone());
                }
                (_, None) => unreachable!("a case or default arm has a block"),
            }
        }
        Ok(())
    }

    /// The slot a `return` leaves its value in, for a function whose returns are
    /// not block terminators. An indirect result is written through the pointer
    /// the caller passed instead.
    fn open_return_value_slot(&mut self, result: QualType) {
        if self.return_abi.indirect {
            return;
        }
        let elem = lower_type(self.context, self.typed, result);
        let (size, align) = source_type_layout(self.typed, result);
        let (size, align) = match self.return_abi.aggregate.as_deref() {
            Some(pieces) => match abi_storage_layout(self.context, pieces) {
                Some((abi_size, abi_align)) => (size.max(abi_size), align.max(abi_align)),
                None => (size, align),
            },
            None => (size, align),
        };
        self.return_slot = Some(self.alloca(elem, size, align));
    }

    /// Stores the zero that C hands back when control reaches the closing brace
    /// of `main` without a `return`.
    fn store_main_exit_status(&mut self, result: QualType) {
        if self.terminated
            || !self.is_main
            || !matches!(self.typed.types().kind(result), TypeKind::Integer(_))
        {
            return;
        }
        let Some(slot) = self.return_slot else {
            return;
        };
        let zero = self
            .emit(b::constant(self.context, 0, slot.elem).build())
            .result();
        self.emit(p::store(self.context, zero, slot.ptr).build());
    }

    fn return_operand(&mut self, result: QualType, returns_void: bool) -> Operand {
        if returns_void {
            return Operand::none();
        }
        if self.return_abi.indirect {
            return if self.return_abi.ty == UnitType::new(self.context) {
                Operand::none()
            } else {
                Operand::from(self.indirect_return.unwrap())
            };
        }
        let slot = self.return_slot.unwrap();
        if self.return_abi.aggregate.is_some() {
            return Operand::from(self.abi_return_value(slot.ptr, result));
        }
        Operand::from(
            self.emit(p::load(self.context, slot.ptr, slot.elem).build())
                .result(),
        )
    }

    fn lower_for_condition(&mut self, condition: NodeId) -> Result<ValueId, Diagnostic> {
        if self.ast.get_node(condition).kind == AstKind::Empty {
            return Ok(self
                .emit(b::constant(self.context, 1, IntegerType::new(self.context, 1)).build())
                .result());
        }
        self.lower_condition(condition)
    }

    fn lower_for_step(&mut self, step: NodeId) -> Result<(), Diagnostic> {
        match self.ast.get_node(step).kind {
            AstKind::Empty => {}
            AstKind::Assign => self.lower_stmt(step)?,
            _ => {
                self.lower_discarded_expr(step)?;
            }
        }
        Ok(())
    }
    /// Lower a statement that carries no control flow of its own.
    fn lower_plain_stmt(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        match ast.get_node(stmt).kind {
            AstKind::EnumDecl | AstKind::Empty => Ok(()),
            AstKind::Decl => {
                let AstLeaf::Decl { .. } = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("decl node carries a decl payload");
                };
                let source_ty = node_type(self.typed, stmt);
                let entity = node_entity(self.typed, stmt);
                let slot = match self.locals.get(&entity) {
                    Some(slot) => *slot,
                    None => self.declare_slot(stmt),
                };
                if let Some(init) = ast.children(stmt).next() {
                    if ast.get_node(init).kind == AstKind::InitializerList {
                        self.lower_initializer(source_ty, slot.ptr, init)?;
                    } else if let TypeKind::Record(id) = self.typed.types().kind(source_ty) {
                        let record = self.typed.record(*id).unwrap().name.clone();
                        self.lower_record_copy(init, slot.ptr, record.as_str())?;
                    } else {
                        let value = self.lower_expr(init)?;
                        self.store_scalar(value, slot.ptr, slot.elem);
                    }
                }
                self.locals.insert(entity, slot);
                Ok(())
            }
            AstKind::Assign => {
                let AstLeaf::Assign(_) = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("assign node carries an assign payload");
                };
                let entity = node_entity(self.typed, stmt);
                let slot = if let Some(slot) = self.locals.get(&entity).copied() {
                    slot
                } else {
                    let global = self.globals[&entity].clone();
                    Slot {
                        ptr: self.symbols.data(self.context, &global.name),
                        elem: global.elem,
                    }
                };
                let value = ast.children(stmt).next().unwrap();
                if let TypeKind::Record(id) = self.typed.types().kind(node_type(self.typed, stmt)) {
                    let record = self.typed.record(*id).unwrap().name.clone();
                    self.lower_record_copy(value, slot.ptr, record.as_str())?;
                } else {
                    let v = self.lower_expr(value)?;
                    self.store_scalar(v, slot.ptr, slot.elem);
                }
                Ok(())
            }
            AstKind::ExprStmt => {
                if let Some(expr) = ast.children(stmt).next() {
                    self.lower_discarded_expr(expr)?;
                }
                Ok(())
            }
            kind => Err(unsupported(ast, stmt, format!("statement {kind:?}"))),
        }
    }

    fn flatten_switch_items(
        &self,
        statement: NodeId,
        items: &mut Vec<SwitchItem>,
    ) -> Result<(), Diagnostic> {
        match self.ast.get_node(statement).kind {
            AstKind::Block => {
                for child in self.ast.children(statement) {
                    self.flatten_switch_items(child, items)?;
                }
            }
            AstKind::Case => {
                let mut children = self.ast.children(statement);
                let value = children.next().unwrap();
                let body = children.next().unwrap();
                let case_value = self
                    .ast
                    .get_annotation(value)
                    .and_then(|annotation| annotation.constant)
                    .ok_or_else(|| unsupported(self.ast, value, "non-constant case".to_string()))?;
                items.push(SwitchItem::Case(case_value));
                self.flatten_switch_items(body, items)?;
            }
            AstKind::Default => {
                items.push(SwitchItem::Default);
                self.flatten_switch_items(self.ast.children(statement).next().unwrap(), items)?;
            }
            _ => items.push(SwitchItem::Statement(statement)),
        }
        Ok(())
    }

    fn lower_condition(&mut self, expression: NodeId) -> Result<ValueId, Diagnostic> {
        let value = self.lower_expr(expression)?;
        Ok(self.truth_value(value))
    }

    pub(super) fn truth_value(&mut self, value: ValueId) -> ValueId {
        let ty = self.context.get_value(value).ty();
        if ty == IntegerType::new(self.context, 1) {
            return value;
        }
        self.compare_against_zero(value, Predicate::Ne)
    }

    pub(super) fn promote_boolean_result(&mut self, value: ValueId, target: TypeId) -> ValueId {
        if self.context.get_value(value).ty() != IntegerType::new(self.context, 1) {
            return value;
        }
        self.emit(b::extui(self.context, value, target).build())
            .result()
    }

    pub(super) fn store_scalar(
        &mut self,
        value: ValueId,
        address: ValueId,
        target: TypeId,
    ) -> ValueId {
        let value = self.promote_boolean_result(value, target);
        self.emit(p::store(self.context, value, address).build());
        value
    }

    /// Compare a scalar with zero using the source domain's equality rules.
    pub(super) fn compare_against_zero(&mut self, value: ValueId, predicate: Predicate) -> ValueId {
        let ty = self.context.get_value(value).ty();
        let is_float = {
            let data = self.context.get_type_data(ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<FloatType>()
                .is_some()
        };
        if is_float {
            let zero = self.float_zero(ty);
            let predicate = match predicate {
                Predicate::Eq => Predicate::Oeq,
                Predicate::Ne => Predicate::Une,
                _ => unreachable!("truth conversion uses equality predicates"),
            };
            return self
                .emit(
                    fp::CmpOpBuilder::new(self.context)
                        .lhs(value)
                        .rhs(zero)
                        .predicate(predicate)
                        .semantics(self.comparison_semantics())
                        .result_type(IntegerType::new(self.context, 1))
                        .build(),
                )
                .result();
        }
        let narrow = {
            let data = self.context.get_type_data(ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<IntegerType>()
                .is_some_and(|integer| integer.width() < 32)
        };
        let (value, ty) = if narrow {
            let i32_ty = IntegerType::new(self.context, 32);
            (
                self.emit(b::extui(self.context, value, i32_ty).build())
                    .result(),
                i32_ty,
            )
        } else {
            (value, ty)
        };
        let is_pointer = {
            let data = self.context.get_type_data(ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<PtrType>()
                .is_some()
        };
        if is_pointer {
            let null = self.null_pointer();
            return self.lower_pointer_compare(predicate, value, null);
        }
        let zero = self.emit(b::constant(self.context, 0, ty).build()).result();
        self.emit(
            b::CmpIOpBuilder::new(self.context)
                .lhs(value)
                .rhs(zero)
                .predicate(predicate)
                .result_type(IntegerType::new(self.context, 1))
                .build(),
        )
        .result()
    }
}
