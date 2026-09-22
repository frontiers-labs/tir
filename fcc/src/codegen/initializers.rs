//! Lowers constant and runtime initializers.

use super::{
    ConstantData, DataRelocation, FnCodegen, Global, lower_type, node_entity, node_type,
    source_type_layout, unsupported,
};
use crate::ast::{Ast, AstKind, AstLeaf, InitializerDesignator, RecordKind};
use crate::diagnostics::Diagnostic;
use crate::sema::{EntityId, QualType, TypeKind, TypedAst, ValueCategory};
use std::collections::{BTreeMap, HashMap};
use tir::ValueId;
use tir::builtin::ops as b;
use tir::graph::{Dag, NodeId};
use tir::ptr::ops as p;
use tir::utils::APFloat;

pub(super) fn constant_initializer_data(
    typed: &TypedAst,
    globals: &HashMap<EntityId, Global>,
    global_strings: &BTreeMap<String, String>,
    target: QualType,
    initializer: NodeId,
) -> Option<ConstantData> {
    let ast = typed.ast();
    match typed.types().kind(target) {
        TypeKind::Integer(_) | TypeKind::Enum(_) => {
            let value = ast.get_annotation(initializer)?.constant?;
            let size = source_type_layout(typed, target).0 as usize;
            Some(ConstantData {
                bytes: value.to_le_bytes()[..size].to_vec(),
                relocations: Vec::new(),
            })
        }
        TypeKind::Float | TypeKind::Double => {
            let value = constant_floating_operand(typed, initializer, target)?;
            let bytes = match typed.types().kind(target) {
                TypeKind::Float => (value.to_bits() as u32).to_le_bytes().to_vec(),
                TypeKind::Double => (value.to_bits() as u64).to_le_bytes().to_vec(),
                _ => unreachable!(),
            };
            Some(ConstantData {
                bytes,
                relocations: Vec::new(),
            })
        }
        TypeKind::Pointer(_) => {
            let initializer = if ast.get_node(initializer).kind == AstKind::Cast {
                ast.children(initializer).next()?
            } else {
                initializer
            };
            let referent = match ast.get_node(initializer).kind {
                AstKind::AddressOf => ast.children(initializer).next()?,
                AstKind::String => initializer,
                AstKind::Var
                    if ast
                        .get_annotation(initializer)
                        .is_some_and(|info| info.category == ValueCategory::Function) =>
                {
                    initializer
                }
                _ => return None,
            };
            let symbol = if let Some(AstLeaf::String(value)) = ast.get_leaf_data(referent) {
                global_strings.get(value)?.clone()
            } else if ast
                .get_annotation(referent)
                .is_some_and(|info| info.category == ValueCategory::Function)
            {
                let AstLeaf::Var(name) = ast.get_leaf_data(referent)? else {
                    return None;
                };
                name.clone()
            } else {
                globals.get(&node_entity(typed, referent))?.name.clone()
            };
            let width = source_type_layout(typed, target).0;
            Some(ConstantData {
                bytes: vec![0; width as usize],
                relocations: vec![DataRelocation {
                    offset: 0,
                    symbol,
                    addend: 0,
                    width,
                }],
            })
        }
        TypeKind::Array(_, Some(_)) | TypeKind::Record(_)
            if ast.get_node(initializer).kind == AstKind::InitializerList =>
        {
            constant_aggregate_initializer_data(typed, globals, global_strings, target, initializer)
        }
        _ => None,
    }
}

fn constant_floating_value(typed: &TypedAst, initializer: NodeId) -> Option<APFloat> {
    let ast = typed.ast();
    let target = node_type(typed, initializer);
    let (exp_width, mant_width) = floating_format(typed, target)?;
    let convert = |value: APFloat| value.convert(exp_width, mant_width, false);
    match ast.get_node(initializer).kind {
        AstKind::FloatLiteral => {
            let AstLeaf::Float(value) = ast.get_leaf_data(initializer)? else {
                return None;
            };
            Some(convert(value.value.clone()))
        }
        AstKind::Neg => constant_floating_operand(typed, ast.children(initializer).next()?, target)
            .map(|value| value.neg()),
        AstKind::Pos => {
            let child = ast.children(initializer).next()?;
            constant_floating_operand(typed, child, target)
        }
        AstKind::Cast => {
            let child = ast.children(initializer).next()?;
            constant_floating_operand(typed, child, target)
        }
        kind @ (AstKind::Add | AstKind::Sub | AstKind::Mul | AstKind::Div) => {
            let mut children = ast.children(initializer);
            let left = constant_floating_operand(typed, children.next()?, target)?;
            let right = constant_floating_operand(typed, children.next()?, target)?;
            Some(match kind {
                AstKind::Add => left.add(&right),
                AstKind::Sub => left.sub(&right),
                AstKind::Mul => left.mul(&right),
                AstKind::Div => left.div(&right),
                _ => unreachable!(),
            })
        }
        _ => integer_as_float(typed, initializer, target),
    }
}

