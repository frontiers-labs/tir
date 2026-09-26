//! Lowers the C [`crate::ast`] to TIR using the `builtin` and `ptr` dialects.
//!
//! The lowering is intentionally memory-based (the unoptimised, "no memory
//! SSA" shape a C frontend emits before any promotion pass): every parameter and
//! local lives in a stack slot produced by `ptr.alloca`, reads become
//! `ptr.load` and writes become `ptr.store`. Arithmetic uses the `builtin`
//! integer ops; C-only literals use the local `cir` dialect.
//!
//! Loops are emitted as `cir` loop ops, which keep the source shape — condition,
//! step and body in regions of their own — for the `raise-loops` pass to read.
//! Everything else is a flat graph of blocks and branches for `restructure` to
//! raise, and so is a whole function holding a label or a `return` under a loop:
//! both name edges a loop region cannot carry.

use std::collections::{BTreeMap, HashMap, HashSet};

use tir::attributes::AttributeValue;
use tir::backend::abi::{ValueKind, type_kind};
use tir::builtin::{FloatType, FnType, IntegerType, ModuleOp, TupleType, UnitType, ops as b};
use tir::func::ops as func_ops;
use tir::graph::{Dag, NodeId};
use tir::ptr::PtrType;
use tir::vector::VectorType;
use tir::{Context, Operation, TypeId, ValueId};

use crate::ast::*;
use crate::cir::{self, StructType};
use crate::diagnostics::{Diagnostic, EmptyTranslationUnit, UnsupportedConstruct};
use crate::lexer::decode_c_escapes;
use crate::sema::{EntityId, QualType, TypeKind, TypedAst};

mod abi;
mod calls;
mod control;
mod data;
mod expressions;
mod initializers;
mod scalar;
mod varargs;

pub use data::lower_data;

use abi::{
    AbiParameter, AbiPiece, AbiReturn, Signature, abi_storage_layout, classify_function_type,
    lower_signature,
};
use initializers::constant_initializer_data;
use varargs::VarargsEntry;

/// Where a `break` or a `continue` leaves the innermost construct that owns it.
#[derive(Clone)]
enum ExitTarget {
    /// A block of the region being emitted: a `switch` break, or any exit of a
    /// function lowered flat.
    Block(tir::BlockHandle),
    /// The enclosing `cir` loop op, left through `cir.break` or `cir.continue`.
    Loop,
}

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

/// The λ and δ values a function body names before the module holds their
/// definitions. Each becomes a placeholder, bound once the whole module is in.
#[derive(Default)]
struct Symbols {
    order: Vec<(String, ValueId)>,
    by_name: HashMap<String, ValueId>,
}

impl Symbols {
    /// The address of the data object named `name`.
    fn data(&mut self, context: &Context, name: &str) -> ValueId {
        self.value(context, name, PtrType::opaque(context))
    }

    /// The λ value of the function named `name`.
    fn function(&mut self, context: &Context, name: &str, signature: &Signature) -> ValueId {
        let ty = FnType::new(
            context,
            &signature.argument_types(context),
            signature.ret.ty,
        );
        self.value(context, name, ty)
    }

    fn value(&mut self, context: &Context, name: &str, ty: TypeId) -> ValueId {
        if let Some(value) = self.by_name.get(name) {
            return *value;
        }
        let value = context.create_value(ty, None).id();
        self.by_name.insert(name.to_string(), value);
        self.order.push((name.to_string(), value));
        value
    }

    /// Point every placeholder at the definition it names, declaring whatever
    /// this unit only references.
    fn bind(self, context: &Context, module: &ModuleOp) {
        let mut defined = HashMap::new();
        for op in module.body().iter(context.clone()) {
            let Some(symbol) = op.clone().as_interface::<dyn tir::Symbol>() else {
                continue;
            };
            if let Some(&result) = op.results().first() {
                defined.insert(symbol.symbol_name(), result);
            }
        }
        let mut bindings = HashMap::new();
        for (name, placeholder) in self.order {
            if let Some(&value) = defined.get(&name) {
                bindings.insert(placeholder, value);
                continue;
            }
            let declaration = match FnType::signature_of(context, placeholder) {
                Some((params, ret)) => func_ops::declare_op(context, &name, ret, &params).id(),
                None => b::global_external(context, &name).build().id(),
            };
            bindings.insert(placeholder, context.get_op(declaration).results()[0]);
            module.body().append(declaration);
        }
        for (&old, &new) in &bindings {
            context.replace_value_uses(old, new);
        }
    }
}

