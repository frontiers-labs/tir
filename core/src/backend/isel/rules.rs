//! Table-driven rule construction and emission.
//!
//! TMDL emits one [`RuleSpec`] per selection rule instead of a generated
//! constructor call chain, and one [`EmitSpec`] per emitted instruction
//! instead of a function body of builder calls. [`build_rules`] and
//! [`emit_with`] interpret the specs.

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::sem::{ExtendSemBytes, ExtendSemBytesTyped, SymKind};
use tir::{Context, NewOp, OpHandle, Operation, PassError};

use crate::backend::isel::{
    EmitRequest, Emitter, FpFlags, ImmRange, RegisterCapability, RegisterRequirement, Rule,
    RuleKind, RuleMatch,
};
use crate::backend::regalloc::RegClassId;

/// Restricts what a boundary (operand) pattern node may bind to, distinguishing
/// register operands from immediates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandConstraint {
    /// Must bind to a non-constant value (a register / SSA value).
    Register,
    /// Must bind to a compile-time constant (an immediate).
    Immediate,
}

/// One slot of the emitted instruction: where its contents come from.
#[derive(Clone, Copy)]
pub enum EmitAttr {
    /// The result port `attr`: a fresh value of the port's class, standing for
    /// `req.results[result]`, which something has to read.
    Result {
        attr: &'static str,
        result: u16,
        class: RegClassId,
    },
    /// [`EmitAttr::Result`] pinned to one required physical register. Where
    /// nothing reads `req.results[result]` the instruction still writes the
    /// register, and the slot names it as a clobber.
    ResultFixedDef {
        attr: &'static str,
        result: u16,
        class: RegClassId,
        index: u16,
    },
    /// The operand port `attr`: the value bound to `symbol`.
    Value { attr: &'static str, symbol: u32 },
    /// [`EmitAttr::Value`] pinned to one required physical register.
    FixedUse {
        attr: &'static str,
        symbol: u32,
        class: RegClassId,
        index: u16,
    },
    /// A hardwired physical register (e.g. the zero register, a clobber).
    Physical {
        attr: &'static str,
        class: RegClassId,
        index: u16,
    },
    /// The constant bound to `symbol`.
    Int { attr: &'static str, symbol: u32 },
    /// The block bound to `symbol` (a branch target).
    Block { attr: &'static str, symbol: u32 },
    /// A path-addressed register read: its symbol binds either a constant
    /// (ISA-parameter reads) or a value with no fixed class.
    IntOrValue { attr: &'static str, symbol: u32 },
}

/// How to build the instruction a rule emits.
pub struct EmitSpec {
    /// `(dialect, op)` identity of the emitted operation.
    pub op: (&'static str, &'static str),
    /// Wraps the built instance into the typed operation. Generated as
    /// `|instance| Box::new(FooOp(instance))`.
    pub wrap: fn(OpHandle) -> Box<dyn Operation>,
    pub attrs: &'static [EmitAttr],
    /// The emitted opcode's own record: its register ports place a slot in the
    /// SSA position the opcode declares for it whatever order the rule binds
    /// them in, and its memory effects say whether it carries a state chain.
    pub info: &'static crate::backend::InstrInfo,
}

/// An emitted instruction and the value it defines for each of the request's
/// results.
pub(crate) type Emitted = (Box<dyn Operation>, Vec<(usize, tir::ValueId)>);

/// Interprets an [`EmitSpec`]: bind each slot from the match and build the op.
/// `RewriteFailed` when a required binding is absent.
///
/// A register slot becomes an SSA operand or result — a result is born typed
/// with its port's register class, which is what makes the class of every
/// machine value a type read — unless it names a physical register, which stays
/// an attribute literal. A slot the instruction must read or write in a fixed
/// register records that in the op's [`PINS_ATTR`] constraint.
pub(crate) fn emit_with(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
    spec: &EmitSpec,
) -> Result<Emitted, PassError> {
    let mut attributes = Vec::new();
    let mut operands: Vec<(usize, tir::ValueId)> = Vec::new();
    let mut results: Vec<(usize, tir::ValueId)> = Vec::new();
    let mut defined: Vec<(usize, tir::ValueId)> = Vec::new();
    let mut pins = std::collections::BTreeMap::new();
    let port = |name: &str| {
        spec.info
            .regs
            .iter()
            .position(|port| port.name == name)
            .ok_or_else(|| {
                PassError::InvalidRuleSet(format!(
                    "{}.{} has no register slot '{name}'",
                    spec.op.0, spec.op.1
                ))
            })
    };
    let bind = |name: &str, value: tir::ValueId| -> Result<(usize, tir::ValueId), PassError> {
        let port = port(name)?;
        if let Some(class) = spec.info.regs[port].class {
            crate::backend::retype_untyped(context, value, class);
        }
        Ok((port, value))
    };
    for entry in spec.attrs {
        let fail = || PassError::RewriteFailed(req.op_id());
        match *entry {
            EmitAttr::Result {
                attr,
                result,
                class,
            } => {
                let value = new_result(context, req, result, class)?.ok_or_else(fail)?;
                defined.push((result as usize, value));
                results.push((port(attr)?, value));
            }
            EmitAttr::ResultFixedDef {
                attr,
                result,
                class,
                index,
            } => match new_result(context, req, result, class)? {
                Some(value) => {
                    pins.insert(attr.to_string(), pin(class, index));
                    defined.push((result as usize, value));
                    results.push((port(attr)?, value));
                }
                None => attributes.push(context.named_attribute(attr, pin(class, index))),
            },
            EmitAttr::Value { attr, symbol } => {
                operands.push(bind(attr, m.value_binding(symbol).ok_or_else(fail)?)?);
            }
            EmitAttr::FixedUse {
                attr,
                symbol,
                class,
                index,
            } => {
                pins.insert(attr.to_string(), pin(class, index));
                operands.push(bind(attr, m.value_binding(symbol).ok_or_else(fail)?)?);
            }
            EmitAttr::Physical { attr, class, index } => {
                attributes.push(context.named_attribute(
                    attr,
                    AttributeValue::Register(RegisterAttr::Physical { class, index }),
                ));
            }
            EmitAttr::Int { attr, symbol } => {
                let value = AttributeValue::Int(m.int_binding(symbol).ok_or_else(fail)?);
                attributes.push(context.named_attribute(attr, value));
            }
            EmitAttr::Block { attr, symbol } => {
                let value = AttributeValue::Block(m.block_binding(symbol).ok_or_else(fail)?);
                attributes.push(context.named_attribute(attr, value));
            }
            EmitAttr::IntOrValue { attr, symbol } => match m.int_binding(symbol) {
                Some(value) => {
                    attributes.push(context.named_attribute(attr, AttributeValue::Int(value)))
                }
                None => operands.push(bind(attr, m.value_binding(symbol).ok_or_else(fail)?)?),
            },
        }
    }
    if !pins.is_empty() {
        attributes.push(context.named_attribute(
            crate::backend::PINS_ATTR,
            AttributeValue::Dict(Box::new(pins)),
        ));
    }
    operands.sort_by_key(|(port, _)| *port);
    results.sort_by_key(|(port, _)| *port);
    let mut operand_values: Vec<tir::ValueId> =
        operands.into_iter().map(|(_, value)| value).collect();
    let mut result_values: Vec<tir::ValueId> =
        results.into_iter().map(|(_, value)| value).collect();
    if (spec.info.effects.reads || spec.info.effects.writes)
        && !req.states.iter().any(|state| {
            context.state_resource(context.get_value(state.observed).ty())
                == Some(tir::builtin::StateResource::Memory)
        })
    {
        return Err(PassError::RewriteFailed(req.op_id()));
    }
    operand_values.extend(req.states.iter().map(|state| state.observed));
    result_values.extend(req.states.iter().filter_map(|state| state.published));
    let instance = NewOp::new_dynamic(
        spec.op,
        context.clone(),
        operand_values,
        result_values,
        vec![],
        attributes,
    );
    Ok(((spec.wrap)(context.add_operation(instance)), defined))
}

/// The value a result port defines: a fresh one of the port's class, standing
/// for the mid-end result the rule covers. `None` where nothing reads that
/// result.
fn new_result(
    context: &Context,
    req: &EmitRequest,
    result: u16,
    class: RegClassId,
) -> Result<Option<tir::ValueId>, PassError> {
    let demanded = req
        .results
        .get(result as usize)
        .ok_or_else(|| PassError::RewriteFailed(req.op_id()))?;
    let ty = crate::backend::RegClassType::new(context, class);
    Ok(demanded.map(|_| context.create_value(ty, None).id()))
}

fn pin(class: RegClassId, index: u16) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Physical { class, index })
}

/// The storage domain of a register operand: integer, float, or either.
#[derive(Clone, Copy)]
pub enum CapabilityKind {
    Integer,
    Float,
    Any,
}

/// A register operand's class and whether the instruction consumes the value's
/// full architectural width.
#[derive(Clone, Copy)]
pub struct RegOperandSpec {
    pub symbol: u32,
    pub class: RegClassId,
    pub whole: bool,
    pub capability: CapabilityKind,
}

/// Storage domain of the register receiving the rule's result.
#[derive(Clone, Copy)]
pub struct ResultRegSpec {
    pub class: RegClassId,
    pub capability: CapabilityKind,
}

/// A semantic graph serialized into the backend's sem blob.
#[derive(Clone, Copy)]
pub struct PatternRef {
    pub offset: u32,
    /// Whether any node carries a width annotation, requiring the
    /// context-resolving replay.
    pub typed: bool,
    /// Width of a floating-point value materializable from the pattern's
    /// integer bit pattern: the pattern root is re-typed to the scalar float
    /// of this width after replay.
    pub float_width: Option<u32>,
}

/// One selection rule, declaratively.
pub struct RuleSpec {
    pub name: &'static str,
    /// Feature ids (`Feature as u16`); the rule is available when any is
    /// enabled.
    pub features: &'static [u16],
    pub pattern: PatternRef,
    /// The semantics of each further result the plan's last instruction
    /// defines, over the same operands: result one, two, and so on.
    pub secondary: &'static [PatternRef],
    pub kind: RuleKind,
    pub fp_flags: FpFlags<PatternRef>,
    /// The emission plan: the instructions this rule emits, in the order they
    /// run. The rule's cost is the sum over them of
    /// [`crate::backend::InstrInfo::cost`] times
    /// [`crate::backend::isel::LATENCY_COST_SCALE`] plus the encoding size.
    pub plan: &'static [&'static EmitSpec],
    pub constraints: &'static [(u32, OperandConstraint)],
    pub registers: &'static [RegOperandSpec],
    pub result: Option<ResultRegSpec>,
    pub imm_ranges: &'static [(u32, ImmRange)],
    pub guarded: Option<PatternRef>,
}

fn build_pattern(
    context: &Context,
    kinds: &[SymKind],
    blob: &[u8],
    pattern: &PatternRef,
) -> crate::sem::SemGraph {
    let mut g = crate::sem::SemGraph::new();
    let root = if pattern.typed {
        g.extend_sem_bytes_typed(context, kinds, blob, pattern.offset)
    } else {
        g.extend_sem_bytes(kinds, blob, pattern.offset)
    };
    if let Some(width) = pattern.float_width {
        let ty = match width {
            32 => crate::builtin::FloatType::f32(context),
            64 => crate::builtin::FloatType::f64(context),
            _ => unreachable!("unsupported scalar float register width {width}"),
        };
        g.annotation_mut(root).actual_type = Some(ty);
    }
    g
}

fn requirement(
    register_widths: &[(&str, u32)],
    class: RegClassId,
    whole: bool,
    capability: CapabilityKind,
) -> Option<RegisterRequirement> {
    let (_, width) = register_widths
        .iter()
        .find(|(name, _)| *name == class.name())?;
    let capability = match capability {
        CapabilityKind::Integer => RegisterCapability::integer(*width),
        CapabilityKind::Float => RegisterCapability::float(*width),
        CapabilityKind::Any => RegisterCapability::any(*width),
    };
    let requirement = if whole {
        RegisterRequirement::whole(capability)
    } else {
        RegisterRequirement::low_bits(capability)
    };
    Some(requirement.at_view_offset(class.info().view.bit_offset))
}

/// Build the rules available under `enabled_features` (feature ids, `Feature
/// as u16`) from the backend's spec table.
#[allow(clippy::too_many_arguments)]
pub fn build_rules(
    context: &Context,
    enabled_features: &[u16],
    kinds: &[SymKind],
    blob: &[u8],
    register_widths: &[(&str, u32)],
    specs: &[&RuleSpec],
) -> Vec<Rule> {
    let mut rules = Vec::new();
    for spec in specs {
        if !spec.features.is_empty() && !spec.features.iter().any(|f| enabled_features.contains(f))
        {
            continue;
        }
        let base_cost = spec
            .plan
            .iter()
            .map(|step| {
                step.info.cost * crate::backend::isel::LATENCY_COST_SCALE
                    + u32::from(step.info.width_bytes.0)
            })
            .sum();
        let operand_registers = spec
            .registers
            .iter()
            .filter_map(|r| {
                requirement(register_widths, r.class, r.whole, r.capability)
                    .map(|req| (r.symbol, req))
            })
            .collect();
        let result_register = spec
            .result
            .and_then(|r| requirement(register_widths, r.class, false, r.capability));
        rules.push(Rule {
            name: spec.name,
            pattern: build_pattern(context, kinds, blob, &spec.pattern),
            secondary: spec
                .secondary
                .iter()
                .map(|pattern| build_pattern(context, kinds, blob, pattern))
                .collect(),
            base_cost,
            kind: spec.kind,
            fp_flags: match &spec.fp_flags {
                FpFlags::None => FpFlags::None,
                FpFlags::Clobber => FpFlags::Clobber,
                FpFlags::Exact(pattern) => {
                    FpFlags::Exact(build_pattern(context, kinds, blob, pattern))
                }
            },
            operand_constraints: spec.constraints.to_vec(),
            operand_registers,
            result_register,
            float_constant_width: spec.pattern.float_width,
            operand_imm_ranges: spec.imm_ranges.to_vec(),
            guarded_semantics: spec
                .guarded
                .map(|g| build_pattern(context, kinds, blob, &g)),
            emit: spec.plan.iter().map(|step| Emitter::Spec(step)).collect(),
        });
    }
    rules
}
