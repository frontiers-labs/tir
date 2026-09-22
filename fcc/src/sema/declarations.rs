//! File-scope declarations, records, enums, and function setup.

use super::references::{
    conflicting_declaration_reference, initializer_reference, object_type_reference,
    redefinition_reference,
};
use super::{
    Analyzer, EntityId, IntegerKind, NodeSemantics, QualType, RecordDefinition, RecordField,
    Symbol, TypeKind, ValueCategory, align_to,
};
use crate::ast::{AstKind, AstLeaf, CType, RecordKind};
use crate::diagnostics::{
    CompleteObjectTypeRequired, ConflictingDeclaration, IntegerConstantRequired, Redefinition,
};
use crate::lang_options::StdVersion;
use std::collections::HashMap;
use tir::graph::{Dag, MutDag, NodeId};

impl Analyzer<'_> {
    pub(super) fn new_entity(&mut self) -> EntityId {
        let entity = EntityId(self.next_entity);
        self.next_entity += 1;
        entity
    }

    pub(super) fn translation_unit(&mut self) {
        let Some(root) = self.ast.root() else {
            return;
        };
        self.scopes.push(HashMap::new());
        let items = self.ast.children(root).collect::<Vec<_>>();
        for item in items {
            match self.ast.get_node(item).kind {
                AstKind::RecordDecl => self.record_declaration(item),
                AstKind::EnumDecl => self.enum_declaration(item),
                AstKind::Function => {
                    self.declare_file_item(item);
                    self.function(item);
                }
                AstKind::Global => {
                    self.declare_file_item(item);
                    self.global_initializer(item);
                }
                AstKind::Prototype | AstKind::Typedef => {
                    self.declare_file_item(item);
                }
                AstKind::DeclGroup => {
                    let declarations = self.ast.children(item).collect::<Vec<_>>();
                    for declaration in declarations {
                        if self.ast.get_node(declaration).kind == AstKind::RecordDecl {
                            self.record_declaration(declaration);
                        } else if self.ast.get_node(declaration).kind == AstKind::EnumDecl {
                            self.enum_declaration(declaration);
                        } else {
                            self.declare_file_item(declaration);
                            if self.ast.get_node(declaration).kind == AstKind::Global {
                                self.global_initializer(declaration);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.scopes.pop();
    }

    pub(super) fn record_declaration(&mut self, node: NodeId) {
        let Some(AstLeaf::Record { id, kind, name }) = self.ast.get_leaf_data(node).cloned() else {
            return;
        };
        let name = name.unwrap_or_else(|| format!("__fcc_anon_struct.{}", id.number()));
        let index = if let Some(&index) = self.record_indices.get(&id) {
            index
        } else {
            let index = self.records.len();
            self.record_indices.insert(id, index);
            self.records.push(RecordDefinition {
                id,
                kind,
                name,
                fields: Vec::new(),
                size: 0,
                align: 1,
            });
            index
        };

        let children = self.ast.children(node).collect::<Vec<_>>();
        if children.is_empty() {
            return;
        }
        let mut fields = Vec::with_capacity(children.len());
        let mut field_spans = HashMap::new();
        let mut offset = 0;
        let mut record_align = 1;
        for field in children {
            if self.ast.get_node(field).kind == AstKind::RecordDecl {
                self.record_declaration(field);
                continue;
            }
            let Some(AstLeaf::Field { name, ty }) = self.ast.get_leaf_data(field).cloned() else {
                continue;
            };
            let span = self.ast.get_node(field).span;
            if let Some(previous) = field_spans.insert(name.clone(), span) {
                self.diagnostics.push(
                    Redefinition::new(span, previous, name, redefinition_reference(self.options))
                        .into(),
                );
                continue;
            }
            let ty = self.canonical_type(&ty);
            let (size, align) = self.type_layout(ty).unwrap_or((0, 1));
            let field_offset = match kind {
                RecordKind::Struct => align_to(offset, align),
                RecordKind::Union => 0,
            };
            fields.push(RecordField {
                name,
                ty,
                offset: field_offset,
            });
            offset = match kind {
                RecordKind::Struct => field_offset + size,
                RecordKind::Union => offset.max(size),
            };
            record_align = record_align.max(align);
        }
        self.records[index].size = align_to(offset, record_align);
        self.records[index].align = record_align;
        self.records[index].fields = fields;
    }

    pub(super) fn declare_file_item(&mut self, node: NodeId) {
        let leaf = self.ast.get_leaf_data(node).cloned();
        let (name, ty, typedef) = match leaf {
            Some(AstLeaf::Function {
                name,
                ret,
                has_parameter_type_list,
                ..
            }) => (
                name,
                self.function_type(node, ret, has_parameter_type_list),
                false,
            ),
            Some(AstLeaf::Global { name, ty, .. }) => (name, self.canonical_type(&ty), false),
            Some(AstLeaf::Typedef { name, ty }) => (name, self.canonical_type(&ty), true),
            _ => return,
        };
        let span = self.ast.get_node(node).span;
        if self.ast.get_node(node).kind == AstKind::Global
            && matches!(self.types.kind(ty), TypeKind::Void)
        {
            self.diagnostics.push(
                CompleteObjectTypeRequired::new(
                    span,
                    format!("object '{name}' cannot have void type"),
                    object_type_reference(self.options),
                )
                .into(),
            );
        }
        let defined = self.ast.get_node(node).kind == AstKind::Function
            || (self.ast.get_node(node).kind == AstKind::Global
                && self.ast.children(node).next().is_some());
        let previous = self.scopes[0].get(&name).cloned();
        let entity = previous
            .as_ref()
            .map(|symbol| symbol.entity)
            .unwrap_or_else(|| self.new_entity());
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(ty),
                entity: Some(entity),
                ..NodeSemantics::default()
            },
        );
        if let Some(previous) = previous {
            if !self.declaration_types_compatible(previous.ty, ty) || previous.typedef != typedef {
                self.diagnostics.push(
                    ConflictingDeclaration::new(
                        span,
                        previous.span,
                        name,
                        conflicting_declaration_reference(self.options),
                    )
                    .into(),
                );
            } else if defined && previous.defined {
                self.diagnostics.push(
                    Redefinition::new(
                        span,
                        previous.span,
                        name,
                        redefinition_reference(self.options),
                    )
                    .into(),
                );
            } else if defined {
                self.scopes[0].get_mut(&name).unwrap().defined = true;
            }
        } else {
            self.scopes[0].insert(
                name,
                Symbol {
                    span,
                    ty,
                    entity,
                    typedef,
                    defined,
                    constant: None,
                },
            );
        }
    }

    pub(super) fn declaration_types_compatible(&self, left: QualType, right: QualType) -> bool {
        if left == right {
            return true;
        }
        match (self.types.kind(left), self.types.kind(right)) {
            (
                TypeKind::Function {
                    ret: left_ret,
                    params: left_params,
                    varargs: left_varargs,
                    prototype: left_prototype,
                },
                TypeKind::Function {
                    ret: right_ret,
                    params: right_params,
                    varargs: right_varargs,
                    prototype: right_prototype,
                },
            ) => {
                left_ret == right_ret
                    && left_params.is_empty()
                    && right_params.is_empty()
                    && !left_varargs
                    && !right_varargs
                    && left_prototype != right_prototype
            }
            _ => false,
        }
    }

    pub(super) fn enum_declaration(&mut self, node: NodeId) {
        let int = self.types.intern(TypeKind::Integer(IntegerKind::Int));
        let mut previous = -1_i64;
        for enumerator in self.ast.children(node).collect::<Vec<_>>() {
            let Some(AstLeaf::Enumerator { name }) = self.ast.get_leaf_data(enumerator).cloned()
            else {
                continue;
            };
            let explicit = self.ast.children(enumerator).next();
            if let Some(expression) = explicit {
                self.node(expression);
            }
            let value = explicit
                .and_then(|expression| self.ast.get_annotation(expression))
                .and_then(|info| info.constant)
                .or_else(|| {
                    explicit
                        .is_none()
                        .then(|| previous.checked_add(1))
                        .flatten()
                });
            let Some(value) = value else {
                self.diagnostics.push(
                    IntegerConstantRequired::new(
                        self.ast.get_node(enumerator).span,
                        "enumerator value is not an integer constant expression",
                        initializer_reference(self.options),
                    )
                    .into(),
                );
                continue;
            };
            previous = value;
            let span = self.ast.get_node(enumerator).span;
            let entity = self.new_entity();
            self.ast.set_annotation(
                enumerator,
                NodeSemantics {
                    ty: Some(int),
                    entity: Some(entity),
                    category: ValueCategory::Value,
                    constant: Some(value),
                    ..NodeSemantics::default()
                },
            );
            let scope = self.scopes.last_mut().unwrap();
            if let Some(existing) = scope.get(&name) {
                self.diagnostics.push(
                    Redefinition::new(
                        span,
                        existing.span,
                        name,
                        redefinition_reference(self.options),
                    )
                    .into(),
                );
            } else {
                scope.insert(
                    name,
                    Symbol {
                        span,
                        ty: int,
                        entity,
                        typedef: false,
                        defined: true,
                        constant: Some(value),
                    },
                );
            }
        }
    }

    pub(super) fn function_type(
        &mut self,
        node: NodeId,
        ret: CType,
        has_parameter_type_list: bool,
    ) -> QualType {
        let ret = self.canonical_type(&ret);
        let children = self.ast.children(node).collect::<Vec<_>>();
        let mut params = Vec::new();
        let mut varargs = false;
        for child in children {
            match self.ast.get_leaf_data(child).cloned() {
                Some(AstLeaf::Param { ty, .. }) => {
                    let ty = self.canonical_parameter_type(&ty);
                    self.ast.set_annotation(
                        child,
                        NodeSemantics {
                            ty: Some(ty),
                            category: ValueCategory::Lvalue,
                            ..NodeSemantics::default()
                        },
                    );
                    params.push(ty);
                }
                _ if self.ast.get_node(child).kind == AstKind::VarArgs => varargs = true,
                _ => break,
            }
        }
        self.types.intern(TypeKind::Function {
            ret,
            params,
            varargs,
            prototype: has_parameter_type_list || self.options.std_version == StdVersion::C23,
        })
    }

    pub(super) fn global_initializer(&mut self, node: NodeId) {
        let Some(initializer) = self.ast.children(node).next() else {
            return;
        };
        self.node(initializer);
        let mut target = self
            .ast
            .get_annotation(node)
            .and_then(|info| info.ty)
            .unwrap();
        if self.ast.get_node(initializer).kind == AstKind::InitializerList
            && let TypeKind::Array(element, None) = self.types.kind(target)
            && let Some(length) = self.inferred_array_length(initializer)
        {
            target = self.types.intern(TypeKind::Array(*element, Some(length)));
            let mut semantics = self.ast.get_annotation(node).cloned().unwrap();
            semantics.ty = Some(target);
            self.ast.set_annotation(node, semantics);
            let Some(AstLeaf::Global { name, .. }) = self.ast.get_leaf_data(node) else {
                unreachable!();
            };
            self.scopes[0].get_mut(name).unwrap().ty = target;
        }
        self.validate_initializer(target, initializer);
    }
}
