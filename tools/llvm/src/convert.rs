//! Lower the [`ast`] into TIR, targeting the `builtin` and `ptr` dialects.
//! Instructions the parser flagged as [`ast::Inst::Unsupported`] — and any
//! construct with no TIR equivalent today — produce an error rather than a
//! silent drop.

use std::collections::HashMap;
use std::sync::Arc;
use tir::BlockHandle;

use tir::attributes::{AttributeValue, Predicate};
use tir::builtin::{self, FloatType, FnType, IntegerType, UnitType, VarArgsType, ops as bops};
use tir::cfg::ops as cbops;
use tir::fp::{
    ArithmeticSemantics, ComparisonBehavior, ComparisonSemantics, Exceptions,
    IntegerConversionSemantics, InvalidConversion, Rounding, RoundingMode, SubnormalMode,
    ops as fp,
};
use tir::func::ops as func_ops;
use tir::ptr::{PtrType, ops as pops};
use tir::{Context, Operand, Operation, Symbol, TypeId, ValueId};

use crate::ast::{self, BinOp, CastOp, Inst, Type};
use crate::error::Error;

/// Lower an LLVM module using the selected target's data layout when supplied.
/// Without a target layout, pointers retain the importer's 64-bit default.
pub fn import(
    context: &Context,
    module: &ast::Module,
    data_layout: Option<&AttributeValue>,
) -> Result<builtin::ModuleOp, Error> {
    let m = bops::module(context, None, Default::default(), tir::InsertionPoint::Root);
    let builder = m.body();
    let mut callees = Callees::default();
    let layout = Layout {
        named: module.named_types.iter().cloned().collect(),
        pointer_width: data_layout
            .and_then(|layout| tir::DataLayout::from_value(context, layout))
            .and_then(|layout| layout.pointer_size())
            .unwrap_or(64),
    };
    if let Some(spec) = data_layout {
        context.set_op_attributes(
            m.id(),
            vec![context.named_attribute(tir::LAYOUT_SPEC, spec.clone())],
        );
    }
    let globals = lower_globals(context, module, &builder, &layout)?;
    let mut function_types: HashMap<_, _> = module
        .functions
        .iter()
        .map(|function| {
            (
                function.name.clone(),
                (
                    function
                        .params
                        .iter()
                        .map(|param| param.ty.clone())
                        .collect(),
                    function.ret.clone(),
                    false,
                ),
            )
        })
        .collect();
    function_types.extend(module.declarations.iter().map(|declaration| {
        (
            declaration.name.clone(),
            (
                declaration.params.clone(),
                declaration.ret.clone(),
                declaration.variadic,
            ),
        )
    }));
    for func in &module.functions {
        lower_function(
            context,
            func,
            &mut callees,
            &globals,
            &layout,
            &function_types,
            builder.end(),
        )?;
    }
    let bindings = callees.bind(context, &m, &builder);
    bops::module_end(context, Default::default(), builder.end());
    for (&old, &new) in &bindings {
        context.replace_value_uses(old, new);
    }
    Ok(m)
}

/// The λ values calls name before their definitions are in the module. LLVM has
/// no overloads, so a name identifies a function outright.
#[derive(Default)]
struct Callees {
    placeholders: Vec<(String, ValueId)>,
    by_name: HashMap<String, ValueId>,
}

impl Callees {
    fn value(&mut self, context: &Context, name: &str, args: &[TypeId], ret: TypeId) -> ValueId {
        if let Some(value) = self.by_name.get(name) {
            return *value;
        }
        let value = context
            .create_value(FnType::new(context, args, ret), None)
            .id();
        self.by_name.insert(name.to_string(), value);
        self.placeholders.push((name.to_string(), value));
        value
    }

    fn bind(
        self,
        context: &Context,
        module: &builtin::ModuleOp,
        body: &BlockHandle,
    ) -> HashMap<ValueId, ValueId> {
        let mut defined = HashMap::new();
        for op in module.body().iter(context.clone()) {
            if let Some(func) = op.as_op::<tir::func::FuncOp>() {
                defined.insert(func.symbol_name(), func.fn_value());
            }
        }
        self.placeholders
            .into_iter()
            .map(|(name, placeholder)| {
                if let Some(value) = defined.get(&name) {
                    return (placeholder, *value);
                }
                let signature = FnType::view(context, context.get_value(placeholder).ty())
                    .expect("a callee placeholder has a function type");
                let declaration = tir::func::DeclareOp::new(
                    context,
                    AttributeValue::Str(name.into()),
                    FnType::new(context, &signature.params(), signature.ret()),
                    Default::default(),
                    body.end(),
                );
                (placeholder, declaration.fn_value())
            })
            .collect()
    }
}

fn lower_type(context: &Context, ty: &Type) -> Result<TypeId, Error> {
    Ok(match ty {
        Type::Int(width) => IntegerType::new(context, u64::from(*width)),
        Type::Void => UnitType::new(context),
        Type::Ptr(None) => PtrType::opaque(context),
        Type::Ptr(Some(pointee)) => PtrType::typed(context, lower_type(context, pointee)?),
        Type::Float(16) => FloatType::f16(context),
        Type::Float(32) => FloatType::f32(context),
        Type::Float(64) => FloatType::f64(context),
        Type::Float(width) => {
            return Err(Error::Unsupported(format!("LLVM float width {width}")));
        }
        Type::Array(_, _) => return Err(Error::Unsupported("LLVM array value".into())),
        Type::Named(name) => {
            return Err(Error::Unsupported(format!(
                "LLVM named aggregate value %{name}"
            )));
        }
        Type::Struct(_) => return Err(Error::Unsupported("LLVM struct value".into())),
    })
}

