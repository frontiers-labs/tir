use std::collections::HashMap;

use schemars::{JsonSchema, Schema, generate::SchemaSettings};
use serde::Serialize;

use crate::{Type as AstType, ast};

mod abi;
mod expr;

use abi::{AbiPassSequence, AbiRegisterSequence, AbiRole, AbiStack};
use expr::Expr;

const VERSION: u8 = 1;

/// # TMDL checked AST
/// Versioned, checked TMDL abstract syntax tree.
#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub(crate) struct Document {
    /// JSON contract revision. Its schema is fixed to `1`.
    #[schemars(extend("const" = 1))]
    version: u8,
    /// Checked input files in command-line order.
    files: Vec<File>,
}

impl Document {
    pub(crate) fn from_ast(files: &[ast::File]) -> Self {
        Self {
            version: VERSION,
            files: files.iter().map(File::from).collect(),
        }
    }
}

pub(crate) fn schema() -> Schema {
    SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<Document>()
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// One checked input file.
struct File {
    /// Input path exactly as passed to `tmdlc`.
    path: String,
    /// Top-level declarations in source order after macro expansion.
    items: Vec<Item>,
}

impl From<&ast::File> for File {
    fn from(file: &ast::File) -> Self {
        Self {
            path: file.file_name.clone(),
            items: file.items.iter().map(Item::from).collect(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// A checked top-level TMDL declaration.
enum Item {
    /// An instruction-set architecture and its parameters.
    Isa {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "IsaRequirement")]
        requires: Option<IsaRequirement>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        parameters: Vec<Parameter>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "TrapHandler")]
        trap_handler: Option<TrapHandler>,
    },
    /// An ABI after ABI inheritance is resolved.
    Abi {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        alias: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        isas: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        base: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        parameters: Vec<Parameter>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "AbiStack")]
        stack: Option<AbiStack>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        roles: Vec<AbiRole>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        arguments: Vec<AbiPassSequence>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        returns: Vec<AbiPassSequence>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        callee_saved: Vec<AbiRegisterSequence>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        reserved: Vec<AbiRegisterSequence>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        classifier: Option<String>,
    },
    /// A register class after register-class inheritance is resolved.
    RegisterClass {
        name: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        isas: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        base: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        file: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        parameters: Vec<Parameter>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        registers: Vec<RegisterDef>,
    },
    /// A reusable instruction template. Template inheritance remains declarative.
    Template {
        name: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        isas: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        template: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        parameters: Vec<Parameter>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        operands: Vec<Operand>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        encoding: Vec<EncodingArm>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Expr")]
        assembly: Option<Expr>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        schedule: Vec<String>,
    },
    /// An instruction declaration. Inherited template fields are not duplicated.
    Instruction {
        name: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        isas: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        template: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        parameters: Vec<Parameter>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        operands: Vec<Operand>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        encoding: Vec<EncodingArm>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Expr")]
        assembly: Option<Expr>,
        behavior: Expr,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        schedule: Vec<String>,
    },
    /// A machine-independent scheduling class.
    #[serde(rename = "sched_class")]
    SchedClass {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "i64")]
        default_latency: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "i64")]
        default_throughput: Option<i64>,
    },
    /// A concrete machine performance model.
    Machine {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        alias: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        isas: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "i64")]
        issue_width: Option<i64>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        buffers: Vec<NamedCount>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        pipeline: Vec<PipelinePhase>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        resources: Vec<MachineUnit>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        register_files: Vec<NamedCount>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        bindings: Vec<UnitBind>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        overrides: Vec<MachineOverride>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        forwards: Vec<Forward>,
    },
}