struct FnCodegen<'a> {
    context: &'a Context,
    symbols: &'a mut Symbols,
    typed: &'a TypedAst,
    ast: &'a Ast,
    builder: tir::BlockHandle,
    locals: HashMap<EntityId, Slot>,
    globals: &'a HashMap<EntityId, Global>,
    /// The `.rodata` symbol each string literal of the unit is stored under.
    strings: &'a BTreeMap<String, String>,
    signatures: &'a HashMap<EntityId, Signature>,
    return_abi: &'a AbiReturn,
    indirect_return: Option<ValueId>,
    /// `main` alone returns 0 when control reaches its closing brace.
    is_main: bool,
    terminated: bool,
    return_slot: Option<Slot>,
    /// The type a `return` converts its value to, for a function returning one.
    result_type: Option<QualType>,
    /// The region the lowering appends blocks to: the function body, or a region
    /// of the `cir` loop op currently being emitted.
    region: tir::RegionId,
    /// The one block every `return` leaves through.
    exit_block: Option<tir::BlockHandle>,
    /// The block each label names, created the first time it is mentioned.
    label_blocks: HashMap<String, tir::BlockHandle>,
    /// Whether this function's loops become `cir` loop ops.
    structured_loops: bool,
    /// Where a `break` and a `continue` leave the innermost construct that owns
    /// them.
    break_targets: Vec<ExitTarget>,
    continue_targets: Vec<ExitTarget>,
    /// Lowered values in the expression subtree currently being emitted. The AST
    /// is a DAG, so shared children reuse their first lowering.
    values: HashMap<NodeId, LoweredExpr>,
    varargs: Option<VarargsEntry>,
}

