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

pub fn import(context: &Context, module: &ast::Module) -> Result<builtin::ModuleOp, Error> {
    let m = bops::module(context, None).build();
    let builder = m.body();
    let mut callees = Callees::default();
    let named_types: HashMap<_, _> = module.named_types.iter().cloned().collect();
    let globals = lower_globals(context, module, &builder, &named_types)?;
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
        builder.append_op(lower_function(
            context,
            func,
            &mut callees,
            &globals,
            &named_types,
            &function_types,
        )?);
    }
    let bindings = callees.bind(context, &m, &builder);
    builder.append_op(bops::module_end(context).build());
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

    /// Pair every placeholder with the λ it names, declaring the functions this
    /// module only calls whose declarations have not appeared in the input.
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
                let signature = context.get_type_data(context.get_value(placeholder).ty());
                let signature = (signature.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<FnType>()
                    .expect("a callee placeholder has a function type");
                let declaration = tir::func::declare_op(
                    context,
                    &name,
                    signature.ret(context),
                    &signature.params(context),
                );
                let value = declaration.fn_value();
                body.append_op(declaration);
                (placeholder, value)
            })
            .collect()
    }
}

fn lower_type(context: &Context, ty: &Type) -> Result<TypeId, Error> {
    Ok(match ty {
        Type::Int(width) => IntegerType::new(context, *width),
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
    named: &HashMap<String, Type>,
) -> Result<HashMap<String, ValueId>, Error> {
    let mut values = HashMap::new();
    for global in &module.globals {
        let size = type_size(&global.ty, named)?;
        let mut builder = match &global.initializer {
            ast::GlobalInitializer::External => bops::global_external(context, &global.name),
            ast::GlobalInitializer::Zero | ast::GlobalInitializer::Null => {
                bops::global_zero(context, &global.name, size, global.align)
            }
            ast::GlobalInitializer::Integer(value) => {
                let mut bytes = vec![0; size as usize];
                let raw = value.to_le_bytes();
                let copied = bytes.len().min(raw.len());
                bytes[..copied].copy_from_slice(&raw[..copied]);
                bops::global_bytes(context, &global.name, bytes, global.align)
            }
            ast::GlobalInitializer::CString(bytes) | ast::GlobalInitializer::Bytes(bytes) => {
                validate_initializer_size(&global.name, size, bytes.len())?;
                bops::global_bytes(context, &global.name, bytes.clone(), global.align)
            }
            ast::GlobalInitializer::Symbols(symbols) => {
                let bytes = vec![0; size as usize];
                let relocations = symbols
                    .iter()
                    .enumerate()
                    .map(|(index, symbol)| {
                        AttributeValue::Dict(Box::new(std::collections::BTreeMap::from([
                            ("offset".into(), AttributeValue::UInt(index as u64 * 8)),
                            ("symbol".into(), AttributeValue::Str(symbol.clone().into())),
                            ("addend".into(), AttributeValue::Int(0)),
                            ("width".into(), AttributeValue::UInt(8)),
                        ])))
                    })
                    .collect::<Vec<_>>();
                bops::global_bytes(context, &global.name, bytes, global.align)
                    .attr("relocations", AttributeValue::Array(relocations.into()))
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
                bops::global_bytes(context, &global.name, vec![0; size as usize], global.align)
                    .attr("symbol_differences", AttributeValue::Array(entries.into()))
            }
        };
        if global.private {
            builder = builder.attr(
                "sym_visibility",
                AttributeValue::Str("private".to_string().into()),
            );
        }
        if global.constant {
            builder = builder.attr("section", AttributeValue::Str(".rodata".to_string().into()));
        }
        let op = builder.build();
        values.insert(global.name.clone(), op.address());
        body.append_op(op);
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

fn type_size(ty: &Type, named: &HashMap<String, Type>) -> Result<u64, Error> {
    Ok(match ty {
        Type::Int(width) | Type::Float(width) => u64::from(width.div_ceil(8)),
        Type::Ptr(_) => 8,
        Type::Array(count, elem) => count * type_size(elem, named)?,
        Type::Named(name) => type_size(
            named
                .get(name)
                .ok_or_else(|| Error::Parse(format!("undefined type %{name}")))?,
            named,
        )?,
        Type::Struct(fields) => struct_layout(fields, named)?.0,
        Type::Void => 0,
    })
}

fn type_align(ty: &Type, named: &HashMap<String, Type>) -> Result<u64, Error> {
    Ok(match ty {
        Type::Array(_, elem) => type_align(elem, named)?,
        Type::Named(name) => type_align(
            named
                .get(name)
                .ok_or_else(|| Error::Parse(format!("undefined type %{name}")))?,
            named,
        )?,
        Type::Struct(fields) => struct_layout(fields, named)?.1,
        _ => type_size(ty, named)?.clamp(1, 8),
    })
}

fn struct_layout(fields: &[Type], named: &HashMap<String, Type>) -> Result<(u64, u64), Error> {
    let mut size: u64 = 0;
    let mut max_align = 1;
    for field in fields {
        let align = type_align(field, named)?;
        size = size.div_ceil(align) * align;
        size += type_size(field, named)?;
        max_align = max_align.max(align);
    }
    Ok((size.div_ceil(max_align) * max_align, max_align))
}

fn lower_function(
    context: &Context,
    func: &ast::Function,
    callees: &mut Callees,
    globals: &HashMap<String, ValueId>,
    named: &HashMap<String, Type>,
    function_types: &HashMap<String, (Vec<Type>, Type, bool)>,
) -> Result<tir::func::FuncOp, Error> {
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
    )
    .build();

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
                named,
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

/// On defined `nsw` executions the signed result fits the original width, so
/// extending it equals computing with extended operands. Overflow is poison in
/// LLVM and permits this concrete result. Inspect only the direct definition;
/// in particular, no fact is carried through a freeze or another instruction.
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

/// Split a proven signed affine index before applying the GEP element scale.
/// The returned constant is interpreted at the binary operation's width.
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

#[allow(clippy::cognitive_complexity, clippy::too_many_arguments)]
fn lower_inst(
    context: &Context,
    inst: &Inst,
    body: &BlockHandle,
    values: &mut HashMap<String, ValueId>,
    by_label: &HashMap<String, BlockHandle>,
    callees: &mut Callees,
    globals: &HashMap<String, ValueId>,
    named: &HashMap<String, Type>,
    function_types: &HashMap<String, (Vec<Type>, Type, bool)>,
    current_label: &str,
    phis: &HashMap<String, &Vec<Inst>>,
    definitions: &HashMap<&str, &Inst>,
) -> Result<(), Error> {
    // Resolve an operand to a value, materialising a `builtin.constant` for
    // inline integer literals (TIR has no inline constants).
    macro_rules! val {
        ($op:expr, $ty:expr) => {
            match $op {
                ast::Operand::Ref(name) => *values
                    .get(name)
                    .ok_or_else(|| Error::UndefinedValue(name.clone()))?,
                ast::Operand::ConstInt(v) => match $ty {
                    Type::Float(32) => {
                        let op = fp::ConstantOpBuilder::new(context)
                            .bits((*v as f32).to_bits() as u64)
                            .result_type(FloatType::f32(context))
                            .build();
                        let id = op.result();
                        body.append_op(op);
                        id
                    }
                    Type::Float(64) => {
                        let op = fp::ConstantOpBuilder::new(context)
                            .bits((*v as f64).to_bits())
                            .result_type(FloatType::f64(context))
                            .build();
                        let id = op.result();
                        body.append_op(op);
                        id
                    }
                    _ => {
                        let c = bops::constant(context, *v, lower_type(context, $ty)?).build();
                        let id = c.result();
                        body.append_op(c);
                        id
                    }
                },
                ast::Operand::ConstFloat(value) => float_constant(context, body, *value, $ty)?,
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
                        let op =
                            bops::fn_to_ptr(context, function, PtrType::opaque(context)).build();
                        let result = op.result();
                        body.append_op(op);
                        result
                    }
                },
                ast::Operand::Null => {
                    let o = pops::null(context, PtrType::opaque(context)).build();
                    let id = o.result();
                    body.append_op(o);
                    id
                }
                ast::Operand::Undef => match $ty {
                    Type::Ptr(_) => {
                        let op = pops::null(context, PtrType::opaque(context)).build();
                        let id = op.result();
                        body.append_op(op);
                        id
                    }
                    Type::Float(32) | Type::Float(64) => {
                        let op = fp::ConstantOpBuilder::new(context)
                            .bits(0)
                            .result_type(lower_type(context, $ty)?)
                            .build();
                        let id = op.result();
                        body.append_op(op);
                        id
                    }
                    _ => constant(context, body, 0, lower_type(context, $ty)?),
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
                        named,
                        definitions,
                    )?
                }
            }
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
            let bits_ty = IntegerType::new(context, width);
            // LLVM fneg flips the sign bit, including for zero and NaN.
            let bits = body
                .append_op(bops::bitcast(context, input, bits_ty).build())
                .result();
            let sign = constant(context, body, 1_i64 << (width - 1), bits_ty);
            let negated = body
                .append_op(bops::xori(context, bits, sign, bits_ty).build())
                .result();
            let op = bops::bitcast(context, negated, lower_type(context, ty)?).build();
            values.insert(result.clone(), op.result());
            body.append_op(op);
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
            let predicate = parse_predicate(if pred == "ult" { "oge" } else { pred })?;
            let op = fp::CmpOpBuilder::new(context)
                .lhs(lhs)
                .rhs(rhs)
                .predicate(predicate)
                .semantics(comparison_semantics(context))
                .result_type(IntegerType::new(context, 1))
                .build();
            let comparison = op.result();
            body.append_op(op);
            let value = if pred == "ult" {
                body.append_op(
                    bops::xori(
                        context,
                        comparison,
                        constant(context, body, 1, IntegerType::new(context, 1)),
                        IntegerType::new(context, 1),
                    )
                    .build(),
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
                let lhs = lower_cast(context, body, CastOp::SExt, false, lhs, destination);
                let rhs = lower_cast(context, body, CastOp::SExt, false, rhs, destination);
                lower_binary(context, body, binary, lhs, rhs, destination)
            } else {
                let input = val!(value, from);
                lower_cast(context, body, *op, *non_negative, input, destination)
            };
            values.insert(result.clone(), id);
        }
        Inst::Alloca { result, ty, align } => {
            let bytes = type_size(ty, named)?;
            let align = align.unwrap_or(type_align(ty, named)?);
            let o = pops::alloca(context, bytes, align, PtrType::opaque(context)).build();
            values.insert(result.clone(), o.result());
            body.append_op(o);
        }
        Inst::Load { result, ty, ptr } => {
            let p = val!(ptr, &Type::Ptr(None));
            let o = pops::load(context, p, lower_type(context, ty)?).build();
            values.insert(result.clone(), o.result());
            body.append_op(o);
        }
        Inst::Store { ty, value, ptr } => {
            let v = val!(value, ty);
            let p = val!(ptr, &Type::Ptr(None));
            body.append_op(pops::store(context, v, p).build());
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
                named,
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
            let selected_value = lower_select(context, body, cond, t, f, ty)?;
            values.insert(result.clone(), selected_value);
        }
        Inst::Br { dest } => {
            lower_br(
                context,
                body,
                dest,
                current_label,
                phis,
                values,
                globals,
                by_label,
            )?;
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
        Inst::Ret { value } => match value {
            None => {
                body.append_op(func_ops::r#return(context, Operand::none()).build());
            }
            Some((ty, op)) => {
                let v = val!(op, ty);
                body.append_op(func_ops::r#return(context, v).build());
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
                    let value = lower_intrinsic(context, body, name, &arg_ids, ret, ret_ty)?;
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
                    let op = bops::ptr_to_fn(context, address, signature).build();
                    let result = op.result();
                    body.append_op(op);
                    result
                }
                _ => return Err(Error::Unsupported("invalid call target".into())),
            };
            let o = func_ops::call(context, callee, arg_ids, ret_ty).build();
            if let Some(name) = result {
                values.insert(name.clone(), o.result());
            }
            body.append_op(o);
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
        ($builder:path) => {{
            body.append_op($builder(context, lhs, rhs, ty).build())
                .result()
        }};
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
                BinOp::FAdd => Box::new(
                    fp::AddOpBuilder::new(context)
                        .lhs(lhs)
                        .rhs(rhs)
                        .semantics(semantics)
                        .result_type(ty)
                        .build(),
                ),
                BinOp::FSub => Box::new(
                    fp::SubOpBuilder::new(context)
                        .lhs(lhs)
                        .rhs(rhs)
                        .semantics(semantics)
                        .result_type(ty)
                        .build(),
                ),
                BinOp::FMul => Box::new(
                    fp::MulOpBuilder::new(context)
                        .lhs(lhs)
                        .rhs(rhs)
                        .semantics(semantics)
                        .result_type(ty)
                        .build(),
                ),
                BinOp::FDiv => Box::new(
                    fp::DivOpBuilder::new(context)
                        .lhs(lhs)
                        .rhs(rhs)
                        .semantics(semantics)
                        .result_type(ty)
                        .build(),
                ),
                _ => unreachable!(),
            }
        }
    };
    let result = operation.value_results()[0];
    body.append(operation.id());
    result
}

#[allow(clippy::too_many_arguments)]
fn lower_br(
    context: &Context,
    body: &BlockHandle,
    destination: &str,
    current_label: &str,
    phis: &HashMap<String, &Vec<Inst>>,
    values: &HashMap<String, ValueId>,
    globals: &HashMap<String, ValueId>,
    by_label: &HashMap<String, BlockHandle>,
) -> Result<(), Error> {
    let target = by_label
        .get(destination)
        .ok_or_else(|| Error::UndefinedBlock(destination.into()))?
        .id();
    let args = phi_arguments(
        context,
        body,
        destination,
        current_label,
        phis,
        values,
        globals,
    )?;
    body.append_op(cbops::br(context, args, target).build());
    Ok(())
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
    body.append_op(
        cbops::cond_br(
            context,
            condition,
            true_args,
            false_args,
            true_block,
            false_block,
        )
        .build(),
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
) -> Result<Option<ValueId>, Error> {
    if name.starts_with("llvm.lifetime.") || name == "llvm.assume" {
        return Ok(None);
    }
    if name.starts_with("llvm.memcpy.") {
        body.append_op(pops::memcpy(context, args[0], args[1], args[2]).build());
        return Ok(None);
    }
    if name.starts_with("llvm.memset.") {
        body.append_op(pops::memset(context, args[0], args[1], args[2]).build());
        return Ok(None);
    }
    if name.starts_with("llvm.umax.") {
        let cmp = lower_icmp(context, body, "uge", ret, args[0], args[1])?;
        return lower_select(context, body, cmp, args[0], args[1], ret).map(Some);
    }
    if name.starts_with("llvm.smax.") {
        let cmp = lower_icmp(context, body, "sge", ret, args[0], args[1])?;
        return lower_select(context, body, cmp, args[0], args[1], ret).map(Some);
    }
    if name == "llvm.load.relative.i64" {
        let address = body
            .append_op(pops::ptradd(context, args[0], args[1], PtrType::opaque(context)).build())
            .result();
        let relative = body
            .append_op(pops::load(context, address, IntegerType::new(context, 32)).build())
            .result();
        let relative = body
            .append_op(bops::extsi(context, relative, IntegerType::new(context, 64)).build())
            .result();
        return Ok(Some(
            body.append_op(pops::ptradd(context, args[0], relative, ret_ty).build())
                .result(),
        ));
    }
    Err(Error::Unsupported(format!("intrinsic {name}")))
}

fn constant(context: &Context, body: &BlockHandle, value: i64, ty: TypeId) -> ValueId {
    let op = bops::constant(context, value, ty).build();
    let result = op.result();
    body.append_op(op);
    result
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
    let op = fp::ConstantOpBuilder::new(context)
        .bits(bits)
        .result_type(ty)
        .build();
    let result = op.result();
    body.append_op(op);
    Ok(result)
}

fn lower_select(
    context: &Context,
    body: &BlockHandle,
    cond: ValueId,
    mut if_true: ValueId,
    mut if_false: ValueId,
    ty: &Type,
) -> Result<ValueId, Error> {
    let result_ty = lower_type(context, ty)?;
    let work_ty = match ty {
        Type::Ptr(_) => {
            let int_ty = IntegerType::new(context, 64);
            let null = body.append_op(pops::null(context, PtrType::opaque(context)).build());
            if_true = body
                .append_op(pops::ptrdiff(context, if_true, null.result(), int_ty).build())
                .result();
            if_false = body
                .append_op(pops::ptrdiff(context, if_false, null.result(), int_ty).build())
                .result();
            int_ty
        }
        Type::Float(width) => {
            let int_ty = IntegerType::new(context, *width);
            if_true = body
                .append_op(bops::bitcast(context, if_true, int_ty).build())
                .result();
            if_false = body
                .append_op(bops::bitcast(context, if_false, int_ty).build())
                .result();
            int_ty
        }
        Type::Int(_) => result_ty,
        _ => return Err(Error::Unsupported("aggregate select".into())),
    };
    let mask = if context.get_value(cond).ty() == work_ty {
        cond
    } else {
        body.append_op(bops::extsi(context, cond, work_ty).build())
            .result()
    };
    let selected_true = body
        .append_op(bops::andi(context, if_true, mask, work_ty).build())
        .result();
    let inverted = body
        .append_op(bops::xori(context, mask, constant(context, body, -1, work_ty), work_ty).build())
        .result();
    let selected_false = body
        .append_op(bops::andi(context, if_false, inverted, work_ty).build())
        .result();
    let selected = body
        .append_op(bops::ori(context, selected_true, selected_false, work_ty).build())
        .result();
    Ok(match ty {
        Type::Ptr(_) => {
            let null = body.append_op(pops::null(context, PtrType::opaque(context)).build());
            body.append_op(pops::ptradd(context, null.result(), selected, result_ty).build())
                .result()
        }
        Type::Float(_) => body
            .append_op(bops::bitcast(context, selected, result_ty).build())
            .result(),
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
    let predicate = parse_integer_predicate(predicate)?;
    Ok(if matches!(ty, Type::Ptr(_)) {
        body.append_op(
            pops::CmpOpBuilder::new(context)
                .lhs(lhs)
                .rhs(rhs)
                .predicate(predicate)
                .result_type(result_ty)
                .build(),
        )
        .result()
    } else {
        body.append_op(
            bops::CmpIOpBuilder::new(context)
                .lhs(lhs)
                .rhs(rhs)
                .predicate(predicate)
                .result_type(result_ty)
                .build(),
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
) -> ValueId {
    macro_rules! append {
        ($op:expr) => {{ body.append_op($op).result() }};
    }
    match cast {
        CastOp::SExt => append!(bops::extsi(context, input, result_ty).build()),
        CastOp::ZExt => append!(bops::extui(context, input, result_ty).build()),
        CastOp::Trunc => append!(bops::trunci(context, input, result_ty).build()),
        CastOp::PtrToInt => {
            let null = append!(pops::null(context, PtrType::opaque(context)).build());
            append!(pops::ptrdiff(context, input, null, result_ty).build())
        }
        CastOp::IntToPtr => {
            let null = append!(pops::null(context, PtrType::opaque(context)).build());
            append!(pops::ptradd(context, null, input, result_ty).build())
        }
        CastOp::SIToFP | CastOp::UIToFP => {
            let semantics = arithmetic_semantics(context);
            let op: Box<dyn Operation> = if cast == CastOp::SIToFP || non_negative {
                Box::new(
                    fp::FromSiOpBuilder::new(context)
                        .input(input)
                        .semantics(semantics)
                        .result_type(result_ty)
                        .build(),
                )
            } else {
                Box::new(
                    fp::FromUiOpBuilder::new(context)
                        .input(input)
                        .semantics(semantics)
                        .result_type(result_ty)
                        .build(),
                )
            };
            let result = op.value_results()[0];
            body.append(op.id());
            result
        }
        CastOp::FPToSI | CastOp::FPToUI => {
            let semantics = integer_conversion_semantics(context);
            let result_data = context.get_type_data(result_ty);
            let result_width = (result_data.as_ref() as &dyn std::any::Any)
                .downcast_ref::<IntegerType>()
                .map(IntegerType::width);
            if cast == CastOp::FPToUI && result_width.is_some_and(|width| width < 64) {
                let wide_ty = IntegerType::new(context, 64);
                let wide = body
                    .append_op(
                        fp::ToSiOpBuilder::new(context)
                            .input(input)
                            .semantics(semantics)
                            .result_type(wide_ty)
                            .build(),
                    )
                    .result();
                return append!(bops::trunci(context, wide, result_ty).build());
            }
            let op: Box<dyn Operation> = if cast == CastOp::FPToSI {
                Box::new(
                    fp::ToSiOpBuilder::new(context)
                        .input(input)
                        .semantics(semantics)
                        .result_type(result_ty)
                        .build(),
                )
            } else {
                Box::new(
                    fp::ToUiOpBuilder::new(context)
                        .input(input)
                        .semantics(semantics)
                        .result_type(result_ty)
                        .build(),
                )
            };
            let result = op.value_results()[0];
            body.append(op.id());
            result
        }
        CastOp::FPExt | CastOp::FPTrunc => append!(
            fp::ConvertOpBuilder::new(context)
                .input(input)
                .semantics(arithmetic_semantics(context))
                .result_type(result_ty)
                .build()
        ),
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
            let original_predecessor = predecessor
                .strip_prefix("llvm.switch.next.")
                .and_then(|suffix| suffix.rsplit_once('.'))
                .and_then(|(prefix, _)| prefix.rsplit_once('.'))
                .map(|(original, _)| original);
            let operand = incoming
                .iter()
                .find(|(_, block)| {
                    block == predecessor
                        || original_predecessor.is_some_and(|original| block == original)
                })
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
                    let op = pops::null(context, PtrType::opaque(context)).build();
                    let result = op.result();
                    body.append_op(op);
                    Ok(result)
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
    named: &HashMap<String, Type>,
    definitions: &HashMap<&str, &Inst>,
) -> Result<ValueId, Error> {
    let i64_ty = IntegerType::new(context, 64);
    let mut offset = None;
    let mut literal_offset = 0i64;
    let mut current = source.clone();
    for (position, (index_ty, index)) in indices.iter().enumerate() {
        let (scale, next, direct) = gep_step(&current, position, index, named)?;
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
                match index_ty {
                    Type::Int(64) => value,
                    Type::Int(_) => {
                        let op = bops::extsi(context, value, i64_ty).build();
                        let result = op.result();
                        body.append_op(op);
                        result
                    }
                    _ => return Err(Error::Unsupported("non-integer getelementptr index".into())),
                }
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
            let scale = constant(context, body, scale as i64, i64_ty);
            let product = bops::muli(context, index, scale, i64_ty).build();
            let product_value = product.result();
            body.append_op(product);
            product_value
        };
        offset = Some(match offset {
            Some(offset) => {
                let add = bops::addi(context, offset, term, i64_ty).build();
                let result = add.result();
                body.append_op(add);
                result
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
        && context.get_value(add.operands()[1]).ty() == i64_ty
        && let Some(definition) = context.get_value(add.operands()[1]).defining_op()
        && let Some(literal) = context.get_op(definition).as_op::<builtin::ConstantOp>()
        && let Some(AttributeValue::Int(value)) = literal.attr("value")
    {
        address = add.operands()[0];
        literal_offset = literal_offset.wrapping_add(value);
    }
    if let Some(offset) = offset {
        address = body
            .append_op(pops::ptradd(context, address, offset, PtrType::opaque(context)).build())
            .result();
    }
    if literal_offset != 0 {
        let literal = constant(context, body, literal_offset, i64_ty);
        address = body
            .append_op(pops::ptradd(context, address, literal, PtrType::opaque(context)).build())
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
    named: &HashMap<String, Type>,
) -> Result<(u64, Type, bool), Error> {
    if position == 0 {
        return Ok((type_size(current, named)?, current.clone(), false));
    }
    let current = match current {
        Type::Named(name) => named
            .get(name)
            .ok_or_else(|| Error::Parse(format!("undefined type %{name}")))?,
        other => other,
    };
    match current {
        Type::Array(_, elem) => Ok((type_size(elem, named)?, (**elem).clone(), false)),
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
                let align = type_align(ty, named)?;
                offset = offset.div_ceil(align) * align + type_size(ty, named)?;
            }
            let align = type_align(field.1, named)?;
            offset = offset.div_ceil(align) * align;
            Ok((offset, field.1.clone(), true))
        }
        scalar => Ok((type_size(scalar, named)?, scalar.clone(), false)),
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

fn parse_predicate(predicate: &str) -> Result<Predicate, Error> {
    Ok(match predicate {
        "oeq" => Predicate::Oeq,
        "ogt" => Predicate::Ogt,
        "oge" => Predicate::Oge,
        "olt" => Predicate::Olt,
        "ole" => Predicate::Ole,
        "une" => Predicate::Une,
        _ => return Err(Error::Unsupported(format!("fcmp {predicate}"))),
    })
}

fn parse_integer_predicate(predicate: &str) -> Result<Predicate, Error> {
    Ok(match predicate {
        "eq" => Predicate::Eq,
        "ne" => Predicate::Ne,
        "slt" => Predicate::Slt,
        "sgt" => Predicate::Sgt,
        "sle" => Predicate::Sle,
        "sge" => Predicate::Sge,
        "ult" => Predicate::Ult,
        "ugt" => Predicate::Ugt,
        "ule" => Predicate::Ule,
        "uge" => Predicate::Uge,
        _ => return Err(Error::Unsupported(format!("pointer icmp {predicate}"))),
    })
}
