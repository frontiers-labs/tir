use schemars::JsonSchema;
use serde::Serialize;

use crate::ast;

use super::expr::Expr;

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// ABI stack layout and save convention.
pub(super) struct AbiStack {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Expr")]
    align: Option<Expr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "AbiStackGrowth")]
    growth: Option<AbiStackGrowth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Expr")]
    red_zone: Option<Expr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Expr")]
    slot_size: Option<Expr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "AbiSaveStyle")]
    save_style: Option<AbiSaveStyle>,
}

impl From<&ast::AbiStack> for AbiStack {
    fn from(stack: &ast::AbiStack) -> Self {
        Self {
            align: stack.align.as_ref().map(Expr::from),
            growth: stack.grows.map(AbiStackGrowth::from),
            red_zone: stack.red_zone.as_ref().map(Expr::from),
            slot_size: stack.slot_size.as_ref().map(Expr::from),
            save_style: stack.save_style.map(AbiSaveStyle::from),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Named ABI role such as the stack pointer or return address.
pub(super) struct AbiRole {
    name: String,
    register: AbiRegister,
}

impl From<&ast::AbiRole> for AbiRole {
    fn from(role: &ast::AbiRole) -> Self {
        Self {
            name: role.name.clone(),
            register: AbiRegister::from(&role.register),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Register identified by register class and architectural name.
pub(super) struct AbiRegister {
    class: String,
    name: String,
}

impl From<&ast::AbiRegister> for AbiRegister {
    fn from(register: &ast::AbiRegister) -> Self {
        Self {
            class: register.class.clone(),
            name: register.name.clone(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// A single ABI register or an inclusive register range.
pub(super) struct AbiRegisterSequence {
    start: AbiRegister,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "AbiRegister")]
    end: Option<AbiRegister>,
}

impl From<&ast::AbiRegisterSequence> for AbiRegisterSequence {
    fn from(sequence: &ast::AbiRegisterSequence) -> Self {
        Self {
            start: AbiRegister::from(&sequence.start),
            end: sequence.end.as_ref().map(AbiRegister::from),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
/// Ordered registers used to pass one kind of ABI value.
pub(super) struct AbiPassSequence {
    value_kind: AbiValueKind,
    registers: Vec<AbiRegisterSequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "AbiOverflow")]
    overflow: Option<AbiOverflow>,
}

impl From<&ast::AbiPassSequence> for AbiPassSequence {
    fn from(sequence: &ast::AbiPassSequence) -> Self {
        Self {
            value_kind: AbiValueKind::from(sequence.kind),
            registers: sequence
                .registers
                .iter()
                .map(AbiRegisterSequence::from)
                .collect(),
            overflow: sequence.overflow.map(AbiOverflow::from),
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
/// ABI value category used for argument and result classification.
pub(super) enum AbiValueKind {
    Int,
    Float,
    Vector,
}

impl From<ast::AbiValueKind> for AbiValueKind {
    fn from(kind: ast::AbiValueKind) -> Self {
        match kind {
            ast::AbiValueKind::Int => Self::Int,
            ast::AbiValueKind::Float => Self::Float,
            ast::AbiValueKind::Vector => Self::Vector,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
/// Destination used after a pass sequence exhausts its registers.
pub(super) enum AbiOverflow {
    Int,
    Float,
    Vector,
    Stack,
}

impl From<ast::AbiOverflow> for AbiOverflow {
    fn from(overflow: ast::AbiOverflow) -> Self {
        match overflow {
            ast::AbiOverflow::Kind(kind) => match kind {
                ast::AbiValueKind::Int => Self::Int,
                ast::AbiValueKind::Float => Self::Float,
                ast::AbiValueKind::Vector => Self::Vector,
            },
            ast::AbiOverflow::Stack => Self::Stack,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
/// Direction in which the ABI stack grows.
enum AbiStackGrowth {
    Down,
    Up,
}

impl From<ast::AbiStackGrowth> for AbiStackGrowth {
    fn from(growth: ast::AbiStackGrowth) -> Self {
        match growth {
            ast::AbiStackGrowth::Down => Self::Down,
            ast::AbiStackGrowth::Up => Self::Up,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
/// Mechanism used to save callee-saved registers.
enum AbiSaveStyle {
    FrameSlots,
    PushPop,
}

impl From<ast::AbiSaveStyle> for AbiSaveStyle {
    fn from(style: ast::AbiSaveStyle) -> Self {
        match style {
            ast::AbiSaveStyle::FrameSlots => Self::FrameSlots,
            ast::AbiSaveStyle::PushPop => Self::PushPop,
        }
    }
}