pub fn codegen(context: &Context, typed: &TypedAst) -> Result<ModuleOp, Diagnostic> {
    let ast = typed.ast();
    let module = b::module(context, None).build();

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
    let mut symbols = Symbols::default();
    let mut globals = HashMap::new();
    let mut global_strings = BTreeMap::new();
    let mut defined_functions = HashSet::new();
    let mut declared_functions = HashSet::new();
    let mut internal_functions = HashSet::new();
    let mut internal_globals = HashSet::new();
    // Entities already given storage: an initialized definition claims the
    // object outright, and repeated tentative definitions reserve it once.
    let mut reserved_globals = HashSet::new();
    // Objects this unit gives storage to, and those it only names.
    let mut defined_globals = HashSet::new();
    let mut declared_globals = HashSet::new();
    // Every string literal of the unit is one `.rodata` symbol, whether an
    // initializer or a function body names it.
    for &item in &items {
        for node in ast.preorder(item) {
            let Some(AstLeaf::String(value)) = ast.get_leaf_data(node) else {
                continue;
            };
            let next = global_strings.len();
            global_strings
                .entry(value.clone())
                .or_insert_with(|| format!(".L.str{next}"));
        }
    }
    for &item in &items {
        match ast.get_node(item).kind {
            AstKind::Prototype | AstKind::Function => {
                let (entity, sig) = lower_signature(context, typed, item)?;
                if ast.get_node(item).kind == AstKind::Function {
                    defined_functions.insert(entity);
                }
                if let Some(AstLeaf::Function {
                    is_static: true, ..
                }) = ast.get_leaf_data(item)
                {
                    internal_functions.insert(entity);
                }
                signatures.insert(entity, sig);
            }
            AstKind::Global => {
                let AstLeaf::Global {
                    name,
                    is_extern,
                    is_static,
                    ..
                } = ast.get_leaf_data(item).unwrap()
                else {
                    unreachable!("global node carries a global payload");
                };
                if ast.children(item).next().is_some() {
                    reserved_globals.insert(node_entity(typed, item));
                }
                if !*is_extern || ast.children(item).next().is_some() {
                    defined_globals.insert(node_entity(typed, item));
                }
                if *is_static {
                    internal_globals.insert(node_entity(typed, item));
                }
                globals.insert(
                    node_entity(typed, item),
                    Global {
                        name: name.clone(),
                        elem: lower_type(context, typed, node_type(typed, item)),
                    },
                );
            }
            AstKind::RecordDecl | AstKind::EnumDecl | AstKind::Typedef | AstKind::Attribute => {}
            _ => return Err(unsupported(ast, item, "top-level item".to_string())),
        }
    }

    let static_locals = collect_static_locals(context, typed, &items, &mut globals);

    for record in typed.records() {
        let fields = record
            .fields
            .iter()
            .map(|field| {
                let attributes = BTreeMap::from([
                    (
                        "name".to_string(),
                        AttributeValue::Str(field.name.clone().into()),
                    ),
                    (
                        "type".to_string(),
                        AttributeValue::Type(lower_type(context, typed, field.ty)),
                    ),
                    ("offset".to_string(), AttributeValue::UInt(field.offset)),
                ]);
                AttributeValue::Dict(Box::new(attributes))
            })
            .collect();
        module.body().append_op(
            cir::DefineStructOpBuilder::new(context)
                .sym_name(record.name.clone())
                .attr("fields", AttributeValue::Array(fields))
                .size(record.size)
                .align(record.align)
                .build(),
        );
    }

    for (value, name) in &global_strings {
        let mut bytes = decode_c_escapes(value).into_bytes();
        bytes.push(0);
        module.body().append_op(
            b::global_bytes(context, name, bytes, 1)
                .attr(
                    "sym_visibility",
                    AttributeValue::Str("private".to_string().into()),
                )
                .attr("section", AttributeValue::Str(".rodata".to_string().into()))
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
                module.body().append_op(func_ops::declare_op(
                    context,
                    name,
                    sig.ret.ty,
                    &sig.argument_types(context),
                ));
            }
            AstKind::Function => {
                let func_op = lower_function(
                    context,
                    typed,
                    item,
                    &signatures,
                    &globals,
                    &global_strings,
                    &mut symbols,
                )?;
                if internal_functions.contains(&node_entity(typed, item)) {
                    let mut attributes = func_op.attributes().to_vec();
                    attributes.push(context.named_attribute(
                        "sym_visibility",
                        AttributeValue::Str("private".to_string().into()),
                    ));
                    context.set_op_attributes(func_op.id(), attributes);
                }
                module.body().append_op(func_op);
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
                    // An object this unit never defines is declared instead:
                    // references to it must still name a symbol of the module.
                    if *is_extern
                        && !defined_globals.contains(&entity)
                        && declared_globals.insert(entity)
                    {
                        module
                            .body()
                            .append_op(b::global_external(context, &global.name).build());
                        continue;
                    }
                    // A tentative definition reserves storage only when no
                    // other declaration of the object defines it.
                    if !is_extern && reserved_globals.insert(entity) {
                        let mut definition = b::global_zero(context, &global.name, size, align);
                        if internal_globals.contains(&entity) {
                            definition = definition.attr(
                                "sym_visibility",
                                AttributeValue::Str("private".to_string().into()),
                            );
                        }
                        module.body().append_op(definition.build());
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
                let mut definition = b::global_bytes(context, &global.name, data.bytes, align);
                if !data.relocations.is_empty() {
                    definition = definition.attr(
                        "relocations",
                        AttributeValue::Array(
                            data.relocations
                                .into_iter()
                                .map(|relocation| {
                                    AttributeValue::Dict(Box::new(BTreeMap::from([
                                        (
                                            "offset".to_string(),
                                            AttributeValue::UInt(relocation.offset),
                                        ),
                                        (
                                            "symbol".to_string(),
                                            AttributeValue::Str(relocation.symbol.into()),
                                        ),
                                        (
                                            "addend".to_string(),
                                            AttributeValue::Int(relocation.addend),
                                        ),
                                        (
                                            "width".to_string(),
                                            AttributeValue::UInt(relocation.width),
                                        ),
                                    ])))
                                })
                                .collect::<Vec<_>>()
                                .into(),
                        ),
                    );
                }
                if internal_globals.contains(&entity) {
                    definition = definition.attr(
                        "sym_visibility",
                        AttributeValue::Str("private".to_string().into()),
                    );
                }
                module.body().append_op(definition.build());
            }
            AstKind::RecordDecl | AstKind::EnumDecl | AstKind::Typedef | AstKind::Attribute => {}
            _ => unreachable!("top-level item was checked before emission"),
        }
    }
    emit_static_locals(
        context,
        typed,
        &module,
        &globals,
        &global_strings,
        static_locals,
    )?;
    symbols.bind(context, &module);
    module.body().append_op(b::module_end(context).build());
    Ok(module)
}