fn constant_floating_operand(typed: &TypedAst, node: NodeId, target: QualType) -> Option<APFloat> {
    let (exp_width, mant_width) = floating_format(typed, target)?;
    match typed.types().kind(node_type(typed, node)) {
        TypeKind::Float | TypeKind::Double => constant_floating_value(typed, node)
            .map(|value| value.convert(exp_width, mant_width, false)),
        TypeKind::Integer(_) | TypeKind::Enum(_) => integer_as_float(typed, node, target),
        _ => None,
    }
}

fn floating_format(typed: &TypedAst, ty: QualType) -> Option<(u32, u32)> {
    match typed.types().kind(ty) {
        TypeKind::Float => Some((8, 23)),
        TypeKind::Double => Some((11, 52)),
        _ => None,
    }
}

fn integer_as_float(typed: &TypedAst, node: NodeId, target: QualType) -> Option<APFloat> {
    let value = typed.ast().get_annotation(node)?.constant?;
    let (exp_width, mant_width) = floating_format(typed, target)?;
    Some(APFloat::from_significand(
        exp_width,
        mant_width,
        false,
        value.is_negative(),
        value.unsigned_abs() as u128,
        0,
    ))
}

fn constant_aggregate_initializer_data(
    typed: &TypedAst,
    globals: &HashMap<EntityId, Global>,
    global_strings: &BTreeMap<String, String>,
    target: QualType,
    initializer: NodeId,
) -> Option<ConstantData> {
    let mut data = ConstantData {
        bytes: vec![0; source_type_layout(typed, target).0 as usize],
        relocations: Vec::new(),
    };
    let entries = initializer_entries(typed.ast(), initializer)?;
    let entries = if matches!(
        typed.types().kind(target),
        TypeKind::Record(id) if typed.record(*id)?.kind == RecordKind::Union
    ) {
        active_union_entries(&entries)
    } else {
        &entries
    };
    for (path, value) in entries {
        let (selected_type, offset) = initializer_subobject(typed, target, path)?;
        let value =
            constant_initializer_data(typed, globals, global_strings, selected_type, *value)?;
        write_constant_data(&mut data, offset as usize, value);
    }
    Some(data)
}

fn write_constant_data(target: &mut ConstantData, offset: usize, mut value: ConstantData) {
    let end = offset + value.bytes.len();
    target.relocations.retain(|relocation| {
        let relocation_start = relocation.offset as usize;
        let relocation_end = relocation_start + relocation.width as usize;
        relocation_end <= offset || relocation_start >= end
    });
    target.bytes[offset..end].copy_from_slice(&value.bytes);
    for relocation in &mut value.relocations {
        relocation.offset += offset as u64;
    }
    target.relocations.extend(value.relocations);
}

fn initializer_entries(ast: &Ast, initializer: NodeId) -> Option<Vec<(Vec<usize>, NodeId)>> {
    ast.children(initializer)
        .map(|value| {
            let path = ast.get_annotation(value)?.initializer_path.clone()?;
            Some((path, designated_initializer_value(ast, value)))
        })
        .collect()
}

fn active_union_entries(entries: &[(Vec<usize>, NodeId)]) -> &[(Vec<usize>, NodeId)] {
    let Some(active_member) = entries.last().and_then(|(path, _)| path.first()) else {
        return entries;
    };
    let start = entries
        .iter()
        .rposition(|(path, _)| path.first() != Some(active_member))
        .map_or(0, |index| index + 1);
    &entries[start..]
}

fn designated_initializer_value(ast: &Ast, mut initializer: NodeId) -> NodeId {
    while ast.get_node(initializer).kind == AstKind::DesignatedInitializer {
        initializer = match ast.get_leaf_data(initializer).unwrap() {
            AstLeaf::DesignatedInitializer(InitializerDesignator::Field(_)) => {
                ast.children(initializer).next().unwrap()
            }
            AstLeaf::DesignatedInitializer(InitializerDesignator::Index) => {
                ast.children(initializer).nth(1).unwrap()
            }
            _ => unreachable!(),
        };
    }
    initializer
}

fn initializer_subobject(
    typed: &TypedAst,
    target: QualType,
    path: &[usize],
) -> Option<(QualType, u64)> {
    let mut selected = target;
    let mut offset = 0;
    for &index in path {
        match typed.types().kind(selected) {
            TypeKind::Record(id) => {
                let field = typed.record(*id)?.fields.get(index)?;
                selected = field.ty;
                offset += field.offset;
            }
            TypeKind::Array(element, Some(length)) if index < *length as usize => {
                let element_size = source_type_layout(typed, *element).0;
                selected = *element;
                offset += index as u64 * element_size;
            }
            _ => return None,
        }
    }
    Some((selected, offset))
}