fn lower_globals(
    context: &Context,
    module: &ast::Module,
    body: &BlockHandle,
    layout: &Layout,
) -> Result<HashMap<String, ValueId>, Error> {
    let mut values = HashMap::new();
    for global in &module.globals {
        let size = type_size(&global.ty, layout)?;
        let mut parts = tir::NewOpParts::default();
        if global.private {
            parts.attributes.push(
                context.named_attribute("sym_visibility", AttributeValue::Str("private".into())),
            );
        }
        if global.constant {
            parts
                .attributes
                .push(context.named_attribute("section", AttributeValue::Str(".rodata".into())));
        }
        let op = match &global.initializer {
            ast::GlobalInitializer::External => {
                bops::global_external(context, &global.name, parts, body.end())
            }
            ast::GlobalInitializer::Zero | ast::GlobalInitializer::Null => {
                bops::global_zero(context, &global.name, size, global.align, parts, body.end())
            }
            ast::GlobalInitializer::Integer(value) => {
                let mut bytes = vec![0; size as usize];
                let raw = value.to_le_bytes();
                let copied = bytes.len().min(raw.len());
                bytes[..copied].copy_from_slice(&raw[..copied]);
                bops::global_bytes(
                    context,
                    &global.name,
                    bytes,
                    global.align,
                    parts,
                    body.end(),
                )
            }
            ast::GlobalInitializer::CString(bytes) | ast::GlobalInitializer::Bytes(bytes) => {
                validate_initializer_size(&global.name, size, bytes.len())?;
                bops::global_bytes(
                    context,
                    &global.name,
                    bytes.clone(),
                    global.align,
                    parts,
                    body.end(),
                )
            }
            ast::GlobalInitializer::Symbols(symbols) => {
                let pointer_bytes = u64::from(layout.pointer_width / 8);
                let bytes = vec![0; size as usize];
                let relocations = symbols
                    .iter()
                    .enumerate()
                    .map(|(index, symbol)| {
                        AttributeValue::Dict(Box::new(std::collections::BTreeMap::from([
                            (
                                "offset".into(),
                                AttributeValue::UInt(index as u64 * pointer_bytes),
                            ),
                            ("symbol".into(), AttributeValue::Str(symbol.clone().into())),
                            ("addend".into(), AttributeValue::Int(0)),
                            ("width".into(), AttributeValue::UInt(pointer_bytes)),
                        ])))
                    })
                    .collect::<Vec<_>>();
                parts.attributes.insert(
                    0,
                    context
                        .named_attribute("relocations", AttributeValue::Array(relocations.into())),
                );
                bops::global_bytes(
                    context,
                    &global.name,
                    bytes,
                    global.align,
                    parts,
                    body.end(),
                )
            }
            ast::GlobalInitializer::SymbolDifferences(differences) => {
                let entries = differences
                    .iter()
                    .enumerate()
                    .map(|(index, difference)| {
                        AttributeValue::Dict(Box::new(std::collections::BTreeMap::from([
                            ("offset".into(), AttributeValue::UInt(index as u64 * 4)),
                            (
                                "symbol".into(),
                                AttributeValue::Str(difference.symbol.clone().into()),
                            ),
                            (
                                "base".into(),
                                AttributeValue::Str(difference.base.clone().into()),
                            ),
                            ("width".into(), AttributeValue::UInt(4)),
                        ])))
                    })
                    .collect::<Vec<_>>();
                parts.attributes.insert(
                    0,
                    context.named_attribute(
                        "symbol_differences",
                        AttributeValue::Array(entries.into()),
                    ),
                );
                bops::global_bytes(
                    context,
                    &global.name,
                    vec![0; size as usize],
                    global.align,
                    parts,
                    body.end(),
                )
            }
        };
        values.insert(global.name.clone(), op.address());
    }
    Ok(values)
}

fn validate_initializer_size(name: &str, expected: u64, actual: usize) -> Result<(), Error> {
    if u64::try_from(actual) != Ok(expected) {
        return Err(Error::Parse(format!(
            "global @{name} initializer has {actual} bytes, expected {expected}"
        )));
    }
    Ok(())
}

struct Layout {
    named: HashMap<String, Type>,
    pointer_width: u32,
}

fn type_size(ty: &Type, layout: &Layout) -> Result<u64, Error> {
    Ok(match ty {
        Type::Int(width) | Type::Float(width) => u64::from(width.div_ceil(8)),
        Type::Ptr(_) => u64::from(layout.pointer_width / 8),
        Type::Array(count, elem) => count * type_size(elem, layout)?,
        Type::Named(name) => type_size(
            layout
                .named
                .get(name)
                .ok_or_else(|| Error::Parse(format!("undefined type %{name}")))?,
            layout,
        )?,
        Type::Struct(fields) => struct_layout(fields, layout)?.0,
        Type::Void => 0,
    })
}

fn type_align(ty: &Type, layout: &Layout) -> Result<u64, Error> {
    Ok(match ty {
        Type::Array(_, elem) => type_align(elem, layout)?,
        Type::Named(name) => type_align(
            layout
                .named
                .get(name)
                .ok_or_else(|| Error::Parse(format!("undefined type %{name}")))?,
            layout,
        )?,
        Type::Struct(fields) => struct_layout(fields, layout)?.1,
        _ => type_size(ty, layout)?.clamp(1, 8),
    })
}

fn struct_layout(fields: &[Type], layout: &Layout) -> Result<(u64, u64), Error> {
    let mut size: u64 = 0;
    let mut max_align = 1;
    for field in fields {
        let align = type_align(field, layout)?;
        size = size.div_ceil(align) * align;
        size += type_size(field, layout)?;
        max_align = max_align.max(align);
    }
    Ok((size.div_ceil(max_align) * max_align, max_align))
}

fn lower_function(
    context: &Context,
    func: &ast::Function,
    callees: &mut Callees,
    globals: &HashMap<String, ValueId>,
    layout: &Layout,
    function_types: &HashMap<String, (Vec<Type>, Type, bool)>,
    insertion: tir::InsertionPoint,
) -> Result<tir::func::FuncOp, Error> {
    // The function's rows share chunks with no other function's.

    let region = context.create_region();
    let mut values: HashMap<String, ValueId> = HashMap::new();

    // Parameters become entry-block arguments.
    let mut entry_args = Vec::new();
    for param in &func.params {
        let value = context.create_value(lower_type(context, &param.ty)?, None);
        values.insert(param.name.clone(), value.id());
        entry_args.push(value);
    }
    for block in &func.blocks {
        for inst in &block.insts {
            if let Inst::Phi { ty, incoming, .. } = inst {
                for (operand, _) in incoming {
                    if let ast::Operand::Ref(name) = operand {
                        let Some(name) = (!values.contains_key(name)).then_some(name) else {
                            continue;
                        };
                        values.insert(
                            name.clone(),
                            context.create_value(lower_type(context, ty)?, None).id(),
                        );
                    }
                }
            }
        }
    }

    // Pre-create every block so branches can resolve targets by label.
    let mut blocks: Vec<BlockHandle> = Vec::new();
    let mut by_label: HashMap<String, BlockHandle> = HashMap::new();
    for (i, block) in func.blocks.iter().enumerate() {
        let mut args = if i == 0 {
            std::mem::take(&mut entry_args)
        } else {
            Vec::new()
        };
        for inst in &block.insts {
            if let Inst::Phi { result, ty, .. } = inst {
                let value = context.create_value(lower_type(context, ty)?, None);
                if let Some(old) = values.insert(result.clone(), value.id()) {
                    context.replace_value_uses(old, value.id());
                }
                args.push(value);
            }
        }
        let created = context.create_block(args);
        region.add_block(created.id());
        if let Some(label) = &block.label {
            by_label.insert(label.clone(), created.clone());
        }
        blocks.push(created);
    }

    let ret_ty = lower_type(context, &func.ret)?;
    let parameters: Vec<_> = func
        .params
        .iter()
        .map(|param| lower_type(context, &param.ty))
        .collect::<Result<_, _>>()?;
    let op = func_ops::func(
        context,
        func.name.as_str(),
        ret_ty,
        FnType::new(context, &parameters, ret_ty),
        Some(region.id()),
        Default::default(),
        insertion,
    );

    let phis: HashMap<_, _> = func
        .blocks
        .iter()
        .filter_map(|block| {
            block
                .label
                .as_ref()
                .map(|label| (label.clone(), &block.insts))
        })
        .collect();
    let definitions: HashMap<_, _> = func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| inst_result_name(inst).map(|name| (name, inst)))
        .collect();
    let implicit_entry_label = func
        .params
        .iter()
        .filter_map(|param| param.name.parse::<u64>().ok())
        .max()
        .map_or(0, |value| value + 1)
        .to_string();
    for (index, (block, created)) in func.blocks.iter().zip(blocks.iter()).enumerate() {
        let builder = created.clone();
        let current_label = block.label.clone().unwrap_or_else(|| {
            if index == 0 {
                implicit_entry_label.clone()
            } else {
                index.to_string()
            }
        });
        for inst in &block.insts {
            let result_name = inst_result_name(inst);
            let old_result = result_name.and_then(|name| values.get(name).copied());
            lower_inst(
                context,
                inst,
                &builder,
                &mut values,
                &by_label,
                callees,
                globals,
                layout,
                function_types,
                &current_label,
                &phis,
                &definitions,
            )?;
            if let Some((old, new)) = old_result
                .zip(result_name.and_then(|name| values.get(name).copied()))
                .filter(|(old, new)| old != new)
            {
                context.replace_value_uses(old, new);
            }
        }
    }

    Ok(op)
}

