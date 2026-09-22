//! Constant evaluation, conversions, and canonical C types.

use super::references::integer_literal_reference;
use super::{
    Analyzer, IntegerKind, QualType, Qualifiers, TypeKind, is_signed_integer,
    unsigned_corresponding, with_qualifier,
};
use crate::ast::{ArrayLength, AstKind, AstLeaf, CParam, CType};
use crate::diagnostics::InvalidIntegerLiteral;
use crate::lang_options::StdVersion;
use crate::lexer::decode_character_constant;
use tir::graph::{Dag, MutDag, NodeId};

impl Analyzer<'_> {
    pub(super) fn constant_value(
        &self,
        node: NodeId,
        kind: AstKind,
        result_ty: QualType,
    ) -> Option<i64> {
        match kind {
            AstKind::Int => {
                let AstLeaf::Int(value) = self.ast.get_leaf_data(node)? else {
                    return None;
                };
                return Some(value.value.to_i64());
            }
            AstKind::Character => {
                let AstLeaf::Character(value) = self.ast.get_leaf_data(node)? else {
                    return None;
                };
                return decode_character_constant(value);
            }
            _ => {}
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        let child_constant = |child| {
            self.ast
                .get_annotation(child)
                .and_then(|info| info.constant)
        };
        if let (AstKind::Cast, [child], TypeKind::Integer(integer)) =
            (kind, children.as_slice(), self.types.kind(result_ty))
            && let Some(AstLeaf::Float(value)) = self.ast.get_leaf_data(*child)
        {
            return self.cast_float_constant(value.value.to_f64(), *integer);
        }
        match (kind, children.as_slice()) {
            (AstKind::LogAnd, [left, right]) => {
                return child_constant(*left).and_then(|left| {
                    if left == 0 {
                        Some(0)
                    } else {
                        child_constant(*right).map(|right| i64::from(right != 0))
                    }
                });
            }
            (AstKind::LogOr, [left, right]) => {
                return child_constant(*left).and_then(|left| {
                    if left != 0 {
                        Some(1)
                    } else {
                        child_constant(*right).map(|right| i64::from(right != 0))
                    }
                });
            }
            (AstKind::Conditional, [condition, when_true, when_false]) => {
                return child_constant(*condition).and_then(|condition| {
                    child_constant(if condition != 0 {
                        *when_true
                    } else {
                        *when_false
                    })
                });
            }
            _ => {}
        }
        let values = self
            .ast
            .children(node)
            .map(child_constant)
            .collect::<Option<Vec<_>>>()?;
        match (kind, values.as_slice()) {
            (AstKind::Add, [left, right]) => left.checked_add(*right),
            (AstKind::Sub, [left, right]) => left.checked_sub(*right),
            (AstKind::Mul, [left, right]) => left.checked_mul(*right),
            (AstKind::Div, [_, 0]) => None,
            (AstKind::Div, [left, right]) => left.checked_div(*right),
            (AstKind::Mod, [_, 0]) => None,
            (AstKind::Mod, [left, right]) => left.checked_rem(*right),
            (AstKind::Shl, [left, right]) => u32::try_from(*right)
                .ok()
                .and_then(|shift| left.checked_shl(shift)),
            (AstKind::Shr, [left, right]) => u32::try_from(*right)
                .ok()
                .and_then(|shift| left.checked_shr(shift)),
            (AstKind::BitAnd, [left, right]) => Some(left & right),
            (AstKind::BitXor, [left, right]) => Some(left ^ right),
            (AstKind::BitOr, [left, right]) => Some(left | right),
            (AstKind::Lt, [left, right]) => Some(i64::from(left < right)),
            (AstKind::Gt, [left, right]) => Some(i64::from(left > right)),
            (AstKind::Le, [left, right]) => Some(i64::from(left <= right)),
            (AstKind::Ge, [left, right]) => Some(i64::from(left >= right)),
            (AstKind::Eq, [left, right]) => Some(i64::from(left == right)),
            (AstKind::Ne, [left, right]) => Some(i64::from(left != right)),
            (AstKind::Cast, [value]) => match self.types.kind(result_ty) {
                TypeKind::Integer(kind) => Some(self.cast_integer_constant(*value, *kind)),
                _ => None,
            },
            (AstKind::Neg, [value]) => value.checked_neg(),
            (AstKind::Pos, [value]) => Some(*value),
            (AstKind::Not, [value]) => Some(i64::from(*value == 0)),
            (AstKind::BitNot, [value]) => Some(!value),
            _ => None,
        }
    }

    pub(super) fn cast_integer_constant(&self, value: i64, kind: IntegerKind) -> i64 {
        if kind == IntegerKind::Bool {
            return i64::from(value != 0);
        }
        let width = self.target.integer_width(kind);
        if width == 64 {
            return value;
        }
        let mask = (1_u64 << width) - 1;
        let bits = (value as u64) & mask;
        if is_signed_integer(kind, self.target) {
            let shift = 64 - width;
            ((bits << shift) as i64) >> shift
        } else {
            bits as i64
        }
    }

    pub(super) fn cast_float_constant(&self, value: f64, kind: IntegerKind) -> Option<i64> {
        if kind == IntegerKind::Bool {
            return Some(i64::from(value != 0.0));
        }
        let value = value.trunc();
        if !value.is_finite() {
            return None;
        }
        let width = self.target.integer_width(kind);
        if is_signed_integer(kind, self.target) {
            let limit = 2_f64.powi((width - 1) as i32);
            (-limit..limit).contains(&value).then_some(value as i64)
        } else {
            let limit = 2_f64.powi(width as i32);
            (0.0..limit)
                .contains(&value)
                .then_some((value as u64) as i64)
        }
    }

    pub(super) fn child_types(&self, node: NodeId) -> Vec<QualType> {
        self.ast
            .children(node)
            .filter_map(|child| self.ast.get_annotation(child).and_then(|info| info.ty))
            .collect()
    }

    pub(super) fn record_operand_conversions(
        &mut self,
        parent: NodeId,
        operands: &[QualType],
        target: QualType,
    ) {
        let children = self.ast.children(parent).collect::<Vec<_>>();
        for (&child, &source) in children.iter().zip(operands) {
            if source != target {
                let mut semantics = self.ast.get_annotation(child).cloned().unwrap_or_default();
                semantics.conversions.push(target);
                self.ast.set_annotation(child, semantics);
            }
        }
    }

    pub(super) fn record_conversion(&mut self, node: NodeId, target: QualType) {
        let mut semantics = self.ast.get_annotation(node).cloned().unwrap_or_default();
        if semantics.ty != Some(target) {
            semantics.conversions.push(target);
            self.ast.set_annotation(node, semantics);
        }
    }

    pub(super) fn assignment_source(
        &mut self,
        target: QualType,
        source: QualType,
        source_node: NodeId,
    ) -> QualType {
        if matches!(self.types.kind(target), TypeKind::Array(_, _)) {
            source
        } else {
            self.value_conversion(source_node, source)
        }
    }

    pub(super) fn value_conversion(&mut self, node: NodeId, source: QualType) -> QualType {
        let target = match self.types.kind(source).clone() {
            TypeKind::Array(element, _) => self.types.intern(TypeKind::Pointer(element)),
            TypeKind::Function { .. } => self.types.intern(TypeKind::Pointer(source)),
            _ => return source,
        };
        self.record_conversion(node, target);
        target
    }

    pub(super) fn is_arithmetic(&self, ty: QualType) -> bool {
        matches!(
            self.types.kind(ty),
            TypeKind::Integer(_)
                | TypeKind::Enum(_)
                | TypeKind::Float
                | TypeKind::Double
                | TypeKind::LongDouble
        )
    }

    pub(super) fn is_integer(&self, ty: QualType) -> bool {
        matches!(
            self.types.kind(ty),
            TypeKind::Integer(_) | TypeKind::Enum(_)
        )
    }

    pub(super) fn is_null_pointer_constant(&self, ty: QualType, node: NodeId) -> bool {
        self.is_integer(ty)
            && self.ast.get_annotation(node).and_then(|info| info.constant) == Some(0)
    }

    pub(super) fn is_scalar(&self, ty: QualType) -> bool {
        self.is_arithmetic(ty) || matches!(self.types.kind(ty), TypeKind::Pointer(_))
    }

    pub(super) fn assignment_compatible(
        &self,
        target: QualType,
        source: QualType,
        source_node: NodeId,
    ) -> bool {
        match (self.types.kind(target), self.types.kind(source)) {
            (TypeKind::Error, _) | (_, TypeKind::Error) => true,
            (
                TypeKind::Integer(_) | TypeKind::Enum(_),
                TypeKind::Integer(_)
                | TypeKind::Enum(_)
                | TypeKind::Float
                | TypeKind::Double
                | TypeKind::LongDouble,
            ) => true,
            (
                TypeKind::Float | TypeKind::Double | TypeKind::LongDouble,
                TypeKind::Integer(_)
                | TypeKind::Enum(_)
                | TypeKind::Float
                | TypeKind::Double
                | TypeKind::LongDouble,
            ) => true,
            (TypeKind::Pointer(_), TypeKind::Pointer(_)) => true,
            (TypeKind::Pointer(target), TypeKind::Array(source, _)) => {
                self.types.kind(*target) == self.types.kind(*source)
            }
            (TypeKind::Pointer(target), TypeKind::Function { .. }) => {
                self.types.kind(*target) == self.types.kind(source)
            }
            (TypeKind::Pointer(_), TypeKind::Integer(_)) => {
                self.is_null_pointer_constant(source, source_node)
            }
            (left, right) => left == right,
        }
    }

    pub(super) fn conversion_message(&self, target: QualType, source: QualType) -> String {
        let target = self.type_category(target);
        let source = self.type_category(source);
        format!("cannot assign value of {source} type to {target}")
    }

    pub(super) fn type_category(&self, ty: QualType) -> &'static str {
        match self.types.kind(ty) {
            TypeKind::Integer(_) => "integer",
            TypeKind::Float | TypeKind::Double | TypeKind::LongDouble => "floating",
            TypeKind::Pointer(_) => "pointer",
            TypeKind::Array(_, _) => "array",
            TypeKind::Function { .. } => "function",
            TypeKind::Record(_) => "record",
            TypeKind::Enum(_) => "enumeration",
            TypeKind::Void => "void",
            TypeKind::Error => "invalid",
        }
    }

    pub(super) fn type_size(&self, ty: QualType) -> Option<u64> {
        self.type_layout(ty).map(|(size, _)| size)
    }

    pub(super) fn type_layout(&self, ty: QualType) -> Option<(u64, u64)> {
        match self.types.kind(ty) {
            TypeKind::Array(element, Some(length)) => {
                let (size, align) = self.type_layout(*element)?;
                Some((size.checked_mul(*length)?, align))
            }
            TypeKind::Record(id) => self
                .record_indices
                .get(id)
                .map(|&index| &self.records[index])
                .filter(|record| record.size != 0)
                .map(|record| (record.size, record.align)),
            kind => self.target.scalar_layout(kind),
        }
    }

    pub(super) fn integer_promotion(&mut self, ty: QualType) -> QualType {
        if self.integer_rank(ty) < 3 {
            self.types.intern(TypeKind::Integer(IntegerKind::Int))
        } else {
            ty
        }
    }

    pub(super) fn integer_literal_type(
        &mut self,
        node: NodeId,
        spelling: &str,
        value: u64,
    ) -> QualType {
        let suffix_start = spelling.trim_end_matches(['u', 'U', 'l', 'L']).len();
        let suffix = spelling[suffix_start..].to_ascii_lowercase();
        if !matches!(
            suffix.as_str(),
            "" | "u" | "l" | "ul" | "lu" | "ll" | "ull" | "llu"
        ) {
            self.diagnostics.push(
                InvalidIntegerLiteral::new(
                    self.ast.get_node(node).span,
                    format!("invalid integer suffix in '{spelling}'"),
                    integer_literal_reference(self.options),
                )
                .into(),
            );
            return self.types.intern(TypeKind::Error);
        }
        let digits = &spelling[..suffix_start];
        let decimal = !(digits.starts_with("0x")
            || digits.starts_with("0X")
            || digits.starts_with("0b")
            || digits.starts_with("0B")
            || (digits.len() > 1 && digits.starts_with('0')));
        let unsigned = suffix.contains('u');
        let long_count = suffix.chars().filter(|&ch| ch == 'l').count();
        let candidates: &[IntegerKind] = match (decimal, unsigned, long_count) {
            (_, true, 2) => &[IntegerKind::UnsignedLongLong],
            (_, true, 1) => &[IntegerKind::UnsignedLong, IntegerKind::UnsignedLongLong],
            (_, true, 0) => &[
                IntegerKind::UnsignedInt,
                IntegerKind::UnsignedLong,
                IntegerKind::UnsignedLongLong,
            ],
            (true, false, 2) => &[IntegerKind::LongLong],
            (true, false, 1) => &[IntegerKind::Long, IntegerKind::LongLong],
            (true, false, 0) => &[IntegerKind::Int, IntegerKind::Long, IntegerKind::LongLong],
            (false, false, 2) => &[IntegerKind::LongLong, IntegerKind::UnsignedLongLong],
            (false, false, 1) => &[
                IntegerKind::Long,
                IntegerKind::UnsignedLong,
                IntegerKind::LongLong,
                IntegerKind::UnsignedLongLong,
            ],
            (false, false, 0) => &[
                IntegerKind::Int,
                IntegerKind::UnsignedInt,
                IntegerKind::Long,
                IntegerKind::UnsignedLong,
                IntegerKind::LongLong,
                IntegerKind::UnsignedLongLong,
            ],
            _ => unreachable!(),
        };
        for &kind in candidates {
            let width = self.target.integer_width(kind);
            let signed = is_signed_integer(kind, self.target);
            let fits = if signed {
                width == 64 && value <= i64::MAX as u64
                    || width < 64 && value < (1u64 << (width - 1))
            } else {
                width == 64 || value < (1u64 << width)
            };
            if fits {
                return self.types.intern(TypeKind::Integer(kind));
            }
        }
        self.diagnostics.push(
            InvalidIntegerLiteral::new(
                self.ast.get_node(node).span,
                format!("integer literal '{spelling}' is too large for its candidate types"),
                integer_literal_reference(self.options),
            )
            .into(),
        );
        self.types.intern(TypeKind::Error)
    }

    pub(super) fn common_arithmetic_type(&mut self, left: QualType, right: QualType) -> QualType {
        let left = self.integer_promotion(left);
        let right = self.integer_promotion(right);
        if left == right {
            return left;
        }
        let (TypeKind::Integer(left_kind), TypeKind::Integer(right_kind)) =
            (self.types.kind(left), self.types.kind(right))
        else {
            return if self.integer_rank(left) >= self.integer_rank(right) {
                left
            } else {
                right
            };
        };
        let (left_kind, right_kind) = (*left_kind, *right_kind);
        let left_signed = is_signed_integer(left_kind, self.target);
        let right_signed = is_signed_integer(right_kind, self.target);
        if left_signed == right_signed {
            return if self.integer_rank(left) >= self.integer_rank(right) {
                left
            } else {
                right
            };
        }
        let (unsigned, unsigned_kind, signed, signed_kind) = if left_signed {
            (right, right_kind, left, left_kind)
        } else {
            (left, left_kind, right, right_kind)
        };
        if self.integer_rank(unsigned) >= self.integer_rank(signed) {
            return unsigned;
        }
        if self.target.integer_width(signed_kind) > self.target.integer_width(unsigned_kind) {
            return signed;
        }
        self.types
            .intern(TypeKind::Integer(unsigned_corresponding(signed_kind)))
    }

    pub(super) fn integer_rank(&self, ty: QualType) -> u8 {
        match self.types.kind(ty) {
            TypeKind::Integer(IntegerKind::Bool) => 0,
            TypeKind::Integer(
                IntegerKind::Char | IntegerKind::SignedChar | IntegerKind::UnsignedChar,
            ) => 1,
            TypeKind::Integer(IntegerKind::Short | IntegerKind::UnsignedShort) => 2,
            TypeKind::Integer(IntegerKind::Int | IntegerKind::UnsignedInt) => 3,
            TypeKind::Integer(IntegerKind::Long | IntegerKind::UnsignedLong) => 4,
            TypeKind::Integer(IntegerKind::LongLong | IntegerKind::UnsignedLongLong) => 5,
            TypeKind::Float => 6,
            TypeKind::Double => 7,
            TypeKind::LongDouble => 8,
            _ => 0,
        }
    }

    /// Declarators nest as deeply as the source spells them, so the
    /// pointer/array/qualifier spine is unwound iteratively; recursing once per
    /// level overflows the stack on inputs with thousands of `*`.
    pub(super) fn canonical_type(&mut self, parsed: &CType) -> QualType {
        let mut spine = Vec::new();
        let mut leaf = parsed;
        while let Some(inner) = leaf.derived_inner() {
            spine.push(leaf);
            leaf = inner;
        }

        let mut ty = self.canonical_leaf_type(leaf);
        while let Some(derived) = spine.pop() {
            ty = self.derive_canonical_type(derived, ty);
        }
        ty
    }

    pub(super) fn derive_canonical_type(&mut self, derived: &CType, inner: QualType) -> QualType {
        match derived {
            CType::Pointer(_) => self.types.intern(TypeKind::Pointer(inner)),
            CType::Array(_, length) => {
                let length = length
                    .as_ref()
                    .and_then(|length| self.constant_array_length(length));
                self.types.intern(TypeKind::Array(inner, length))
            }
            CType::Const(_) => with_qualifier(inner, Qualifiers::CONST),
            CType::Volatile(_) => with_qualifier(inner, Qualifiers::VOLATILE),
            CType::Restrict(_) => with_qualifier(inner, Qualifiers::RESTRICT),
            CType::Attributed(_, _) => inner,
            _ => unreachable!("not a derived type"),
        }
    }

    pub(super) fn constant_array_length(&mut self, length: &ArrayLength) -> Option<u64> {
        let expression = length.expression()?;
        if self.ast.get_annotation(expression).is_none() {
            self.node(expression);
        }
        self.ast
            .get_annotation(expression)
            .and_then(|semantics| semantics.constant)
            .and_then(|value| value.try_into().ok())
    }

    pub(super) fn canonical_leaf_type(&mut self, parsed: &CType) -> QualType {
        match parsed {
            CType::Invalid(_) => self.types.intern(TypeKind::Error),
            CType::Void => self.types.intern(TypeKind::Void),
            CType::Bool => self.types.intern(TypeKind::Integer(IntegerKind::Bool)),
            CType::Char => self.types.intern(TypeKind::Integer(IntegerKind::Char)),
            CType::SignedChar => self
                .types
                .intern(TypeKind::Integer(IntegerKind::SignedChar)),
            CType::UnsignedChar => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedChar)),
            CType::Short => self.types.intern(TypeKind::Integer(IntegerKind::Short)),
            CType::UnsignedShort => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedShort)),
            CType::Int => self.types.intern(TypeKind::Integer(IntegerKind::Int)),
            CType::UnsignedInt => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedInt)),
            CType::Long => self.types.intern(TypeKind::Integer(IntegerKind::Long)),
            CType::UnsignedLong => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedLong)),
            CType::LongLong => self.types.intern(TypeKind::Integer(IntegerKind::LongLong)),
            CType::UnsignedLongLong => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedLongLong)),
            CType::Float => self.types.intern(TypeKind::Float),
            CType::Double => self.types.intern(TypeKind::Double),
            CType::LongDouble => self.types.intern(TypeKind::LongDouble),
            CType::Function {
                ret,
                params,
                varargs,
                has_parameter_type_list,
            } => {
                let ret = self.canonical_type(ret);
                let params = self.canonical_params(params);
                self.types.intern(TypeKind::Function {
                    ret,
                    params,
                    varargs: *varargs,
                    prototype: *has_parameter_type_list
                        || self.options.std_version == StdVersion::C23,
                })
            }
            CType::Record(_, id, _) => self.types.intern(TypeKind::Record(*id)),
            CType::Enum(name) => self.types.intern(TypeKind::Enum(name.clone())),
            CType::Named(name) => self
                .lookup(name)
                .filter(|symbol| symbol.typedef)
                .map(|symbol| symbol.ty)
                .unwrap_or_else(|| self.types.intern(TypeKind::Error)),
            CType::Pointer(_)
            | CType::Array(..)
            | CType::Const(_)
            | CType::Volatile(_)
            | CType::Restrict(_)
            | CType::Attributed(..) => unreachable!("derived type is not a leaf"),
        }
    }

    pub(super) fn canonical_params(&mut self, params: &[CParam]) -> Vec<QualType> {
        params
            .iter()
            .map(|param| self.canonical_parameter_type(&param.ty))
            .collect()
    }

    pub(super) fn canonical_parameter_type(&mut self, parsed: &CType) -> QualType {
        let ty = self.canonical_type(parsed);
        match self.types.kind(ty).clone() {
            TypeKind::Array(element, _) => self.types.intern(TypeKind::Pointer(element)),
            TypeKind::Function { .. } => self.types.intern(TypeKind::Pointer(ty)),
            _ => ty,
        }
    }
}
