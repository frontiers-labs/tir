//! Local declarations and aggregate initializer checks.

use super::references::{
    initializer_reference, object_type_reference, qualifier_reference, redefinition_reference,
};
use super::{Analyzer, NodeSemantics, QualType, Symbol, TypeKind, ValueCategory};
use crate::ast::{AstKind, AstLeaf, InitializerDesignator, RecordKind};
use crate::diagnostics::{
    CompleteObjectTypeRequired, IncompatibleConversion, InvalidOperands, InvalidTypeQualifier,
    Redefinition,
};
use tir::graph::{Dag, MutDag, NodeId};

impl Analyzer<'_> {
    pub(super) fn declaration(&mut self, node: NodeId) {
        let leaf = self.ast.get_leaf_data(node).cloned();
        let (name, parsed_ty, typedef) = match leaf {
            Some(AstLeaf::Param { name, ty }) | Some(AstLeaf::Decl { name, ty }) => {
                (name, ty, false)
            }
            Some(AstLeaf::Typedef { name, ty }) => (name, ty, true),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        let span = self.ast.get_node(node).span;
        self.validate_parsed_type(span, &parsed_ty);
        let children = self.ast.children(node).collect::<Vec<_>>();
        let mut ty = if self.ast.get_node(node).kind == AstKind::Param {
            self.canonical_parameter_type(&parsed_ty)
        } else {
            self.canonical_type(&parsed_ty)
        };
        let previous = self.scopes.last().unwrap().get(&name).cloned();
        let redefined = previous.is_some();
        let entity = self.new_entity();
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(ty),
                entity: Some(entity),
                category: ValueCategory::Lvalue,
                ..NodeSemantics::default()
            },
        );
        if let Some(previous) = previous {
            self.diagnostics.push(
                Redefinition::new(
                    span,
                    previous.span,
                    name.clone(),
                    redefinition_reference(self.options),
                )
                .into(),
            );
        } else {
            self.scopes.last_mut().unwrap().insert(
                name.clone(),
                Symbol {
                    span,
                    ty,
                    entity,
                    typedef,
                    defined: true,
                    constant: None,
                },
            );
        }
        for &child in &children {
            self.node(child);
        }
        if self.ast.get_node(node).kind == AstKind::Decl
            && let Some(&initializer) = children.first()
            && self.ast.get_node(initializer).kind == AstKind::InitializerList
            && let TypeKind::Array(element, None) = self.types.kind(ty)
            && let Some(length) = self.inferred_array_length(initializer)
        {
            ty = self.types.intern(TypeKind::Array(*element, Some(length)));
            self.ast.set_annotation(
                node,
                NodeSemantics {
                    ty: Some(ty),
                    entity: Some(entity),
                    category: ValueCategory::Lvalue,
                    ..NodeSemantics::default()
                },
            );
            if !redefined {
                self.scopes.last_mut().unwrap().get_mut(&name).unwrap().ty = ty;
            }
        }
        if !typedef {
            let message = match self.types.kind(ty) {
                TypeKind::Void => Some(format!("object '{name}' cannot have void type")),
                TypeKind::Record(_) if self.type_layout(ty).is_none() => {
                    Some(format!("object '{name}' has incomplete struct type"))
                }
                TypeKind::Array(_, _) if self.type_layout(ty).is_none() => {
                    Some(format!("object '{name}' has incomplete array type"))
                }
                _ => None,
            };
            if let Some(message) = message {
                self.diagnostics.push(
                    CompleteObjectTypeRequired::new(
                        span,
                        message,
                        object_type_reference(self.options),
                    )
                    .into(),
                );
            }
        }
        if ty.qualifiers.is_restrict() && !matches!(self.types.kind(ty), TypeKind::Pointer(_)) {
            self.diagnostics.push(
                InvalidTypeQualifier::new(
                    span,
                    "restrict qualifier requires a pointer-derived object type",
                    qualifier_reference(self.options),
                )
                .into(),
            );
        }
        if self.ast.get_node(node).kind == AstKind::Decl
            && let Some(&initializer) = children.first()
        {
            self.validate_initializer(ty, initializer);
        }
    }

    pub(super) fn inferred_array_length(&self, initializer: NodeId) -> Option<u64> {
        let mut next = 0_u64;
        let mut length = 0_u64;
        for value in self.ast.children(initializer) {
            if let Some(AstLeaf::DesignatedInitializer(InitializerDesignator::Index)) =
                self.ast.get_leaf_data(value)
            {
                let index = self.ast.children(value).next().unwrap();
                next = self.ast.get_annotation(index)?.constant?.try_into().ok()?;
            }
            next = next.checked_add(1)?;
            length = length.max(next);
        }
        Some(length)
    }

    pub(super) fn validate_initializer(&mut self, target: QualType, initializer: NodeId) {
        if self.ast.get_node(initializer).kind == AstKind::InitializerList {
            self.validate_initializer_list(target, initializer);
            return;
        }
        let source = self
            .ast
            .get_annotation(initializer)
            .and_then(|info| info.ty)
            .unwrap_or(target);
        let source = self.assignment_source(target, source, initializer);
        if !self.assignment_compatible(target, source, initializer) {
            self.diagnostics.push(
                IncompatibleConversion::new(
                    self.ast.get_node(initializer).span,
                    None,
                    format!(
                        "cannot initialize {} with {} value",
                        self.type_category(target),
                        self.type_category(source)
                    ),
                    initializer_reference(self.options),
                )
                .into(),
            );
        } else {
            self.record_conversion(initializer, target);
        }
    }

    pub(super) fn validate_initializer_list(&mut self, target: QualType, initializer: NodeId) {
        let aggregate = match self.types.kind(target) {
            TypeKind::Record(id) => {
                if self.records[self.record_indices[id]].kind == RecordKind::Union {
                    "union"
                } else {
                    "record"
                }
            }
            TypeKind::Array(_, Some(_)) => "array",
            _ => {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(initializer).span,
                        None,
                        "brace initializer requires an aggregate object",
                        initializer_reference(self.options),
                    )
                    .into(),
                );
                return;
            }
        };
        let values = self.ast.children(initializer).collect::<Vec<_>>();
        let mut next_path = self.first_initializer_path(target);
        for value in values {
            if self.ast.get_node(value).kind == AstKind::DesignatedInitializer {
                if let Some(path) = self.validate_designated_initializer(target, value) {
                    self.set_initializer_path(value, path.clone());
                    next_path = self.next_initializer_path(target, &path);
                }
            } else if let Some(path) = next_path {
                let selected_type = self.initializer_path_type(target, &path).unwrap();
                self.set_initializer_path(value, path.clone());
                self.validate_initializer_value(selected_type, value);
                next_path = self.next_initializer_path(target, &path);
            } else {
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(value).span,
                        format!("too many initializers for {aggregate}"),
                        initializer_reference(self.options),
                    )
                    .into(),
                );
            }
        }
    }

    pub(super) fn first_initializer_path(&self, target: QualType) -> Option<Vec<usize>> {
        (self.initializer_member_count(target)? != 0).then(|| vec![0])
    }

    pub(super) fn initializer_member_count(&self, target: QualType) -> Option<usize> {
        match self.types.kind(target) {
            TypeKind::Record(id) => {
                let record = &self.records[self.record_indices[id]];
                Some(if record.kind == RecordKind::Union {
                    usize::from(!record.fields.is_empty())
                } else {
                    record.fields.len()
                })
            }
            TypeKind::Array(_, Some(length)) => Some(*length as usize),
            _ => None,
        }
    }

    pub(super) fn initializer_member_type(
        &self,
        target: QualType,
        index: usize,
    ) -> Option<QualType> {
        match self.types.kind(target) {
            TypeKind::Record(id) => self.records[self.record_indices[id]]
                .fields
                .get(index)
                .map(|field| field.ty),
            TypeKind::Array(element, Some(length)) if index < *length as usize => Some(*element),
            _ => None,
        }
    }

    pub(super) fn initializer_path_type(
        &self,
        target: QualType,
        path: &[usize],
    ) -> Option<QualType> {
        path.iter().try_fold(target, |target, &index| {
            self.initializer_member_type(target, index)
        })
    }

    pub(super) fn next_initializer_path(
        &self,
        target: QualType,
        path: &[usize],
    ) -> Option<Vec<usize>> {
        let mut next = path.to_vec();
        while let Some(index) = next.pop() {
            let parent = self.initializer_path_type(target, &next)?;
            if index + 1 < self.initializer_member_count(parent)? {
                next.push(index + 1);
                return Some(next);
            }
        }
        None
    }

    pub(super) fn set_initializer_path(&mut self, initializer: NodeId, path: Vec<usize>) {
        let mut semantics = self
            .ast
            .get_annotation(initializer)
            .cloned()
            .unwrap_or_default();
        semantics.initializer_path = Some(path);
        self.ast.set_annotation(initializer, semantics);
    }

    pub(super) fn validate_designated_initializer(
        &mut self,
        target: QualType,
        initializer: NodeId,
    ) -> Option<Vec<usize>> {
        let (index, selected, selected_type) = match self.ast.get_leaf_data(initializer).cloned()? {
            AstLeaf::DesignatedInitializer(InitializerDesignator::Field(name)) => {
                let TypeKind::Record(id) = self.types.kind(target) else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "field designator requires a record",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                let Some((index, field)) = self.records[self.record_indices[id]]
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, field)| field.name == name)
                    .map(|(index, field)| (index, field.ty))
                else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            format!("record has no member named '{name}'"),
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                (index, self.ast.children(initializer).next().unwrap(), field)
            }
            AstLeaf::DesignatedInitializer(InitializerDesignator::Index) => {
                let TypeKind::Array(element, Some(length)) = self.types.kind(target) else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "array designator requires an array",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                let (element, length) = (*element, *length);
                let mut children = self.ast.children(initializer);
                let index_expression = children.next().unwrap();
                let selected = children.next().unwrap();
                let Some(index) = self
                    .ast
                    .get_annotation(index_expression)
                    .and_then(|info| info.constant)
                    .filter(|index| *index >= 0)
                    .map(|index| index as usize)
                else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "array designator requires a nonnegative integer constant",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                if index >= length as usize {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "array designator index exceeds array bounds",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                }
                (index, selected, element)
            }
            _ => unreachable!(),
        };
        self.ast.set_annotation(
            initializer,
            NodeSemantics {
                member_index: Some(index),
                ..NodeSemantics::default()
            },
        );
        let mut path = vec![index];
        if self.ast.get_node(selected).kind == AstKind::DesignatedInitializer {
            path.extend(self.validate_designated_initializer(selected_type, selected)?);
        } else {
            self.validate_initializer_value(selected_type, selected);
        }
        Some(path)
    }

    pub(super) fn validate_initializer_value(&mut self, target: QualType, value: NodeId) {
        if self.ast.get_node(value).kind == AstKind::DesignatedInitializer {
            self.validate_designated_initializer(target, value);
            return;
        }
        if self.ast.get_node(value).kind == AstKind::InitializerList {
            self.validate_initializer_list(target, value);
            return;
        }
        let source = self
            .ast
            .get_annotation(value)
            .and_then(|info| info.ty)
            .unwrap_or(target);
        let source = self.assignment_source(target, source, value);
        if !self.assignment_compatible(target, source, value) {
            self.diagnostics.push(
                IncompatibleConversion::new(
                    self.ast.get_node(value).span,
                    None,
                    format!(
                        "cannot initialize array element of {} type with {} value",
                        self.type_category(target),
                        self.type_category(source)
                    ),
                    initializer_reference(self.options),
                )
                .into(),
            );
        } else {
            self.record_conversion(value, target);
        }
    }
}