fn inst_result_name(inst: &Inst) -> Option<&str> {
    match inst {
        Inst::Binary { result, .. }
        | Inst::FNeg { result, .. }
        | Inst::ICmp { result, .. }
        | Inst::FCmp { result, .. }
        | Inst::Cast { result, .. }
        | Inst::Alloca { result, .. }
        | Inst::Load { result, .. }
        | Inst::GetElementPtr { result, .. }
        | Inst::Phi { result, .. }
        | Inst::Select { result, .. } => Some(result),
        Inst::Call { result, .. } => result.as_deref(),
        _ => None,
    }
}

fn widening_no_wrap<'a>(
    cast: &Inst,
    definitions: &HashMap<&str, &'a Inst>,
    values: &HashMap<String, ValueId>,
) -> Option<(BinOp, &'a ast::Operand, &'a ast::Operand)> {
    let Inst::Cast {
        op: CastOp::SExt,
        from: Type::Int(source_width),
        to: Type::Int(destination_width),
        value: ast::Operand::Ref(name),
        ..
    } = cast
    else {
        return None;
    };
    if source_width >= destination_width {
        return None;
    }
    let Inst::Binary {
        op: op @ (BinOp::Add | BinOp::Sub),
        no_signed_wrap: true,
        ty: Type::Int(width),
        lhs,
        rhs,
        ..
    } = *definitions.get(name.as_str())?
    else {
        return None;
    };
    if width != source_width {
        return None;
    }
    let variable = match (lhs, rhs) {
        (ast::Operand::Ref(name), ast::Operand::ConstInt(_))
        | (ast::Operand::ConstInt(_), ast::Operand::Ref(name)) => name,
        _ => return None,
    };
    values.contains_key(variable).then_some((*op, lhs, rhs))
}

fn affine_no_wrap_index<'a>(
    index_type: &Type,
    index: &ast::Operand,
    definitions: &HashMap<&str, &'a Inst>,
    values: &HashMap<String, ValueId>,
) -> Option<(&'a Type, &'a ast::Operand, i64)> {
    let (Type::Int(64), ast::Operand::Ref(name)) = (index_type, index) else {
        return None;
    };
    let cast = *definitions.get(name.as_str())?;
    let Inst::Cast {
        from: source @ Type::Int(width),
        to,
        ..
    } = cast
    else {
        return None;
    };
    if to != index_type || *width == 0 || *width >= 64 {
        return None;
    }
    let (BinOp::Add, lhs, rhs) = widening_no_wrap(cast, definitions, values)? else {
        return None;
    };
    let (variable, constant) = match (lhs, rhs) {
        (variable @ ast::Operand::Ref(_), ast::Operand::ConstInt(constant))
        | (ast::Operand::ConstInt(constant), variable @ ast::Operand::Ref(_)) => {
            (variable, constant)
        }
        _ => return None,
    };
    Some((source, variable, gep_index(*constant, *width)))
}

#[allow(clippy::too_many_arguments)]
fn lower_operand(
    context: &Context,
    body: &BlockHandle,
    operand: &ast::Operand,
    ty: &Type,
    values: &HashMap<String, ValueId>,
    callees: &mut Callees,
    globals: &HashMap<String, ValueId>,
    layout: &Layout,
    function_types: &HashMap<String, (Vec<Type>, Type, bool)>,
    definitions: &HashMap<&str, &Inst>,
) -> Result<ValueId, Error> {
    Ok(match operand {
        ast::Operand::Ref(name) => *values
            .get(name)
            .ok_or_else(|| Error::UndefinedValue(name.clone()))?,
        ast::Operand::ConstInt(v) => match ty {
            Type::Float(32) => {
                let op = fp::ConstantOp::new(
                    context,
                    tir::attributes::AttributeValue::UInt((*v as f32).to_bits() as u64),
                    FloatType::f32(context),
                    Default::default(),
                    body.end(),
                );
                op.result()
            }
            Type::Float(64) => {
                let op = fp::ConstantOp::new(
                    context,
                    tir::attributes::AttributeValue::UInt((*v as f64).to_bits()),
                    FloatType::f64(context),
                    Default::default(),
                    body.end(),
                );
                op.result()
            }
            _ => {
                let c = bops::constant(
                    context,
                    *v,
                    lower_type(context, ty)?,
                    Default::default(),
                    body.end(),
                );
                c.result()
            }
        },
        ast::Operand::ConstFloat(value) => float_constant(context, body, *value, ty)?,
        ast::Operand::Global(name) => match globals.get(name) {
            Some(value) => *value,
            None => {
                let (params, ret, variadic) = function_types
                    .get(name)
                    .ok_or_else(|| Error::UndefinedValue(format!("@{name}")))?;
                let mut params = params
                    .iter()
                    .map(|ty| lower_type(context, ty))
                    .collect::<Result<Vec<_>, _>>()?;
                if *variadic {
                    params.push(VarArgsType::new(context));
                }
                let ret = lower_type(context, ret)?;
                let function = callees.value(context, name, &params, ret);
                let op = bops::fn_to_ptr(
                    context,
                    function,
                    PtrType::opaque(context),
                    Default::default(),
                    body.end(),
                );
                op.result()
            }
        },
        ast::Operand::Null => {
            let o = pops::null(
                context,
                PtrType::opaque(context),
                Default::default(),
                body.end(),
            );
            o.result()
        }
        ast::Operand::Undef => match ty {
            Type::Ptr(_) => {
                let op = pops::null(
                    context,
                    PtrType::opaque(context),
                    Default::default(),
                    body.end(),
                );
                op.result()
            }
            Type::Float(32) | Type::Float(64) => {
                let op = fp::ConstantOp::new(
                    context,
                    tir::attributes::AttributeValue::UInt(0),
                    lower_type(context, ty)?,
                    Default::default(),
                    body.end(),
                );
                op.result()
            }
            _ => constant(context, body, 0, lower_type(context, ty)?),
        },
        ast::Operand::GetElementPtr {
            source,
            base,
            indices,
        } => {
            let base = match base.as_ref() {
                ast::Operand::Ref(name) => *values
                    .get(name)
                    .ok_or_else(|| Error::UndefinedValue(name.clone()))?,
                ast::Operand::Global(name) => *globals
                    .get(name)
                    .ok_or_else(|| Error::UndefinedValue(format!("@{name}")))?,
                _ => return Err(Error::Unsupported("nested getelementptr base".into())),
            };
            lower_gep(
                context,
                body,
                base,
                source,
                indices,
                values,
                layout,
                definitions,
            )?
        }
    })
}

