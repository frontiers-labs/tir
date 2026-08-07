//! Lowers the C [`crate::ast`] to TIR using the `builtin` and `ptr` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any mem2reg pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops; C-only literals and variadic markers use the local `cir` dialect.

use std::collections::{BTreeMap, HashMap, HashSet};

use tir::attributes::AttributeValue;
use tir::backend::abi::{Overflow, ValueKind, type_kind};
use tir::builtin::{FloatType, IntegerType, ModuleOp, TokenType, TupleType, UnitType, ops as b};
use tir::graph::{Dag, NodeId};
use tir::ptr::{PtrType, ops as p};
use tir::{Context, IRBuilder, Operand, Operation, RegionId, TypeId, ValueId};

use crate::ast::*;
use crate::cir::{self, StructType, VarArgsType};
use crate::diagnostics::{Diagnostic, EmptyTranslationUnit, UnsupportedConstruct};
use crate::lexer::{decode_c_escapes, decode_character_constant};
use crate::sema::{EntityId, QualType, TargetProfile, TypeKind, TypedAst, ValueCategory};

/// A local variable: the pointer to its stack slot and the slot's element type.
#[derive(Clone, Copy)]
struct Slot {
    ptr: ValueId,
    elem: TypeId,
}

#[derive(Clone)]
struct Global {
    name: String,
    elem: TypeId,
}

struct ConstantData {
    bytes: Vec<u8>,
    relocations: Vec<DataRelocation>,
}

struct DataRelocation {
    offset: u64,
    symbol: String,
    addend: i64,
    width: u64,
}

#[derive(Clone, Copy)]
enum BreakScope {
    Loop(ValueId),
    Switch(Slot),
}

enum SwitchItem {
    Case(i64),
    Default,
    Statement(NodeId),
}

#[derive(Clone, Copy)]
enum LoweredExpr {
    Value(ValueId),
    Address { ptr: ValueId, elem: TypeId },
}

struct FnCodegen<'a> {
    context: &'a Context,
    typed: &'a TypedAst,
    ast: &'a Ast,
    builder: IRBuilder,
    region: RegionId,
    locals: HashMap<EntityId, Slot>,
    globals: &'a HashMap<EntityId, Global>,
    signatures: &'a HashMap<EntityId, Signature>,
    return_abi: &'a AbiReturn,
    indirect_return: Option<ValueId>,
    loop_scopes: Vec<ValueId>,
    break_scopes: Vec<BreakScope>,
    terminated: bool,
    /// Lowered values in the expression subtree currently being emitted. The AST
    /// is a DAG, so shared children reuse their first lowering.
    values: HashMap<NodeId, LoweredExpr>,
}

#[derive(Clone)]
struct Signature {
    ret: AbiReturn,
    params: Vec<AbiParameter>,
    varargs: bool,
}

#[derive(Clone)]
struct AbiParameter {
    pieces: Vec<AbiPiece>,
    grouped: bool,
    indirect: bool,
    alignment: u64,
}

#[derive(Clone, Copy)]
struct AbiPiece {
    offset: u64,
    ty: TypeId,
}

#[derive(Clone)]
struct AbiReturn {
    ty: TypeId,
    aggregate: Option<Vec<AbiPiece>>,
    indirect: bool,
}

#[derive(Default)]
struct AbiRegisterUsage {
    integers: usize,
    floats: usize,
}

impl AbiRegisterUsage {
    fn reserve_indirect_result(&mut self, target: TargetProfile) {
        let Some((kind, slot)) = target.indirect_result_argument_slots() else {
            return;
        };
        match kind {
            ValueKind::Int => self.integers = self.integers.max(slot),
            ValueKind::Float => self.floats = self.floats.max(slot),
            ValueKind::Vector => {}
        }
    }

    fn align_group(
        &mut self,
        context: &Context,
        target: TargetProfile,
        source_alignment: u64,
        pieces: &[AbiPiece],
    ) {
        for kind in [ValueKind::Int, ValueKind::Float] {
            if !pieces
                .iter()
                .any(|piece| type_kind(context, piece.ty) == kind)
            {
                continue;
            }
            let slot = match kind {
                ValueKind::Int => &mut self.integers,
                ValueKind::Float => &mut self.floats,
                ValueKind::Vector => unreachable!(),
            };
            *slot = target.align_argument_slot(kind, source_alignment, *slot);
        }
    }

    fn has_direct_registers(
        &self,
        context: &Context,
        target: TargetProfile,
        pieces: &[AbiPiece],
    ) -> bool {
        let mut integers = 0;
        let mut floats = 0;
        for piece in pieces {
            match type_kind(context, piece.ty) {
                ValueKind::Int => integers += 1,
                ValueKind::Float => floats += 1,
                ValueKind::Vector => return false,
            }
        }
        self.integers + integers <= target.argument_registers(ValueKind::Int)
            && self.floats + floats <= target.argument_registers(ValueKind::Float)
    }

    fn consume(&mut self, context: &Context, target: TargetProfile, pieces: &[AbiPiece]) {
        let integer_limit = target.argument_registers(ValueKind::Int);
        let float_limit = target.argument_registers(ValueKind::Float);
        for piece in pieces {
            match type_kind(context, piece.ty) {
                ValueKind::Float if self.floats < float_limit => {
                    self.floats += 1;
                }
                ValueKind::Float
                    if target.float_argument_overflow() == Overflow::Chain(ValueKind::Int)
                        && self.integers < integer_limit =>
                {
                    self.integers += 1;
                }
                ValueKind::Int if self.integers < integer_limit => {
                    self.integers += 1;
                }
                _ => {}
            }
        }
    }

    fn consume_group(&mut self, context: &Context, target: TargetProfile, pieces: &[AbiPiece]) {
        if self.has_direct_registers(context, target, pieces) {
            self.consume(context, target, pieces);
        } else {
            for piece in pieces {
                match type_kind(context, piece.ty) {
                    ValueKind::Int => {
                        self.integers = target.argument_registers(ValueKind::Int);
                    }
                    ValueKind::Float => {
                        self.floats = target.argument_registers(ValueKind::Float);
                    }
                    ValueKind::Vector => {}
                }
            }
        }
    }
}

impl Signature {
    fn argument_types(&self, context: &Context) -> Vec<TypeId> {
        let mut args = Vec::new();
        if self.ret.indirect {
            args.push(PtrType::opaque(context));
        }
        for parameter in &self.params {
            if parameter.grouped {
                args.push(TupleType::new(
                    context,
                    parameter.pieces.iter().map(|piece| piece.ty).collect(),
                ));
            } else {
                args.extend(parameter.pieces.iter().map(|piece| piece.ty));
            }
        }
        if self.varargs {
            args.push(VarArgsType::new(context));
        }
        args
    }

    fn argument_alignments(&self) -> Vec<u64> {
        let mut alignments = Vec::new();
        if self.ret.indirect {
            alignments.push(1);
        }
        for parameter in &self.params {
            if parameter.grouped {
                alignments.push(parameter.alignment);
            } else {
                alignments.extend(std::iter::repeat_n(1, parameter.pieces.len()));
            }
        }
        alignments
    }
}

/// Lower a translation unit into a `builtin.module` in `context`.
pub fn codegen(context: &Context, typed: &TypedAst) -> Result<ModuleOp, Diagnostic> {
    let ast = typed.ast();
    let module = b::module(context, None).build();
    let mut module_builder = IRBuilder::new(module.body());

    let root = ast.root().ok_or_else(EmptyTranslationUnit::new)?;
    let mut items = Vec::new();
    for item in ast.children(root) {
        if ast.get_node(item).kind == AstKind::DeclGroup {
            items.extend(ast.children(item));
        } else {
            items.push(item);
        }
    }
    let mut signatures = HashMap::new();
    let mut globals = HashMap::new();
    let mut global_strings = BTreeMap::new();
    let mut defined_functions = HashSet::new();
    let mut declared_functions = HashSet::new();
    // Entities already given storage: an initialized definition claims the
    // object outright, and repeated tentative definitions reserve it once.
    let mut reserved_globals = HashSet::new();
    for &item in &items {
        match ast.get_node(item).kind {
            AstKind::Prototype | AstKind::Function => {
                let (entity, sig) = lower_signature(context, typed, item)?;
                if ast.get_node(item).kind == AstKind::Function {
                    defined_functions.insert(entity);
                }
                signatures.insert(entity, sig);
            }
            AstKind::Global => {
                let AstLeaf::Global { name, .. } = ast.get_leaf_data(item).unwrap() else {
                    unreachable!("global node carries a global payload");
                };
                if ast.children(item).next().is_some() {
                    reserved_globals.insert(node_entity(typed, item));
                }
                globals.insert(
                    node_entity(typed, item),
                    Global {
                        name: name.clone(),
                        elem: lower_type(context, typed, node_type(typed, item)),
                    },
                );
                for node in ast.preorder(item) {
                    let Some(AstLeaf::String(value)) = ast.get_leaf_data(node) else {
                        continue;
                    };
                    let next = global_strings.len();
                    global_strings
                        .entry(value.clone())
                        .or_insert_with(|| format!(".L.global.str{next}"));
                }
            }
            AstKind::RecordDecl | AstKind::EnumDecl | AstKind::Typedef | AstKind::Attribute => {}
            _ => return Err(unsupported(ast, item, "top-level item".to_string())),
        }
    }

    for record in typed.records() {
        let fields = record
            .fields
            .iter()
            .map(|field| {
                AttributeValue::Dict(BTreeMap::from([
                    ("name".to_string(), AttributeValue::Str(field.name.clone())),
                    (
                        "type".to_string(),
                        AttributeValue::Type(lower_type(context, typed, field.ty)),
                    ),
                    ("offset".to_string(), AttributeValue::UInt(field.offset)),
                ]))
            })
            .collect();
        module_builder.insert(
            cir::DefineStructOpBuilder::new(context)
                .attr("sym_name", AttributeValue::Str(record.name.clone()))
                .attr("fields", AttributeValue::Array(fields))
                .attr("size", AttributeValue::UInt(record.size))
                .attr("align", AttributeValue::UInt(record.align))
                .build(),
        );
    }

    for (value, name) in &global_strings {
        module_builder.insert(
            cir::GlobalStringOpBuilder::new(context)
                .attr("sym_name", AttributeValue::Str(name.clone()))
                .attr("value", AttributeValue::Str(value.clone()))
                .build(),
        );
    }

    for item in items {
        match ast.get_node(item).kind {
            AstKind::Prototype => {
                let AstLeaf::Function { name, .. } = ast.get_leaf_data(item).unwrap() else {
                    unreachable!("prototype node carries a function payload");
                };
                let entity = node_entity(typed, item);
                // A symbol is named once however many times C declares it: the
                // definition already declares it, and a repeated prototype
                // re-declares the same entity rather than overloading it.
                if defined_functions.contains(&entity) || !declared_functions.insert(entity) {
                    continue;
                }
                let sig = signatures.get(&entity).unwrap();
                module_builder.insert(b::declare_op(
                    context,
                    name,
                    sig.ret.ty,
                    &sig.argument_types(context),
                ));
            }
            AstKind::Function => {
                let func_op = lower_function(context, typed, item, &signatures, &globals)?;
                module_builder.insert(func_op);
            }
            AstKind::Global => {
                let AstLeaf::Global { is_extern, .. } = ast.get_leaf_data(item).unwrap() else {
                    unreachable!("global node carries a global payload");
                };
                let source_ty = node_type(typed, item);
                let (size, align) = source_type_layout(typed, source_ty);
                let entity = node_entity(typed, item);
                let global = &globals[&entity];
                let Some(initializer) = ast.children(item).next() else {
                    // A tentative definition reserves storage only when no
                    // other declaration of the object defines it.
                    if !is_extern && reserved_globals.insert(entity) {
                        module_builder.insert(
                            cir::ZeroGlobalOpBuilder::new(context)
                                .attr("sym_name", AttributeValue::Str(global.name.clone()))
                                .attr("size", AttributeValue::UInt(size))
                                .attr("align", AttributeValue::UInt(align))
                                .build(),
                        );
                    }
                    continue;
                };
                let Some(data) = constant_initializer_data(
                    typed,
                    &globals,
                    &global_strings,
                    source_ty,
                    initializer,
                ) else {
                    return Err(unsupported(
                        ast,
                        initializer,
                        "non-constant global initializer".to_string(),
                    ));
                };
                module_builder.insert(
                    cir::GlobalOpBuilder::new(context)
                        .attr("sym_name", AttributeValue::Str(global.name.clone()))
                        .attr(
                            "bytes",
                            AttributeValue::Array(
                                data.bytes
                                    .into_iter()
                                    .map(|byte| AttributeValue::UInt(u64::from(byte)))
                                    .collect(),
                            ),
                        )
                        .attr(
                            "relocations",
                            AttributeValue::Array(
                                data.relocations
                                    .into_iter()
                                    .map(|relocation| {
                                        AttributeValue::Dict(BTreeMap::from([
                                            (
                                                "offset".to_string(),
                                                AttributeValue::UInt(relocation.offset),
                                            ),
                                            (
                                                "symbol".to_string(),
                                                AttributeValue::Str(relocation.symbol),
                                            ),
                                            (
                                                "addend".to_string(),
                                                AttributeValue::Int(relocation.addend),
                                            ),
                                            (
                                                "width".to_string(),
                                                AttributeValue::UInt(relocation.width),
                                            ),
                                        ]))
                                    })
                                    .collect(),
                            ),
                        )
                        .attr("align", AttributeValue::UInt(align))
                        .build(),
                );
            }
            AstKind::RecordDecl | AstKind::EnumDecl | AstKind::Typedef | AstKind::Attribute => {}
            _ => unreachable!("top-level item was checked before emission"),
        }
    }
    module_builder.insert(b::module_end(context).build());
    Ok(module)
}

