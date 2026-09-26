//! SysV variadic argument cursor setup and reads in ordinary pointer IR.

use super::{FnCodegen, LoweredExpr, lower_type, node_type, unsupported};
use crate::diagnostics::Diagnostic;
use crate::sema::TypeKind;
use tir::attributes::Predicate;
use tir::builtin::{IntegerType, ops as b};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};

pub(super) struct VarargsEntry {
    pub(super) gp_start: usize,
    pub(super) fp_start: usize,
    pub(super) stack_slots: usize,
    pub(super) gp: Vec<tir::ValueId>,
    pub(super) fp: Vec<tir::ValueId>,
    pub(super) entry_sp: tir::ValueId,
    pub(super) register_area: Option<tir::ValueId>,
}

impl FnCodegen<'_> {
    pub(super) fn init_varargs(&mut self) {
        let Some(inputs) = &self.varargs else {
            return;
        };
        let gp_start = inputs.gp_start;
        let fp_start = inputs.fp_start;
        let gp = inputs.gp.clone();
        let fp = inputs.fp.clone();
        let area = self.alloca(IntegerType::new(self.context, 8), 176, 16).ptr;
        for (slot, value) in (gp_start..).zip(gp) {
            let address = self.offset_address(area, (slot * 8) as u64);
            self.emit(p::store(self.context, value, address).build());
        }
        for (slot, value) in (fp_start..).zip(fp) {
            let address = self.offset_address(area, (48 + slot * 16) as u64);
            self.emit(p::store(self.context, value, address).build());
        }
        self.varargs.as_mut().unwrap().register_area = Some(area);
    }

    pub(super) fn lower_va_start(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let list_node = self.ast.children(node).next().unwrap();
        let LoweredExpr::Address { ptr: list, .. } = self.values[&list_node] else {
            return Err(unsupported(self.ast, node, "va_start list".to_string()));
        };
        let Some(inputs) = &self.varargs else {
            return Err(unsupported(self.ast, node, "va_start function".to_string()));
        };
        let gp_offset = (inputs.gp_start * 8) as i64;
        let fp_offset = (48 + inputs.fp_start * 16) as i64;
        let overflow_offset = (8 + inputs.stack_slots * 8) as u64;
        let entry_sp = inputs.entry_sp;
        let register_area = inputs.register_area.unwrap();
        let i32_ty = IntegerType::new(self.context, 32);
        for (field, value) in [(0, gp_offset), (4, fp_offset)] {
            let address = self.offset_address(list, field);
            let value = self
                .emit(b::constant(self.context, value, i32_ty).build())
                .result();
            self.emit(p::store(self.context, value, address).build());
        }
        let overflow = self.offset_address(entry_sp, overflow_offset);
        let overflow_field = self.offset_address(list, 8);
        self.emit(p::store(self.context, overflow, overflow_field).build());
        let register_field = self.offset_address(list, 16);
        self.emit(p::store(self.context, register_area, register_field).build());
        Ok(LoweredExpr::Value(
            self.context
                .create_value(tir::builtin::UnitType::new(self.context), None)
                .id(),
        ))
    }

    pub(super) fn lower_va_end(&mut self) -> LoweredExpr {
        LoweredExpr::Value(
            self.context
                .create_value(tir::builtin::UnitType::new(self.context), None)
                .id(),
        )
    }

    pub(super) fn lower_va_arg(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let ast = self.ast;
        if !self.typed.target().uses_sysv_abi() {
            return Err(unsupported(ast, node, "va_arg target ABI".to_string()));
        }
        let list_node = ast.children(node).next().unwrap();
        let LoweredExpr::Address { ptr: list, .. } = self.values[&list_node] else {
            return Err(unsupported(ast, node, "va_arg list".to_string()));
        };
        let (offset_field, register_limit, step) =
            match self.typed.types().kind(node_type(self.typed, node)) {
                TypeKind::Integer(_)
                    if self
                        .typed
                        .integer_width(node_type(self.typed, node))
                        .is_some_and(|width| width <= 64) =>
                {
                    (0, 48, 8)
                }
                TypeKind::Double => (4, 176, 16),
                _ => return Err(unsupported(ast, node, "va_arg type".to_string())),
            };
        let result_ty = lower_type(self.context, self.typed, node_type(self.typed, node));
        let offset_ptr = self.offset_address(list, offset_field);
        let offset_ty = IntegerType::new(self.context, 32);
        let offset = self
            .emit(p::load(self.context, offset_ptr, offset_ty).build())
            .result();
        let limit = self
            .emit(b::constant(self.context, register_limit, offset_ty).build())
            .result();
        let from_registers = self
            .emit(
                b::CmpIOpBuilder::new(self.context)
                    .lhs(offset)
                    .rhs(limit)
                    .predicate(Predicate::Ult)
                    .result_type(IntegerType::new(self.context, 1))
                    .build(),
            )
            .result();

        let register_block = self.new_block();
        let overflow_block = self.new_block();
        let merge = self.new_block();
        let result = self
            .context
            .append_block_argument(merge.id(), result_ty)
            .id();
        self.branch_on(from_registers, &register_block, &overflow_block);

        self.enter_block(register_block);
        let register_area = self.offset_address(list, 16);
        let register_area = self
            .emit(p::load(self.context, register_area, PtrType::opaque(self.context)).build())
            .result();
        let wide = self
            .emit(b::extui(self.context, offset, IntegerType::new(self.context, 64)).build())
            .result();
        let address = self
            .emit(
                p::ptradd(
                    self.context,
                    register_area,
                    wide,
                    PtrType::opaque(self.context),
                )
                .build(),
            )
            .result();
        let register_value = self
            .emit(p::load(self.context, address, result_ty).build())
            .result();
        let increment = self
            .emit(b::constant(self.context, step, offset_ty).build())
            .result();
        let next = self
            .emit(b::addi(self.context, offset, increment, offset_ty).build())
            .result();
        self.emit(p::store(self.context, next, offset_ptr).build());
        self.branch_to(&merge, vec![register_value]);

        self.enter_block(overflow_block);
        let overflow_field = self.offset_address(list, 8);
        let overflow = self
            .emit(p::load(self.context, overflow_field, PtrType::opaque(self.context)).build())
            .result();
        let overflow_value = self
            .emit(p::load(self.context, overflow, result_ty).build())
            .result();
        let eight = self
            .emit(b::constant(self.context, 8, IntegerType::new(self.context, 64)).build())
            .result();
        let next = self
            .emit(p::ptradd(self.context, overflow, eight, PtrType::opaque(self.context)).build())
            .result();
        self.emit(p::store(self.context, next, overflow_field).build());
        self.branch_to(&merge, vec![overflow_value]);

        self.enter_block(merge);
        Ok(LoweredExpr::Value(result))
    }
}