#[allow(clippy::cognitive_complexity, clippy::too_many_arguments)]
fn lower_inst(
    context: &Context,
    inst: &Inst,
    body: &BlockHandle,
    values: &mut HashMap<String, ValueId>,
    by_label: &HashMap<String, BlockHandle>,
    callees: &mut Callees,
    globals: &HashMap<String, ValueId>,
    layout: &Layout,
    function_types: &HashMap<String, (Vec<Type>, Type, bool)>,
    current_label: &str,
    phis: &HashMap<String, &Vec<Inst>>,
    definitions: &HashMap<&str, &Inst>,
) -> Result<(), Error> {
    macro_rules! val {
        ($operand:expr, $ty:expr) => {
            lower_operand(
                context,
                body,
                $operand,
                $ty,
                values,
                callees,
                globals,
                layout,
                function_types,
                definitions,
            )?
        };
    }
    match inst {
        Inst::Binary {
            result,
            op,
            ty,
            lhs,
            rhs,
            ..
        } => {
            let t = lower_type(context, ty)?;
            let l = val!(lhs, ty);
            let r = val!(rhs, ty);
            let id = lower_binary(context, body, *op, l, r, t);
            values.insert(result.clone(), id);
        }
        Inst::FNeg { result, ty, value } => {
            let input = val!(value, ty);
            let width = match ty {
                Type::Float(width @ (32 | 64)) => *width,
                _ => return Err(Error::Unsupported("fneg type".into())),
            };
            let bits_ty = IntegerType::new(context, u64::from(width));
            // LLVM fneg flips the sign bit, including for zero and NaN.
            let bits =
                bops::bitcast(context, input, bits_ty, Default::default(), body.end()).result();
            let sign = constant(context, body, 1_i64 << (width - 1), bits_ty);
            let negated =
                bops::xori(context, bits, sign, bits_ty, Default::default(), body.end()).result();
            let op = bops::bitcast(
                context,
                negated,
                lower_type(context, ty)?,
                Default::default(),
                body.end(),
            );
            values.insert(result.clone(), op.result());
        }
        Inst::ICmp {
            result,
            pred,
            ty,
            lhs,
            rhs,
        } => {
            let l = val!(lhs, ty);
            let r = val!(rhs, ty);
            let id = lower_icmp(context, body, pred, ty, l, r)?;
            values.insert(result.clone(), id);
        }
        Inst::FCmp {
            result,
            pred,
            ty,
            lhs,
            rhs,
        } => {
            let lhs = val!(lhs, ty);
            let rhs = val!(rhs, ty);
            let predicate = parse_predicate(
                if pred == "ult" { "oge" } else { pred },
                Predicate::FLOAT,
                "fcmp",
            )?;
            let op = fp::CmpOp::new(
                context,
                lhs,
                rhs,
                tir::attributes::AttributeValue::Predicate(predicate),
                tir::attributes::AttributeValue::FpSemantics(comparison_semantics(context)),
                IntegerType::new(context, 1),
                vec![],
                vec![],
                Default::default(),
                body.end(),
            );
            let comparison = op.result();

            let value = if pred == "ult" {
                let one = constant(context, body, 1, IntegerType::new(context, 1));
                bops::xori(
                    context,
                    comparison,
                    one,
                    IntegerType::new(context, 1),
                    Default::default(),
                    body.end(),
                )
                .result()
            } else {
                comparison
            };
            values.insert(result.clone(), value);
        }
        Inst::Cast {
            result,
            op,
            non_negative,
            from,
            value,
            to,
        } => {
            let destination = lower_type(context, to)?;
            let id = if let Some((binary, lhs, rhs)) = widening_no_wrap(inst, definitions, values) {
                // Lower literals at the source width first: a positive LLVM
                // spelling can denote a negative narrow bit pattern.
                let lhs = val!(lhs, from);
                let rhs = val!(rhs, from);
                let lhs = lower_cast(context, body, CastOp::SExt, false, lhs, destination, layout);
                let rhs = lower_cast(context, body, CastOp::SExt, false, rhs, destination, layout);
                lower_binary(context, body, binary, lhs, rhs, destination)
            } else {
                let input = val!(value, from);
                lower_cast(
                    context,
                    body,
                    *op,
                    *non_negative,
                    input,
                    destination,
                    layout,
                )
            };
            values.insert(result.clone(), id);
        }
        Inst::Alloca { result, ty, align } => {
            let bytes = type_size(ty, layout)?;
            let align = align.unwrap_or(type_align(ty, layout)?);
            let o = pops::alloca(
                context,
                bytes,
                align,
                PtrType::opaque(context),
                vec![],
                Default::default(),
                body.end(),
            );
            values.insert(result.clone(), o.result());
        }
        Inst::Load { result, ty, ptr } => {
            let p = val!(ptr, &Type::Ptr(None));
            let o = pops::load(
                context,
                p,
                lower_type(context, ty)?,
                vec![],
                vec![],
                Default::default(),
                body.end(),
            );
            values.insert(result.clone(), o.result());
        }
        Inst::Store { ty, value, ptr } => {
            let v = val!(value, ty);
            let p = val!(ptr, &Type::Ptr(None));
            pops::store(
                context,
                v,
                p,
                vec![],
                vec![],
                Default::default(),
                body.end(),
            );
        }
        Inst::GetElementPtr {
            result,
            source,
            base,
            indices,
        } => {
            let base = val!(base, &Type::Ptr(None));
            let address = lower_gep(
                context,
                body,
                base,
                source,
                indices,
                values,
                layout,
                definitions,
            )?;
            values.insert(result.clone(), address);
        }
        Inst::Phi { .. } => {}
        Inst::Select {
            result,
            cond,
            ty,
            if_true,
            if_false,
        } => {
            let cond = val!(cond, &Type::Int(1));
            let t = val!(if_true, ty);
            let f = val!(if_false, ty);
            let selected_value = lower_select(context, body, cond, t, f, ty, layout)?;
            values.insert(result.clone(), selected_value);
        }
        Inst::Br { dest } => {
            let target = by_label
                .get(dest)
                .ok_or_else(|| Error::UndefinedBlock(dest.clone()))?
                .id();
            let args = phi_arguments(context, body, dest, current_label, phis, values, globals)?;
            cbops::br(context, args, target, Default::default(), body.end());
        }
        Inst::CondBr {
            cond,
            if_true,
            if_false,
        } => {
            let c = val!(cond, &Type::Int(1));
            lower_cond_br(
                context,
                body,
                c,
                if_true,
                if_false,
                current_label,
                phis,
                values,
                globals,
                by_label,
            )?;
        }
        Inst::Switch {
            ty,
            value,
            default,
            cases,
        } => {
            let selector = val!(value, ty);
            lower_switch(
                context,
                body,
                selector,
                ty,
                default,
                cases,
                current_label,
                phis,
                values,
                globals,
                by_label,
            )?;
        }
        Inst::Ret { value } => match value {
            None => {
                func_ops::r#return(
                    context,
                    Operand::none(),
                    vec![],
                    Default::default(),
                    body.end(),
                );
            }
            Some((ty, op)) => {
                let v = val!(op, ty);
                func_ops::r#return(context, v, vec![], Default::default(), body.end());
            }
        },
        Inst::Call {
            result,
            ret,
            callee,
            args,
        } => {
            let mut arg_ids = Vec::with_capacity(args.len());
            for (ty, op) in args {
                arg_ids.push(val!(op, ty));
            }
            let ret_ty = lower_type(context, ret)?;
            match callee {
                ast::Operand::Global(name) if name.starts_with("llvm.") => {
                    let value =
                        lower_intrinsic(context, body, name, &arg_ids, ret, ret_ty, layout)?;
                    if let (Some(result), Some(value)) = (result, value) {
                        values.insert(result.clone(), value);
                    }
                    return Ok(());
                }
                _ => {}
            }
            let arg_types: Vec<_> = arg_ids
                .iter()
                .map(|&arg| context.get_value(arg).ty())
                .collect();
            let signature = FnType::new(context, &arg_types, ret_ty);
            let callee = match callee {
                ast::Operand::Global(name) => {
                    let declared = function_types.get(name);
                    let mut params = declared
                        .map(|(params, _, _)| {
                            params
                                .iter()
                                .map(|ty| lower_type(context, ty))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .transpose()?
                        .unwrap_or_else(|| arg_types.clone());
                    if declared.is_some_and(|(_, _, variadic)| *variadic) {
                        params.push(VarArgsType::new(context));
                    }
                    callees.value(context, name.as_str(), &params, ret_ty)
                }
                ast::Operand::Ref(name) => {
                    let address = *values
                        .get(name)
                        .ok_or_else(|| Error::UndefinedValue(name.clone()))?;
                    let op = bops::ptr_to_fn(
                        context,
                        address,
                        signature,
                        Default::default(),
                        body.end(),
                    );
                    op.result()
                }
                _ => return Err(Error::Unsupported("invalid call target".into())),
            };
            let o = func_ops::call(
                context,
                callee,
                arg_ids,
                ret_ty,
                vec![],
                vec![],
                Default::default(),
                body.end(),
            );
            if let Some(name) = result {
                values.insert(name.clone(), o.result());
            }
        }
        Inst::Unsupported(opcode) => return Err(Error::Unsupported(opcode.clone())),
    }
    Ok(())
}
fn lower_binary(
    context: &Context,
    body: &BlockHandle,
    kind: BinOp,
    lhs: ValueId,
    rhs: ValueId,
    ty: TypeId,
) -> ValueId {
    macro_rules! integer {
        ($constructor:path) => {{ $constructor(context, lhs, rhs, ty, Default::default(), body.end()).result() }};
    }
    let operation: Box<dyn Operation> = match kind {
        BinOp::Add => return integer!(bops::addi),
        BinOp::Sub => return integer!(bops::subi),
        BinOp::Mul => return integer!(bops::muli),
        BinOp::And => return integer!(bops::andi),
        BinOp::Or => return integer!(bops::ori),
        BinOp::Xor => return integer!(bops::xori),
        BinOp::Shl => return integer!(bops::shli),
        BinOp::LShr => return integer!(bops::shrui),
        BinOp::AShr => return integer!(bops::shrsi),
        BinOp::SDiv => return integer!(bops::divsi),
        BinOp::UDiv => return integer!(bops::divui),
        BinOp::SRem => return integer!(bops::remsi),
        BinOp::URem => return integer!(bops::remui),
        kind => {
            let semantics = arithmetic_semantics(context);
            match kind {
                BinOp::FAdd => Box::new(fp::AddOp::new(
                    context,
                    lhs,
                    rhs,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                )),
                BinOp::FSub => Box::new(fp::SubOp::new(
                    context,
                    lhs,
                    rhs,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                )),
                BinOp::FMul => Box::new(fp::MulOp::new(
                    context,
                    lhs,
                    rhs,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                )),
                BinOp::FDiv => Box::new(fp::DivOp::new(
                    context,
                    lhs,
                    rhs,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                )),
                _ => unreachable!(),
            }
        }
    };
    operation.value_results()[0]
}

#[allow(clippy::too_many_arguments)]
fn lower_cond_br(
    context: &Context,
    body: &BlockHandle,
    condition: ValueId,
    if_true: &str,
    if_false: &str,
    current_label: &str,
    phis: &HashMap<String, &Vec<Inst>>,
    values: &HashMap<String, ValueId>,
    globals: &HashMap<String, ValueId>,
    by_label: &HashMap<String, BlockHandle>,
) -> Result<(), Error> {
    let true_block = by_label
        .get(if_true)
        .ok_or_else(|| Error::UndefinedBlock(if_true.into()))?
        .id();
    let false_block = by_label
        .get(if_false)
        .ok_or_else(|| Error::UndefinedBlock(if_false.into()))?
        .id();
    let true_args = phi_arguments(context, body, if_true, current_label, phis, values, globals)?;
    let false_args = phi_arguments(
        context,
        body,
        if_false,
        current_label,
        phis,
        values,
        globals,
    )?;
    cbops::cond_br(
        context,
        condition,
        true_args,
        false_args,
        true_block,
        false_block,
        Default::default(),
        body.end(),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn lower_switch(
    context: &Context,
    body: &BlockHandle,
    selector: ValueId,
    ty: &Type,
    default: &str,
    cases: &[(i64, String)],
    current_label: &str,
    phis: &HashMap<String, &Vec<Inst>>,
    values: &HashMap<String, ValueId>,
    globals: &HashMap<String, ValueId>,
    by_label: &HashMap<String, BlockHandle>,
) -> Result<(), Error> {
    let Type::Int(width) = ty else {
        return Err(Error::Parse(format!("switch on a non-integer type {ty:?}")));
    };
    let successor = |label: &str| -> Result<tir::cfg::Successor, Error> {
        let block = by_label
            .get(label)
            .ok_or_else(|| Error::UndefinedBlock(label.into()))?
            .id();
        let args = phi_arguments(context, body, label, current_label, phis, values, globals)?;
        Ok((block, args))
    };
    let cases = cases
        .iter()
        .map(|(case, label)| {
            Ok((
                tir::binding::truncate_bits(*case, *width),
                successor(label)?,
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let default = successor(default)?;
    tir::cfg::SwitchOp::with_successors(
        context,
        selector,
        cases,
        default,
        Default::default(),
        body.end(),
    );
    Ok(())
}

fn lower_intrinsic(
    context: &Context,
    body: &BlockHandle,
    name: &str,
    args: &[ValueId],
    ret: &Type,
    ret_ty: TypeId,
    layout: &Layout,
) -> Result<Option<ValueId>, Error> {
    if name.starts_with("llvm.lifetime.") || name == "llvm.assume" {
        return Ok(None);
    }
    if name.starts_with("llvm.memcpy.") {
        pops::memcpy(
            context,
            args[0],
            args[1],
            args[2],
            vec![],
            vec![],
            Default::default(),
            body.end(),
        );
        return Ok(None);
    }
    if name.starts_with("llvm.memset.") {
        pops::memset(
            context,
            args[0],
            args[1],
            args[2],
            vec![],
            vec![],
            Default::default(),
            body.end(),
        );
        return Ok(None);
    }
    if name.starts_with("llvm.umax.") {
        let cmp = lower_icmp(context, body, "uge", ret, args[0], args[1])?;
        return lower_select(context, body, cmp, args[0], args[1], ret, layout).map(Some);
    }
    if name.starts_with("llvm.smax.") {
        let cmp = lower_icmp(context, body, "sge", ret, args[0], args[1])?;
        return lower_select(context, body, cmp, args[0], args[1], ret, layout).map(Some);
    }
    if name == "llvm.load.relative.i64" {
        let offset_ty = IntegerType::new(context, u64::from(layout.pointer_width));
        let offset = resize_signed(context, body, args[1], offset_ty);
        let address = pops::ptradd(
            context,
            args[0],
            offset,
            PtrType::opaque(context),
            Default::default(),
            body.end(),
        )
        .result();
        let relative = pops::load(
            context,
            address,
            IntegerType::new(context, 32),
            vec![],
            vec![],
            Default::default(),
            body.end(),
        )
        .result();
        let relative = resize_signed(context, body, relative, offset_ty);
        return Ok(Some(
            pops::ptradd(
                context,
                args[0],
                relative,
                ret_ty,
                Default::default(),
                body.end(),
            )
            .result(),
        ));
    }
    Err(Error::Unsupported(format!("intrinsic {name}")))
}

fn resize_unsigned(context: &Context, body: &BlockHandle, value: ValueId, ty: TypeId) -> ValueId {
    resize_integer(context, body, value, ty, false)
}

fn resize_signed(context: &Context, body: &BlockHandle, value: ValueId, ty: TypeId) -> ValueId {
    resize_integer(context, body, value, ty, true)
}

fn resize_integer(
    context: &Context,
    body: &BlockHandle,
    value: ValueId,
    ty: TypeId,
    signed: bool,
) -> ValueId {
    let width = |ty| IntegerType::view(context, ty).map(|integer| integer.width());
    match width(context.get_value(value).ty()).cmp(&width(ty)) {
        std::cmp::Ordering::Less if signed => {
            bops::extsi(context, value, ty, Default::default(), body.end()).result()
        }
        std::cmp::Ordering::Less => {
            bops::extui(context, value, ty, Default::default(), body.end()).result()
        }
        std::cmp::Ordering::Greater => {
            bops::trunci(context, value, ty, Default::default(), body.end()).result()
        }
        std::cmp::Ordering::Equal => value,
    }
}

fn constant(context: &Context, body: &BlockHandle, value: i64, ty: TypeId) -> ValueId {
    let op = bops::constant(context, value, ty, Default::default(), body.end());
    op.result()
}

fn float_constant(
    context: &Context,
    body: &BlockHandle,
    value: f64,
    ty: &Type,
) -> Result<ValueId, Error> {
    let (bits, ty) = match ty {
        Type::Float(32) => ((value as f32).to_bits() as u64, FloatType::f32(context)),
        Type::Float(64) => (value.to_bits(), FloatType::f64(context)),
        _ => {
            return Err(Error::Unsupported(
                "floating literal with non-floating type".into(),
            ));
        }
    };
    let op = fp::ConstantOp::new(
        context,
        tir::attributes::AttributeValue::UInt(bits),
        ty,
        Default::default(),
        body.end(),
    );
    Ok(op.result())
}

fn lower_select(
    context: &Context,
    body: &BlockHandle,
    cond: ValueId,
    mut if_true: ValueId,
    mut if_false: ValueId,
    ty: &Type,
    layout: &Layout,
) -> Result<ValueId, Error> {
    let result_ty = lower_type(context, ty)?;
    let work_ty = match ty {
        Type::Ptr(_) => {
            let int_ty = IntegerType::new(context, u64::from(layout.pointer_width));
            let null = pops::null(
                context,
                PtrType::opaque(context),
                Default::default(),
                body.end(),
            );
            if_true = pops::ptrdiff(
                context,
                if_true,
                null.result(),
                int_ty,
                Default::default(),
                body.end(),
            )
            .result();
            if_false = pops::ptrdiff(
                context,
                if_false,
                null.result(),
                int_ty,
                Default::default(),
                body.end(),
            )
            .result();
            int_ty
        }
        Type::Float(width) => {
            let int_ty = IntegerType::new(context, u64::from(*width));
            if_true =
                bops::bitcast(context, if_true, int_ty, Default::default(), body.end()).result();
            if_false =
                bops::bitcast(context, if_false, int_ty, Default::default(), body.end()).result();
            int_ty
        }
        Type::Int(_) => result_ty,
        _ => return Err(Error::Unsupported("aggregate select".into())),
    };
    let mask = if context.get_value(cond).ty() == work_ty {
        cond
    } else {
        bops::extsi(context, cond, work_ty, Default::default(), body.end()).result()
    };
    let selected_true = bops::andi(
        context,
        if_true,
        mask,
        work_ty,
        Default::default(),
        body.end(),
    )
    .result();
    let all_ones = constant(context, body, -1, work_ty);
    let inverted = bops::xori(
        context,
        mask,
        all_ones,
        work_ty,
        Default::default(),
        body.end(),
    )
    .result();
    let selected_false = bops::andi(
        context,
        if_false,
        inverted,
        work_ty,
        Default::default(),
        body.end(),
    )
    .result();
    let selected = bops::ori(
        context,
        selected_true,
        selected_false,
        work_ty,
        Default::default(),
        body.end(),
    )
    .result();
    Ok(match ty {
        Type::Ptr(_) => {
            let null = pops::null(
                context,
                PtrType::opaque(context),
                Default::default(),
                body.end(),
            );
            pops::ptradd(
                context,
                null.result(),
                selected,
                result_ty,
                Default::default(),
                body.end(),
            )
            .result()
        }
        Type::Float(_) => {
            bops::bitcast(context, selected, result_ty, Default::default(), body.end()).result()
        }
        _ => selected,
    })
}

fn lower_icmp(
    context: &Context,
    body: &BlockHandle,
    predicate: &str,
    ty: &Type,
    lhs: ValueId,
    rhs: ValueId,
) -> Result<ValueId, Error> {
    let result_ty = IntegerType::new(context, 1);
    let predicate = parse_predicate(predicate, Predicate::INTEGER, "pointer icmp")?;
    Ok(if matches!(ty, Type::Ptr(_)) {
        pops::CmpOp::new(
            context,
            lhs,
            rhs,
            tir::attributes::AttributeValue::Predicate(predicate),
            result_ty,
            Default::default(),
            body.end(),
        )
        .result()
    } else {
        bops::CmpIOp::new(
            context,
            lhs,
            rhs,
            tir::attributes::AttributeValue::Predicate(predicate),
            result_ty,
            Default::default(),
            body.end(),
        )
        .result()
    })
}

fn lower_cast(
    context: &Context,
    body: &BlockHandle,
    cast: CastOp,
    non_negative: bool,
    input: ValueId,
    result_ty: TypeId,
    layout: &Layout,
) -> ValueId {
    match cast {
        CastOp::SExt => {
            bops::extsi(context, input, result_ty, Default::default(), body.end()).result()
        }
        CastOp::ZExt => {
            bops::extui(context, input, result_ty, Default::default(), body.end()).result()
        }
        CastOp::Trunc => {
            bops::trunci(context, input, result_ty, Default::default(), body.end()).result()
        }
        // Both casts zero-extend or truncate between the integer and the
        // pointer-sized address, which `ptr.ptrdiff` and `ptr.ptradd` take.
        CastOp::PtrToInt => {
            let null = pops::null(
                context,
                PtrType::opaque(context),
                Default::default(),
                body.end(),
            )
            .result();
            let address_ty = IntegerType::new(context, u64::from(layout.pointer_width));
            let address = pops::ptrdiff(
                context,
                input,
                null,
                address_ty,
                Default::default(),
                body.end(),
            )
            .result();
            resize_unsigned(context, body, address, result_ty)
        }
        CastOp::IntToPtr => {
            let address_ty = IntegerType::new(context, u64::from(layout.pointer_width));
            let address = resize_unsigned(context, body, input, address_ty);
            let null = pops::null(
                context,
                PtrType::opaque(context),
                Default::default(),
                body.end(),
            )
            .result();
            pops::ptradd(
                context,
                null,
                address,
                result_ty,
                Default::default(),
                body.end(),
            )
            .result()
        }
        CastOp::SIToFP | CastOp::UIToFP => {
            let semantics = arithmetic_semantics(context);
            let op: Box<dyn Operation> = if cast == CastOp::SIToFP || non_negative {
                Box::new(fp::FromSiOp::new(
                    context,
                    input,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    result_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                ))
            } else {
                Box::new(fp::FromUiOp::new(
                    context,
                    input,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    result_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                ))
            };
            op.value_results()[0]
        }
        CastOp::FPToSI | CastOp::FPToUI => {
            let semantics = integer_conversion_semantics(context);
            let result_width = IntegerType::view(context, result_ty).map(|integer| integer.width());
            if cast == CastOp::FPToUI && result_width.is_some_and(|width| width < 64) {
                let wide_ty = IntegerType::new(context, 64);
                let wide = fp::ToSiOp::new(
                    context,
                    input,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    wide_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                )
                .result();
                return bops::trunci(context, wide, result_ty, Default::default(), body.end())
                    .result();
            }
            let op: Box<dyn Operation> = if cast == CastOp::FPToSI {
                Box::new(fp::ToSiOp::new(
                    context,
                    input,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    result_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                ))
            } else {
                Box::new(fp::ToUiOp::new(
                    context,
                    input,
                    tir::attributes::AttributeValue::FpSemantics(semantics),
                    result_ty,
                    vec![],
                    vec![],
                    Default::default(),
                    body.end(),
                ))
            };
            op.value_results()[0]
        }
        CastOp::FPExt | CastOp::FPTrunc => fp::ConvertOp::new(
            context,
            input,
            tir::attributes::AttributeValue::FpSemantics(arithmetic_semantics(context)),
            result_ty,
            vec![],
            vec![],
            Default::default(),
            body.end(),
        )
        .result(),
    }
}

fn phi_arguments(
    context: &Context,
    body: &BlockHandle,
    destination: &str,
    predecessor: &str,
    phis: &HashMap<String, &Vec<Inst>>,
    values: &HashMap<String, ValueId>,
    globals: &HashMap<String, ValueId>,
) -> Result<Vec<ValueId>, Error> {
    phis.get(destination)
        .into_iter()
        .flat_map(|insts| insts.iter())
        .filter_map(|inst| match inst {
            Inst::Phi { ty, incoming, .. } => Some((ty, incoming)),
            _ => None,
        })
        .map(|(ty, incoming)| {
            let operand = incoming
                .iter()
                .find(|(_, block)| block == predecessor)
                .map(|(operand, _)| operand)
                .ok_or_else(|| {
                    Error::Parse(format!(
                        "phi in %{destination} has no incoming value from %{predecessor}"
                    ))
                })?;
            match operand {
                ast::Operand::Ref(name) => values
                    .get(name)
                    .copied()
                    .ok_or_else(|| Error::UndefinedValue(name.clone())),
                ast::Operand::ConstInt(value) => {
                    Ok(constant(context, body, *value, lower_type(context, ty)?))
                }
                ast::Operand::ConstFloat(value) => float_constant(context, body, *value, ty),
                ast::Operand::Null => {
                    let op = pops::null(
                        context,
                        PtrType::opaque(context),
                        Default::default(),
                        body.end(),
                    );
                    Ok(op.result())
                }
                ast::Operand::Global(name) => globals
                    .get(name)
                    .copied()
                    .ok_or_else(|| Error::UndefinedValue(format!("@{name}"))),
                _ => Err(Error::Unsupported("non-integer phi operand".into())),
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn lower_gep(
    context: &Context,
    body: &BlockHandle,
    base: ValueId,
    source: &Type,
    indices: &[(Type, ast::Operand)],
    values: &HashMap<String, ValueId>,
    layout: &Layout,
    definitions: &HashMap<&str, &Inst>,
) -> Result<ValueId, Error> {
    let offset_ty = IntegerType::new(context, u64::from(layout.pointer_width));
    let mut offset = None;
    let mut literal_offset = 0i64;
    let mut current = source.clone();
    for (position, (index_ty, index)) in indices.iter().enumerate() {
        let (scale, next, direct) = gep_step(&current, position, index, layout)?;
        let (index_ty, index) = if let Some((ty, variable, constant)) =
            affine_no_wrap_index(index_ty, index, definitions, values)
        {
            literal_offset = literal_offset.wrapping_add(constant.wrapping_mul(scale as i64));
            (ty, variable)
        } else {
            (index_ty, index)
        };
        if let ast::Operand::ConstInt(value) = index {
            let value = if direct {
                scale as i64
            } else {
                let Type::Int(width) = index_ty else {
                    return Err(Error::Unsupported("non-integer getelementptr index".into()));
                };
                gep_index(*value, *width).wrapping_mul(scale as i64)
            };
            literal_offset = literal_offset.wrapping_add(value);
            current = next;
            continue;
        }
        let index = match index {
            ast::Operand::Ref(name) => {
                let value = *values
                    .get(name)
                    .ok_or_else(|| Error::UndefinedValue(name.clone()))?;
                // An index is sign-extended or truncated to the pointer width.
                if !matches!(index_ty, Type::Int(_)) {
                    return Err(Error::Unsupported("non-integer getelementptr index".into()));
                }
                resize_signed(context, body, value, offset_ty)
            }
            _ => return Err(Error::Unsupported("non-integer getelementptr index".into())),
        };
        if scale == 0 {
            current = next;
            continue;
        }
        let term = if scale == 1 {
            index
        } else {
            let scale = constant(context, body, scale as i64, offset_ty);
            let product = bops::muli(
                context,
                index,
                scale,
                offset_ty,
                Default::default(),
                body.end(),
            );
            product.result()
        };
        offset = Some(match offset {
            Some(offset) => {
                let add = bops::addi(
                    context,
                    offset,
                    term,
                    offset_ty,
                    Default::default(),
                    body.end(),
                );
                add.result()
            }
            None => term,
        });
        current = next;
    }
    let mut address = base;
    // A previously lowered GEP leaves its literal byte displacement outermost.
    // Combine that displacement here so the next dynamic index precedes it.
    if let Some(definition) = context.get_value(base).defining_op()
        && let Some(add) = context.get_op(definition).as_op::<tir::ptr::PtrAddOp>()
        && context.get_value(add.operands()[1]).ty() == offset_ty
        && let Some(definition) = context.get_value(add.operands()[1]).defining_op()
        && let Some(literal) = context.get_op(definition).as_op::<builtin::ConstantOp>()
        && let Some(AttributeValue::Int(value)) = literal.attr("value")
    {
        address = add.operands()[0];
        literal_offset = literal_offset.wrapping_add(value);
    }
    if let Some(offset) = offset {
        address = pops::ptradd(
            context,
            address,
            offset,
            PtrType::opaque(context),
            Default::default(),
            body.end(),
        )
        .result();
    }
    if literal_offset != 0 {
        let literal = constant(context, body, literal_offset, offset_ty);
        address = pops::ptradd(
            context,
            address,
            literal,
            PtrType::opaque(context),
            Default::default(),
            body.end(),
        )
        .result();
    }
    Ok(address)
}

fn gep_index(value: i64, width: u32) -> i64 {
    if width == 0 || width >= 64 {
        return value;
    }
    let shift = 64 - width;
    (value << shift) >> shift
}

fn gep_step(
    current: &Type,
    position: usize,
    index: &ast::Operand,
    layout: &Layout,
) -> Result<(u64, Type, bool), Error> {
    if position == 0 {
        return Ok((type_size(current, layout)?, current.clone(), false));
    }
    let current = match current {
        Type::Named(name) => layout
            .named
            .get(name)
            .ok_or_else(|| Error::Parse(format!("undefined type %{name}")))?,
        other => other,
    };
    match current {
        Type::Array(_, elem) => Ok((type_size(elem, layout)?, (**elem).clone(), false)),
        Type::Struct(fields) => {
            let ast::Operand::ConstInt(field) = index else {
                return Err(Error::Unsupported(
                    "dynamic struct getelementptr index".into(),
                ));
            };
            let field = usize::try_from(*field)
                .ok()
                .and_then(|field| fields.get(field).map(|ty| (field, ty)))
                .ok_or_else(|| Error::Parse("struct getelementptr index out of range".into()))?;
            let mut offset: u64 = 0;
            for ty in &fields[..field.0] {
                let align = type_align(ty, layout)?;
                offset = offset.div_ceil(align) * align + type_size(ty, layout)?;
            }
            let align = type_align(field.1, layout)?;
            offset = offset.div_ceil(align) * align;
            Ok((offset, field.1.clone(), true))
        }
        scalar => Ok((type_size(scalar, layout)?, scalar.clone(), false)),
    }
}

fn arithmetic_semantics(context: &Context) -> Arc<tir::fp::Semantics> {
    context.intern_fp_semantics(ArithmeticSemantics::strict(
        RoundingMode::TiesToEven,
        Exceptions::Ignore,
    ))
}

fn comparison_semantics(context: &Context) -> Arc<tir::fp::Semantics> {
    context.intern_fp_semantics(tir::fp::Semantics::Comparison(ComparisonSemantics {
        behavior: ComparisonBehavior::Quiet,
        exceptions: Exceptions::Ignore,
        subnormals: SubnormalMode::Gradual,
    }))
}

fn integer_conversion_semantics(context: &Context) -> Arc<tir::fp::Semantics> {
    context.intern_fp_semantics(tir::fp::Semantics::IntegerConversion(
        IntegerConversionSemantics {
            rounding: Rounding::Fixed(RoundingMode::TowardZero),
            exceptions: Exceptions::Ignore,
            subnormals: SubnormalMode::Gradual,
            invalid: InvalidConversion::Indeterminate,
        },
    ))
}

fn parse_predicate(
    predicate: &str,
    vocabulary: &[Predicate],
    op: &str,
) -> Result<Predicate, Error> {
    Predicate::parse(predicate)
        .filter(|parsed| vocabulary.contains(parsed))
        .ok_or_else(|| Error::Unsupported(format!("{op} {predicate}")))
}