/// A construct the parser accepts but codegen does not lower yet.
fn unsupported(ast: &Ast, node: NodeId, what: String) -> Diagnostic {
    UnsupportedConstruct::new(ast.get_node(node).span, what).into()
}

fn lower_type(context: &Context, typed: &TypedAst, ty: QualType) -> TypeId {
    match typed.types().kind(ty) {
        TypeKind::Void => UnitType::new(context),
        TypeKind::Integer(_) => IntegerType::new(context, typed.integer_width(ty).unwrap()),
        TypeKind::Pointer(pointee) => {
            if matches!(typed.types().kind(*pointee), TypeKind::Record(_)) {
                PtrType::opaque(context)
            } else {
                PtrType::typed(context, lower_type(context, typed, *pointee))
            }
        }
        TypeKind::Array(pointee, _) => {
            PtrType::typed(context, lower_type(context, typed, *pointee))
        }
        TypeKind::Enum(_) => IntegerType::new(context, 32),
        TypeKind::Double => FloatType::f64(context),
        TypeKind::Error | TypeKind::Float | TypeKind::LongDouble | TypeKind::Function { .. } => {
            IntegerType::new(context, 64)
        }
        TypeKind::Record(id) => StructType::new(context, &typed.record(*id).unwrap().name),
    }
}

fn source_type_layout(typed: &TypedAst, ty: QualType) -> (u64, u64) {
    match typed.types().kind(ty) {
        TypeKind::Array(element, Some(length)) => {
            let (size, align) = source_type_layout(typed, *element);
            (size * length, align)
        }
        TypeKind::Record(id) => {
            let record = typed.record(*id).unwrap();
            (record.size, record.align)
        }
        kind => typed.target().scalar_layout(kind).unwrap_or((1, 1)),
    }
}

