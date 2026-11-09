use super::*;
use tir_symbolic::lang::{SymKind, SymPayload};
use tir_symbolic::sem::{SemGraph, SmtOracle, copy_subgraph};
use tir_symbolic::term::Terms;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InstructionValueKind {
    Integer,
    Float,
    RawBits,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct InstructionProofSymbol {
    pub(super) width: u32,
    pub(super) kind: InstructionValueKind,
}

impl InstructionProofSymbol {
    pub(super) fn integer(width: u32) -> Self {
        Self {
            width,
            kind: InstructionValueKind::Integer,
        }
    }
}

fn instruction_register_type(
    tables: &TargetTables,
    class: &str,
    width: u32,
) -> InstructionProofSymbol {
    let (float, default_width) = tables.reg_kinds.get(class).copied().unwrap_or((false, 64));
    match register_source_kind(float, default_width) {
        RegisterSourceKind::Integer => InstructionProofSymbol::integer(width),
        RegisterSourceKind::RawBits => InstructionProofSymbol {
            width: width.div_ceil(8) * 8,
            kind: InstructionValueKind::RawBits,
        },
        RegisterSourceKind::Float(exponent, mantissa) => InstructionProofSymbol {
            width: 1 + exponent + mantissa,
            kind: InstructionValueKind::Float,
        },
    }
}

pub(super) fn instruction_symbol_widths(
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    behavior: &sem_expr_state::BehaviorGraph,
    params: &HashMap<String, i64>,
) -> Option<Vec<InstructionProofSymbol>> {
    let mut types = HashMap::new();
    for ((class, _), symbol) in &behavior.register_symbols {
        types.insert(
            *symbol,
            InstructionProofSymbol::integer(tables.transfer.width(class, params).unwrap_or(0)),
        );
    }
    for (name, symbol) in &behavior.variable_symbols {
        let ty = match ctx.ops_map.get(name) {
            Some(Type::Struct(class)) => {
                instruction_register_type(tables, class, tables.transfer.width(class, params)?)
            }
            Some(Type::Bits(bits)) => InstructionProofSymbol::integer(u32::from(*bits)),
            Some(Type::Integer) => InstructionProofSymbol::integer(64),
            None if ctx.isa_param_values.contains_key(name) => InstructionProofSymbol::integer(64),
            _ => InstructionProofSymbol {
                width: 0,
                kind: InstructionValueKind::Unknown,
            },
        };
        types.insert(*symbol, ty);
    }
    let mut result = vec![
        InstructionProofSymbol {
            width: 0,
            kind: InstructionValueKind::Unknown
        };
        *types.keys().max()? as usize + 1
    ];
    for (symbol, ty) in types {
        result[symbol as usize] = ty;
    }
    Some(result)
}

pub(super) fn instruction_value_graph(
    source: &SemGraph,
    root: tir_adt::NodeId,
    symbols: &HashMap<String, u32>,
    ctx: &InstrEmitCtx<'_>,
    parameters: &HashMap<String, i64>,
) -> (SemGraph, tir_adt::NodeId) {
    let bindings: HashMap<_, _> = symbols
        .iter()
        .filter_map(|(name, symbol)| {
            if ctx.ops_map.contains_key(name) || !ctx.isa_param_values.contains_key(name) {
                None
            } else {
                parameters.get(name).map(|value| (*symbol, *value))
            }
        })
        .collect();
    let mut graph = SemGraph::<()>::new();
    let root = tir_symbolic::sem::copy_subgraph_with(
        &mut graph,
        source,
        root,
        &mut HashMap::new(),
        &mut |graph, node| {
            if let Some(SymPayload::SymbolId(symbol)) = source.get_leaf_data(node)
                && let Some(value) = bindings.get(symbol)
            {
                // Execution binds ISA parameters as signed 64-bit values.
                let constant = graph.add_node(SymKind::Constant);
                graph.set_leaf_data(
                    constant,
                    SymPayload::Int(tir_adt::APInt::new_signed(64, *value)),
                );
                return tir_symbolic::sem::CopyAction::Replace(constant);
            }
            tir_symbolic::sem::CopyAction::Keep
        },
    );
    (graph, root)
}

#[derive(Clone)]
enum InstructionProofValue {
    Scalar(tir_adt::NodeId, u32),
    RawBits(tir_adt::NodeId, u32),
    Float(tir_adt::NodeId, u32),
    Iterator(Vec<InstructionProofValue>),
}

impl InstructionProofValue {
    fn scalar(self) -> Option<(tir_adt::NodeId, u32)> {
        match self {
            Self::Scalar(node, width) | Self::RawBits(node, width) | Self::Float(node, width) => {
                Some((node, width))
            }
            _ => None,
        }
    }
    pub(super) fn integer(self) -> Option<(tir_adt::NodeId, u32)> {
        match self {
            Self::Scalar(node, width) => Some((node, width)),
            _ => None,
        }
    }
    fn integer_view(self) -> Option<(tir_adt::NodeId, u32)> {
        match self {
            Self::Scalar(node, width) | Self::RawBits(node, width) => Some((node, width)),
            _ => None,
        }
    }
    fn raw_bits(self) -> Option<(tir_adt::NodeId, u32)> {
        match self {
            Self::Scalar(node, width) | Self::RawBits(node, width) | Self::Float(node, width) => {
                Some((node, width))
            }
            _ => None,
        }
    }
}

pub(super) fn instruction_constant(graph: &SemGraph, root: tir_adt::NodeId) -> Option<u64> {
    let mut value = SemGraph::<()>::new();
    let (root, _) = instruction_integer_value(graph, root, &mut value, &[], &[])?.integer()?;
    let mut term = SemGraph::<()>::new();
    copy_subgraph(&mut term, &value, root, &mut HashMap::new());
    tir_symbolic::lang::execute_pure(&term, &[]).map(|value| value.to_u64())
}

fn instruction_iterator_value(
    source: &SemGraph,
    root: tir_adt::NodeId,
    graph: &mut SemGraph,
    arguments: &[InstructionProofValue],
    symbols: &[InstructionProofSymbol],
) -> Option<InstructionProofValue> {
    use InstructionProofValue::{Iterator, RawBits, Scalar};
    use tir_symbolic::sem::{con, op};
    let children: Vec<_> = source.children(root).collect();
    let mut child = |slot: usize| {
        instruction_integer_value(source, *children.get(slot)?, graph, arguments, symbols)
    };
    match *source.get_node(root) {
        SymKind::Split => {
            let (value, bits) = child(0)?.raw_bits()?;
            let count = u32::try_from(instruction_constant(source, *children.get(1)?)?).ok()?;
            if count == 0 || !bits.is_multiple_of(8) {
                return None;
            }
            let lane = if let Some(width) = children.get(2) {
                u32::try_from(instruction_constant(source, *width)?).ok()?
            } else {
                if !bits.is_multiple_of(count) || !(bits / count).is_multiple_of(8) {
                    return None;
                }
                bits / count
            };
            if lane == 0 || lane > 64 || count.checked_mul(lane)? > bits {
                return None;
            }
            let mut lanes = Vec::new();
            for index in 0..count {
                let low = index * lane;
                let hi = con(graph, u64::from(low + lane - 1), 64);
                let lo = con(graph, u64::from(low), 64);
                lanes.push(Scalar(op(graph, SymKind::Extract, &[value, hi, lo]), lane));
            }
            Some(Iterator(lanes))
        }
        SymKind::Zip => {
            let mut inputs = Vec::new();
            for slot in 0..children.len() {
                let Iterator(lanes) = child(slot)? else {
                    return None;
                };
                inputs.push(lanes);
            }
            let count = inputs.first()?.len();
            if inputs.iter().any(|lanes| lanes.len() != count) {
                return None;
            }
            Some(Iterator(
                (0..count)
                    .map(|lane| Iterator(inputs.iter().map(|input| input[lane].clone()).collect()))
                    .collect(),
            ))
        }
        SymKind::Map => {
            let Iterator(lanes) = child(0)? else {
                return None;
            };
            let body = *children.get(1)?;
            let mut result = Vec::new();
            for lane in lanes {
                let args = match lane {
                    Iterator(parts) => parts,
                    scalar => vec![scalar],
                };
                result.push(instruction_integer_value(
                    source, body, graph, &args, symbols,
                )?);
            }
            Some(Iterator(result))
        }
        SymKind::IterConcat => {
            let mut lanes = Vec::new();
            for slot in 0..children.len() {
                let Iterator(part) = child(slot)? else {
                    return None;
                };
                lanes.extend(part);
            }
            let mut lanes = lanes.into_iter();
            let first = lanes.next()?;
            if matches!(&first, Scalar(_, width) if *width > 64) {
                return None;
            }
            let (mut value, mut bits) = first.scalar()?;
            for lane in lanes {
                if matches!(&lane, Scalar(_, width) if *width > 64) {
                    return None;
                }
                let (next, width) = lane.scalar()?;
                value = op(graph, SymKind::Concat, &[next, value]);
                bits = bits.checked_add(width)?;
            }
            if !bits.is_multiple_of(8) {
                bits = bits.checked_add(7)? / 8 * 8;
                let width = con(graph, u64::from(bits), 64);
                value = op(graph, SymKind::ZExt, &[value, width]);
            }
            Some(RawBits(value, bits))
        }
        _ => None,
    }
}

fn instruction_integer_value(
    source: &SemGraph,
    root: tir_adt::NodeId,
    graph: &mut SemGraph,
    arguments: &[InstructionProofValue],
    symbols: &[InstructionProofSymbol],
) -> Option<InstructionProofValue> {
    use InstructionProofValue::{Float, RawBits, Scalar};
    use tir_symbolic::sem::{con, op};
    let children: Vec<_> = source.children(root).collect();
    let kind = *source.get_node(root);
    let mut child = |slot: usize| {
        instruction_integer_value(source, *children.get(slot)?, graph, arguments, symbols)
    };
    match kind {
        SymKind::Arg => {
            let Some(SymPayload::Int(index)) = source.get_leaf_data(root) else {
                return None;
            };
            arguments.get(index.to_u64() as usize).cloned()
        }
        SymKind::Symbol | SymKind::Constant => {
            let payload = source.get_leaf_data(root)?.clone();
            let ty = match &payload {
                SymPayload::SymbolId(symbol) => *symbols.get(*symbol as usize)?,
                SymPayload::Int(value) => {
                    InstructionProofSymbol::integer(u32::try_from(value.width()).ok()?)
                }
                _ => return None,
            };
            if ty.width == 0 {
                return None;
            }
            let node = graph.add_node(kind);
            graph.set_leaf_data(node, payload);
            Some(match ty.kind {
                InstructionValueKind::Integer => Scalar(node, ty.width),
                InstructionValueKind::RawBits => RawBits(node, ty.width),
                InstructionValueKind::Float => Float(node, ty.width),
                InstructionValueKind::Unknown => return None,
            })
        }

        SymKind::Split | SymKind::Zip | SymKind::Map | SymKind::IterConcat => {
            instruction_iterator_value(source, root, graph, arguments, symbols)
        }
        SymKind::Extract => {
            let (value, bits) = child(0)?.raw_bits()?;
            let high = instruction_constant(source, *children.get(1)?)?;
            let low = instruction_constant(source, *children.get(2)?)?;
            if low > high || high >= u64::from(bits) {
                return None;
            }
            let hi = con(graph, high, 64);
            let lo = con(graph, low, 64);
            Some(Scalar(
                op(graph, kind, &[value, hi, lo]),
                u32::try_from(high - low + 1).ok()?,
            ))
        }
        SymKind::SExt | SymKind::ZExt => {
            let (value, bits) = child(0)?.integer()?;
            let width = u32::try_from(instruction_constant(source, *children.get(1)?)?).ok()?;
            if width < bits || width == 0 {
                return None;
            }
            let size = con(graph, u64::from(width), 64);
            Some(Scalar(op(graph, kind, &[value, size]), width))
        }
        SymKind::Not | SymKind::Bitcast => {
            let input = child(0)?;
            let (mut value, mut bits) = if kind == SymKind::Bitcast {
                input.raw_bits()?
            } else {
                input.integer_view()?
            };
            if kind == SymKind::Bitcast && !bits.is_multiple_of(8) {
                bits = bits.checked_add(7)? / 8 * 8;
                let size = con(graph, u64::from(bits), 64);
                value = op(graph, SymKind::ZExt, &[value, size]);
            }
            let value = op(graph, kind, &[value]);
            Some(if kind == SymKind::Bitcast {
                RawBits(value, bits)
            } else {
                Scalar(value, bits)
            })
        }
        SymKind::Add
        | SymKind::Sub
        | SymKind::And
        | SymKind::Or
        | SymKind::Xor
        | SymKind::ShiftLeft
        | SymKind::ShiftRightLogic
        | SymKind::ShiftRightArithmetic
        | SymKind::Concat => {
            let (mut left, left_bits) = child(0)?.integer_view()?;
            let (mut right, right_bits) = child(1)?.integer_view()?;
            let bits = if kind == SymKind::Concat {
                left_bits.checked_add(right_bits)?
            } else {
                left_bits.max(right_bits)
            };
            if kind != SymKind::Concat && left_bits != right_bits {
                let narrow = if left_bits < bits {
                    &mut left
                } else {
                    &mut right
                };
                // Runtime arithmetic widens operands before evaluation. Only a
                // literal's signedness is known here; other mixed widths fail closed.
                let Some(SymPayload::Int(value)) = graph.get_leaf_data(*narrow) else {
                    return None;
                };
                let value = if value.is_signed() {
                    value.sign_extend(u64::from(bits))
                } else {
                    value.zero_extend(u64::from(bits))
                };
                let constant = graph.add_node(SymKind::Constant);
                graph.set_leaf_data(constant, SymPayload::Int(value));
                *narrow = constant;
            }
            Some(Scalar(op(graph, kind, &[left, right]), bits))
        }
        _ => None,
    }
}

pub(super) fn instruction_value_proves(
    source: &SemGraph,
    actual: tir_adt::NodeId,
    expected: tir_adt::NodeId,
    bits: u32,
    widths: &[InstructionProofSymbol],
) -> bool {
    let mut lhs = SemGraph::<()>::new();
    let mut rhs = SemGraph::<()>::new();
    let Some((_, left_bits)) = instruction_integer_value(source, actual, &mut lhs, &[], widths)
        .and_then(InstructionProofValue::scalar)
    else {
        return false;
    };
    let Some((_, right_bits)) = instruction_integer_value(source, expected, &mut rhs, &[], widths)
        .and_then(InstructionProofValue::scalar)
    else {
        return false;
    };
    if left_bits != bits || right_bits != bits {
        return false;
    }
    let terms = Terms::new();
    // Queried zero-width symbols already failed normalization above.
    let types: Vec<_> = widths
        .iter()
        .map(|width| terms.int(u64::from(width.width.max(1))))
        .collect();
    SmtOracle.equivalent_typed(&terms, &lhs, &rhs, &types)
}
