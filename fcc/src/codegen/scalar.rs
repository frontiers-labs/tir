//! Scalar conversions and arithmetic lowering for function bodies.

use super::{FnCodegen, LoweredExpr, Slot, lower_type, source_type_layout};
use crate::ast::AstKind;
use crate::sema::{QualType, TypeKind};
use std::{cmp::Ordering, sync::Arc};
use tir::attributes::Predicate;
use tir::builtin::{FloatType, IntegerType, ops as b};
use tir::fp::{
    ArithmeticSemantics, ComparisonBehavior, ComparisonSemantics, Exceptions,
    IntegerConversionSemantics, InvalidConversion, Rounding, RoundingMode, SubnormalMode,
    ops as fp,
};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::{Operation, TypeId, ValueId};

impl FnCodegen<'_> {
    pub(super) fn emit<T: Operation>(&self, op: T) -> T {
        self.builder.append_op(op)
    }

    fn arithmetic_semantics(&self) -> Arc<tir::fp::Semantics> {
        self.context
            .intern_fp_semantics(ArithmeticSemantics::strict(
                RoundingMode::TiesToEven,
                Exceptions::Ignore,
            ))
    }

    pub(super) fn comparison_semantics(&self) -> Arc<tir::fp::Semantics> {
        self.context
            .intern_fp_semantics(tir::fp::Semantics::Comparison(ComparisonSemantics {
                behavior: ComparisonBehavior::Quiet,
                exceptions: Exceptions::Ignore,
                subnormals: SubnormalMode::Gradual,
            }))
    }

    fn integer_conversion_semantics(&self) -> Arc<tir::fp::Semantics> {
        self.context
            .intern_fp_semantics(tir::fp::Semantics::IntegerConversion(
                IntegerConversionSemantics {
                    rounding: Rounding::Fixed(RoundingMode::TowardZero),
                    exceptions: Exceptions::Ignore,
                    subnormals: SubnormalMode::Gradual,
                    invalid: InvalidConversion::Indeterminate,
                },
            ))
    }

    pub(super) fn float_constant(&self, bits: u64, ty: TypeId) -> ValueId {
        self.emit(
            fp::ConstantOpBuilder::new(self.context)
                .bits(bits)
                .result_type(ty)
                .build(),
        )
        .result()
    }

    pub(super) fn float_zero(&self, ty: TypeId) -> ValueId {
        self.float_constant(0, ty)
    }

    pub(super) fn float_one(&self, ty: TypeId) -> ValueId {
        let data = self.context.get_type_data(ty);
        let float = (data.as_ref() as &dyn std::any::Any)
            .downcast_ref::<FloatType>()
            .expect("floating constant type");
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
        let op = self.emit(p::alloca(self.context, size, align, ptr_ty).build());
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
                LoweredExpr::Value(self.emit(p::null(self.context, target).build()).result())
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
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(source) == Some(true) {
                self.emit(
                    fp::FromSiOpBuilder::new(self.context)
                        .input(value)
                        .semantics(self.arithmetic_semantics())
                        .result_type(target_ty)
                        .build(),
                )
                .result()
            } else {
                self.emit(
                    fp::FromUiOpBuilder::new(self.context)
                        .input(value)
                        .semantics(self.arithmetic_semantics())
                        .result_type(target_ty)
                        .build(),
                )
                .result()
            };
        }
        if source_floating && let Some(target_width) = self.typed.integer_width(target) {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(target) == Some(true) {
                self.emit(
                    fp::ToSiOpBuilder::new(self.context)
                        .input(value)
                        .semantics(self.integer_conversion_semantics())
                        .result_type(target_ty)
                        .build(),
                )
                .result()
            } else if target_width < 64 {
                // Every value an unsigned type narrower than 64 bits can hold is
                // also representable as a signed 64-bit integer, so the truncated
                // signed conversion is exact and avoids a narrow unsigned conversion.
                let wide = IntegerType::new(self.context, 64);
                let converted = self
                    .emit(
                        fp::ToSiOpBuilder::new(self.context)
                            .input(value)
                            .semantics(self.integer_conversion_semantics())
                            .result_type(wide)
                            .build(),
                    )
                    .result();
                self.emit(b::trunci(self.context, converted, target_ty).build())
                    .result()
            } else {
                self.emit(
                    fp::ToUiOpBuilder::new(self.context)
                        .input(value)
                        .semantics(self.integer_conversion_semantics())
                        .result_type(target_ty)
                        .build(),
                )
                .result()
            };
        }
        if source_floating && target_floating {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.context.get_value(value).ty() == target_ty {
                value
            } else {
                self.emit(
                    fp::ConvertOpBuilder::new(self.context)
                        .input(value)
                        .semantics(self.arithmetic_semantics())
                        .result_type(target_ty)
                        .build(),
                )
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

    /// `value`, an integer `from` bits wide, as one of `to` bits.
    fn resize(&mut self, value: ValueId, from: u32, to: u32, signed: bool) -> ValueId {
        let ty = IntegerType::new(self.context, to);
        match to.cmp(&from) {
            Ordering::Greater if signed => self
                .emit(b::extsi(self.context, value, ty).build())
                .result(),
            Ordering::Greater => self
                .emit(b::extui(self.context, value, ty).build())
                .result(),
            Ordering::Less => self
                .emit(b::trunci(self.context, value, ty).build())
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
                self.emit($op(self.context, lhs, rhs, ty).build()).result()
            };
        }
        match kind {
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
        self.emit(
            b::CmpIOpBuilder::new(self.context)
                .lhs(lhs)
                .rhs(rhs)
                .predicate(predicate)
                .result_type(IntegerType::new(self.context, 1))
                .build(),
        )
        .result()
    }

    /// A pointer's address as an integer: the distance from the null pointer,
    /// which is the address zero.
    fn pointer_as_address(&mut self, pointer: ValueId) -> ValueId {
        let address_ty = IntegerType::new(self.context, self.typed.target().pointer_width());
        let null = self.null_pointer();
        self.emit(p::ptrdiff(self.context, pointer, null, address_ty).build())
            .result()
    }

    /// The pointer an address names: the offset from the null pointer.
    fn address_as_pointer(&mut self, address: ValueId) -> ValueId {
        let null = self.null_pointer();
        self.emit(p::ptradd(self.context, null, address, PtrType::opaque(self.context)).build())
            .result()
    }

    pub(super) fn null_pointer(&mut self) -> ValueId {
        self.emit(p::null(self.context, PtrType::opaque(self.context)).build())
            .result()
    }

    /// Pointers compare as unsigned addresses, so a relational C comparison of
    /// two of them maps onto the unsigned predicates alone.
    pub(super) fn lower_pointer_compare(
        &mut self,
        predicate: Predicate,
        lhs: ValueId,
        rhs: ValueId,
    ) -> ValueId {
        self.emit(
            p::CmpOpBuilder::new(self.context)
                .lhs(lhs)
                .rhs(rhs)
                .predicate(predicate)
                .result_type(IntegerType::new(self.context, 1))
                .build(),
        )
        .result()
    }

    /// C comparisons are ordered (false when either operand is NaN), except
    /// `!=`, which is the unordered-inclusive negation of `==`.
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
        self.emit(
            fp::CmpOpBuilder::new(self.context)
                .lhs(lhs)
                .rhs(rhs)
                .predicate(predicate)
                .semantics(self.comparison_semantics())
                .result_type(IntegerType::new(self.context, 1))
                .build(),
        )
        .result()
    }

    pub(super) fn lower_floating_binary(
        &mut self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
        source_ty: QualType,
    ) -> ValueId {
        let ty = lower_type(self.context, self.typed, source_ty);
        self.emit_fp_binop(kind, lhs, rhs, ty)
    }

    pub(super) fn emit_fp_binop(
        &self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
        ty: TypeId,
    ) -> ValueId {
        macro_rules! emit {
            ($builder:ident) => {
                self.emit(
                    fp::$builder::new(self.context)
                        .lhs(lhs)
                        .rhs(rhs)
                        .semantics(self.arithmetic_semantics())
                        .result_type(ty)
                        .build(),
                )
                .result()
            };
        }
        match kind {
            AstKind::Add | AstKind::AddAssign => emit!(AddOpBuilder),
            AstKind::Sub | AstKind::SubAssign => emit!(SubOpBuilder),
            AstKind::Mul | AstKind::MulAssign => emit!(MulOpBuilder),
            AstKind::Div | AstKind::DivAssign => emit!(DivOpBuilder),
            _ => unreachable!(),
        }
    }

    pub(super) fn lower_pointer_offset(
        &mut self,
        base: ValueId,
        index: ValueId,
        index_ty: QualType,
        pointer_ty: QualType,
        subtract: bool,
    ) -> ValueId {
        let TypeKind::Pointer(pointee) = self.typed.types().kind(pointer_ty) else {
            unreachable!("pointer arithmetic result has pointer type")
        };
        let pointer_width = self.typed.target().pointer_width();
        let offset_ty = IntegerType::new(self.context, pointer_width);
        let index_width = self.typed.integer_width(index_ty).unwrap();
        let index = if index_width < pointer_width {
            if self.typed.integer_is_signed(index_ty).unwrap() {
                self.emit(b::extsi(self.context, index, offset_ty).build())
                    .result()
            } else {
                self.emit(b::extui(self.context, index, offset_ty).build())
                    .result()
            }
        } else if index_width > pointer_width {
            self.emit(b::trunci(self.context, index, offset_ty).build())
                .result()
        } else {
            index
        };
        let size = source_type_layout(self.typed, *pointee).0;
        let scale = self
            .emit(b::constant(self.context, size as i64, offset_ty).build())
            .result();
        let offset = self
            .emit(b::muli(self.context, index, scale, offset_ty).build())
            .result();
        let offset = if subtract {
            let zero = self
                .emit(b::constant(self.context, 0, offset_ty).build())
                .result();
            self.emit(b::subi(self.context, zero, offset, offset_ty).build())
                .result()
        } else {
            offset
        };
        self.emit(
            p::ptradd(
                self.context,
                base,
                offset,
                lower_type(self.context, self.typed, pointer_ty),
            )
            .build(),
        )
        .result()
    }

    pub(super) fn lower_pointer_difference(
        &mut self,
        lhs: ValueId,
        rhs: ValueId,
        pointer_ty: QualType,
        result_ty: QualType,
    ) -> ValueId {
        let TypeKind::Pointer(pointee) = self.typed.types().kind(pointer_ty) else {
            unreachable!("pointer difference operand has pointer type")
        };
        let result_ty = lower_type(self.context, self.typed, result_ty);
        let bytes = self
            .emit(p::ptrdiff(self.context, lhs, rhs, result_ty).build())
            .result();
        let size = source_type_layout(self.typed, *pointee).0;
        if size == 1 {
            return bytes;
        }
        let divisor = self
            .emit(b::constant(self.context, size as i64, result_ty).build())
            .result();
        self.emit(b::divsi(self.context, bytes, divisor, result_ty).build())
            .result()
    }

    pub(super) fn offset_address(&mut self, base: ValueId, offset: u64) -> ValueId {
        if offset == 0 {
            return base;
        }
        let offset_ty = IntegerType::new(self.context, self.typed.target().pointer_width());
        let offset = self
            .emit(b::constant(self.context, offset as i64, offset_ty).build())
            .result();
        self.emit(p::ptradd(self.context, base, offset, PtrType::opaque(self.context)).build())
            .result()
    }
}