fn constant_initializer_data(
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

fn node_type(typed: &TypedAst, node: NodeId) -> QualType {
    typed
        .ast()
        .get_annotation(node)
        .and_then(|info| info.ty)
        .expect("semantic analysis annotates codegen nodes")
}

fn converted_node_type(typed: &TypedAst, node: NodeId) -> QualType {
    let semantics = typed.ast().get_annotation(node).unwrap();
    semantics
        .conversions
        .last()
        .copied()
        .or(semantics.ty)
        .expect("semantic analysis annotates codegen nodes")
}

fn node_entity(typed: &TypedAst, node: NodeId) -> EntityId {
    typed
        .ast()
        .get_annotation(node)
        .and_then(|info| info.entity)
        .expect("semantic analysis resolves codegen names")
}

fn lower_signature(
    context: &Context,
    typed: &TypedAst,
    item: NodeId,
) -> Result<(EntityId, Signature), Diagnostic> {
    let ast = typed.ast();
    let AstLeaf::Function { .. } = ast.get_leaf_data(item).unwrap() else {
        unreachable!("function-like node carries a function payload");
    };
    Ok((
        node_entity(typed, item),
        classify_function_type(context, typed, node_type(typed, item)),
    ))
}

fn classify_function_type(context: &Context, typed: &TypedAst, ty: QualType) -> Signature {
    let TypeKind::Function {
        ret,
        params: source_params,
        varargs,
        ..
    } = typed.types().kind(ty)
    else {
        unreachable!("function signature has function semantic type")
    };
    let ret = classify_abi_return(context, typed, *ret);
    let mut register_usage = AbiRegisterUsage::default();
    if ret.indirect {
        register_usage.reserve_indirect_result(typed.target());
    }
    let params = source_params
        .iter()
        .map(|&param| classify_abi_parameter(context, typed, param, &mut register_usage))
        .collect();
    Signature {
        ret,
        params,
        varargs: *varargs,
    }
}

fn classify_abi_return(context: &Context, typed: &TypedAst, ty: QualType) -> AbiReturn {
    if let Some(pieces) = classify_sysv_eightbytes(context, typed, ty)
        .or_else(|| classify_riscv_fp_aggregate(context, typed, ty))
        .or_else(|| classify_aapcs64_hfa(context, typed, ty))
        .or_else(|| classify_aapcs64_composite(context, typed, ty))
        .or_else(|| {
            typed
                .target()
                .uses_riscv_abi()
                .then(|| classify_integer_carriers(context, typed, ty))
                .flatten()
        })
        .or_else(|| classify_integer_aggregate(context, typed, ty))
    {
        let ty = match pieces.as_slice() {
            [piece] => piece.ty,
            pieces => TupleType::new(context, pieces.iter().map(|piece| piece.ty).collect()),
        };
        return AbiReturn {
            ty,
            aggregate: Some(pieces),
            indirect: false,
        };
    }
    let record = matches!(typed.types().kind(ty), TypeKind::Record(_));
    if typed.target().uses_sysv_abi() && record {
        return AbiReturn {
            ty: PtrType::opaque(context),
            aggregate: None,
            indirect: true,
        };
    }
    if (typed.target().uses_aapcs64_abi() || typed.target().uses_riscv_abi())
        && record
        && source_type_layout(typed, ty).0 > 16
    {
        return AbiReturn {
            ty: UnitType::new(context),
            aggregate: None,
            indirect: true,
        };
    }
    AbiReturn {
        ty: lower_type(context, typed, ty),
        aggregate: None,
        indirect: false,
    }
}

fn classify_abi_parameter(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
    register_usage: &mut AbiRegisterUsage,
) -> AbiParameter {
    let sysv_pieces = classify_sysv_eightbytes(context, typed, ty);
    let riscv_pieces = classify_riscv_fp_aggregate(context, typed, ty);
    let hfa_pieces = classify_aapcs64_hfa(context, typed, ty);
    if sysv_pieces.is_none()
        && let Some(pieces) = classify_sysv_memory_carriers(context, typed, ty)
    {
        register_usage.consume_group(context, typed.target(), &pieces);
        return AbiParameter {
            pieces,
            grouped: true,
            indirect: false,
            alignment: 1,
        };
    }
    if riscv_pieces.is_none()
        && hfa_pieces.is_none()
        && (typed.target().uses_aapcs64_abi() || typed.target().uses_riscv_abi())
        && matches!(typed.types().kind(ty), TypeKind::Record(_))
        && source_type_layout(typed, ty).0 > 16
    {
        let pieces = vec![AbiPiece {
            offset: 0,
            ty: PtrType::opaque(context),
        }];
        register_usage.consume(context, typed.target(), &pieces);
        return AbiParameter {
            pieces,
            grouped: false,
            indirect: true,
            alignment: 1,
        };
    }
    let composite_pieces = hfa_pieces
        .is_none()
        .then(|| classify_aapcs64_composite(context, typed, ty))
        .flatten();
    let (pieces, grouped) = match sysv_pieces {
        Some(pieces) => {
            let grouped = pieces.len() > 1;
            (Some(pieces), grouped)
        }
        None => match riscv_pieces {
            Some(pieces)
                if register_usage.has_direct_registers(context, typed.target(), &pieces) =>
            {
                (Some(pieces), false)
            }
            Some(_) => (classify_integer_carriers(context, typed, ty), false),
            None => match hfa_pieces {
                Some(pieces) => {
                    let grouped = pieces.len() > 1;
                    (Some(pieces), grouped)
                }
                None => match composite_pieces {
                    Some(pieces) => {
                        let grouped = pieces.len() > 1;
                        (Some(pieces), grouped)
                    }
                    None if typed.target().uses_riscv_abi() => {
                        (classify_integer_carriers(context, typed, ty), false)
                    }
                    None => (classify_integer_aggregate(context, typed, ty), false),
                },
            },
        },
    };
    let pieces = pieces.unwrap_or_else(|| {
        vec![AbiPiece {
            offset: 0,
            ty: lower_type(context, typed, ty),
        }]
    });
    let source_alignment = source_type_layout(typed, ty).1;
    let alignment = if grouped
        && pieces.iter().any(|piece| {
            let kind = type_kind(context, piece.ty);
            typed
                .target()
                .align_argument_slot(kind, source_alignment, 1)
                != 1
        }) {
        source_alignment
    } else {
        1
    };
    if grouped {
        register_usage.align_group(context, typed.target(), alignment, &pieces);
        register_usage.consume_group(context, typed.target(), &pieces);
    } else {
        register_usage.consume(context, typed.target(), &pieces);
    }
    AbiParameter {
        pieces,
        grouped,
        indirect: false,
        alignment,
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SysvClass {
    #[default]
    None,
    Integer,
    Sse,
}

impl SysvClass {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (class, other) if class == other => class,
            (Self::None, class) | (class, Self::None) => class,
            (Self::Integer, _) | (_, Self::Integer) => Self::Integer,
            _ => Self::Sse,
        }
    }
}

fn classify_sysv_eightbytes(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_sysv_abi() || !matches!(typed.types().kind(ty), TypeKind::Record(_)) {
        return None;
    }
    let (size, _) = source_type_layout(typed, ty);
    if !(1..=16).contains(&size) {
        return None;
    }
    let mut classes = vec![SysvClass::None; size.div_ceil(8) as usize];
    classify_sysv_fields(typed, ty, 0, &mut classes)?;
    Some(
        classes
            .into_iter()
            .enumerate()
            .filter_map(|(index, class)| {
                let ty = match class {
                    SysvClass::None => return None,
                    SysvClass::Integer => IntegerType::new(context, 64),
                    SysvClass::Sse => FloatType::f64(context),
                };
                Some(AbiPiece {
                    offset: index as u64 * 8,
                    ty,
                })
            })
            .collect(),
    )
}

fn classify_sysv_memory_carriers(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_sysv_abi() || !matches!(typed.types().kind(ty), TypeKind::Record(_)) {
        return None;
    }
    let (size, _) = source_type_layout(typed, ty);
    if size == 0 {
        return None;
    }
    let carrier = IntegerType::new(context, 64);
    Some(
        (0..size.div_ceil(8))
            .map(|index| AbiPiece {
                offset: index * 8,
                ty: carrier,
            })
            .collect(),
    )
}

fn classify_sysv_fields(
    typed: &TypedAst,
    ty: QualType,
    offset: u64,
    classes: &mut [SysvClass],
) -> Option<()> {
    let scalar_class = match typed.types().kind(ty) {
        TypeKind::Integer(_) | TypeKind::Enum(_) | TypeKind::Pointer(_) => Some(SysvClass::Integer),
        TypeKind::Double => Some(SysvClass::Sse),
        _ => None,
    };
    if let Some(class) = scalar_class {
        let (size, align) = source_type_layout(typed, ty);
        if !offset.is_multiple_of(align) {
            return None;
        }
        let first = usize::try_from(offset / 8).ok()?;
        let last = usize::try_from((offset + size - 1) / 8).ok()?;
        for slot in classes.get_mut(first..=last)? {
            *slot = slot.merge(class);
        }
        return Some(());
    }

    match typed.types().kind(ty) {
        TypeKind::Record(id) => {
            let record = typed.record(*id)?;
            for field in &record.fields {
                let (_, align) = source_type_layout(typed, field.ty);
                let field_offset = offset + field.offset;
                if !field_offset.is_multiple_of(align) {
                    return None;
                }
                classify_sysv_fields(typed, field.ty, field_offset, classes)?;
            }
            Some(())
        }
        TypeKind::Array(element, Some(length)) => {
            let stride = source_type_layout(typed, *element).0;
            for index in 0..*length {
                classify_sysv_fields(typed, *element, offset + index * stride, classes)?;
            }
            Some(())
        }
        _ => None,
    }
}

fn classify_riscv_fp_aggregate(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_riscv_hard_float_abi() {
        return None;
    }
    let TypeKind::Record(id) = typed.types().kind(ty) else {
        return None;
    };
    let record = typed.record(*id)?;
    if record.kind != RecordKind::Struct {
        return None;
    }
    let mut pieces = vec![];
    if !flatten_aggregate_fields(context, typed, ty, 0, &mut pieces) {
        return None;
    }
    let kinds = pieces
        .iter()
        .map(|piece| type_kind(context, piece.ty))
        .collect::<Vec<_>>();
    use ValueKind::{Float, Int};
    if !matches!(
        kinds.as_slice(),
        [Float] | [Float, Float] | [Float, Int] | [Int, Float]
    ) {
        return None;
    }
    Some(pieces)
}

fn classify_aapcs64_hfa(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_aapcs64_abi() {
        return None;
    }
    let TypeKind::Record(id) = typed.types().kind(ty) else {
        return None;
    };
    if typed.record(*id)?.kind != RecordKind::Struct {
        return None;
    }
    let mut pieces = vec![];
    if !flatten_aggregate_fields(context, typed, ty, 0, &mut pieces)
        || !(1..=4).contains(&pieces.len())
        || pieces
            .iter()
            .any(|piece| type_kind(context, piece.ty) != ValueKind::Float)
        || source_type_layout(typed, ty).0 != pieces.len() as u64 * 8
    {
        return None;
    }
    Some(pieces)
}

fn classify_aapcs64_composite(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_aapcs64_abi()
        || !matches!(typed.types().kind(ty), TypeKind::Record(_))
        || !(1..=16).contains(&source_type_layout(typed, ty).0)
    {
        return None;
    }
    let size = source_type_layout(typed, ty).0;
    let carrier = IntegerType::new(context, 64);
    Some(
        (0..size.div_ceil(8))
            .map(|index| AbiPiece {
                offset: index * 8,
                ty: carrier,
            })
            .collect(),
    )
}

fn flatten_aggregate_fields(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
    offset: u64,
    pieces: &mut Vec<AbiPiece>,
) -> bool {
    match typed.types().kind(ty) {
        TypeKind::Double => {
            pieces.push(AbiPiece {
                offset,
                ty: FloatType::f64(context),
            });
            true
        }
        TypeKind::Integer(_) => {
            let Some(width) = typed.integer_width(ty) else {
                return false;
            };
            if width > typed.target().pointer_width() {
                return false;
            }
            pieces.push(AbiPiece {
                offset,
                ty: lower_type(context, typed, ty),
            });
            true
        }
        TypeKind::Enum(_) => {
            pieces.push(AbiPiece {
                offset,
                ty: IntegerType::new(context, 32),
            });
            true
        }
        TypeKind::Record(id) => {
            let Some(record) = typed.record(*id) else {
                return false;
            };
            record.kind == RecordKind::Struct
                && record.fields.iter().all(|field| {
                    flatten_aggregate_fields(
                        context,
                        typed,
                        field.ty,
                        offset + field.offset,
                        pieces,
                    )
                })
        }
        TypeKind::Array(element, Some(length)) => {
            let stride = source_type_layout(typed, *element).0;
            (0..*length).all(|index| {
                flatten_aggregate_fields(context, typed, *element, offset + index * stride, pieces)
            })
        }
        _ => false,
    }
}

fn classify_integer_aggregate(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !matches!(typed.types().kind(ty), TypeKind::Record(_)) || !is_integer_aggregate(typed, ty) {
        return None;
    }
    classify_integer_carriers(context, typed, ty)
}

fn classify_integer_carriers(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !matches!(typed.types().kind(ty), TypeKind::Record(_)) {
        return None;
    }
    let (size, _) = source_type_layout(typed, ty);
    let scalar_width = u64::from(typed.target().pointer_width() / 8);
    if size <= scalar_width && size.is_power_of_two() {
        return Some(vec![AbiPiece {
            offset: 0,
            ty: IntegerType::new(context, (size * 8) as u32),
        }]);
    }
    if size == scalar_width * 2 {
        let carrier = IntegerType::new(context, typed.target().pointer_width());
        return Some(vec![
            AbiPiece {
                offset: 0,
                ty: carrier,
            },
            AbiPiece {
                offset: scalar_width,
                ty: carrier,
            },
        ]);
    }
    None
}

fn abi_piece_size(context: &Context, ty: TypeId) -> Option<u64> {
    let ty = context.get_type_data(ty);
    let ty = ty.as_ref() as &dyn std::any::Any;
    if let Some(integer) = ty.downcast_ref::<IntegerType>() {
        return Some(u64::from(integer.width().div_ceil(8)));
    }
    if let Some(float) = ty.downcast_ref::<FloatType>() {
        return Some(u64::from(float.bit_width() / 8));
    }
    None
}

fn abi_storage_layout(context: &Context, pieces: &[AbiPiece]) -> Option<(u64, u64)> {
    pieces.iter().try_fold((0, 1), |(size, align), piece| {
        let piece_size = abi_piece_size(context, piece.ty)?;
        Some((size.max(piece.offset + piece_size), align.max(piece_size)))
    })
}

fn is_integer_aggregate(typed: &TypedAst, ty: QualType) -> bool {
    match typed.types().kind(ty) {
        TypeKind::Integer(_) | TypeKind::Enum(_) | TypeKind::Pointer(_) => true,
        TypeKind::Array(element, _) => is_integer_aggregate(typed, *element),
        TypeKind::Record(id) => typed.record(*id).is_some_and(|record| {
            record
                .fields
                .iter()
                .all(|field| is_integer_aggregate(typed, field.ty))
        }),
        _ => false,
    }
}

fn lower_function(
    context: &Context,
    typed: &TypedAst,
    func: NodeId,
    signatures: &HashMap<EntityId, Signature>,
    globals: &HashMap<EntityId, Global>,
) -> Result<impl Operation, Diagnostic> {
    let ast = typed.ast();
    let AstLeaf::Function { name, .. } = ast.get_leaf_data(func).unwrap() else {
        unreachable!("function node carries a function payload");
    };
    let signature = &signatures[&node_entity(typed, func)];

    // Entry block arguments carry the incoming parameter values; parameters are
    // the function node's leading children.
    let mut param_values = Vec::new();
    if signature.ret.indirect {
        param_values.push(context.create_value(PtrType::opaque(context), None));
    }
    for parameter in &signature.params {
        if parameter.grouped {
            let ty = TupleType::new(
                context,
                parameter.pieces.iter().map(|piece| piece.ty).collect(),
            );
            param_values.push(context.create_value(ty, None));
        } else {
            param_values.extend(
                parameter
                    .pieces
                    .iter()
                    .map(|piece| context.create_value(piece.ty, None)),
            );
        }
    }
    let param_ids: Vec<ValueId> = param_values.iter().map(|v| v.id()).collect();

    let region = context.create_region();
    let block = context.create_block(param_values);
    region.add_block(block.id());

    let mut func_builder = b::func(context, name.as_str(), signature.ret.ty, Some(region.id()));
    if signature.ret.indirect {
        func_builder = func_builder.result_address();
    }
    let argument_alignments = signature.argument_alignments();
    if argument_alignments.iter().any(|&alignment| alignment > 1) {
        func_builder = func_builder.argument_alignments(&argument_alignments);
    }
    let func_op = func_builder.build();
    let indirect_return = signature.ret.indirect.then(|| param_ids[0]);
    let parameter_start = usize::from(signature.ret.indirect);

    let mut cg = FnCodegen {
        context,
        typed,
        ast,
        builder: IRBuilder::new(func_op.body()),
        region: region.id(),
        locals: HashMap::new(),
        globals,
        signatures,
        return_abi: &signature.ret,
        indirect_return,
        loop_scopes: Vec::new(),
        break_scopes: Vec::new(),
        terminated: false,
        values: HashMap::new(),
    };
    cg.lower_body(func, &param_ids[parameter_start..], &signature.params)?;

    Ok(func_op)
}

impl FnCodegen<'_> {
    fn alloca(&mut self, elem: TypeId, size: u64, align: u64) -> Slot {
        let ptr_ty = PtrType::typed(self.context, elem);
        let op = self
            .builder
            .insert(p::alloca(self.context, size, align, ptr_ty).build());
        Slot {
            ptr: op.result(),
            elem,
        }
    }

    fn apply_conversions(&mut self, node: NodeId, mut expression: LoweredExpr) -> LoweredExpr {
        let semantics = self.ast.get_annotation(node).unwrap();
        let mut source = semantics.ty.unwrap();
        for &target in &semantics.conversions {
            expression = if self.typed.integer_width(source).is_some()
                && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
                && semantics.constant == Some(0)
            {
                let target = lower_type(self.context, self.typed, target);
                LoweredExpr::Value(
                    self.builder
                        .insert(b::constant(self.context, 0, target).build())
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

    fn convert_scalar(&mut self, value: ValueId, source: QualType, target: QualType) -> ValueId {
        if self.typed.integer_width(source).is_some()
            && matches!(self.typed.types().kind(target), TypeKind::Double)
        {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(source) == Some(true) {
                self.builder
                    .insert(b::sitofp(self.context, value, target_ty).build())
                    .result()
            } else {
                self.builder
                    .insert(b::uitofp(self.context, value, target_ty).build())
                    .result()
            };
        }
        if matches!(self.typed.types().kind(source), TypeKind::Double)
            && let Some(target_width) = self.typed.integer_width(target)
        {
            let target_ty = lower_type(self.context, self.typed, target);
            return if self.typed.integer_is_signed(target) == Some(true) {
                self.builder
                    .insert(b::fptosi(self.context, value, target_ty).build())
                    .result()
            } else if target_width < 64 {
                // Every value an unsigned type narrower than 64 bits can hold is
                // also representable as a signed 64-bit integer, so the truncated
                // signed conversion is exact and avoids a narrow `fptoui`.
                let wide = IntegerType::new(self.context, 64);
                let converted = self
                    .builder
                    .insert(b::fptosi(self.context, value, wide).build())
                    .result();
                self.builder
                    .insert(b::trunci(self.context, converted, target_ty).build())
                    .result()
            } else {
                self.builder
                    .insert(b::fptoui(self.context, value, target_ty).build())
                    .result()
            };
        }
        if let Some(source_width) = self.typed.integer_width(source)
            && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
        {
            let target_width = self.typed.target().pointer_width();
            let target_ty = IntegerType::new(self.context, target_width);
            if source_width < target_width {
                return if self.typed.integer_is_signed(source).unwrap() {
                    self.builder
                        .insert(b::extsi(self.context, value, target_ty).build())
                        .result()
                } else {
                    self.builder
                        .insert(b::extui(self.context, value, target_ty).build())
                        .result()
                };
            }
            if source_width > target_width {
                return self
                    .builder
                    .insert(b::trunci(self.context, value, target_ty).build())
                    .result();
            }
            return value;
        }
        let (Some(source_width), Some(target_width)) = (
            self.typed.integer_width(source),
            self.typed.integer_width(target),
        ) else {
            return value;
        };
        let target_ty = lower_type(self.context, self.typed, target);
        if source_width < target_width {
            if self.typed.integer_is_signed(source).unwrap() {
                self.builder
                    .insert(b::extsi(self.context, value, target_ty).build())
                    .result()
            } else {
                self.builder
                    .insert(b::extui(self.context, value, target_ty).build())
                    .result()
            }
        } else if source_width > target_width {
            self.builder
                .insert(b::trunci(self.context, value, target_ty).build())
                .result()
        } else {
            value
        }
    }

    fn lower_integer_binary(
        &mut self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
        source_ty: QualType,
    ) -> ValueId {
        let ty = lower_type(self.context, self.typed, source_ty);
        match kind {
            AstKind::Add | AstKind::AddAssign => self
                .builder
                .insert(b::addi(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Sub | AstKind::SubAssign => self
                .builder
                .insert(b::subi(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Mul | AstKind::MulAssign => self
                .builder
                .insert(b::muli(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Div | AstKind::DivAssign
                if self.typed.integer_is_signed(source_ty).unwrap() =>
            {
                self.builder
                    .insert(b::divsi(self.context, lhs, rhs, ty).build())
                    .result()
            }
            AstKind::Div | AstKind::DivAssign => self
                .builder
                .insert(b::divui(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Mod | AstKind::ModAssign
                if self.typed.integer_is_signed(source_ty).unwrap() =>
            {
                self.builder
                    .insert(b::remsi(self.context, lhs, rhs, ty).build())
                    .result()
            }
            AstKind::Mod | AstKind::ModAssign => self
                .builder
                .insert(b::remui(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::BitAnd | AstKind::AndAssign => self
                .builder
                .insert(b::andi(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::BitXor | AstKind::XorAssign => self
                .builder
                .insert(b::xori(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::BitOr | AstKind::OrAssign => self
                .builder
                .insert(b::ori(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Shl | AstKind::ShlAssign => self
                .builder
                .insert(b::shli(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Shr | AstKind::ShrAssign
                if self.typed.integer_is_signed(source_ty).unwrap() =>
            {
                self.builder
                    .insert(b::shrsi(self.context, lhs, rhs, ty).build())
                    .result()
            }
            AstKind::Shr | AstKind::ShrAssign => self
                .builder
                .insert(b::shrui(self.context, lhs, rhs, ty).build())
                .result(),
            _ => unreachable!(),
        }
    }

    fn lower_integer_compare(
        &mut self,
        kind: AstKind,
        lhs: ValueId,
        rhs: ValueId,
        source_ty: QualType,
    ) -> ValueId {
        let signed = self.typed.integer_is_signed(source_ty).unwrap_or(true);
        let predicate = match (kind, signed) {
            (AstKind::Lt, true) => "slt",
            (AstKind::Lt, false) => "ult",
            (AstKind::Gt, true) => "sgt",
            (AstKind::Gt, false) => "ugt",
            (AstKind::Le, true) => "sle",
            (AstKind::Le, false) => "ule",
            (AstKind::Ge, true) => "sge",
            (AstKind::Ge, false) => "uge",
            (AstKind::Eq, _) => "eq",
            (AstKind::Ne, _) => "ne",
            _ => unreachable!(),
        };
        self.builder
            .insert(
                b::CmpIOpBuilder::new(self.context)
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
    fn lower_double_compare(&mut self, kind: AstKind, lhs: ValueId, rhs: ValueId) -> ValueId {
        let predicate = match kind {
            AstKind::Lt => "olt",
            AstKind::Gt => "ogt",
            AstKind::Le => "ole",
            AstKind::Ge => "oge",
            AstKind::Eq => "oeq",
            AstKind::Ne => "une",
            _ => unreachable!(),
        };
        self.builder
            .insert(
                b::CmpFOpBuilder::new(self.context)
                    .lhs(lhs)
                    .rhs(rhs)
                    .predicate(predicate)
                    .result_type(IntegerType::new(self.context, 1))
                    .build(),
            )
            .result()
    }

    fn lower_double_binary(&mut self, kind: AstKind, lhs: ValueId, rhs: ValueId) -> ValueId {
        let ty = FloatType::f64(self.context);
        match kind {
            AstKind::Add | AstKind::AddAssign => self
                .builder
                .insert(b::addf(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Sub | AstKind::SubAssign => self
                .builder
                .insert(b::subf(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Mul | AstKind::MulAssign => self
                .builder
                .insert(b::mulf(self.context, lhs, rhs, ty).build())
                .result(),
            AstKind::Div | AstKind::DivAssign => self
                .builder
                .insert(b::divf(self.context, lhs, rhs, ty).build())
                .result(),
            _ => unreachable!(),
        }
    }

    fn lower_pointer_offset(
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
                self.builder
                    .insert(b::extsi(self.context, index, offset_ty).build())
                    .result()
            } else {
                self.builder
                    .insert(b::extui(self.context, index, offset_ty).build())
                    .result()
            }
        } else if index_width > pointer_width {
            self.builder
                .insert(b::trunci(self.context, index, offset_ty).build())
                .result()
        } else {
            index
        };
        let size = source_type_layout(self.typed, *pointee).0;
        let scale = self
            .builder
            .insert(b::constant(self.context, size as i64, offset_ty).build())
            .result();
        let offset = self
            .builder
            .insert(b::muli(self.context, index, scale, offset_ty).build())
            .result();
        let offset = if subtract {
            let zero = self
                .builder
                .insert(b::constant(self.context, 0, offset_ty).build())
                .result();
            self.builder
                .insert(b::subi(self.context, zero, offset, offset_ty).build())
                .result()
        } else {
            offset
        };
        self.builder
            .insert(
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

    fn lower_pointer_difference(
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
            .builder
            .insert(p::ptrdiff(self.context, lhs, rhs, result_ty).build())
            .result();
        let size = source_type_layout(self.typed, *pointee).0;
        if size == 1 {
            return bytes;
        }
        let divisor = self
            .builder
            .insert(b::constant(self.context, size as i64, result_ty).build())
            .result();
        self.builder
            .insert(b::divsi(self.context, bytes, divisor, result_ty).build())
            .result()
    }

    fn offset_address(&mut self, base: ValueId, offset: u64, element: QualType) -> ValueId {
        let element = lower_type(self.context, self.typed, element);
        self.offset_address_ir(base, offset, element)
    }

    fn offset_address_ir(&mut self, base: ValueId, offset: u64, element: TypeId) -> ValueId {
        if offset == 0 {
            return base;
        }
        let offset_ty = IntegerType::new(self.context, self.typed.target().pointer_width());
        let offset = self
            .builder
            .insert(b::constant(self.context, offset as i64, offset_ty).build())
            .result();
        self.builder
            .insert(
                p::ptradd(
                    self.context,
                    base,
                    offset,
                    PtrType::typed(self.context, element),
                )
                .build(),
            )
            .result()
    }

    fn lower_initializer(
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
        self.builder
            .insert(p::store(self.context, value, address).build());
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
            let member_address = self.offset_address(address, offset, member);
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
        let selected_address = self.offset_address(address, offset, selected_type);
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
                let field_address = self.offset_address(address, offset, field);
                self.zero_initialize(field, field_address, initializer)?;
            }
            return Ok(());
        }
        if let TypeKind::Array(element, Some(length)) = self.typed.types().kind(target) {
            let (element, length) = (*element, *length);
            let element_size = source_type_layout(self.typed, element).0;
            for index in 0..length {
                let element_address = self.offset_address(address, index * element_size, element);
                self.zero_initialize(element, element_address, initializer)?;
            }
            return Ok(());
        }
        let ir_type = lower_type(self.context, self.typed, target);
        let value = match self.typed.types().kind(target) {
            TypeKind::Double => self
                .builder
                .insert(b::constantf(self.context, 0.0, ir_type).build())
                .result(),
            TypeKind::Integer(_) | TypeKind::Enum(_) => self
                .builder
                .insert(b::constant(self.context, 0, ir_type).build())
                .result(),
            _ => {
                return Err(unsupported(
                    self.ast,
                    initializer,
                    "zero initialization of aggregate array element".to_string(),
                ));
            }
        };
        self.builder
            .insert(p::store(self.context, value, address).build());
        Ok(())
    }

    /// Lower a function: spill parameters into stack slots, then lower each body
    /// statement in source order (statement order is a side-effect ordering, so it
    /// stays top-down; only the expressions within use the post-order iterator).
    fn lower_body(
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
                        self.builder
                            .insert(
                                b::TupleGetOpBuilder::new(self.context)
                                    .tuple(tuple)
                                    .attr("index", AttributeValue::UInt(index as u64))
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
                let address = self.offset_address_ir(slot.ptr, piece.offset, piece.ty);
                self.builder
                    .insert(p::store(self.context, value, address).build());
            }
            self.locals.insert(node_entity(self.typed, param), slot);
        }

        for stmt in ast.children(func).skip(params.len()) {
            self.lower_stmt(stmt)?;
        }
        let returns_void = match self.typed.types().kind(node_type(self.typed, func)) {
            TypeKind::Function { ret, .. } => {
                matches!(self.typed.types().kind(*ret), TypeKind::Void)
            }
            _ => false,
        };
        if returns_void && !self.terminated {
            self.builder
                .insert(b::r#return(self.context, Operand::none()).build());
            self.terminated = true;
        }

        Ok(())
    }

    fn lower_stmt(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let ast = self.ast;
        if self.terminated {
            if !self.contains_label(stmt) {
                return Ok(());
            }
            let block = self.context.create_block(vec![]);
            self.context.get_region(self.region).add_block(block.id());
            self.builder = IRBuilder::new(block);
            self.terminated = false;
        }
        match ast.get_node(stmt).kind {
            AstKind::DeclGroup => {
                let declarations = ast.children(stmt).collect::<Vec<_>>();
                for declaration in declarations {
                    self.lower_stmt(declaration)?;
                }
                Ok(())
            }
            AstKind::EnumDecl | AstKind::Empty => Ok(()),
            AstKind::Decl => {
                let AstLeaf::Decl { .. } = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("decl node carries a decl payload");
                };
                let source_ty = node_type(self.typed, stmt);
                let array = match self.typed.types().kind(source_ty) {
                    TypeKind::Array(element, Some(length)) => Some((*element, *length)),
                    _ => None,
                };
                let elem = match array {
                    Some((element, _)) => lower_type(self.context, self.typed, element),
                    _ => lower_type(self.context, self.typed, source_ty),
                };
                let (size, align) = source_type_layout(self.typed, source_ty);
                let slot = self.alloca(elem, size, align);
                if let Some(init) = ast.children(stmt).next() {
                    if ast.get_node(init).kind == AstKind::InitializerList {
                        self.lower_initializer(source_ty, slot.ptr, init)?;
                    } else if let TypeKind::Record(id) = self.typed.types().kind(source_ty) {
                        let record = self.typed.record(*id).unwrap().name.clone();
                        self.lower_record_copy(init, slot.ptr, record.as_str())?;
                    } else {
                        let value = self.lower_expr(init)?;
                        self.builder
                            .insert(p::store(self.context, value, slot.ptr).build());
                    }
                }
                self.locals.insert(node_entity(self.typed, stmt), slot);
                Ok(())
            }
            AstKind::Assign => {
                let AstLeaf::Assign(_) = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("assign node carries an assign payload");
                };
                let slot = self.locals[&node_entity(self.typed, stmt)];
                let value = ast.children(stmt).next().unwrap();
                if let TypeKind::Record(id) = self.typed.types().kind(node_type(self.typed, stmt)) {
                    let record = self.typed.record(*id).unwrap().name.clone();
                    self.lower_record_copy(value, slot.ptr, record.as_str())?;
                } else {
                    let v = self.lower_expr(value)?;
                    self.builder
                        .insert(p::store(self.context, v, slot.ptr).build());
                }
                Ok(())
            }
            AstKind::Return => {
                let operand = match ast.children(stmt).next() {
                    Some(e) if self.return_abi.indirect => {
                        self.lower_indirect_return(e)?;
                        if self.return_abi.ty == UnitType::new(self.context) {
                            Operand::none()
                        } else {
                            Operand::from(self.indirect_return.unwrap())
                        }
                    }
                    Some(e) => Operand::from(self.lower_return_value(e)?),
                    None => Operand::none(),
                };
                self.builder
                    .insert(b::r#return(self.context, operand).build());
                self.terminated = true;
                Ok(())
            }
            AstKind::ExprStmt => {
                if let Some(expr) = ast.children(stmt).next() {
                    self.lower_expr(expr)?;
                }
                Ok(())
            }
            AstKind::Block => {
                for child in ast.children(stmt) {
                    self.lower_stmt(child)?;
                }
                Ok(())
            }
            AstKind::While => {
                let mut children = ast.children(stmt);
                let condition = children.next().unwrap();
                let body = children.next().unwrap();
                let scope = self
                    .context
                    .create_value(TokenType::new(self.context), None);

                let condition_region = self.context.create_region();
                let condition_block = self.context.create_block(vec![]);
                condition_region.add_block(condition_block.id());
                self.in_block(condition_block, |cg| {
                    let value = cg.lower_condition(condition)?;
                    cg.builder
                        .insert(cir::ops::condition(cg.context, value).build());
                    Ok(())
                })?;

                let body_region = self.context.create_region();
                let body_block = self.context.create_block(vec![scope.clone()]);
                body_region.add_block(body_block.id());
                self.loop_scopes.push(scope.id());
                self.break_scopes.push(BreakScope::Loop(scope.id()));
                self.in_block(body_block.clone(), |cg| {
                    cg.lower_stmt(body)?;
                    cg.ensure_cir_yield(body_block);
                    Ok(())
                })?;
                self.break_scopes.pop();
                self.loop_scopes.pop();

                self.builder.insert(
                    cir::ops::r#while(
                        self.context,
                        Some(condition_region.id()),
                        Some(body_region.id()),
                    )
                    .build(),
                );
                Ok(())
            }
            AstKind::DoWhile => {
                let mut children = ast.children(stmt);
                let body = children.next().unwrap();
                let condition = children.next().unwrap();

                let scope = self
                    .context
                    .create_value(TokenType::new(self.context), None);
                let body_region = self.context.create_region();
                let body_block = self.context.create_block(vec![scope.clone()]);
                body_region.add_block(body_block.id());
                self.loop_scopes.push(scope.id());
                self.break_scopes.push(BreakScope::Loop(scope.id()));
                self.in_block(body_block.clone(), |cg| {
                    cg.lower_stmt(body)?;
                    cg.ensure_cir_yield(body_block);
                    Ok(())
                })?;
                self.break_scopes.pop();
                self.loop_scopes.pop();

                let condition_region = self.context.create_region();
                let condition_block = self.context.create_block(vec![]);
                condition_region.add_block(condition_block.id());
                self.in_block(condition_block, |cg| {
                    let value = cg.lower_condition(condition)?;
                    cg.builder
                        .insert(cir::ops::condition(cg.context, value).build());
                    Ok(())
                })?;

                self.builder.insert(
                    cir::ops::r#do(
                        self.context,
                        Some(body_region.id()),
                        Some(condition_region.id()),
                    )
                    .build(),
                );
                Ok(())
            }
            AstKind::For => {
                let children = ast.children(stmt).collect::<Vec<_>>();
                let [init, condition, step, body] = children.as_slice() else {
                    unreachable!("for statement has four children");
                };
                if ast.get_node(*init).kind != AstKind::Empty {
                    self.lower_stmt(*init)?;
                }
                let scope = self
                    .context
                    .create_value(TokenType::new(self.context), None);

                let condition_region = self.context.create_region();
                let condition_block = self.context.create_block(vec![]);
                condition_region.add_block(condition_block.id());
                self.in_block(condition_block, |cg| {
                    let value = if ast.get_node(*condition).kind == AstKind::Empty {
                        cg.builder
                            .insert(
                                b::constant(cg.context, 1, IntegerType::new(cg.context, 1)).build(),
                            )
                            .result()
                    } else {
                        cg.lower_condition(*condition)?
                    };
                    cg.builder
                        .insert(cir::ops::condition(cg.context, value).build());
                    Ok(())
                })?;

                let body_region = self.context.create_region();
                let body_block = self.context.create_block(vec![scope.clone()]);
                body_region.add_block(body_block.id());
                self.loop_scopes.push(scope.id());
                self.break_scopes.push(BreakScope::Loop(scope.id()));
                self.in_block(body_block.clone(), |cg| {
                    cg.lower_stmt(*body)?;
                    cg.ensure_cir_yield(body_block);
                    Ok(())
                })?;
                self.break_scopes.pop();
                self.loop_scopes.pop();

                let step_region = self.context.create_region();
                let step_block = self.context.create_block(vec![]);
                step_region.add_block(step_block.id());
                self.in_block(step_block.clone(), |cg| {
                    match ast.get_node(*step).kind {
                        AstKind::Empty => {}
                        AstKind::Assign => cg.lower_stmt(*step)?,
                        _ => {
                            cg.lower_expr(*step)?;
                        }
                    }
                    cg.ensure_cir_yield(step_block);
                    Ok(())
                })?;

                self.builder.insert(
                    cir::ops::r#for(
                        self.context,
                        Some(condition_region.id()),
                        Some(body_region.id()),
                        Some(step_region.id()),
                    )
                    .build(),
                );
                Ok(())
            }
            AstKind::Switch => self.lower_switch(stmt),
            AstKind::If => {
                let mut children = ast.children(stmt);
                let condition = children.next().unwrap();
                let then_stmt = children.next().unwrap();
                let else_stmt = children.next();
                let condition = self.lower_condition(condition)?;

                let then_region = self.context.create_region();
                let then_block = self.context.create_block(vec![]);
                then_region.add_block(then_block.id());
                self.in_block(then_block.clone(), |cg| {
                    cg.lower_stmt(then_stmt)?;
                    cg.ensure_cir_yield(then_block);
                    Ok(())
                })?;

                let else_region = self.context.create_region();
                let else_block = self.context.create_block(vec![]);
                else_region.add_block(else_block.id());
                self.in_block(else_block.clone(), |cg| {
                    if let Some(else_stmt) = else_stmt {
                        cg.lower_stmt(else_stmt)?;
                    }
                    cg.ensure_cir_yield(else_block);
                    Ok(())
                })?;

                self.builder.insert(
                    cir::ops::r#if(
                        self.context,
                        condition,
                        Some(then_region.id()),
                        Some(else_region.id()),
                    )
                    .build(),
                );
                Ok(())
            }
            AstKind::Goto => {
                let AstLeaf::Label(label) = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("goto node carries a label payload");
                };
                self.builder.insert(
                    cir::GotoOpBuilder::new(self.context)
                        .attr("label", AttributeValue::Str(label.clone()))
                        .build(),
                );
                Ok(())
            }
            AstKind::Label => {
                let AstLeaf::Label(label) = ast.get_leaf_data(stmt).unwrap() else {
                    unreachable!("label node carries a label payload");
                };
                self.builder.insert(
                    cir::LabelOpBuilder::new(self.context)
                        .attr("label", AttributeValue::Str(label.clone()))
                        .build(),
                );
                self.lower_stmt(ast.children(stmt).next().unwrap())
            }
            AstKind::Break => {
                match *self.break_scopes.last().unwrap() {
                    BreakScope::Loop(scope) => {
                        self.builder
                            .insert(cir::ops::r#break(self.context, scope).build());
                    }
                    BreakScope::Switch(done) => {
                        let one = self
                            .builder
                            .insert(b::constant(self.context, 1, done.elem).build())
                            .result();
                        self.builder
                            .insert(p::store(self.context, one, done.ptr).build());
                    }
                }
                self.terminated = true;
                Ok(())
            }
            AstKind::Continue => {
                let scope = *self.loop_scopes.last().unwrap();
                self.builder
                    .insert(cir::ops::r#continue(self.context, scope).build());
                self.terminated = true;
                Ok(())
            }
            kind => Err(unsupported(ast, stmt, format!("statement {kind:?}"))),
        }
    }

    fn contains_label(&self, statement: NodeId) -> bool {
        self.ast.get_node(statement).kind == AstKind::Label
            || self
                .ast
                .children(statement)
                .any(|child| self.contains_label(child))
    }

    fn lower_switch(&mut self, stmt: NodeId) -> Result<(), Diagnostic> {
        let mut children = self.ast.children(stmt);
        let value = self.lower_expr(children.next().unwrap())?;
        let body = children.next().unwrap();
        let value_ty = self.context.get_value(value).ty();
        let i32_ty = IntegerType::new(self.context, 32);
        let active = self.alloca(i32_ty, 4, 4);
        let done = self.alloca(i32_ty, 4, 4);
        let zero = self
            .builder
            .insert(b::constant(self.context, 0, i32_ty).build())
            .result();
        self.builder
            .insert(p::store(self.context, zero, active.ptr).build());
        self.builder
            .insert(p::store(self.context, zero, done.ptr).build());

        let mut items = Vec::new();
        self.flatten_switch_items(body, &mut items)?;
        let mut case_conditions = HashMap::new();
        let mut any_match = zero;
        for (index, item) in items.iter().enumerate() {
            let SwitchItem::Case(case_value) = item else {
                continue;
            };
            let case_value = self
                .builder
                .insert(b::constant(self.context, *case_value, value_ty).build())
                .result();
            let condition = self
                .builder
                .insert(
                    b::CmpIOpBuilder::new(self.context)
                        .lhs(value)
                        .rhs(case_value)
                        .predicate("eq")
                        .result_type(IntegerType::new(self.context, 1))
                        .build(),
                )
                .result();
            let condition_value = self
                .builder
                .insert(b::extui(self.context, condition, i32_ty).build())
                .result();
            any_match = self
                .builder
                .insert(b::ori(self.context, any_match, condition_value, i32_ty).build())
                .result();
            case_conditions.insert(index, condition);
        }
        let default_condition = self
            .builder
            .insert(
                b::CmpIOpBuilder::new(self.context)
                    .lhs(any_match)
                    .rhs(zero)
                    .predicate("eq")
                    .result_type(IntegerType::new(self.context, 1))
                    .build(),
            )
            .result();

        self.break_scopes.push(BreakScope::Switch(done));
        for (index, item) in items.into_iter().enumerate() {
            let activation = match &item {
                SwitchItem::Case(_) => Some(case_conditions[&index]),
                SwitchItem::Default => Some(default_condition),
                SwitchItem::Statement(_) => None,
            };
            let condition = self.switch_item_condition(active, done, activation, i32_ty);
            let then_region = self.context.create_region();
            let then_block = self.context.create_block(vec![]);
            then_region.add_block(then_block.id());
            self.in_block(then_block.clone(), |cg| {
                match item {
                    SwitchItem::Case(_) | SwitchItem::Default => {
                        let one = cg
                            .builder
                            .insert(b::constant(cg.context, 1, i32_ty).build())
                            .result();
                        cg.builder
                            .insert(p::store(cg.context, one, active.ptr).build());
                    }
                    SwitchItem::Statement(statement) => cg.lower_stmt(statement)?,
                }
                cg.ensure_cir_yield(then_block);
                Ok(())
            })?;

            let else_region = self.context.create_region();
            let else_block = self.context.create_block(vec![]);
            else_region.add_block(else_block.id());
            self.in_block(else_block.clone(), |cg| {
                cg.ensure_cir_yield(else_block);
                Ok(())
            })?;
            self.builder.insert(
                cir::ops::r#if(
                    self.context,
                    condition,
                    Some(then_region.id()),
                    Some(else_region.id()),
                )
                .build(),
            );
        }
        self.break_scopes.pop();
        Ok(())
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

    fn switch_item_condition(
        &mut self,
        active: Slot,
        done: Slot,
        activation: Option<ValueId>,
        i32_ty: TypeId,
    ) -> ValueId {
        let active = self
            .builder
            .insert(p::load(self.context, active.ptr, active.elem).build())
            .result();
        let active = self.truth_value(active);
        let selected = if let Some(activation) = activation {
            let active = self
                .builder
                .insert(b::extui(self.context, active, i32_ty).build())
                .result();
            let activation = self
                .builder
                .insert(b::extui(self.context, activation, i32_ty).build())
                .result();
            let selected = self
                .builder
                .insert(b::ori(self.context, active, activation, i32_ty).build())
                .result();
            self.truth_value(selected)
        } else {
            active
        };
        let done = self
            .builder
            .insert(p::load(self.context, done.ptr, done.elem).build())
            .result();
        let zero = self
            .builder
            .insert(b::constant(self.context, 0, i32_ty).build())
            .result();
        let not_done = self
            .builder
            .insert(
                b::CmpIOpBuilder::new(self.context)
                    .lhs(done)
                    .rhs(zero)
                    .predicate("eq")
                    .result_type(IntegerType::new(self.context, 1))
                    .build(),
            )
            .result();
        let selected = self
            .builder
            .insert(b::extui(self.context, selected, i32_ty).build())
            .result();
        let not_done = self
            .builder
            .insert(b::extui(self.context, not_done, i32_ty).build())
            .result();
        let condition = self
            .builder
            .insert(b::andi(self.context, selected, not_done, i32_ty).build())
            .result();
        self.truth_value(condition)
    }

    fn in_block<T>(
        &mut self,
        block: std::sync::Arc<tir::Block>,
        lower: impl FnOnce(&mut Self) -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let region = self.context.parent_region(block.id()).unwrap();
        let outer = std::mem::replace(&mut self.builder, IRBuilder::new(block));
        let outer_region = std::mem::replace(&mut self.region, region);
        let outer_terminated = std::mem::replace(&mut self.terminated, false);
        let result = lower(self);
        self.builder = outer;
        self.region = outer_region;
        self.terminated = outer_terminated;
        result
    }

    fn ensure_cir_yield(&mut self, block: std::sync::Arc<tir::Block>) {
        let terminated = block.op_ids().last().is_some_and(|op| {
            self.context
                .get_op(*op)
                .as_interface::<dyn tir::Terminator>()
                .is_some()
        });
        if !terminated {
            self.builder.insert(cir::ops::r#yield(self.context).build());
        }
    }

    fn lower_condition(&mut self, expression: NodeId) -> Result<ValueId, Diagnostic> {
        let value = self.lower_expr(expression)?;
        Ok(self.truth_value(value))
    }

    fn truth_value(&mut self, value: ValueId) -> ValueId {
        let ty = self.context.get_value(value).ty();
        if ty == IntegerType::new(self.context, 1) {
            return value;
        }
        self.compare_against_zero(value, "ne")
    }

    fn promote_boolean_result(&mut self, value: ValueId, target: TypeId) -> ValueId {
        if self.context.get_value(value).ty() != IntegerType::new(self.context, 1) {
            return value;
        }
        self.builder
            .insert(b::extui(self.context, value, target).build())
            .result()
    }

    /// `value <predicate> 0`, at `int` width like every other C comparison: a
    /// promoted value is nonzero exactly when the original is, and
    /// zero-extension preserves that for either signedness.
    fn compare_against_zero(&mut self, value: ValueId, predicate: &str) -> ValueId {
        let ty = self.context.get_value(value).ty();
        let narrow = {
            let data = self.context.get_type_data(ty);
            (data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<IntegerType>()
                .is_some_and(|integer| integer.width() < 32)
        };
        let (value, ty) = if narrow {
            let i32_ty = IntegerType::new(self.context, 32);
            (
                self.builder
                    .insert(b::extui(self.context, value, i32_ty).build())
                    .result(),
                i32_ty,
            )
        } else {
            (value, ty)
        };
        let zero = self
            .builder
            .insert(b::constant(self.context, 0, ty).build())
            .result();
        self.builder
            .insert(
                b::CmpIOpBuilder::new(self.context)
                    .lhs(value)
                    .rhs(zero)
                    .predicate(predicate)
                    .result_type(IntegerType::new(self.context, 1))
                    .build(),
            )
            .result()
    }

    fn materialize(&mut self, expression: LoweredExpr) -> ValueId {
        match expression {
            LoweredExpr::Value(value) => value,
            LoweredExpr::Address { ptr, elem } => self
                .builder
                .insert(p::load(self.context, ptr, elem).build())
                .result(),
        }
    }

    fn lower_record_copy(
        &mut self,
        node: NodeId,
        destination: ValueId,
        record: &str,
    ) -> Result<(), Diagnostic> {
        let LoweredExpr::Address { ptr: source, .. } = self.lower_expr_value(node)? else {
            return Err(unsupported(
                self.ast,
                node,
                "non-addressable struct source".to_string(),
            ));
        };
        if self.ast.get_node(node).kind == AstKind::Call {
            let (size, _) = source_type_layout(self.typed, node_type(self.typed, node));
            let size = self
                .builder
                .insert(
                    b::constant(
                        self.context,
                        size as i64,
                        IntegerType::new(self.context, 64),
                    )
                    .build(),
                )
                .result();
            self.builder
                .insert(p::memcpy(self.context, destination, source, size).build());
            return Ok(());
        }
        self.builder
            .insert(cir::ops::copy_struct(self.context, destination, source, record).build());
        Ok(())
    }

    fn lower_abi_argument(
        &mut self,
        node: NodeId,
        expression: LoweredExpr,
        parameter: &AbiParameter,
    ) -> Result<Vec<ValueId>, Diagnostic> {
        if parameter.indirect {
            let LoweredExpr::Address { ptr: source, .. } = expression else {
                return Err(unsupported(
                    self.ast,
                    node,
                    "non-addressable aggregate argument".to_string(),
                ));
            };
            let source_ty = node_type(self.typed, node);
            let (size, align) = source_type_layout(self.typed, source_ty);
            let destination = self
                .builder
                .insert(p::alloca(self.context, size, align, PtrType::opaque(self.context)).build())
                .result();
            let size = self
                .builder
                .insert(
                    b::constant(
                        self.context,
                        size as i64,
                        IntegerType::new(self.context, 64),
                    )
                    .build(),
                )
                .result();
            self.builder
                .insert(p::memcpy(self.context, destination, source, size).build());
            return Ok(vec![destination]);
        }
        if matches!(
            self.typed.types().kind(node_type(self.typed, node)),
            TypeKind::Record(_)
        ) {
            let LoweredExpr::Address { ptr, .. } = expression else {
                return Err(unsupported(
                    self.ast,
                    node,
                    "non-addressable aggregate argument".to_string(),
                ));
            };
            let ptr = self.prepare_abi_source(
                ptr,
                node_type(self.typed, node),
                parameter.pieces.as_slice(),
            );
            let values = parameter
                .pieces
                .iter()
                .map(|piece| {
                    let address = self.offset_address_ir(ptr, piece.offset, piece.ty);
                    self.builder
                        .insert(p::load(self.context, address, piece.ty).build())
                        .result()
                })
                .collect::<Vec<_>>();
            if parameter.grouped {
                let ty = TupleType::new(
                    self.context,
                    parameter.pieces.iter().map(|piece| piece.ty).collect(),
                );
                return Ok(vec![
                    self.builder
                        .insert(
                            b::MakeTupleOpBuilder::new(self.context)
                                .elements(values)
                                .result_type(ty)
                                .build(),
                        )
                        .result(),
                ]);
            }
            return Ok(values);
        }
        Ok(vec![self.materialize(expression)])
    }

    fn prepare_abi_source(
        &mut self,
        source: ValueId,
        source_ty: QualType,
        pieces: &[AbiPiece],
    ) -> ValueId {
        let (size, align) = source_type_layout(self.typed, source_ty);
        let Some((abi_size, abi_align)) = abi_storage_layout(self.context, pieces) else {
            return source;
        };
        if abi_size <= size {
            return source;
        }

        let destination = self
            .builder
            .insert(
                p::alloca(
                    self.context,
                    abi_size,
                    align.max(abi_align),
                    PtrType::opaque(self.context),
                )
                .build(),
            )
            .result();
        for piece in pieces {
            let zero = match type_kind(self.context, piece.ty) {
                ValueKind::Float => self
                    .builder
                    .insert(b::constantf(self.context, 0.0, piece.ty).build())
                    .result(),
                ValueKind::Int => self
                    .builder
                    .insert(b::constant(self.context, 0, piece.ty).build())
                    .result(),
                ValueKind::Vector => unreachable!("ABI padding uses scalar carriers"),
            };
            let address = self.offset_address_ir(destination, piece.offset, piece.ty);
            self.builder
                .insert(p::store(self.context, zero, address).build());
        }
        let size = self
            .builder
            .insert(
                b::constant(
                    self.context,
                    size as i64,
                    IntegerType::new(self.context, 64),
                )
                .build(),
            )
            .result();
        self.builder
            .insert(p::memcpy(self.context, destination, source, size).build());
        destination
    }

    fn lower_return_value(&mut self, node: NodeId) -> Result<ValueId, Diagnostic> {
        let expression = self.lower_expr_value(node)?;
        let Some(pieces) = self.return_abi.aggregate.as_deref() else {
            return Ok(self.materialize(expression));
        };
        let LoweredExpr::Address { ptr, .. } = expression else {
            return Err(unsupported(
                self.ast,
                node,
                "non-addressable aggregate return value".to_string(),
            ));
        };
        let ptr = self.prepare_abi_source(ptr, node_type(self.typed, node), pieces);
        let values = pieces
            .iter()
            .map(|piece| {
                let address = self.offset_address_ir(ptr, piece.offset, piece.ty);
                self.builder
                    .insert(p::load(self.context, address, piece.ty).build())
                    .result()
            })
            .collect::<Vec<_>>();
        if let [value] = values.as_slice() {
            return Ok(*value);
        }
        Ok(self
            .builder
            .insert(
                b::MakeTupleOpBuilder::new(self.context)
                    .elements(values)
                    .result_type(self.return_abi.ty)
                    .build(),
            )
            .result())
    }

    fn lower_indirect_return(&mut self, node: NodeId) -> Result<(), Diagnostic> {
        let expression = self.lower_expr_value(node)?;
        let LoweredExpr::Address { ptr: source, .. } = expression else {
            unreachable!("aggregate return expressions lower to addresses");
        };
        let (size, _) = source_type_layout(self.typed, node_type(self.typed, node));
        let size = self
            .builder
            .insert(
                b::constant(
                    self.context,
                    size as i64,
                    IntegerType::new(self.context, 64),
                )
                .build(),
            )
            .result();
        self.builder.insert(
            p::memcpy(
                self.context,
                self.indirect_return
                    .expect("indirect return has a destination argument"),
                source,
                size,
            )
            .build(),
        );
        Ok(())
    }

    fn lower_expr(&mut self, root: NodeId) -> Result<ValueId, Diagnostic> {
        let expression = self.lower_expr_value(root)?;
        Ok(self.materialize(expression))
    }

    fn lower_expr_value(&mut self, root: NodeId) -> Result<LoweredExpr, Diagnostic> {
        self.values.clear();
        self.lower_expr_node(root)
    }

    fn lower_expr_node(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        if let Some(expression) = self.values.get(&node) {
            return Ok(*expression);
        }
        let ast = self.ast;
        let kind = ast.get_node(node).kind;
        if let Some(value) = ast.get_annotation(node).and_then(|info| info.constant)
            && matches!(
                self.typed.types().kind(node_type(self.typed, node)),
                TypeKind::Integer(_) | TypeKind::Enum(_)
            )
        {
            let ty = lower_type(self.context, self.typed, node_type(self.typed, node));
            let expression = LoweredExpr::Value(
                self.builder
                    .insert(b::constant(self.context, value, ty).build())
                    .result(),
            );
            let expression = self.apply_conversions(node, expression);
            self.values.insert(node, expression);
            return Ok(expression);
        }
        if matches!(kind, AstKind::LogAnd | AstKind::LogOr) {
            return self.lower_logical(node, kind);
        }
        if kind == AstKind::Conditional {
            return self.lower_conditional(node);
        }

        for child in ast.children(node) {
            self.lower_expr_node(child)?;
        }

        {
            let expression = match ast.get_node(node).kind {
                AstKind::Int => {
                    let AstLeaf::Int(n) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("int node carries an int payload");
                    };
                    let ty = lower_type(self.context, self.typed, node_type(self.typed, node));
                    LoweredExpr::Value(
                        self.builder
                            .insert(b::constant(self.context, n.value.to_i64(), ty).build())
                            .result(),
                    )
                }
                AstKind::FloatLiteral => {
                    let AstLeaf::Float(n) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("floating literal node carries a floating payload");
                    };
                    LoweredExpr::Value(
                        self.builder
                            .insert(
                                b::constantf(self.context, n.value, FloatType::f64(self.context))
                                    .build(),
                            )
                            .result(),
                    )
                }
                AstKind::Character => {
                    let AstLeaf::Character(spelling) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("character node carries a character payload");
                    };
                    let Some(value) = decode_character_constant(spelling) else {
                        return Err(unsupported(
                            ast,
                            node,
                            "multi-character constant".to_string(),
                        ));
                    };
                    let ty = lower_type(self.context, self.typed, node_type(self.typed, node));
                    LoweredExpr::Value(
                        self.builder
                            .insert(b::constant(self.context, value, ty).build())
                            .result(),
                    )
                }
                AstKind::SizeofType | AstKind::SizeofExpr => {
                    let value = ast.get_annotation(node).unwrap().constant.unwrap();
                    let ty = lower_type(self.context, self.typed, node_type(self.typed, node));
                    LoweredExpr::Value(
                        self.builder
                            .insert(b::constant(self.context, value, ty).build())
                            .result(),
                    )
                }
                AstKind::String => {
                    let AstLeaf::String(value) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("string node carries a string payload");
                    };
                    let i8_ty = IntegerType::new(self.context, 8);
                    let ptr_ty = PtrType::typed(self.context, i8_ty);
                    LoweredExpr::Value(
                        self.builder
                            .insert(cir::string_op(self.context, value, ptr_ty))
                            .result(),
                    )
                }
                AstKind::Var => {
                    let AstLeaf::Var(name) = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("var node carries a var payload");
                    };
                    if let Some(value) = ast.get_annotation(node).and_then(|info| info.constant) {
                        let ty = lower_type(self.context, self.typed, node_type(self.typed, node));
                        LoweredExpr::Value(
                            self.builder
                                .insert(b::constant(self.context, value, ty).build())
                                .result(),
                        )
                    } else if ast
                        .get_annotation(node)
                        .is_some_and(|info| info.category == ValueCategory::Function)
                    {
                        let ptr_ty = lower_type(
                            self.context,
                            self.typed,
                            converted_node_type(self.typed, node),
                        );
                        LoweredExpr::Value(
                            self.builder
                                .insert(b::addr_of_op(self.context, name, ptr_ty))
                                .result(),
                        )
                    } else {
                        let entity = node_entity(self.typed, node);
                        if let Some(slot) = self.locals.get(&entity).copied() {
                            LoweredExpr::Address {
                                ptr: slot.ptr,
                                elem: slot.elem,
                            }
                        } else {
                            let global = &self.globals[&entity];
                            let ptr_ty = PtrType::typed(self.context, global.elem);
                            LoweredExpr::Address {
                                ptr: self
                                    .builder
                                    .insert(b::addr_of_op(self.context, &global.name, ptr_ty))
                                    .result(),
                                elem: global.elem,
                            }
                        }
                    }
                }
                AstKind::Member => {
                    let AstLeaf::Member { indirect, .. } = ast.get_leaf_data(node).unwrap() else {
                        unreachable!("member node carries a member payload");
                    };
                    let base_node = ast.children(node).next().unwrap();
                    let base_value = self.values[&base_node];
                    let base_ptr = if *indirect {
                        self.materialize(base_value)
                    } else if let LoweredExpr::Address { ptr, .. } = base_value {
                        ptr
                    } else {
                        return Err(unsupported(
                            ast,
                            node,
                            "non-addressable member base".to_string(),
                        ));
                    };
                    let elem = lower_type(self.context, self.typed, node_type(self.typed, node));
                    let ptr_ty = if matches!(
                        self.typed.types().kind(node_type(self.typed, node)),
                        TypeKind::Record(_)
                    ) {
                        PtrType::opaque(self.context)
                    } else {
                        PtrType::typed(self.context, elem)
                    };
                    let field = ast.get_annotation(node).unwrap().member_index.unwrap() as u64;
                    let base_ty = node_type(self.typed, base_node);
                    let record = match self.typed.types().kind(base_ty) {
                        TypeKind::Record(id) => self.typed.record(*id).unwrap(),
                        TypeKind::Pointer(pointee) => {
                            let TypeKind::Record(id) = self.typed.types().kind(*pointee) else {
                                unreachable!("member base has a record type")
                            };
                            self.typed.record(*id).unwrap()
                        }
                        _ => unreachable!("member base has a record type"),
                    };
                    let member = self.builder.insert(
                        cir::ops::get_member(
                            self.context,
                            base_ptr,
                            field,
                            record.name.as_str(),
                            ptr_ty,
                        )
                        .build(),
                    );
                    LoweredExpr::Address {
                        ptr: member.result(),
                        elem,
                    }
                }
                kind @ (AstKind::Call | AstKind::CallExpr) => {
                    let designator_ty = ast
                        .get_annotation(node)
                        .and_then(|semantics| semantics.call_designator_ty)
                        .expect("semantic analysis records the call designator type");
                    let (name, sig, callee, arguments) = if kind == AstKind::Call {
                        let AstLeaf::Call(name) = ast.get_leaf_data(node).unwrap() else {
                            unreachable!("call node carries a call payload");
                        };
                        let entity = node_entity(self.typed, node);
                        let (sig, callee) = match self.typed.types().kind(designator_ty) {
                            TypeKind::Function { .. } => (self.signatures[&entity].clone(), None),
                            TypeKind::Pointer(pointee) => {
                                let sig =
                                    classify_function_type(self.context, self.typed, *pointee);
                                let callee = if let Some(slot) = self.locals.get(&entity).copied() {
                                    self.materialize(LoweredExpr::Address {
                                        ptr: slot.ptr,
                                        elem: slot.elem,
                                    })
                                } else {
                                    let global = &self.globals[&entity];
                                    let ptr_ty = PtrType::typed(self.context, global.elem);
                                    let address = self
                                        .builder
                                        .insert(b::addr_of_op(self.context, &global.name, ptr_ty))
                                        .result();
                                    self.materialize(LoweredExpr::Address {
                                        ptr: address,
                                        elem: global.elem,
                                    })
                                };
                                (sig, Some(callee))
                            }
                            _ => unreachable!("call designator is a function or function pointer"),
                        };
                        (
                            Some(name.clone()),
                            sig,
                            callee,
                            ast.children(node).collect::<Vec<_>>(),
                        )
                    } else {
                        let children = ast.children(node).collect::<Vec<_>>();
                        let callee_node = children[0];
                        let function_ty = match self.typed.types().kind(designator_ty) {
                            TypeKind::Function { .. } => designator_ty,
                            TypeKind::Pointer(pointee) => *pointee,
                            _ => unreachable!(
                                "call expression designator is a function or function pointer"
                            ),
                        };
                        (
                            None,
                            classify_function_type(self.context, self.typed, function_ty),
                            Some(self.materialize(self.values[&callee_node])),
                            children[1..].to_vec(),
                        )
                    };
                    let mut args = Vec::new();
                    let mut argument_alignments = Vec::new();
                    for (index, &argument) in arguments.iter().enumerate() {
                        let expression = self.values[&argument];
                        if let Some(parameter) = sig.params.get(index) {
                            args.extend(self.lower_abi_argument(argument, expression, parameter)?);
                            if parameter.grouped {
                                argument_alignments.push(parameter.alignment);
                            } else {
                                argument_alignments
                                    .extend(std::iter::repeat_n(1, parameter.pieces.len()));
                            }
                        } else {
                            args.push(self.materialize(expression));
                            argument_alignments.push(1);
                        }
                    }
                    let source_ty = node_type(self.typed, node);
                    let elem = lower_type(self.context, self.typed, source_ty);
                    if sig.ret.indirect {
                        let (size, align) = source_type_layout(self.typed, source_ty);
                        let slot = self.alloca(elem, size, align);
                        args.insert(0, slot.ptr);
                        argument_alignments.insert(0, 1);
                        if let Some(callee) = callee {
                            let mut call = b::IndirectCallOpBuilder::new(self.context)
                                .callee(callee)
                                .args(args)
                                .result_address()
                                .result_type(sig.ret.ty);
                            if argument_alignments.iter().any(|&alignment| alignment > 1) {
                                call = call.argument_alignments(&argument_alignments);
                            }
                            self.builder.insert(call.build());
                        } else {
                            let mut call = b::CallOpBuilder::new(self.context)
                                .args(args)
                                .attr(
                                    "callee",
                                    AttributeValue::Str(
                                        name.clone().expect("direct call has a symbol name"),
                                    ),
                                )
                                .result_address()
                                .result_type(sig.ret.ty);
                            if argument_alignments.iter().any(|&alignment| alignment > 1) {
                                call = call.argument_alignments(&argument_alignments);
                            }
                            self.builder.insert(call.build());
                        }
                        LoweredExpr::Address {
                            ptr: slot.ptr,
                            elem,
                        }
                    } else {
                        let result = if let Some(callee) = callee {
                            let mut call = b::IndirectCallOpBuilder::new(self.context)
                                .callee(callee)
                                .args(args)
                                .result_type(sig.ret.ty);
                            if argument_alignments.iter().any(|&alignment| alignment > 1) {
                                call = call.argument_alignments(&argument_alignments);
                            }
                            self.builder.insert(call.build()).result()
                        } else {
                            let mut call = b::CallOpBuilder::new(self.context)
                                .args(args)
                                .attr(
                                    "callee",
                                    AttributeValue::Str(
                                        name.clone().expect("direct call has a symbol name"),
                                    ),
                                )
                                .result_type(sig.ret.ty);
                            if argument_alignments.iter().any(|&alignment| alignment > 1) {
                                call = call.argument_alignments(&argument_alignments);
                            }
                            self.builder.insert(call.build()).result()
                        };
                        if let Some(pieces) = sig.ret.aggregate.as_deref() {
                            let (size, align) = source_type_layout(self.typed, source_ty);
                            let (abi_size, abi_align) = abi_storage_layout(self.context, pieces)
                                .expect("classified aggregate returns use scalar ABI pieces");
                            let slot = self.alloca(elem, size.max(abi_size), align.max(abi_align));
                            for (index, piece) in pieces.iter().enumerate() {
                                let value = if pieces.len() == 1 {
                                    result
                                } else {
                                    self.builder
                                        .insert(
                                            b::TupleGetOpBuilder::new(self.context)
                                                .tuple(result)
                                                .attr("index", AttributeValue::UInt(index as u64))
                                                .result_type(piece.ty)
                                                .build(),
                                        )
                                        .result()
                                };
                                let address =
                                    self.offset_address_ir(slot.ptr, piece.offset, piece.ty);
                                self.builder
                                    .insert(p::store(self.context, value, address).build());
                            }
                            LoweredExpr::Address {
                                ptr: slot.ptr,
                                elem,
                            }
                        } else {
                            LoweredExpr::Value(result)
                        }
                    }
                }
                kind @ (AstKind::Add
                | AstKind::Sub
                | AstKind::Mul
                | AstKind::Div
                | AstKind::Mod) => {
                    let mut children = ast.children(node);
                    let lhs_node = children.next().unwrap();
                    let rhs_node = children.next().unwrap();
                    let lhs = self.values[&lhs_node];
                    let rhs = self.values[&rhs_node];
                    let l = self.materialize(lhs);
                    let r = self.materialize(rhs);
                    let source_ty = node_type(self.typed, node);
                    let lhs_ty = converted_node_type(self.typed, lhs_node);
                    let rhs_ty = converted_node_type(self.typed, rhs_node);
                    let value = match (
                        kind,
                        self.typed.types().kind(lhs_ty),
                        self.typed.types().kind(rhs_ty),
                    ) {
                        (AstKind::Sub, TypeKind::Pointer(_), TypeKind::Pointer(_)) => {
                            self.lower_pointer_difference(l, r, lhs_ty, source_ty)
                        }
                        (
                            AstKind::Add | AstKind::Sub,
                            TypeKind::Pointer(_),
                            TypeKind::Integer(_),
                        ) => self.lower_pointer_offset(l, r, rhs_ty, lhs_ty, kind == AstKind::Sub),
                        (AstKind::Add, TypeKind::Integer(_), TypeKind::Pointer(_)) => {
                            self.lower_pointer_offset(r, l, lhs_ty, rhs_ty, false)
                        }
                        _ if matches!(self.typed.types().kind(source_ty), TypeKind::Double) => {
                            self.lower_double_binary(kind, l, r)
                        }
                        _ => self.lower_integer_binary(kind, l, r, source_ty),
                    };
                    LoweredExpr::Value(value)
                }
                kind @ (AstKind::BitAnd
                | AstKind::BitXor
                | AstKind::BitOr
                | AstKind::Shl
                | AstKind::Shr) => {
                    let mut children = ast.children(node);
                    let lhs_node = children.next().unwrap();
                    let rhs_node = children.next().unwrap();
                    let result_ty =
                        lower_type(self.context, self.typed, node_type(self.typed, node));
                    let lhs = self.materialize(self.values[&lhs_node]);
                    let rhs = self.materialize(self.values[&rhs_node]);
                    let lhs = self.promote_boolean_result(lhs, result_ty);
                    let rhs = self.promote_boolean_result(rhs, result_ty);
                    LoweredExpr::Value(self.lower_integer_binary(
                        kind,
                        lhs,
                        rhs,
                        node_type(self.typed, node),
                    ))
                }
                kind @ (AstKind::Neg | AstKind::Pos | AstKind::Not | AstKind::BitNot) => {
                    let child = ast.children(node).next().unwrap();
                    let operand = self.materialize(self.values[&child]);
                    let result_ty =
                        lower_type(self.context, self.typed, node_type(self.typed, node));
                    let value = match kind {
                        AstKind::Pos => operand,
                        AstKind::Neg => {
                            let zero = self
                                .builder
                                .insert(b::constant(self.context, 0, result_ty).build())
                                .result();
                            self.builder
                                .insert(b::subi(self.context, zero, operand, result_ty).build())
                                .result()
                        }
                        AstKind::BitNot => {
                            let ones = self
                                .builder
                                .insert(b::constant(self.context, -1, result_ty).build())
                                .result();
                            self.builder
                                .insert(b::xori(self.context, operand, ones, result_ty).build())
                                .result()
                        }
                        AstKind::Not => {
                            let comparison = self.compare_against_zero(operand, "eq");
                            self.builder
                                .insert(b::extui(self.context, comparison, result_ty).build())
                                .result()
                        }
                        _ => unreachable!(),
                    };
                    LoweredExpr::Value(value)
                }
                AstKind::AddressOf => {
                    let child = ast.children(node).next().unwrap();
                    let LoweredExpr::Address { ptr, .. } = self.values[&child] else {
                        return Err(unsupported(
                            ast,
                            node,
                            "non-addressable address-of operand".to_string(),
                        ));
                    };
                    LoweredExpr::Value(ptr)
                }
                AstKind::Deref => {
                    let child = ast.children(node).next().unwrap();
                    let ptr = self.materialize(self.values[&child]);
                    if ast
                        .get_annotation(node)
                        .is_some_and(|info| info.category == ValueCategory::Function)
                    {
                        LoweredExpr::Value(ptr)
                    } else {
                        let elem =
                            lower_type(self.context, self.typed, node_type(self.typed, node));
                        LoweredExpr::Address { ptr, elem }
                    }
                }
                kind
                @ (AstKind::PreInc | AstKind::PreDec | AstKind::PostInc | AstKind::PostDec) => {
                    let child = ast.children(node).next().unwrap();
                    let LoweredExpr::Address { ptr, elem } = self.values[&child] else {
                        return Err(unsupported(
                            ast,
                            node,
                            "non-addressable increment operand".to_string(),
                        ));
                    };
                    let old = self
                        .builder
                        .insert(p::load(self.context, ptr, elem).build())
                        .result();
                    let operand_ty = node_type(self.typed, child);
                    let increment = matches!(kind, AstKind::PreInc | AstKind::PostInc);
                    let new =
                        if let TypeKind::Pointer(pointee) = self.typed.types().kind(operand_ty) {
                            let offset_ty =
                                IntegerType::new(self.context, self.typed.target().pointer_width());
                            let size = source_type_layout(self.typed, *pointee).0 as i64;
                            let offset = self
                                .builder
                                .insert(
                                    b::constant(
                                        self.context,
                                        if increment { size } else { -size },
                                        offset_ty,
                                    )
                                    .build(),
                                )
                                .result();
                            self.builder
                                .insert(p::ptradd(self.context, old, offset, elem).build())
                                .result()
                        } else {
                            let one = self
                                .builder
                                .insert(b::constant(self.context, 1, elem).build())
                                .result();
                            if increment {
                                self.builder
                                    .insert(b::addi(self.context, old, one, elem).build())
                                    .result()
                            } else {
                                self.builder
                                    .insert(b::subi(self.context, old, one, elem).build())
                                    .result()
                            }
                        };
                    self.builder
                        .insert(p::store(self.context, new, ptr).build());
                    LoweredExpr::Value(if matches!(kind, AstKind::PostInc | AstKind::PostDec) {
                        old
                    } else {
                        new
                    })
                }
                kind @ (AstKind::Lt
                | AstKind::Gt
                | AstKind::Le
                | AstKind::Ge
                | AstKind::Eq
                | AstKind::Ne) => {
                    let mut children = ast.children(node);
                    let lhs_node = children.next().unwrap();
                    let rhs_node = children.next().unwrap();
                    let lhs = self.materialize(self.values[&lhs_node]);
                    let rhs = self.materialize(self.values[&rhs_node]);
                    // The common type of the usual arithmetic conversions, not
                    // the operand's own type: it decides signed vs unsigned.
                    let operand_ty = converted_node_type(self.typed, lhs_node);
                    let value = match self.typed.types().kind(operand_ty) {
                        TypeKind::Double => self.lower_double_compare(kind, lhs, rhs),
                        _ => self.lower_integer_compare(kind, lhs, rhs, operand_ty),
                    };
                    LoweredExpr::Value(value)
                }
                AstKind::Comma => {
                    let rhs = ast.children(node).nth(1).unwrap();
                    LoweredExpr::Value(self.materialize(self.values[&rhs]))
                }
                AstKind::Cast => {
                    let child = ast.children(node).next().unwrap();
                    let value = self.materialize(self.values[&child]);
                    let source = node_type(self.typed, child);
                    let target = node_type(self.typed, node);
                    let value = if self.typed.integer_width(source).is_some()
                        && matches!(self.typed.types().kind(target), TypeKind::Pointer(_))
                        && ast
                            .get_annotation(child)
                            .is_some_and(|semantics| semantics.constant == Some(0))
                    {
                        let target = lower_type(self.context, self.typed, target);
                        self.builder
                            .insert(b::constant(self.context, 0, target).build())
                            .result()
                    } else {
                        self.convert_scalar(value, source, target)
                    };
                    LoweredExpr::Value(value)
                }
                kind @ (AstKind::AddAssign
                | AstKind::SubAssign
                | AstKind::MulAssign
                | AstKind::DivAssign
                | AstKind::ModAssign
                | AstKind::ShlAssign
                | AstKind::ShrAssign
                | AstKind::AndAssign
                | AstKind::XorAssign
                | AstKind::OrAssign) => {
                    let mut children = ast.children(node);
                    let lhs_node = children.next().unwrap();
                    let LoweredExpr::Address { ptr, elem } = self.values[&lhs_node] else {
                        return Err(unsupported(
                            ast,
                            node,
                            "non-addressable compound assignment".to_string(),
                        ));
                    };
                    let rhs_node = children.next().unwrap();
                    let rhs = self.materialize(self.values[&rhs_node]);
                    let lhs = self
                        .builder
                        .insert(p::load(self.context, ptr, elem).build())
                        .result();
                    let source_ty = node_type(self.typed, lhs_node);
                    let value = match self.typed.types().kind(source_ty) {
                        TypeKind::Pointer(_) => self.lower_pointer_offset(
                            lhs,
                            rhs,
                            node_type(self.typed, rhs_node),
                            source_ty,
                            kind == AstKind::SubAssign,
                        ),
                        TypeKind::Double => self.lower_double_binary(kind, lhs, rhs),
                        _ => self.lower_integer_binary(kind, lhs, rhs, source_ty),
                    };
                    self.builder
                        .insert(p::store(self.context, value, ptr).build());
                    LoweredExpr::Value(value)
                }
                AstKind::AssignExpr => {
                    let mut children = ast.children(node);
                    let lhs_node = children.next().unwrap();
                    let lhs = self.values[&lhs_node];
                    let rhs = self.values[&children.next().unwrap()];
                    let LoweredExpr::Address { ptr, elem } = lhs else {
                        return Err(unsupported(
                            ast,
                            node,
                            "non-addressable assignment".to_string(),
                        ));
                    };
                    if let TypeKind::Record(id) =
                        self.typed.types().kind(node_type(self.typed, lhs_node))
                    {
                        let LoweredExpr::Address { ptr: source, .. } = rhs else {
                            return Err(unsupported(
                                ast,
                                node,
                                "non-addressable struct source".to_string(),
                            ));
                        };
                        self.builder.insert(
                            cir::ops::copy_struct(
                                self.context,
                                ptr,
                                source,
                                self.typed.record(*id).unwrap().name.as_str(),
                            )
                            .build(),
                        );
                        LoweredExpr::Address { ptr, elem }
                    } else {
                        let value = self.materialize(rhs);
                        self.builder
                            .insert(p::store(self.context, value, ptr).build());
                        LoweredExpr::Value(value)
                    }
                }
                // The richer operators (division, comparison, logical, unary,
                // calls) are parsed but not yet lowered; stub them out for now.
                kind => {
                    return Err(unsupported(ast, node, format!("expression {kind:?}")));
                }
            };
            let expression = if ast
                .get_annotation(node)
                .is_some_and(|semantics| !semantics.conversions.is_empty())
            {
                self.apply_conversions(node, expression)
            } else {
                expression
            };
            self.values.insert(node, expression);
            Ok(expression)
        }
    }

    fn lower_logical(&mut self, node: NodeId, kind: AstKind) -> Result<LoweredExpr, Diagnostic> {
        let mut children = self.ast.children(node);
        let lhs_node = children.next().unwrap();
        let rhs_node = children.next().unwrap();
        let lhs = self.lower_expr_node(lhs_node)?;
        let lhs = self.materialize(lhs);
        let condition = self.truth_value(lhs);
        let result_ty = IntegerType::new(self.context, 32);
        let result = self.alloca(result_ty, 4, 4);

        let then_region = self.context.create_region();
        let then_block = self.context.create_block(vec![]);
        then_region.add_block(then_block.id());
        self.in_block(then_block.clone(), |cg| {
            cg.lower_logical_arm(
                rhs_node,
                result,
                result_ty,
                then_block,
                kind == AstKind::LogAnd,
                1,
            )
        })?;

        let else_region = self.context.create_region();
        let else_block = self.context.create_block(vec![]);
        else_region.add_block(else_block.id());
        self.in_block(else_block.clone(), |cg| {
            cg.lower_logical_arm(
                rhs_node,
                result,
                result_ty,
                else_block,
                kind == AstKind::LogOr,
                0,
            )
        })?;

        self.builder.insert(
            cir::ops::r#if(
                self.context,
                condition,
                Some(then_region.id()),
                Some(else_region.id()),
            )
            .build(),
        );
        let expression = LoweredExpr::Value(
            self.builder
                .insert(p::load(self.context, result.ptr, result.elem).build())
                .result(),
        );
        self.values.insert(node, expression);
        Ok(expression)
    }

    fn lower_logical_arm(
        &mut self,
        rhs_node: NodeId,
        result: Slot,
        result_ty: TypeId,
        block: std::sync::Arc<tir::Block>,
        evaluate_rhs: bool,
        constant: i64,
    ) -> Result<(), Diagnostic> {
        let value = if evaluate_rhs {
            let rhs = self.lower_expr_node(rhs_node)?;
            let rhs = self.materialize(rhs);
            let rhs = self.truth_value(rhs);
            self.builder
                .insert(b::extui(self.context, rhs, result_ty).build())
                .result()
        } else {
            self.builder
                .insert(b::constant(self.context, constant, result_ty).build())
                .result()
        };
        self.builder
            .insert(p::store(self.context, value, result.ptr).build());
        self.ensure_cir_yield(block);
        Ok(())
    }

    fn lower_conditional(&mut self, node: NodeId) -> Result<LoweredExpr, Diagnostic> {
        let mut children = self.ast.children(node);
        let condition_node = children.next().unwrap();
        let then_node = children.next().unwrap();
        let else_node = children.next().unwrap();
        let condition = self.lower_expr_node(condition_node)?;
        let condition = self.materialize(condition);
        let condition = self.truth_value(condition);
        let source_ty = node_type(self.typed, node);
        let result_ty = lower_type(self.context, self.typed, source_ty);
        let (size, align) = source_type_layout(self.typed, source_ty);
        let result = self.alloca(result_ty, size, align);

        let then_region = self.context.create_region();
        let then_block = self.context.create_block(vec![]);
        then_region.add_block(then_block.id());
        self.in_block(then_block.clone(), |cg| {
            cg.lower_conditional_arm(then_node, result, then_block)
        })?;

        let else_region = self.context.create_region();
        let else_block = self.context.create_block(vec![]);
        else_region.add_block(else_block.id());
        self.in_block(else_block.clone(), |cg| {
            cg.lower_conditional_arm(else_node, result, else_block)
        })?;

        self.builder.insert(
            cir::ops::r#if(
                self.context,
                condition,
                Some(then_region.id()),
                Some(else_region.id()),
            )
            .build(),
        );
        let expression = LoweredExpr::Value(
            self.builder
                .insert(p::load(self.context, result.ptr, result.elem).build())
                .result(),
        );
        self.values.insert(node, expression);
        Ok(expression)
    }

    fn lower_conditional_arm(
        &mut self,
        node: NodeId,
        result: Slot,
        block: std::sync::Arc<tir::Block>,
    ) -> Result<(), Diagnostic> {
        let value = self.lower_expr_node(node)?;
        let value = self.materialize(value);
        self.builder
            .insert(p::store(self.context, value, result.ptr).build());
        self.ensure_cir_yield(block);
        Ok(())
    }
}

/// Lower frontend data definitions immediately ahead of the machine backend.
/// String uses become addresses into `.rodata`; scalar globals become symbols
/// in `.data`.
pub fn lower_data(context: &Context, module: &ModuleOp) -> Result<(), tir::PassError> {
    use tir::attributes::AttributeValue;
    use tir::backend::{
        DataRelocOpBuilder, LiteralOpBuilder, SectionEndOpBuilder, SectionOpBuilder,
        SymbolEndOpBuilder, SymbolOpBuilder,
    };

    let mut rewriter = tir::Rewriter::new(context.clone());
    let mut strings: Vec<(String, String)> = Vec::new();
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut globals = Vec::new();
    let mut zero_globals = Vec::new();

    let module_body = module.body();
    for op_id in module_body.op_ids() {
        let op = context.get_op(op_id);
        if let Some(global) = op.clone().as_op::<cir::GlobalOp>() {
            globals.push((
                global.sym_name(),
                global.bytes(),
                global.relocations(),
                global.align(),
            ));
            rewriter.erase_op(&tir::OperationRef::new(op, Some(module_body.clone()), None))?;
        } else if let Some(string) = op.clone().as_op::<cir::GlobalStringOp>() {
            let label = string.sym_name();
            let value = string.value();
            labels.insert(value.clone(), label.clone());
            strings.push((label, decode_c_escapes(&value)));
            rewriter.erase_op(&tir::OperationRef::new(op, Some(module_body.clone()), None))?;
        } else if let Some(global) = op.clone().as_op::<cir::ZeroGlobalOp>() {
            zero_globals.push((global.sym_name(), global.size(), global.align()));
            rewriter.erase_op(&tir::OperationRef::new(op, Some(module_body.clone()), None))?;
        }
    }

    for op_id in module.body().op_ids() {
        let op = context.get_op(op_id);
        if op.clone().as_op::<tir::builtin::FuncOp>().is_none() {
            continue;
        }
        let region = context.get_region(op.regions[0]);
        for block in region.iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                let Some(string) = op.clone().as_op::<cir::StringOp>() else {
                    continue;
                };
                let value = string
                    .attributes()
                    .iter()
                    .find(|attr| attr.name == "value")
                    .and_then(|attr| match &attr.value {
                        AttributeValue::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .expect("cir.string must carry a value");
                let label = labels
                    .entry(value.clone())
                    .or_insert_with(|| {
                        let label = format!(".L.str{}", strings.len());
                        strings.push((label.clone(), decode_c_escapes(&value)));
                        label
                    })
                    .clone();
                let result_ty = context.get_value(string.result()).ty();
                let addr = b::addr_of_op(context, &label, result_ty);
                rewriter.replace_op(
                    &tir::OperationRef::new(op, Some(block.clone()), None),
                    &addr,
                )?;
            }
        }
    }

    if !globals.is_empty() {
        let section = SectionOpBuilder::new(context)
            .attr("name", AttributeValue::Str(".data".to_string()))
            .build();
        let mut section_builder = IRBuilder::new(section.body());
        for (name, bytes, mut relocations, align) in globals {
            let symbol = SymbolOpBuilder::new(context)
                .attr("name", AttributeValue::Str(name))
                .attr("binding", AttributeValue::Str("global".to_string()))
                .attr("kind", AttributeValue::Str("object".to_string()))
                .attr("align", AttributeValue::UInt(align))
                .build();
            let mut symbol_builder = IRBuilder::new(symbol.body());
            relocations.sort_by_key(|relocation| relocation.0);
            let mut cursor = 0;
            for (offset, target, addend, width) in relocations {
                for &byte in &bytes[cursor..offset as usize] {
                    symbol_builder.insert(
                        LiteralOpBuilder::new(context)
                            .attr("kind", AttributeValue::Str("byte".to_string()))
                            .attr("value", AttributeValue::Int(i64::from(byte)))
                            .build(),
                    );
                }
                symbol_builder.insert(
                    DataRelocOpBuilder::new(context)
                        .attr("symbol", AttributeValue::Str(target))
                        .attr("width", AttributeValue::UInt(width))
                        .attr("addend", AttributeValue::Int(addend))
                        .build(),
                );
                cursor = (offset + width) as usize;
            }
            for &byte in &bytes[cursor..] {
                symbol_builder.insert(
                    LiteralOpBuilder::new(context)
                        .attr("kind", AttributeValue::Str("byte".to_string()))
                        .attr("value", AttributeValue::Int(i64::from(byte)))
                        .build(),
                );
            }
            symbol_builder.insert(SymbolEndOpBuilder::new(context).build());
            section_builder.insert(symbol);
        }
        section_builder.insert(SectionEndOpBuilder::new(context).build());
        let end = module_body.op_ids().len().saturating_sub(1);
        module_body.insert(end, section.id());
    }

    if !zero_globals.is_empty() {
        let section = SectionOpBuilder::new(context)
            .attr("name", AttributeValue::Str(".bss".to_string()))
            .build();
        let mut section_builder = IRBuilder::new(section.body());
        for (name, size, align) in zero_globals {
            let symbol = SymbolOpBuilder::new(context)
                .attr("name", AttributeValue::Str(name))
                .attr("binding", AttributeValue::Str("global".to_string()))
                .attr("kind", AttributeValue::Str("object".to_string()))
                .attr("align", AttributeValue::UInt(align))
                .build();
            let mut symbol_builder = IRBuilder::new(symbol.body());
            symbol_builder.insert(
                LiteralOpBuilder::new(context)
                    .attr("kind", AttributeValue::Str("space".to_string()))
                    .attr("value", AttributeValue::Int(size as i64))
                    .build(),
            );
            symbol_builder.insert(SymbolEndOpBuilder::new(context).build());
            section_builder.insert(symbol);
        }
        section_builder.insert(SectionEndOpBuilder::new(context).build());
        let end = module_body.op_ids().len().saturating_sub(1);
        module_body.insert(end, section.id());
    }

    if strings.is_empty() {
        return Ok(());
    }

    let section = SectionOpBuilder::new(context)
        .attr("name", AttributeValue::Str(".rodata".to_string()))
        .build();
    let mut section_builder = IRBuilder::new(section.body());
    for (label, value) in strings {
        let symbol = SymbolOpBuilder::new(context)
            .attr("name", AttributeValue::Str(label))
            .attr("binding", AttributeValue::Str("local".to_string()))
            .attr("kind", AttributeValue::Str("object".to_string()))
            .build();
        let mut symbol_builder = IRBuilder::new(symbol.body());
        symbol_builder.insert(
            LiteralOpBuilder::new(context)
                .attr("kind", AttributeValue::Str("asciz".to_string()))
                .attr("value", AttributeValue::Str(value))
                .build(),
        );
        symbol_builder.insert(SymbolEndOpBuilder::new(context).build());
        section_builder.insert(symbol);
    }
    section_builder.insert(SectionEndOpBuilder::new(context).build());

    // Splice the section in ahead of the module terminator.
    let end = module_body.op_ids().len().saturating_sub(1);
    module_body.insert(end, section.id());
    Ok(())
}