impl FnCodegen<'_> {
    pub(super) fn lower_initializer(
        &mut self,
        target: QualType,
        address: ValueId,
        initializer: NodeId,
    ) -> Result<(), Diagnostic> {
        let aggregate = matches!(
            self.typed.types().kind(target),
            TypeKind::Record(_) | TypeKind::Array(_, Some(_))
        );
        if aggregate && self.ast.get_node(initializer).kind == AstKind::InitializerList {
            return self.lower_aggregate_initializer(target, address, initializer);
        }
        if self.ast.get_node(initializer).kind == AstKind::InitializerList {
            let value = self.ast.children(initializer).next().unwrap();
            return self.lower_initializer(target, address, value);
        }
        let value = self.lower_expr(initializer)?;
        self.store_scalar(value, address, lower_type(self.context, self.typed, target));
        Ok(())
    }

    fn lower_aggregate_initializer(
        &mut self,
        target: QualType,
        address: ValueId,
        initializer: NodeId,
    ) -> Result<(), Diagnostic> {
        let entries = initializer_entries(self.ast, initializer)
            .expect("semantic analysis resolves aggregate initializer paths");
        let (kind, members) = match self.typed.types().kind(target) {
            TypeKind::Record(id) => {
                let record = self.typed.record(*id).unwrap();
                (
                    Some(record.kind),
                    record
                        .fields
                        .iter()
                        .map(|field| (field.ty, field.offset))
                        .collect::<Vec<_>>(),
                )
            }
            TypeKind::Array(element, Some(length)) => {
                let element_size = source_type_layout(self.typed, *element).0;
                (
                    None,
                    (0..*length)
                        .map(|index| (*element, index * element_size))
                        .collect(),
                )
            }
            _ => unreachable!(),
        };
        if kind == Some(RecordKind::Union) {
            if let Some(&(storage_type, _)) = members
                .iter()
                .max_by_key(|(member, _)| source_type_layout(self.typed, *member).0)
            {
                self.zero_initialize(storage_type, address, initializer)?;
            }
            for (path, value) in active_union_entries(&entries) {
                self.lower_initializer_path(target, address, path, *value)?;
            }
            return Ok(());
        }
        for (index, (member, offset)) in members.into_iter().enumerate() {
            let member_entries = entries
                .iter()
                .filter(|(path, _)| path.first() == Some(&index))
                .collect::<Vec<_>>();
            let member_address = self.offset_address(address, offset);
            if member_entries.is_empty() {
                self.zero_initialize(member, member_address, initializer)?;
                continue;
            }
            if member_entries[0].0.len() > 1 {
                self.zero_initialize(member, member_address, initializer)?;
            }
            for (path, value) in member_entries {
                if path.len() == 1 {
                    self.lower_initializer(member, member_address, *value)?;
                } else {
                    self.lower_initializer_path(member, member_address, &path[1..], *value)?;
                }
            }
        }
        Ok(())
    }

    fn lower_initializer_path(
        &mut self,
        target: QualType,
        address: ValueId,
        path: &[usize],
        initializer: NodeId,
    ) -> Result<(), Diagnostic> {
        let (selected_type, offset) = initializer_subobject(self.typed, target, path).unwrap();
        let selected_address = self.offset_address(address, offset);
        self.lower_initializer(selected_type, selected_address, initializer)
    }

    fn zero_initialize(
        &mut self,
        target: QualType,
        address: ValueId,
        initializer: NodeId,
    ) -> Result<(), Diagnostic> {
        if let TypeKind::Record(id) = self.typed.types().kind(target) {
            let record = self.typed.record(*id).unwrap();
            let kind = record.kind;
            let mut fields = record
                .fields
                .iter()
                .map(|field| (field.ty, field.offset))
                .collect::<Vec<_>>();
            if kind == RecordKind::Union {
                fields = fields
                    .into_iter()
                    .max_by_key(|(field, _)| source_type_layout(self.typed, *field).0)
                    .into_iter()
                    .collect();
            }
            for (field, offset) in fields {
                let field_address = self.offset_address(address, offset);
                self.zero_initialize(field, field_address, initializer)?;
            }
            return Ok(());
        }
        if let TypeKind::Array(element, Some(length)) = self.typed.types().kind(target) {
            let (element, length) = (*element, *length);
            let element_size = source_type_layout(self.typed, element).0;
            for index in 0..length {
                let element_address = self.offset_address(address, index * element_size);
                self.zero_initialize(element, element_address, initializer)?;
            }
            return Ok(());
        }
        let ir_type = lower_type(self.context, self.typed, target);
        let value = match self.typed.types().kind(target) {
            TypeKind::Float | TypeKind::Double => self.float_zero(ir_type),
            TypeKind::Integer(_) | TypeKind::Enum(_) => self
                .emit(b::constant(self.context, 0, ir_type).build())
                .result(),
            _ => {
                return Err(unsupported(
                    self.ast,
                    initializer,
                    "zero initialization of aggregate array element".to_string(),
                ));
            }
        };
        self.emit(p::store(self.context, value, address).build());
        Ok(())
    }
}
