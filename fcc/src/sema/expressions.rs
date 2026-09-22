//! Expression inference and operand diagnostics.

use super::references::{
    assignment_reference, call_designator_reference, call_reference, cast_reference,
    conditional_reference, increment_reference, member_reference, operand_reference,
    simple_assignment_reference, sizeof_reference, type_specifier_reference, undeclared_reference,
};
use super::{
    Analyzer, IntegerKind, NodeSemantics, QualType, Symbol, TypeKind, ValueCategory, operator_text,
    unsigned_corresponding,
};
use crate::ast::{AstKind, AstLeaf, CType};
use crate::diagnostics::{
    ArgumentMismatch, CalledObjectNotFunction, CompleteObjectTypeRequired, IncompatibleConversion,
    InvalidOperands, InvalidTypeSpecifiers, ModifiableLvalueRequired, Span, UndeclaredIdentifier,
};
use tir::graph::{Dag, MutDag, NodeId};

impl Analyzer<'_> {
    pub(super) fn validate_parsed_type(&mut self, span: Span, parsed: &CType) {
        match parsed {
            CType::Invalid(spelling) => self.diagnostics.push(
                InvalidTypeSpecifiers::new(
                    span,
                    spelling.clone(),
                    type_specifier_reference(self.options),
                )
                .into(),
            ),
            CType::Const(inner)
            | CType::Volatile(inner)
            | CType::Restrict(inner)
            | CType::Pointer(inner)
            | CType::Array(inner, _)
            | CType::Attributed(inner, _) => self.validate_parsed_type(span, inner),
            CType::Function { ret, params, .. } => {
                self.validate_parsed_type(span, ret);
                for param in params {
                    self.validate_parsed_type(span, &param.ty);
                }
            }
            _ => {}
        }
    }

    pub(super) fn lookup(&self, name: &str) -> Option<Symbol> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    pub(super) fn require_name(&mut self, node: NodeId, name: &str) -> Option<Symbol> {
        let symbol = self.lookup(name);
        if symbol.is_none() {
            self.diagnostics.push(
                UndeclaredIdentifier::new(
                    self.ast.get_node(node).span,
                    name,
                    undeclared_reference(self.options),
                )
                .into(),
            );
        }
        symbol
    }

    pub(super) fn infer_expression(&mut self, node: NodeId) {
        let kind = self.ast.get_node(node).kind;
        let int = self.types.intern(TypeKind::Integer(IntegerKind::Int));
        let error = self.types.intern(TypeKind::Error);
        let mut semantics = NodeSemantics::default();
        let (ty, category) = match kind {
            AstKind::Int => {
                let Some(AstLeaf::Int(literal)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                (
                    self.integer_literal_type(node, &literal.spelling, literal.value.to_u64()),
                    ValueCategory::Value,
                )
            }
            AstKind::FloatLiteral => {
                let Some(AstLeaf::Float(literal)) = self.ast.get_leaf_data(node) else {
                    return;
                };
                let kind = match literal.kind {
                    crate::lexer::FloatingLiteralKind::Float => TypeKind::Float,
                    crate::lexer::FloatingLiteralKind::Double => TypeKind::Double,
                    crate::lexer::FloatingLiteralKind::LongDouble => TypeKind::LongDouble,
                };
                (self.types.intern(kind), ValueCategory::Value)
            }
            AstKind::Character => (int, ValueCategory::Value),
            AstKind::String => {
                let char_ty = self.types.intern(TypeKind::Integer(IntegerKind::Char));
                (
                    self.types.intern(TypeKind::Array(char_ty, None)),
                    ValueCategory::Lvalue,
                )
            }
            AstKind::Var => {
                let Some(AstLeaf::Var(name)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                self.infer_var(node, &name, error, &mut semantics)
            }
            AstKind::Member => {
                let Some(AstLeaf::Member { name, indirect }) =
                    self.ast.get_leaf_data(node).cloned()
                else {
                    return;
                };
                self.infer_member(node, &name, indirect, error, &mut semantics)
            }
            AstKind::Call => {
                let Some(AstLeaf::Call(name)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                self.infer_call(node, &name, error, &mut semantics)
            }
            AstKind::CallExpr => self.infer_call_expr(node, error, &mut semantics),
            AstKind::Add | AstKind::Sub | AstKind::Mul | AstKind::Div => {
                self.infer_additive(node, kind, error)
            }
            AstKind::Mod
            | AstKind::Shl
            | AstKind::Shr
            | AstKind::BitAnd
            | AstKind::BitXor
            | AstKind::BitOr => self.infer_bitwise(node, kind, error),
            AstKind::Lt | AstKind::Gt | AstKind::Le | AstKind::Ge | AstKind::Eq | AstKind::Ne => {
                self.infer_comparison(node, kind, int, error)
            }
            AstKind::LogAnd | AstKind::LogOr => self.infer_logical(node, kind, int, error),
            AstKind::Neg | AstKind::Pos | AstKind::BitNot | AstKind::Not => {
                self.infer_unary(node, kind, int, error)
            }
            AstKind::AddressOf => self.infer_address_of(node, kind, error),
            AstKind::Deref => self.infer_deref(node, kind, error),
            AstKind::Comma => {
                let operands = self.child_types(node);
                (
                    operands.last().copied().unwrap_or(error),
                    ValueCategory::Value,
                )
            }
            AstKind::Conditional => self.infer_conditional(node, error),
            AstKind::Cast => {
                let Some(AstLeaf::Type(parsed)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                let target = self.canonical_type(&parsed);
                self.infer_cast(node, target, error)
            }
            AstKind::SizeofType | AstKind::SizeofExpr => {
                let operand_ty = if kind == AstKind::SizeofType {
                    let Some(AstLeaf::Type(parsed)) = self.ast.get_leaf_data(node).cloned() else {
                        return;
                    };
                    self.canonical_type(&parsed)
                } else {
                    self.child_types(node).first().copied().unwrap_or(error)
                };
                self.annotate_sizeof(node, operand_ty);
                return;
            }
            AstKind::Assign => {
                let Some(AstLeaf::Assign(name)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                let Some(symbol) = self.require_name(node, &name) else {
                    self.ast.set_annotation(
                        node,
                        NodeSemantics {
                            ty: Some(error),
                            ..NodeSemantics::default()
                        },
                    );
                    return;
                };
                semantics.entity = Some(symbol.entity);
                self.infer_assign(node, symbol.ty, error)
            }
            AstKind::AssignExpr => self.infer_assign_expr(node, error),
            AstKind::AddAssign
            | AstKind::SubAssign
            | AstKind::MulAssign
            | AstKind::DivAssign
            | AstKind::ModAssign
            | AstKind::ShlAssign
            | AstKind::ShrAssign
            | AstKind::AndAssign
            | AstKind::XorAssign
            | AstKind::OrAssign => self.infer_compound_assign(node, error),
            AstKind::PreInc | AstKind::PreDec | AstKind::PostInc | AstKind::PostDec => {
                self.infer_incdec(node, kind, error)
            }
            _ => return,
        };
        semantics.ty = Some(ty);
        semantics.category = category;
        semantics.constant = semantics
            .constant
            .or_else(|| self.constant_value(node, kind, ty));
        self.ast.set_annotation(node, semantics);
    }

    pub(super) fn infer_var(
        &mut self,
        node: NodeId,
        name: &str,
        error: QualType,
        semantics: &mut NodeSemantics,
    ) -> (QualType, ValueCategory) {
        match self.require_name(node, name) {
            Some(symbol) if !symbol.typedef => {
                semantics.entity = Some(symbol.entity);
                semantics.constant = symbol.constant;
                let category = if symbol.constant.is_some() {
                    ValueCategory::Value
                } else if matches!(self.types.kind(symbol.ty), TypeKind::Function { .. }) {
                    ValueCategory::Function
                } else {
                    ValueCategory::Lvalue
                };
                (symbol.ty, category)
            }
            _ => (error, ValueCategory::Value),
        }
    }

    pub(super) fn infer_member(
        &mut self,
        node: NodeId,
        name: &str,
        indirect: bool,
        error: QualType,
        semantics: &mut NodeSemantics,
    ) -> (QualType, ValueCategory) {
        let base = self.ast.children(node).next().unwrap();
        let base_ty = self
            .ast
            .get_annotation(base)
            .and_then(|info| info.ty)
            .unwrap_or(error);
        let record = if indirect {
            match self.types.kind(base_ty) {
                TypeKind::Pointer(pointee) => match self.types.kind(*pointee) {
                    TypeKind::Record(id) => Some(*id),
                    _ => None,
                },
                _ => None,
            }
        } else {
            match self.types.kind(base_ty) {
                TypeKind::Record(id) => Some(*id),
                _ => None,
            }
        };
        let field = record
            .and_then(|id| self.record_indices.get(&id).copied())
            .and_then(|index| {
                self.records[index]
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, field)| field.name == name)
            });
        if let Some((index, field)) = field {
            semantics.member_index = Some(index);
            (field.ty, ValueCategory::Lvalue)
        } else {
            if let Some(id) = record {
                let record_name = self.records[self.record_indices[&id]].name.clone();
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(node).span,
                        format!("struct '{record_name}' has no member named '{name}'"),
                        member_reference(self.options),
                    )
                    .into(),
                );
            } else if !matches!(self.types.kind(base_ty), TypeKind::Error) {
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(node).span,
                        "member access requires a struct operand".to_string(),
                        member_reference(self.options),
                    )
                    .into(),
                );
            }
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_call(
        &mut self,
        node: NodeId,
        name: &str,
        error: QualType,
        semantics: &mut NodeSemantics,
    ) -> (QualType, ValueCategory) {
        match self.require_name(node, name) {
            Some(symbol) => {
                let function_ty = match self.types.kind(symbol.ty) {
                    TypeKind::Function { .. } => Some(symbol.ty),
                    TypeKind::Pointer(pointee)
                        if matches!(self.types.kind(*pointee), TypeKind::Function { .. }) =>
                    {
                        Some(*pointee)
                    }
                    _ => None,
                };
                if let Some(function_ty) = function_ty {
                    semantics.call_designator_ty = Some(symbol.ty);
                    semantics.entity = Some(symbol.entity);
                    let arguments = self.ast.children(node).collect::<Vec<_>>();
                    self.check_call(node, name, symbol.span, function_ty, &arguments, error)
                } else {
                    self.diagnostics.push(
                        CalledObjectNotFunction::new(
                            self.ast.get_node(node).span,
                            symbol.span,
                            name,
                            call_designator_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            None => (error, ValueCategory::Value),
        }
    }

    pub(super) fn infer_call_expr(
        &mut self,
        node: NodeId,
        error: QualType,
        semantics: &mut NodeSemantics,
    ) -> (QualType, ValueCategory) {
        let children = self.ast.children(node).collect::<Vec<_>>();
        let callee = children[0];
        let callee_info = self.ast.get_annotation(callee).cloned().unwrap_or_default();
        let designator_ty = callee_info.ty.unwrap_or(error);
        let function_ty = match self.types.kind(designator_ty) {
            TypeKind::Function { .. } => Some(designator_ty),
            TypeKind::Pointer(pointee)
                if matches!(self.types.kind(*pointee), TypeKind::Function { .. }) =>
            {
                Some(*pointee)
            }
            _ => None,
        };
        if let Some(function_ty) = function_ty {
            semantics.call_designator_ty = Some(designator_ty);
            self.check_call(
                node,
                "called expression",
                self.ast.get_node(callee).span,
                function_ty,
                &children[1..],
                error,
            )
        } else {
            self.diagnostics.push(
                CalledObjectNotFunction::new(
                    self.ast.get_node(node).span,
                    self.ast.get_node(callee).span,
                    "expression".to_string(),
                    call_designator_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_cast(
        &mut self,
        node: NodeId,
        target: QualType,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let operand = self.ast.children(node).next().unwrap();
        let source = self.child_types(node).first().copied().unwrap_or(error);
        let source = self.value_conversion(operand, source);
        let valid = matches!(self.types.kind(target), TypeKind::Void)
            || self.is_scalar(target) && self.is_scalar(source);
        if !valid
            && self.types.kind(target) != &TypeKind::Error
            && self.types.kind(source) != &TypeKind::Error
        {
            self.diagnostics.push(
                IncompatibleConversion::new(
                    self.ast.get_node(node).span,
                    None,
                    format!(
                        "cannot cast {} expression to {} type",
                        self.type_category(source),
                        self.type_category(target)
                    ),
                    cast_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else {
            (target, ValueCategory::Value)
        }
    }

    pub(super) fn annotate_sizeof(&mut self, node: NodeId, operand_ty: QualType) {
        let size = self.type_size(operand_ty);
        if size.is_none()
            && matches!(
                self.types.kind(operand_ty),
                TypeKind::Void | TypeKind::Function { .. } | TypeKind::Array(_, None)
            )
        {
            self.diagnostics.push(
                CompleteObjectTypeRequired::new(
                    self.ast.get_node(node).span,
                    "sizeof requires a complete object type",
                    sizeof_reference(self.options),
                )
                .into(),
            );
        }
        let size_ty = self.types.intern(TypeKind::Integer(unsigned_corresponding(
            self.target.pointer_sized_integer(),
        )));
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(size_ty),
                entity: None,
                category: ValueCategory::Value,
                constant: size.map(|value| value as i64),
                conversions: Vec::new(),
                member_index: None,
                initializer_path: None,
                call_designator_ty: None,
            },
        );
    }

    pub(super) fn infer_assign(
        &mut self,
        node: NodeId,
        target: QualType,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let rhs = self.ast.children(node).next().unwrap();
        let source = self
            .ast
            .get_annotation(rhs)
            .and_then(|info| info.ty)
            .unwrap_or(error);
        let source = self.assignment_source(target, source, rhs);
        if target.qualifiers.is_const() {
            self.diagnostics.push(
                ModifiableLvalueRequired::new(
                    self.ast.get_node(node).span,
                    "left operand is not a modifiable lvalue",
                    assignment_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else if !self.assignment_compatible(target, source, rhs) {
            self.diagnostics.push(
                IncompatibleConversion::new(
                    self.ast.get_node(node).span,
                    None,
                    self.conversion_message(target, source),
                    simple_assignment_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else {
            self.record_conversion(rhs, target);
            (target, ValueCategory::Value)
        }
    }
    pub(super) fn infer_additive(
        &mut self,
        node: NodeId,
        kind: AstKind,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let children = self.ast.children(node).collect::<Vec<_>>();
        let mut operands = self.child_types(node);
        for (&child, operand) in children.iter().zip(&mut operands) {
            let element = match self.types.kind(*operand) {
                TypeKind::Array(element, _) => Some(*element),
                _ => None,
            };
            if let Some(element) = element {
                let pointer = self.types.intern(TypeKind::Pointer(element));
                self.record_conversion(child, pointer);
                *operand = pointer;
            }
        }
        let pointer_result = if operands.len() == 2 {
            match (
                self.types.kind(operands[0]),
                self.types.kind(operands[1]),
                kind,
            ) {
                (
                    TypeKind::Pointer(_),
                    TypeKind::Integer(_) | TypeKind::Enum(_),
                    AstKind::Add | AstKind::Sub,
                ) => Some(operands[0]),
                (TypeKind::Integer(_) | TypeKind::Enum(_), TypeKind::Pointer(_), AstKind::Add) => {
                    Some(operands[1])
                }
                _ => None,
            }
        } else {
            None
        };
        let pointer_difference = if operands.len() == 2 && kind == AstKind::Sub {
            match (self.types.kind(operands[0]), self.types.kind(operands[1])) {
                (TypeKind::Pointer(left), TypeKind::Pointer(right))
                    if self.types.kind(*left) == self.types.kind(*right)
                        && self.type_size(*left).is_some() =>
                {
                    Some(
                        self.types
                            .intern(TypeKind::Integer(self.target.pointer_sized_integer())),
                    )
                }
                _ => None,
            }
        } else {
            None
        };
        let subtracts_pointers = operands.len() == 2
            && kind == AstKind::Sub
            && operands
                .iter()
                .all(|ty| matches!(self.types.kind(*ty), TypeKind::Pointer(_)));
        if let Some(result) = pointer_difference {
            (result, ValueCategory::Value)
        } else if subtracts_pointers {
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    "pointer subtraction requires pointers to compatible complete object types",
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else if let Some(result) = pointer_result {
            // The scaling in codegen reads the index operand's width, so an
            // enum index has to reach it as its promoted integer type.
            for (index, &operand) in operands.iter().enumerate() {
                if matches!(self.types.kind(operand), TypeKind::Enum(_)) {
                    let promoted = self.integer_promotion(operand);
                    self.record_conversion(children[index], promoted);
                }
            }
            let TypeKind::Pointer(pointee) = self.types.kind(result) else {
                unreachable!("pointer arithmetic result has pointer type")
            };
            if self.type_size(*pointee).is_some() {
                (result, ValueCategory::Value)
            } else {
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(node).span,
                        "pointer arithmetic requires a pointer to a complete object type",
                        operand_reference(self.options, kind),
                    )
                    .into(),
                );
                (error, ValueCategory::Value)
            }
        } else if operands.len() == 2 && operands.iter().all(|&ty| self.is_arithmetic(ty)) {
            let result = self.common_arithmetic_type(operands[0], operands[1]);
            self.record_operand_conversions(node, &operands, result);
            (result, ValueCategory::Value)
        } else if operands
            .iter()
            .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
        {
            (error, ValueCategory::Value)
        } else {
            let operator = operator_text(kind);
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    format!("operator '{operator}' requires arithmetic operands"),
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_bitwise(
        &mut self,
        node: NodeId,
        kind: AstKind,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let operands = self.child_types(node);
        if operands.len() == 2 && operands.iter().all(|&ty| self.is_integer(ty)) {
            let result = self.common_arithmetic_type(operands[0], operands[1]);
            self.record_operand_conversions(node, &operands, result);
            (result, ValueCategory::Value)
        } else if operands
            .iter()
            .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
        {
            (error, ValueCategory::Value)
        } else {
            let operator = operator_text(kind);
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    format!("operator '{operator}' requires integer operands"),
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_comparison(
        &mut self,
        node: NodeId,
        kind: AstKind,
        int: QualType,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let children = self.ast.children(node).collect::<Vec<_>>();
        let mut operands = self.child_types(node);
        for (&child, operand) in children.iter().zip(&mut operands) {
            *operand = self.value_conversion(child, *operand);
        }
        let arithmetic = operands.len() == 2 && operands.iter().all(|&ty| self.is_arithmetic(ty));
        let pointers = operands.len() == 2
            && operands
                .iter()
                .all(|&ty| matches!(self.types.kind(ty), TypeKind::Pointer(_)));
        if operands.len() == 2 && matches!(kind, AstKind::Eq | AstKind::Ne) {
            for (pointer, null) in [(0, 1), (1, 0)] {
                if matches!(self.types.kind(operands[pointer]), TypeKind::Pointer(_))
                    && self.is_null_pointer_constant(operands[null], children[null])
                {
                    self.record_conversion(children[null], operands[pointer]);
                    return (int, ValueCategory::Value);
                }
            }
        }
        if arithmetic {
            // The result is `int`, but the operands are still brought to
            // their common type first, so the comparison never sees two
            // different widths.
            let common = self.common_arithmetic_type(operands[0], operands[1]);
            self.record_operand_conversions(node, &operands, common);
            (int, ValueCategory::Value)
        } else if pointers {
            (int, ValueCategory::Value)
        } else if operands
            .iter()
            .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
        {
            (error, ValueCategory::Value)
        } else {
            let operator = operator_text(kind);
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    format!("operator '{operator}' requires compatible scalar operands"),
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_logical(
        &mut self,
        node: NodeId,
        kind: AstKind,
        int: QualType,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let operands = self.child_types(node);
        if operands.len() == 2 && operands.iter().all(|&ty| self.is_scalar(ty)) {
            (int, ValueCategory::Value)
        } else if operands
            .iter()
            .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
        {
            (error, ValueCategory::Value)
        } else {
            let operator = operator_text(kind);
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    format!("operator '{operator}' requires scalar operands"),
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_unary(
        &mut self,
        node: NodeId,
        kind: AstKind,
        int: QualType,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let operand = self.child_types(node).first().copied().unwrap_or(error);
        let valid = match kind {
            AstKind::Neg | AstKind::Pos => self.is_arithmetic(operand),
            AstKind::BitNot => self.is_integer(operand),
            AstKind::Not => self.is_scalar(operand),
            _ => unreachable!(),
        };
        if valid {
            if kind == AstKind::Not {
                (int, ValueCategory::Value)
            } else {
                // The operation runs at the promoted type, so the
                // operand is converted to it like a binary operand is.
                let result = self.integer_promotion(operand);
                self.record_operand_conversions(node, &[operand], result);
                (result, ValueCategory::Value)
            }
        } else if self.types.kind(operand) == &TypeKind::Error {
            (error, ValueCategory::Value)
        } else {
            let expected = if kind == AstKind::BitNot {
                "integer"
            } else if kind == AstKind::Not {
                "scalar"
            } else {
                "arithmetic"
            };
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    format!(
                        "operator '{}' requires an {expected} operand",
                        operator_text(kind)
                    ),
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_address_of(
        &mut self,
        node: NodeId,
        kind: AstKind,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let child = self.ast.children(node).next().unwrap();
        let info = self.ast.get_annotation(child).cloned().unwrap_or_default();
        let operand = info.ty.unwrap_or(error);
        if matches!(
            info.category,
            ValueCategory::Lvalue | ValueCategory::Function
        ) {
            (
                self.types.intern(TypeKind::Pointer(operand)),
                ValueCategory::Value,
            )
        } else if self.types.kind(operand) == &TypeKind::Error {
            (error, ValueCategory::Value)
        } else {
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    "operator '&' requires an lvalue or function operand",
                    operand_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_deref(
        &mut self,
        node: NodeId,
        kind: AstKind,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let operand = self.child_types(node).first().copied().unwrap_or(error);
        match self.types.kind(operand).clone() {
            TypeKind::Pointer(pointee) => {
                let category = if matches!(self.types.kind(pointee), TypeKind::Function { .. }) {
                    ValueCategory::Function
                } else {
                    ValueCategory::Lvalue
                };
                (pointee, category)
            }
            TypeKind::Error => (error, ValueCategory::Value),
            _ => {
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(node).span,
                        "operator '*' requires a pointer operand",
                        operand_reference(self.options, kind),
                    )
                    .into(),
                );
                (error, ValueCategory::Value)
            }
        }
    }

    pub(super) fn infer_conditional(
        &mut self,
        node: NodeId,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let children = self.ast.children(node).collect::<Vec<_>>();
        let types = self.child_types(node);
        if children.len() != 3 || types.len() != 3 {
            (error, ValueCategory::Value)
        } else if !self.is_scalar(types[0]) {
            if self.types.kind(types[0]) != &TypeKind::Error {
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(children[0]).span,
                        "conditional operator requires a scalar condition",
                        conditional_reference(self.options),
                    )
                    .into(),
                );
            }
            (error, ValueCategory::Value)
        } else if self.is_arithmetic(types[1]) && self.is_arithmetic(types[2]) {
            let common = self.common_arithmetic_type(types[1], types[2]);
            self.record_conversion(children[1], common);
            self.record_conversion(children[2], common);
            (common, ValueCategory::Value)
        } else if types[1] == types[2]
            || matches!(self.types.kind(types[1]), TypeKind::Pointer(_))
                && self.is_null_pointer_constant(types[2], children[2])
        {
            self.record_conversion(children[2], types[1]);
            (types[1], ValueCategory::Value)
        } else if matches!(self.types.kind(types[2]), TypeKind::Pointer(_))
            && self.is_null_pointer_constant(types[1], children[1])
        {
            self.record_conversion(children[1], types[2]);
            (types[2], ValueCategory::Value)
        } else {
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    "conditional operator has incompatible result operands",
                    conditional_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }

    pub(super) fn infer_assign_expr(
        &mut self,
        node: NodeId,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let children = self.ast.children(node).collect::<Vec<_>>();
        let lhs = children[0];
        let rhs = children[1];
        let lhs_info = self.ast.get_annotation(lhs).cloned().unwrap_or_default();
        let lhs_ty = lhs_info.ty.unwrap_or(error);
        if lhs_info.category != ValueCategory::Lvalue || lhs_ty.qualifiers.is_const() {
            self.diagnostics.push(
                ModifiableLvalueRequired::new(
                    self.ast.get_node(node).span,
                    "left operand is not a modifiable lvalue",
                    assignment_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else {
            let source = self
                .ast
                .get_annotation(rhs)
                .and_then(|info| info.ty)
                .unwrap_or(error);
            let source = self.assignment_source(lhs_ty, source, rhs);
            if !self.assignment_compatible(lhs_ty, source, rhs) {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(node).span,
                        None,
                        self.conversion_message(lhs_ty, source),
                        simple_assignment_reference(self.options),
                    )
                    .into(),
                );
                (error, ValueCategory::Value)
            } else {
                self.record_conversion(rhs, lhs_ty);
                (lhs_ty, ValueCategory::Value)
            }
        }
    }

    pub(super) fn infer_compound_assign(
        &mut self,
        node: NodeId,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let children = self.ast.children(node).collect::<Vec<_>>();
        let lhs = children.first().copied();
        let lhs_info = lhs.and_then(|child| self.ast.get_annotation(child));
        if lhs_info.is_none_or(|info| info.category != ValueCategory::Lvalue) {
            self.diagnostics.push(
                ModifiableLvalueRequired::new(
                    self.ast.get_node(node).span,
                    "left operand is not a modifiable lvalue",
                    assignment_reference(self.options),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else {
            let lhs_ty = lhs_info.and_then(|info| info.ty).unwrap_or(error);
            if lhs_ty.qualifiers.is_const() {
                self.diagnostics.push(
                    ModifiableLvalueRequired::new(
                        self.ast.get_node(node).span,
                        "left operand is not a modifiable lvalue",
                        assignment_reference(self.options),
                    )
                    .into(),
                );
                (error, ValueCategory::Value)
            } else {
                // `E1 op= E2` is `E1 = E1 op E2`: the operation itself
                // runs at the usual-arithmetic-conversions common type,
                // and only the store narrows back to E1's type.
                if let Some(rhs) = children.get(1).copied()
                    && let Some(rhs_ty) = self.ast.get_annotation(rhs).and_then(|info| info.ty)
                    && self.is_arithmetic(lhs_ty)
                    && self.is_arithmetic(rhs_ty)
                {
                    let common = self.common_arithmetic_type(lhs_ty, rhs_ty);
                    self.record_conversion(rhs, common);
                }
                (lhs_ty, ValueCategory::Value)
            }
        }
    }

    pub(super) fn infer_incdec(
        &mut self,
        node: NodeId,
        kind: AstKind,
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let child = self.ast.children(node).next().unwrap();
        let info = self.ast.get_annotation(child).cloned().unwrap_or_default();
        let operand_ty = info.ty.unwrap_or(error);
        if info.category != ValueCategory::Lvalue || operand_ty.qualifiers.is_const() {
            self.diagnostics.push(
                ModifiableLvalueRequired::new(
                    self.ast.get_node(node).span,
                    "operand is not a modifiable lvalue",
                    increment_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        } else if self.is_arithmetic(operand_ty)
            || matches!(self.types.kind(operand_ty), TypeKind::Pointer(_))
        {
            (operand_ty, ValueCategory::Value)
        } else {
            self.diagnostics.push(
                InvalidOperands::new(
                    self.ast.get_node(node).span,
                    "increment and decrement require real or pointer operands",
                    increment_reference(self.options, kind),
                )
                .into(),
            );
            (error, ValueCategory::Value)
        }
    }
    pub(super) fn check_call(
        &mut self,
        node: NodeId,
        callee: &str,
        declaration_span: Span,
        function_ty: QualType,
        arguments: &[NodeId],
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let TypeKind::Function {
            ret,
            params,
            varargs,
            prototype,
        } = self.types.kind(function_ty).clone()
        else {
            unreachable!("call checker receives a function type")
        };
        let actual = arguments.len();
        let valid = if !prototype {
            true
        } else if varargs {
            actual >= params.len()
        } else {
            actual == params.len()
        };
        if !valid {
            self.diagnostics.push(
                ArgumentMismatch::new(
                    self.ast.get_node(node).span,
                    declaration_span,
                    format!(
                        "function '{callee}' expects {} arguments but {actual} was provided",
                        params.len()
                    ),
                    call_reference(self.options),
                )
                .into(),
            );
        }
        for (index, (&argument, &parameter)) in arguments.iter().zip(&params).enumerate() {
            let source = self
                .ast
                .get_annotation(argument)
                .and_then(|info| info.ty)
                .unwrap_or(error);
            let source = self.assignment_source(parameter, source, argument);
            if !self.assignment_compatible(parameter, source, argument) {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(argument).span,
                        Some(declaration_span),
                        format!(
                            "argument {} to '{callee}' has incompatible {} type",
                            index + 1,
                            self.type_category(source)
                        ),
                        call_reference(self.options),
                    )
                    .into(),
                );
            } else {
                self.record_conversion(argument, parameter);
            }
        }
        let promoted_start = if !prototype {
            0
        } else if varargs {
            params.len()
        } else {
            arguments.len()
        };
        for &argument in arguments.iter().skip(promoted_start) {
            let source = self
                .ast
                .get_annotation(argument)
                .and_then(|info| info.ty)
                .unwrap_or(error);
            let source = self.value_conversion(argument, source);
            let promoted = match self.types.kind(source) {
                TypeKind::Float => self.types.intern(TypeKind::Double),
                TypeKind::Integer(_) | TypeKind::Enum(_) => self.integer_promotion(source),
                _ => source,
            };
            if promoted != source {
                self.record_conversion(argument, promoted);
            }
        }
        (ret, ValueCategory::Value)
    }
}