impl From<&ast::Item> for Item {
    fn from(item: &ast::Item) -> Self {
        match item {
            ast::Item::Isa(isa) => Self::Isa {
                name: isa.name.clone(),
                requires: isa.requires.as_ref().map(IsaRequirement::from),
                parameters: parameters(&isa.parameters),
                trap_handler: isa.trap_handler.as_ref().map(TrapHandler::from),
            },
            ast::Item::Abi(abi) => Self::Abi {
                name: abi.name.clone(),
                alias: abi.alias.clone(),
                isas: abi.for_isas.clone(),
                base: abi.base.clone(),
                parameters: parameters(&abi.parameters),
                stack: abi.stack.as_ref().map(AbiStack::from),
                roles: abi.roles.iter().map(AbiRole::from).collect(),
                arguments: abi.args.iter().map(AbiPassSequence::from).collect(),
                returns: abi.rets.iter().map(AbiPassSequence::from).collect(),
                callee_saved: abi
                    .callee_saved
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(AbiRegisterSequence::from)
                    .collect(),
                reserved: abi
                    .reserved
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(AbiRegisterSequence::from)
                    .collect(),
                classifier: abi.classifier.clone(),
            },
            ast::Item::RegisterClass(class) => Self::RegisterClass {
                name: class.name.clone(),
                isas: class.for_isas.clone(),
                base: class.base.clone(),
                file: class.file.clone(),
                parameters: parameters(&class.parameters),
                registers: class.registers.iter().map(RegisterDef::from).collect(),
            },
            ast::Item::Template(template) => Self::Template {
                name: template.name.clone(),
                isas: template.for_isas.clone(),
                template: template.parent_template.clone(),
                parameters: parameters(&template.params),
                operands: operands(&template.operands),
                encoding: template.encoding.iter().map(EncodingArm::from).collect(),
                assembly: template.asm.as_ref().map(Expr::from),
                schedule: schedule_classes(&template.schedule),
            },
            ast::Item::Instruction(instruction) => Self::Instruction {
                name: instruction.name.clone(),
                isas: instruction.for_isas.clone(),
                template: instruction.parent_template.clone(),
                parameters: parameters(&instruction.params),
                operands: operands(&instruction.operands),
                encoding: instruction.encoding.iter().map(EncodingArm::from).collect(),
                assembly: instruction.asm.as_ref().map(Expr::from),
                behavior: Expr::from(&instruction.behavior),
                schedule: schedule_classes(&instruction.schedule),
            },
            ast::Item::Unit(class) => Self::SchedClass {
                name: class.name.clone(),
                default_latency: class.default_latency,
                default_throughput: class.default_throughput,
            },
            ast::Item::Machine(machine) => Self::Machine {
                name: machine.name.clone(),
                alias: machine.alias.clone(),
                isas: machine.for_isas.clone(),
                issue_width: machine.issue_width,
                buffers: named_counts(&machine.buffers),
                pipeline: machine.pipeline.iter().map(PipelinePhase::from).collect(),
                resources: machine.resources.iter().map(MachineUnit::from).collect(),
                register_files: named_counts(&machine.reg_files),
                bindings: machine.binds.iter().map(UnitBind::from).collect(),
                overrides: machine
                    .overrides
                    .iter()
                    .map(MachineOverride::from)
                    .collect(),
                forwards: machine.forwards.iter().map(Forward::from).collect(),
            },
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// Boolean composition of ISA requirements.
enum IsaRequirement {
    Single { isa: String },
    Any { isas: Vec<String> },
    All { isas: Vec<String> },
}

impl From<&ast::IsaRequirement> for IsaRequirement {
    fn from(requirement: &ast::IsaRequirement) -> Self {
        match requirement {
            ast::IsaRequirement::Single(isa) => Self::Single { isa: isa.clone() },
            ast::IsaRequirement::Any(isas) => Self::Any { isas: isas.clone() },
            ast::IsaRequirement::All(isas) => Self::All { isas: isas.clone() },
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Architectural synchronous trap handler.
struct TrapHandler {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parameters: Vec<String>,
    body: Expr,
}

impl From<&ast::TrapHandler> for TrapHandler {
    fn from(handler: &ast::TrapHandler) -> Self {
        Self {
            parameters: handler.params.clone(),
            body: Expr::from(&handler.body),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Named declaration parameter with an optional value.
struct Parameter {
    name: String,
    #[serde(rename = "type")]
    ty: Type,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Expr")]
    value: Option<Expr>,
}

fn parameters(params: &HashMap<String, (AstType, Option<ast::Expr>)>) -> Vec<Parameter> {
    let mut result = params
        .iter()
        .map(|(name, (ty, value))| Parameter {
            name: name.clone(),
            ty: Type::from(ty),
            value: value.as_ref().map(Expr::from),
        })
        .collect::<Vec<_>>();
    result.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
    result
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Named instruction operand.
struct Operand {
    name: String,
    #[serde(rename = "type")]
    ty: Type,
}

fn operands(values: &[(String, AstType)]) -> Vec<Operand> {
    values
        .iter()
        .map(|(name, ty)| Operand {
            name: name.clone(),
            ty: Type::from(ty),
        })
        .collect()
}

fn schedule_classes(schedule: &Option<ast::Schedule>) -> Vec<String> {
    schedule
        .as_ref()
        .map(|schedule| schedule.classes.clone())
        .unwrap_or_default()
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// A single register or an inclusive register-name range.
enum RegisterDef {
    Single {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        alias: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "u16")]
        index: Option<u16>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        traits: Vec<RegisterTrait>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        subregisters: Vec<Register>,
    },
    Range {
        start: String,
        end: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        alias_pattern: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        traits: Vec<RegisterTrait>,
    },
}

impl From<&ast::RegisterDef> for RegisterDef {
    fn from(register: &ast::RegisterDef) -> Self {
        match register {
            ast::RegisterDef::Single(register) => Self::Single {
                name: register.name.clone(),
                alias: register.alias.clone(),
                index: register.index,
                traits: register.traits.iter().map(RegisterTrait::from).collect(),
                subregisters: register.subregisters.iter().map(Register::from).collect(),
            },
            ast::RegisterDef::Range(range) => Self::Range {
                start: range.start.clone(),
                end: range.end.clone(),
                alias_pattern: range.alias_pattern.clone(),
                traits: range.traits.iter().map(RegisterTrait::from).collect(),
            },
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// A nested subregister declaration.
struct Register {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "u16")]
    index: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    traits: Vec<RegisterTrait>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subregisters: Vec<Register>,
}

impl From<&ast::Register> for Register {
    fn from(register: &ast::Register) -> Self {
        Self {
            name: register.name.clone(),
            alias: register.alias.clone(),
            index: register.index,
            traits: register.traits.iter().map(RegisterTrait::from).collect(),
            subregisters: register.subregisters.iter().map(Register::from).collect(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
/// Semantic and ABI traits attached to a register.
enum RegisterTrait {
    HardwiredZero,
    ProgramCounter,
    StatusFlag,
    Float,
    Polymorphic,
}

impl From<&ast::RegisterTrait> for RegisterTrait {
    fn from(trait_: &ast::RegisterTrait) -> Self {
        match trait_ {
            ast::RegisterTrait::HardwiredZero => Self::HardwiredZero,
            ast::RegisterTrait::ProgramCounter => Self::ProgramCounter,
            ast::RegisterTrait::StatusFlag => Self::StatusFlag,
            ast::RegisterTrait::Float => Self::Float,
            ast::RegisterTrait::Polymorphic => Self::Polymorphic,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
/// A type allowed in a checked TMDL declaration.
enum Type {
    /// TMDL `String`.
    String,
    /// TMDL `Integer`.
    Integer,
    /// A fixed-width bit vector.
    Bits { width: u16 },
    /// A bit vector whose width is an ISA-parameter expression.
    BitsExpr { width: Box<Expr> },
    /// A named register class or other structural type.
    Named { name: String },
}

impl From<&AstType> for Type {
    fn from(ty: &AstType) -> Self {
        match ty {
            AstType::String => Self::String,
            AstType::Integer => Self::Integer,
            AstType::Bits(width) => Self::Bits { width: *width },
            AstType::BitsExpr(width) => Self::BitsExpr {
                width: Box::new(Expr::from(width.as_ref())),
            },
            AstType::Struct(name) => Self::Named { name: name.clone() },
            AstType::Var(_) | AstType::Fn(_, _) | AstType::Con(_, _) => {
                unreachable!("inferred types are not part of the checked AST output")
            }
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// One inclusive bit range in an instruction encoding.
struct EncodingArm {
    /// Lowest encoded bit.
    start: u16,
    /// Highest encoded bit; omitted for a single-bit arm.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "u16")]
    end: Option<u16>,
    value: Expr,
}

impl From<&ast::EncodingArm> for EncodingArm {
    fn from(arm: &ast::EncodingArm) -> Self {
        Self {
            start: arm.start,
            end: arm.end,
            value: Expr::from(&arm.value),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// A named buffer or physical-register-file size.
struct NamedCount {
    name: String,
    count: i64,
}

fn named_counts(values: &[(String, i64)]) -> Vec<NamedCount> {
    values
        .iter()
        .map(|(name, count)| NamedCount {
            name: name.clone(),
            count: *count,
        })
        .collect()
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// One ordered phase in a machine pipeline.
struct PipelinePhase {
    name: String,
    protection: Protection,
}

impl From<&ast::PipelinePhase> for PipelinePhase {
    fn from(phase: &ast::PipelinePhase) -> Self {
        Self {
            name: phase.name.clone(),
            protection: Protection::from(phase.protection),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
/// Hardware hazard protection for a pipeline phase.
enum Protection {
    Protected,
    Unprotected,
    Hard,
}

impl From<ast::Protection> for Protection {
    fn from(protection: ast::Protection) -> Self {
        match protection {
            ast::Protection::Protected => Self::Protected,
            ast::Protection::Unprotected => Self::Unprotected,
            ast::Protection::Hard => Self::Hard,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// A functional resource and its parallel unit count.
struct MachineUnit {
    name: String,
    units: i64,
}

impl From<&ast::MachineUnit> for MachineUnit {
    fn from(unit: &ast::MachineUnit) -> Self {
        Self {
            name: unit.name.clone(),
            units: unit.units,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Machine-specific timing and resource use for a scheduling class.
struct UnitBind {
    unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "i64")]
    latency: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "i64")]
    throughput: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    reads: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    writes: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uses: Vec<String>,
}

impl From<&ast::UnitBind> for UnitBind {
    fn from(binding: &ast::UnitBind) -> Self {
        Self {
            unit: binding.unit.clone(),
            latency: binding.latency,
            throughput: binding.throughput,
            reads: binding.reads.clone(),
            writes: binding.writes.clone(),
            uses: binding.uses.clone(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Machine-specific timing override for one instruction.
struct MachineOverride {
    instruction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "i64")]
    latency: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "i64")]
    throughput: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    reads: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    writes: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uses: Vec<String>,
}

impl From<&ast::MachineOverride> for MachineOverride {
    fn from(override_: &ast::MachineOverride) -> Self {
        Self {
            instruction: override_.instruction.clone(),
            latency: override_.latency,
            throughput: override_.throughput,
            reads: override_.reads.clone(),
            writes: override_.writes.clone(),
            uses: override_.uses.clone(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// A resource forwarding path and its latency.
struct Forward {
    from: String,
    to: String,
    latency: i64,
}

impl From<&ast::Forward> for Forward {
    fn from(forward: &ast::Forward) -> Self {
        Self {
            from: forward.from.clone(),
            to: forward.to.clone(),
            latency: forward.latency,
        }
    }
}
