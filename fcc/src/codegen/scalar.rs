//! Scalar conversions and arithmetic lowering for function bodies.

use super::{FnCodegen, LoweredExpr, Slot, lower_type};
use crate::ast::AstKind;
use crate::sema::{IntegerKind, QualType, TypeKind};
use std::cmp::Ordering;
use tir::NodeId;
use tir::attributes::Predicate;
use tir::builtin::{FloatType, IntegerType, ops as b};
use tir::fp::{self, BinaryOp};
use tir::ptr::{PtrType, ops as p};
use tir::{TypeId, ValueId};

impl FnCodegen<'_> {
    pub(super) fn float_constant(&self, bits: u64, ty: TypeId) -> ValueId {
        let float = FloatType::view(&self.context, ty).expect("floating constant type");
        let bits_ty = IntegerType::new(self.context, u64::from(float.bit_width()));
        let bits = b::constant(self.context, bits as i64, bits_ty)
            .at(self.builder.end())
            .build()
            .result();
        b::bitcast(self.context, bits, ty)
            .at(self.builder.end())
            .build()
            .result()
    }

    pub(super) fn float_zero(&self, ty: TypeId) -> ValueId {
        self.float_constant(0, ty)
    }

    pub(super) fn float_one(&self, ty: TypeId) -> ValueId {
        let float = FloatType::view(&self.context, ty).expect("floating constant type");
        self.float_constant(
            match float.bit_width() {
                32 => 0x3f80_0000,
                64 => 0x3ff0_0000_0000_0000,
                _ => unreachable!("fcc supports binary32 and binary64"),
            },
            ty,
        )
    }

    pub(super) fn alloca(&mut self, elem: TypeId, size: u64, align: u64) -> Slot {
        let ptr_ty = PtrType::opaque(self.context);
        let op = p::alloca(self.context, size, align, ptr_ty)
            .at(self.builder.end())
            .build();
        Slot {
            ptr: op.result(),
            elem,
        }
    }

    pub(super) fn apply_conversions(
        &mut self,
        node: NodeId,
        mut expression: LoweredExpr,
    ) -> LoweredExpr {
        let semantics = self.ast.get_annotation(node).unwrap();
        let mut source = semantics.ty.unwrap();
        for &target in &semantics.conversions {
            expression = if self.typed.integer_width(source).is_some()
                && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
                && semantics.constant == Some(0)
            {
                let target = lower_type(self.context, self.typed, target);
                LoweredExpr::Value(
                    p::null(self.context, target)
                        .at(self.builder.end())
                        .build()
                        .result(),
                )
            } else if matches!(self.typed.types().kind(source), TypeKind::Array(_, _))
                && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
            {
                match expression {
                    LoweredExpr::Value(ptr) | LoweredExpr::Address { ptr, .. } => {
                        LoweredExpr::Value(ptr)
                    }
                }
            } else {
                let value = self.materialize(expression);
                LoweredExpr::Value(self.convert_scalar(value, source, target))
            };
            source = target;
        }
        expression
    }

    pub(super) fn convert_scalar(
        &mut self,
        value: ValueId,
        source: QualType,
        target: QualType,
    ) -> ValueId {
        let source_floating = matches!(
            self.typed.types().kind(source),
            TypeKind::Float | TypeKind::Double
        );
        let target_floating = matches!(
            self.typed.types().kind(target),
            TypeKind::Float | TypeKind::Double
        );
        if self.typed.integer_width(source).is_some() && target_floating {
            let source_ty = lower_type(self.context, self.typed, source);
            let value = if self.context.get_value(value).ty() == source_ty {
                value
            } else {
                self.promote_boolean_result(value, source_ty)
            };
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(source) == Some(true) {
                fp::from_si(self.context, value, target_ty)
                    .at(self.builder.end())
                    .build()
                    .result()
            } else {
                fp::from_ui(self.context, value, target_ty)
                    .at(self.builder.end())
                    .build()
                    .result()
            };
        }
        if source_floating && self.typed.integer_width(target).is_some() {
            let target_ty = lower_type(self.context, self.typed, target);
            if matches!(
                self.typed.types().kind(target),
                TypeKind::Integer(IntegerKind::Bool)
            ) {
                let condition = self.compare_against_zero(value, Predicate::Ne);
                return self.promote_boolean_result(condition, target_ty);
            }
            return if self.typed.integer_is_signed(target) == Some(true) {
                fp::to_si(self.context, value, target_ty)
                    .at(self.builder.end())
                    .build()
                    .result()
            } else {
                fp::to_ui(self.context, value, target_ty)
                    .at(self.builder.end())
                    .build()
                    .result()
            };
        }
        if source_floating && target_floating {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.context.get_value(value).ty() == target_ty {
                value
            } else {
                fp::convert(self.context, value, target_ty)
                    .at(self.builder.end())
                    .build()
                    .result()
            };
        }
        if let Some(source_width) = self.typed.integer_width(source)
            && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
        {
            let address_width = self.typed.target().pointer_width();
            let signed = self.typed.integer_is_signed(source).unwrap();
            let address = self.resize(value, source_width, address_width, signed);
            return self.address_as_pointer(address);
        }
        if matches!(self.typed.types().kind(source), TypeKind::Pointer(_))
            && let Some(target_width) = self.typed.integer_width(target)
        {
            let address_width = self.typed.target().pointer_width();
            let address = self.pointer_as_address(value);
            return self.resize(address, address_width, target_width, false);
        }
        let (Some(source_width), Some(target_width)) = (
            self.typed.integer_width(source),
            self.typed.integer_width(target),
        ) else {
            return value;
        };
        // A comparison's C type is `int` but its value is `!i1`, so the widths
        // below describe the source type and not what is being converted.
        let value =
            self.promote_boolean_result(value, lower_type(self.context, self.typed, source));
        let signed = self.typed.integer_is_signed(source).unwrap();
        self.resize(value, source_width, target_width, signed)
    }

    fn resize(&mut self, value: ValueId, from: u32, to: u32, signed: bool) -> ValueId {
        let ty = IntegerType::new(self.context, u64::from(to));
        match to.cmp(&from) {
            Ordering::Greater if signed => b::extsi(self.context, value, ty)
                .at(self.builder.end())
                .build()
                .result(),
            Ordering::Greater => b::extui(self.context, value, ty)
                .at(self.builder.end())
                .build()
                .result(),
            Ordering::Less => b::trunci(self.context, value, ty)
                .at(self.builder.end())
                .build()
                .result(),
            Ordering::Equal => value,
        }
    }

    pub(super) fn lower_integer_binary(
        &mut self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
        source_ty: QualType,
    ) -> ValueId {
        let ty = lower_type(self.context, self.typed, source_ty);
        let signed = || self.typed.integer_is_signed(source_ty).unwrap();
        macro_rules! bin {
            ($op:path) => {
                $op(self.context, lhs, rhs, ty)
                    .at(self.builder.end())
                    .build()
                    .result()
            };
        }
        // A signed `int` or wider overflows only in undefined behavior: the
        // op says so with `nsw`, and an overflow is poison. A narrower type
        // was promoted and does not reach here as itself.
        let int = self.typed.target().integer_width(IntegerKind::Int);
        let nsw = signed() && self.typed.integer_width(source_ty).unwrap_or(0) >= int;
        match kind {
            AstKind::Add | AstKind::AddAssign if nsw => b::addi(self.context, lhs, rhs, ty)
                .nsw(true)
                .at(self.builder.end())
                .build()
                .result(),
            AstKind::Sub | AstKind::SubAssign if nsw => b::subi(self.context, lhs, rhs, ty)
                .nsw(true)
                .at(self.builder.end())
                .build()
                .result(),
            AstKind::Add | AstKind::AddAssign => bin!(b::addi),
            AstKind::Sub | AstKind::SubAssign => bin!(b::subi),
            AstKind::Mul | AstKind::MulAssign => bin!(b::muli),
            AstKind::Div | AstKind::DivAssign if signed() => bin!(b::divsi),
            AstKind::Div | AstKind::DivAssign => bin!(b::divui),
            AstKind::Mod | AstKind::ModAssign if signed() => bin!(b::remsi),
            AstKind::Mod | AstKind::ModAssign => bin!(b::remui),
            AstKind::BitAnd | AstKind::AndAssign => bin!(b::andi),
            AstKind::BitXor | AstKind::XorAssign => bin!(b::xori),
            AstKind::BitOr | AstKind::OrAssign => bin!(b::ori),
            AstKind::Shl | AstKind::ShlAssign => bin!(b::shli),
            AstKind::Shr | AstKind::ShrAssign if signed() => bin!(b::shrsi),
            AstKind::Shr | AstKind::ShrAssign => bin!(b::shrui),
            _ => unreachable!(),
        }
    }

    pub(super) fn lower_integer_compare(
        &mut self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
        source_ty: QualType,
    ) -> ValueId {
        let signed = self.typed.integer_is_signed(source_ty).unwrap_or(true);
        let predicate = match (kind, signed) {
            (AstKind::Lt, true) => Predicate::Slt,
            (AstKind::Lt, false) => Predicate::Ult,
            (AstKind::Gt, true) => Predicate::Sgt,
            (AstKind::Gt, false) => Predicate::Ugt,
            (AstKind::Le, true) => Predicate::Sle,
            (AstKind::Le, false) => Predicate::Ule,
            (AstKind::Ge, true) => Predicate::Sge,
            (AstKind::Ge, false) => Predicate::Uge,
            (AstKind::Eq, _) => Predicate::Eq,
            (AstKind::Ne, _) => Predicate::Ne,
            _ => unreachable!(),
        };
        b::CmpIOpBuilder::new(self.context)
            .lhs(lhs)
            .rhs(rhs)
            .predicate(predicate)
            .result_type(IntegerType::new(self.context, 1))
            .at(self.builder.end())
            .build()
            .result()
    }

    fn pointer_as_address(&mut self, pointer: ValueId) -> ValueId {
        let address_ty =
            IntegerType::new(self.context, u64::from(self.typed.target().pointer_width()));
        let null = self.null_pointer();
        p::ptrdiff(self.context, pointer, null, address_ty)
            .at(self.builder.end())
            .build()
            .result()
    }

    fn address_as_pointer(&mut self, address: ValueId) -> ValueId {
        let null = self.null_pointer();
        p::ptradd(self.context, null, address, PtrType::opaque(self.context))
            .at(self.builder.end())
            .build()
            .result()
    }

    pub(super) fn null_pointer(&mut self) -> ValueId {
        p::null(self.context, PtrType::opaque(self.context))
            .at(self.builder.end())
            .build()
            .result()
    }

    pub(super) fn lower_pointer_compare(
        &mut self,
        predicate: Predicate,
        lhs: ValueId,
        rhs: ValueId,
    ) -> ValueId {
        tir::ptr::CmpOpBuilder::new(self.context)
            .lhs(lhs)
            .rhs(rhs)
            .predicate(predicate)
            .result_type(IntegerType::new(self.context, 1))
            .at(self.builder.end())
            .build()
            .result()
    }

    pub(super) fn lower_floating_compare(
        &mut self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
    ) -> ValueId {
        let predicate = match kind {
            AstKind::Lt => Predicate::Olt,
            AstKind::Gt => Predicate::Ogt,
            AstKind::Le => Predicate::Ole,
            AstKind::Ge => Predicate::Oge,
            AstKind::Eq => Predicate::Oeq,
            AstKind::Ne => Predicate::Une,
            _ => unreachable!(),
        };
        self.emit_fp_compare(predicate, lhs, rhs)
    }

    pub(super) fn emit_fp_compare(
        &self,
        predicate: Predicate,
        lhs: ValueId,
        rhs: ValueId,
    ) -> ValueId {
        fp::CmpOpBuilder::new(self.context)
            .lhs(lhs)
            .rhs(rhs)
            .predicate(predicate)
            .result_type(IntegerType::new(self.context, 1))
            .at(self.builder.end())
            .build()
            .result()
    }

    pub(super) fn emit_fp_binop(&self, kind: AstKind, lhs: ValueId, rhs: ValueId) -> ValueId {
        let kind = match kind {
            AstKind::Add | AstKind::AddAssign => BinaryOp::Add,
            AstKind::Sub | AstKind::SubAssign => BinaryOp::Sub,
            AstKind::Mul | AstKind::MulAssign => BinaryOp::Mul,
            AstKind::Div | AstKind::DivAssign => BinaryOp::Div,
            _ => unreachable!(),
        };
        kind.materialize(self.context, &self.builder, lhs, rhs)
    }

    pub(super) fn lower_pointer_offset(
        &mut self,
        base: ValueId,
        index: ValueId,
        index_ty: QualType,
        pointer_ty: QualType,
        subtract: bool,
        stride: u64,
    ) -> ValueId {
        let pointer_width = self.typed.target().pointer_width();
        let offset_ty = IntegerType::new(self.context, u64::from(pointer_width));
        // C adds the index's value, so it widens by the signedness of its type;
        // the offset is then as wide as the pointer, as `ptr.ptradd` requires.
        let index = self.resize(
            index,
            self.typed.integer_width(index_ty).unwrap(),
            pointer_width,
            self.typed.integer_is_signed(index_ty).unwrap(),
        );
        let scale = b::constant(self.context, stride as i64, offset_ty)
            .at(self.builder.end())
            .build()
            .result();
        let offset = b::muli(self.context, index, scale, offset_ty)
            .at(self.builder.end())
            .build()
            .result();
        let offset = if subtract {
            let zero = b::constant(self.context, 0, offset_ty)
                .at(self.builder.end())
                .build()
                .result();
            b::subi(self.context, zero, offset, offset_ty)
                .at(self.builder.end())
                .build()
                .result()
        } else {
            offset
        };
        p::ptradd(
            self.context,
            base,
            offset,
            lower_type(self.context, self.typed, pointer_ty),
        )
        .at(self.builder.end())
        .build()
        .result()
    }

    pub(super) fn lower_pointer_difference(
        &mut self,
        lhs: ValueId,
        rhs: ValueId,
        stride: u64,
        result_ty: QualType,
    ) -> ValueId {
        // The byte distance is a pointer-sized integer; the element count is
        // divided out at that width, then fitted to `ptrdiff_t`.
        let pointer_width = self.typed.target().pointer_width();
        let address_ty = IntegerType::new(self.context, u64::from(pointer_width));
        let mut count = p::ptrdiff(self.context, lhs, rhs, address_ty)
            .at(self.builder.end())
            .build()
            .result();
        if stride != 1 {
            let divisor = b::constant(self.context, stride as i64, address_ty)
                .at(self.builder.end())
                .build()
                .result();
            count = b::divsi(self.context, count, divisor, address_ty)
                .at(self.builder.end())
                .build()
                .result();
        }
        let result_width = self.typed.integer_width(result_ty).unwrap();
        self.resize(count, pointer_width, result_width, true)
    }

    pub(super) fn offset_address(&mut self, base: ValueId, offset: u64) -> ValueId {
        if offset == 0 {
            return base;
        }
        let offset_ty =
            IntegerType::new(self.context, u64::from(self.typed.target().pointer_width()));
        let offset = b::constant(self.context, offset as i64, offset_ty)
            .at(self.builder.end())
            .build()
            .result();
        p::ptradd(self.context, base, offset, PtrType::opaque(self.context))
            .at(self.builder.end())
            .build()
            .result()
    }
}
