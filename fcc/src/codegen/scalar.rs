//! Scalar conversions and arithmetic lowering for function bodies.

use super::{FnCodegen, LoweredExpr, Slot, lower_type, source_type_layout};
use crate::ast::AstKind;
use crate::sema::{IntegerKind, QualType, TypeKind};
use std::{cmp::Ordering, sync::Arc};
use tir::NodeId;
use tir::attributes::Predicate;
use tir::builtin::{FloatType, IntegerType, ops as b};
use tir::fp::{
    ArithmeticSemantics, ComparisonBehavior, ComparisonSemantics, Exceptions,
    IntegerConversionSemantics, InvalidConversion, Rounding, RoundingMode, SubnormalMode,
    ops as fp,
};
use tir::ptr::{PtrType, ops as p};
use tir::{TypeId, ValueId};

impl FnCodegen<'_> {
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
        fp::constant(
            self.context,
            tir::attributes::AttributeValue::UInt(bits),
            ty,
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
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
        let op = p::alloca(
            self.context,
            size,
            align,
            ptr_ty,
            vec![],
            Default::default(),
            self.builder.end(),
        );
        Slot {
            ptr: op.results()[0],
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
                    p::null(self.context, target, Default::default(), self.builder.end()).results()
                        [0],
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
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(source) == Some(true) {
                fp::from_si(
                    self.context,
                    value,
                    tir::attributes::AttributeValue::FpSemantics(self.arithmetic_semantics()),
                    target_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            } else {
                fp::from_ui(
                    self.context,
                    value,
                    tir::attributes::AttributeValue::FpSemantics(self.arithmetic_semantics()),
                    target_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            };
        }
        if source_floating && let Some(target_width) = self.typed.integer_width(target) {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(target) == Some(true) {
                fp::to_si(
                    self.context,
                    value,
                    tir::attributes::AttributeValue::FpSemantics(
                        self.integer_conversion_semantics(),
                    ),
                    target_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            } else if target_width < 64 {
                // Every value an unsigned type narrower than 64 bits can hold is
                // also representable as a signed 64-bit integer, so the truncated
                // signed conversion is exact and avoids a narrow unsigned conversion.
                let wide = IntegerType::new(self.context, 64);
                let converted = fp::to_si(
                    self.context,
                    value,
                    tir::attributes::AttributeValue::FpSemantics(
                        self.integer_conversion_semantics(),
                    ),
                    wide,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0];
                b::trunci(
                    self.context,
                    converted,
                    target_ty,
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            } else {
                fp::to_ui(
                    self.context,
                    value,
                    tir::attributes::AttributeValue::FpSemantics(
                        self.integer_conversion_semantics(),
                    ),
                    target_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            };
        }
        if source_floating && target_floating {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.context.get_value(value).ty() == target_ty {
                value
            } else {
                fp::convert(
                    self.context,
                    value,
                    tir::attributes::AttributeValue::FpSemantics(self.arithmetic_semantics()),
                    target_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
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
            Ordering::Greater if signed => b::extsi(
                self.context,
                value,
                ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0],
            Ordering::Greater => b::extui(
                self.context,
                value,
                ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0],
            Ordering::Less => b::trunci(
                self.context,
                value,
                ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0],
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
                $op(
                    self.context,
                    lhs,
                    rhs,
                    ty,
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            };
        }
        // A signed `int` or wider overflows only in undefined behavior: the
        // op says so with `nsw`, and an overflow is poison. A narrower type
        // was promoted and does not reach here as itself.
        let int = self.typed.target().integer_width(IntegerKind::Int);
        let nsw = signed() && self.typed.integer_width(source_ty).unwrap_or(0) >= int;
        match kind {
            AstKind::Add | AstKind::AddAssign if nsw => b::addi(
                self.context,
                lhs,
                rhs,
                ty,
                tir::NewOpParts {
                    attributes: vec![
                        (self.context)
                            .named_attribute("nsw", tir::attributes::AttributeValue::Bool(true)),
                    ],
                },
                self.builder.end(),
            )
            .results()[0],
            AstKind::Sub | AstKind::SubAssign if nsw => b::subi(
                self.context,
                lhs,
                rhs,
                ty,
                tir::NewOpParts {
                    attributes: vec![
                        (self.context)
                            .named_attribute("nsw", tir::attributes::AttributeValue::Bool(true)),
                    ],
                },
                self.builder.end(),
            )
            .results()[0],
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
        b::cmpi(
            self.context,
            lhs,
            rhs,
            tir::attributes::AttributeValue::Predicate(predicate),
            IntegerType::new(self.context, 1),
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
    }

    fn pointer_as_address(&mut self, pointer: ValueId) -> ValueId {
        let address_ty =
            IntegerType::new(self.context, u64::from(self.typed.target().pointer_width()));
        let null = self.null_pointer();
        p::ptrdiff(
            self.context,
            pointer,
            null,
            address_ty,
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
    }

    fn address_as_pointer(&mut self, address: ValueId) -> ValueId {
        let null = self.null_pointer();
        p::ptradd(
            self.context,
            null,
            address,
            PtrType::opaque(self.context),
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
    }

    pub(super) fn null_pointer(&mut self) -> ValueId {
        p::null(
            self.context,
            PtrType::opaque(self.context),
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
    }

    pub(super) fn lower_pointer_compare(
        &mut self,
        predicate: Predicate,
        lhs: ValueId,
        rhs: ValueId,
    ) -> ValueId {
        p::cmp(
            self.context,
            lhs,
            rhs,
            tir::attributes::AttributeValue::Predicate(predicate),
            IntegerType::new(self.context, 1),
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
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
        fp::cmp(
            self.context,
            lhs,
            rhs,
            tir::attributes::AttributeValue::Predicate(predicate),
            tir::attributes::AttributeValue::FpSemantics(self.comparison_semantics()),
            IntegerType::new(self.context, 1),
            vec![],
            vec![],
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
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
                fp::$builder(
                    self.context,
                    lhs,
                    rhs,
                    tir::attributes::AttributeValue::FpSemantics(self.arithmetic_semantics()),
                    ty,
                    vec![],
                    vec![],
                    Default::default(),
                    self.builder.end(),
                )
                .results()[0]
            };
        }
        match kind {
            AstKind::Add | AstKind::AddAssign => emit!(add),
            AstKind::Sub | AstKind::SubAssign => emit!(sub),
            AstKind::Mul | AstKind::MulAssign => emit!(mul),
            AstKind::Div | AstKind::DivAssign => emit!(div),
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
        let offset_ty = IntegerType::new(self.context, u64::from(pointer_width));
        // C adds the index's value, so it widens by the signedness of its type;
        // the offset is then as wide as the pointer, as `ptr.ptradd` requires.
        let index = self.resize(
            index,
            self.typed.integer_width(index_ty).unwrap(),
            pointer_width,
            self.typed.integer_is_signed(index_ty).unwrap(),
        );
        let size = source_type_layout(self.typed, *pointee).0;
        let scale = b::constant(
            self.context,
            size as i64,
            offset_ty,
            Default::default(),
            self.builder.end(),
        )
        .results()[0];
        let offset = b::muli(
            self.context,
            index,
            scale,
            offset_ty,
            Default::default(),
            self.builder.end(),
        )
        .results()[0];
        let offset = if subtract {
            let zero = b::constant(
                self.context,
                0,
                offset_ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0];
            b::subi(
                self.context,
                zero,
                offset,
                offset_ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0]
        } else {
            offset
        };
        p::ptradd(
            self.context,
            base,
            offset,
            lower_type(self.context, self.typed, pointer_ty),
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
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
        // The byte distance is a pointer-sized integer; the element count is
        // divided out at that width, then fitted to `ptrdiff_t`.
        let pointer_width = self.typed.target().pointer_width();
        let address_ty = IntegerType::new(self.context, u64::from(pointer_width));
        let mut count = p::ptrdiff(
            self.context,
            lhs,
            rhs,
            address_ty,
            Default::default(),
            self.builder.end(),
        )
        .results()[0];
        let size = source_type_layout(self.typed, *pointee).0;
        if size != 1 {
            let divisor = b::constant(
                self.context,
                size as i64,
                address_ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0];
            count = b::divsi(
                self.context,
                count,
                divisor,
                address_ty,
                Default::default(),
                self.builder.end(),
            )
            .results()[0];
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
        let offset = b::constant(
            self.context,
            offset as i64,
            offset_ty,
            Default::default(),
            self.builder.end(),
        )
        .results()[0];
        p::ptradd(
            self.context,
            base,
            offset,
            PtrType::opaque(self.context),
            Default::default(),
            self.builder.end(),
        )
        .results()[0]
    }
}