fn collect_static_locals(
    context: &Context,
    typed: &TypedAst,
    items: &[NodeId],
    globals: &mut HashMap<EntityId, Global>,
) -> Vec<NodeId> {
    let ast = typed.ast();
    let mut static_locals = Vec::new();
    for &item in items {
        if ast.get_node(item).kind != AstKind::Function {
            continue;
        }
        for node in ast.preorder(item) {
            if !matches!(
                ast.get_leaf_data(node),
                Some(AstLeaf::Decl {
                    is_static: true,
                    ..
                })
            ) {
                continue;
            }
            let entity = node_entity(typed, node);
            globals.insert(
                entity,
                Global {
                    name: format!(".L.fcc.static.{}", static_locals.len()),
                    elem: lower_type(context, typed, node_type(typed, node)),
                },
            );
            static_locals.push(node);
        }
    }
    static_locals
}

fn emit_static_locals(
    context: &Context,
    typed: &TypedAst,
    module: &ModuleOp,
    globals: &HashMap<EntityId, Global>,
    global_strings: &BTreeMap<String, String>,
    static_locals: Vec<NodeId>,
) -> Result<(), Diagnostic> {
    let ast = typed.ast();
    for node in static_locals {
        let source_ty = node_type(typed, node);
        let (size, align) = source_type_layout(typed, source_ty);
        let global = &globals[&node_entity(typed, node)];
        let Some(initializer) = ast.children(node).next() else {
            module.body().append_op(
                b::global_zero(context, &global.name, size, align)
                    .attr(
                        "sym_visibility",
                        AttributeValue::Str("private".to_string().into()),
                    )
                    .build(),
            );
            continue;
        };
        let Some(data) =
            constant_initializer_data(typed, globals, global_strings, source_ty, initializer)
        else {
            return Err(unsupported(
                ast,
                initializer,
                "non-constant static initializer".to_string(),
            ));
        };
        let mut definition = b::global_bytes(context, &global.name, data.bytes, align).attr(
            "sym_visibility",
            AttributeValue::Str("private".to_string().into()),
        );
        if !data.relocations.is_empty() {
            definition = definition.attr(
                "relocations",
                AttributeValue::Array(
                    data.relocations
                        .into_iter()
                        .map(|relocation| {
                            AttributeValue::Dict(Box::new(BTreeMap::from([
                                (
                                    "offset".to_string(),
                                    AttributeValue::UInt(relocation.offset),
                                ),
                                (
                                    "symbol".to_string(),
                                    AttributeValue::Str(relocation.symbol.into()),
                                ),
                                ("addend".to_string(), AttributeValue::Int(relocation.addend)),
                                ("width".to_string(), AttributeValue::UInt(relocation.width)),
                            ])))
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ),
            );
        }
        module.body().append_op(definition.build());
    }
    Ok(())
}

/// A construct the parser accepts but codegen does not lower yet.
fn unsupported(ast: &Ast, node: NodeId, what: String) -> Diagnostic {
    UnsupportedConstruct::new(ast.get_node(node).span, what).into()
}

fn lower_type(context: &Context, typed: &TypedAst, ty: QualType) -> TypeId {
    match typed.types().kind(ty) {
        TypeKind::Void => UnitType::new(context),
        TypeKind::Integer(_) => IntegerType::new(context, typed.integer_width(ty).unwrap()),
        TypeKind::Pointer(_) | TypeKind::Array(_, _) => PtrType::opaque(context),
        TypeKind::Enum(_) => IntegerType::new(context, 32),
        TypeKind::Float => FloatType::f32(context),
        TypeKind::Double => FloatType::f64(context),
        TypeKind::ComplexFloat => VectorType::fixed(context, FloatType::f32(context), 2),
        TypeKind::ComplexDouble => TupleType::new(context, vec![FloatType::f64(context); 2]),
        TypeKind::ComplexLongDouble => IntegerType::new(context, 256),
        TypeKind::VaList => cir::VaListType::new(context),
        TypeKind::Error | TypeKind::LongDouble | TypeKind::Function { .. } => {
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

/// Whether `statement` holds an edge a `cir` loop region cannot carry: a label or
/// a `goto`, which name blocks anywhere in the function, or a `return` under a
/// loop, which leaves for the function's one exit block. One of them anywhere in a
/// function lowers all of its loops flat.
fn crosses_loop_boundary(ast: &Ast, statement: NodeId, in_loop: bool) -> bool {
    let kind = ast.get_node(statement).kind;
    match kind {
        AstKind::Goto | AstKind::Label => return true,
        AstKind::Return => return in_loop,
        _ => {}
    }
    let in_loop = in_loop || matches!(kind, AstKind::While | AstKind::DoWhile | AstKind::For);
    ast.children(statement)
        .any(|child| crosses_loop_boundary(ast, child, in_loop))
}

fn lower_function(
    context: &Context,
    typed: &TypedAst,
    func: NodeId,
    signatures: &HashMap<EntityId, Signature>,
    globals: &HashMap<EntityId, Global>,
    strings: &BTreeMap<String, String>,
    symbols: &mut Symbols,
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
    let named_arguments = param_values.len();
    let mut varargs = None;
    if ast
        .preorder(func)
        .any(|node| ast.get_node(node).kind == AstKind::VaStart)
    {
        if !signature.varargs || !typed.target().uses_sysv_abi() {
            return Err(unsupported(ast, func, "va_start target ABI".to_string()));
        }
        if signature.params.iter().any(|parameter| {
            parameter
                .pieces
                .iter()
                .any(|piece| type_kind(context, piece.ty) == ValueKind::Vector)
        }) {
            return Err(unsupported(
                ast,
                func,
                "variadic vector parameter".to_string(),
            ));
        }
        let usage = signature.register_usage;
        let gp_start = usage.integers;
        let fp_start = usage.floats;
        let mut gp = Vec::new();
        let mut fp = Vec::new();
        for _ in gp_start..typed.target().argument_registers(ValueKind::Int) {
            let value = context.create_value(IntegerType::new(context, 64), None);
            gp.push(value.id());
            param_values.push(value);
        }
        for _ in fp_start..typed.target().argument_registers(ValueKind::Float) {
            let value = context.create_value(FloatType::f64(context), None);
            fp.push(value.id());
            param_values.push(value);
        }
        let entry_sp = context.create_value(PtrType::opaque(context), None);
        param_values.push(entry_sp.clone());
        varargs = Some(VarargsEntry {
            gp_start,
            fp_start,
            stack_slots: usage.stack_slots,
            gp,
            fp,
            entry_sp: entry_sp.id(),
            register_area: None,
        });
    }
    let param_ids: Vec<ValueId> = param_values.iter().map(|v| v.id()).collect();

    let region = context.create_region();
    let block = context.create_block(param_values);
    region.add_block(block.id());

    let mut func_builder = func_ops::func(
        context,
        name.as_str(),
        signature.ret.ty,
        FnType::new(
            context,
            &signature.argument_types(context),
            signature.ret.ty,
        ),
        Some(region.id()),
    );
    if signature.ret.indirect {
        func_builder = func_builder.result_address();
    }
    if let Some(varargs) = &varargs {
        func_builder = func_builder
            .implicit_arguments(param_ids.len() - named_arguments)
            .entry_sp(varargs.entry_sp);
    }
    let argument_alignments = signature.argument_alignments();
    if argument_alignments.iter().any(|&alignment| alignment > 1) {
        func_builder = func_builder.argument_alignments(&argument_alignments);
    }
    let noalias = signature.noalias_arguments();
    if !noalias.is_empty() {
        func_builder = func_builder.noalias(&noalias);
    }
    let func_op = func_builder.build();
    let indirect_return = signature.ret.indirect.then(|| param_ids[0]);
    let parameter_start = usize::from(signature.ret.indirect);

    let mut cg = FnCodegen {
        context,
        symbols,
        typed,
        ast,
        builder: func_op.body(),
        locals: HashMap::new(),
        globals,
        strings,
        signatures,
        return_abi: &signature.ret,
        indirect_return,
        is_main: name == "main",
        terminated: false,
        return_slot: None,
        result_type: None,
        region: region.id(),
        exit_block: None,
        label_blocks: HashMap::new(),
        structured_loops: false,
        break_targets: Vec::new(),
        continue_targets: Vec::new(),
        values: HashMap::new(),
        varargs,
    };
    cg.init_varargs();
    cg.lower_body(
        func,
        &param_ids[parameter_start..named_arguments],
        &signature.params,
    )?;

    Ok(func_op)
}
