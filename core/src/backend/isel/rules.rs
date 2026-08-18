//! Table-driven rule construction and emission.
//!
//! TMDL emits one [`RuleSpec`] per selection rule instead of a generated
//! constructor call chain, and one [`EmitSpec`] per emitter instead of a
//! function body of builder calls. [`build_rules`] and [`emit_with`] interpret
//! the specs.

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::graph::OperandConstraint;
use tir::sem::{ExtendSemBytes, ExtendSemBytesTyped, SymKind};
use tir::{Context, OpHandle, OpInstance, Operation, PassError};

use crate::backend::isel::{
    EmitRequest, ImmRange, RegisterCapability, RegisterRequirement, Rule, RuleEmitFn, RuleKind,
    RuleMatch,
};
use crate::backend::regalloc::RegClassId;
use crate::graph::MetaMutDag;

/// One attribute of the emitted instruction: where its value comes from.
#[derive(Clone, Copy)]
pub enum EmitAttr {
    /// `req.results[result]` as a virtual register of the class.
    Result {
        attr: &'static str,
        result: u16,
        class: RegClassId,
    },
    /// `req.results[result]` defined in one required physical register.
    ResultFixedDef {
        attr: &'static str,
        result: u16,
        class: RegClassId,
        index: u16,
    },
    /// The value bound to `symbol` as a virtual register of the class.
    Value {
        attr: &'static str,
        symbol: u32,
        class: RegClassId,
    },
    /// The value bound to `symbol`, read from one physical register.
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
    /// Every attribute the op declares; emission fills all of them.
    pub declared: &'static [&'static str],
}

/// Interprets an [`EmitSpec`]: bind each attribute from the match and build
/// the op. `RewriteFailed` when a required binding is absent.
pub fn emit_with(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
    spec: &EmitSpec,
) -> Result<Box<dyn Operation>, PassError> {
    let mut attributes = Vec::with_capacity(spec.attrs.len());
    for entry in spec.attrs {
        let fail = || PassError::RewriteFailed(req.op_id());
        let value = match *entry {
            EmitAttr::Result { result, class, .. } => {
                let id = req.results.get(result as usize).ok_or_else(fail)?.number();
                AttributeValue::Register(RegisterAttr::Virtual {
                    id,
                    class: Some(class),
                })
            }
            EmitAttr::ResultFixedDef {
                result,
                class,
                index,
                ..
            } => {
                let id = req.results.get(result as usize).ok_or_else(fail)?.number();
                AttributeValue::Register(RegisterAttr::FixedDef { id, class, index })
            }
            EmitAttr::Value { symbol, class, .. } => {
                let src = m.value_binding(symbol).ok_or_else(fail)?;
                AttributeValue::Register(RegisterAttr::Virtual {
                    id: src.number(),
                    class: Some(class),
                })
            }
            EmitAttr::FixedUse {
                symbol,
                class,
                index,
                ..
            } => {
                let src = m.value_binding(symbol).ok_or_else(fail)?;
                AttributeValue::Register(RegisterAttr::FixedUse {
                    id: src.number(),
                    class,
                    index,
                })
            }
            EmitAttr::Physical { class, index, .. } => {
                AttributeValue::Register(RegisterAttr::Physical { class, index })
            }
            EmitAttr::Int { symbol, .. } => {
                AttributeValue::Int(m.int_binding(symbol).ok_or_else(fail)?)
            }
            EmitAttr::Block { symbol, .. } => {
                AttributeValue::Block(m.block_binding(symbol).ok_or_else(fail)?)
            }
            EmitAttr::IntOrValue { symbol, .. } => {
                if let Some(v) = m.int_binding(symbol) {
                    AttributeValue::Int(v)
                } else {
                    let src = m.value_binding(symbol).ok_or_else(fail)?;
                    AttributeValue::Register(RegisterAttr::Virtual {
                        id: src.number(),
                        class: None,
                    })
                }
            }
        };
        let attr = match *entry {
            EmitAttr::Result { attr, .. }
            | EmitAttr::ResultFixedDef { attr, .. }
            | EmitAttr::Value { attr, .. }
            | EmitAttr::FixedUse { attr, .. }
            | EmitAttr::Physical { attr, .. }
            | EmitAttr::Int { attr, .. }
            | EmitAttr::Block { attr, .. }
            | EmitAttr::IntOrValue { attr, .. } => attr,
        };
        attributes.push(context.named_attribute(attr, value));
    }
    for declared in spec.declared {
        if !attributes
            .iter()
            .any(|a| Some(a.name) == context.sym(declared))
        {
            panic!("Missing required attribute: {declared}");
        }
    }
    let instance = OpInstance::new_dynamic(
        spec.op,
        context.as_context_ref(),
        vec![],
        vec![],
        vec![],
        attributes,
    );
    Ok((spec.wrap)(context.add_operation(instance)))
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
    /// `(mnemonic, encoding_bytes)` terms; the rule's cost is the sum of each
    /// term's latency times [`crate::backend::isel::LATENCY_COST_SCALE`] plus
    /// its encoding size.
    pub cost_terms: &'static [(&'static str, u32)],
    pub kind: RuleKind,
    /// Emitter for the prelude instruction, when the rule emits a flag-setting
    /// companion first. Generated as a shim over [`emit_with`].
    pub prelude_emit: Option<RuleEmitFn>,
    /// Generated shim over [`emit_with`] with the rule's [`EmitSpec`].
    pub emit_fn: RuleEmitFn,
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
        g.set_actual_type(root, ty);
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
    instruction_cost: fn(&str) -> u32,
    specs: &[&RuleSpec],
) -> Vec<Rule> {
    let mut rules = Vec::new();
    for spec in specs {
        if !spec.features.is_empty() && !spec.features.iter().any(|f| enabled_features.contains(f))
        {
            continue;
        }
        let base_cost = spec
            .cost_terms
            .iter()
            .map(|(mnemonic, bytes)| {
                instruction_cost(mnemonic) * crate::backend::isel::LATENCY_COST_SCALE + bytes
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
            base_cost,
            kind: spec.kind,
            prelude_emit: spec.prelude_emit,
            operand_constraints: spec.constraints.to_vec(),
            operand_registers,
            result_register,
            float_constant_width: spec.pattern.float_width,
            operand_imm_ranges: spec.imm_ranges.to_vec(),
            guarded_semantics: spec
                .guarded
                .map(|g| build_pattern(context, kinds, blob, &g)),
            emit_fn: spec.emit_fn,
        });
    }
    rules
}
