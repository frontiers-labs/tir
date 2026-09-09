use std::any::Any;
use std::sync::Arc;

use tir::parse::common::Cursor;
use tir::{
    Context, Error, ExitScope, ExitScopeKind, ExitTarget, IRFormatter, NonLocalExit, Operation,
    Terminator, TirType, Type, TypeConstraint, TypeId, dialect, operation, parse::Span,
};

pub mod ops {
    pub use super::{
        BreakOp, ConditionOp, ContinueOp, CopyStructOp, DefineStructOp, DoOp, ForOp, GetMemberOp,
        VaArgOp, VaEndOp, VaStartOp, WhileOp, YieldOp, r#break, condition, r#continue, copy_struct,
        define_struct, r#do, r#for, get_member, va_arg, va_end, va_start, r#while, r#yield,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
            DefineStructOp,
            GetMemberOp,
            CopyStructOp,
            VaStartOp,
            VaArgOp,
            VaEndOp,
            ForOp,
            WhileOp,
            DoOp,
            ConditionOp,
            YieldOp,
            BreakOp,
            ContinueOp,
        ],
        types: [StructType, VarArgsType, VaListType],
    }
}

pub struct StructType {
    name: String,
}

impl StructType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, name: impl Into<String>) -> TypeId {
        context.get_type_id(Arc::new(Self { name: name.into() }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl TypeConstraint for StructType {}

impl Type for StructType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "struct"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        if !parser.parse_token("<") {
            return Err((parser.span(), Error::ExpectedToken("<")));
        }
        let name = parser
            .parse_string()
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("struct name")))?;
        if !parser.parse_token(">") {
            return Err((parser.span(), Error::ExpectedToken(">")));
        }
        Ok(Self::new(context, name))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write(format!("struct<\"{}\">", self.name))
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any)
            .downcast_ref::<StructType>()
            .is_some_and(|other| other.name == self.name)
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write(self.name.as_bytes());
    }
}

operation! {
    DefineStructOp {
        name: "define_struct",
        dialect: "cir",
        attributes: A {
            sym_name: "Str",
            fields: "Array",
            size: "UInt",
            align: "UInt",
        },
    }
}

operation! {
    GetMemberOp {
        name: "get_member",
        dialect: "cir",
        operands: O {
            base: "tir::ptr::PtrType",
        },
        attributes: A {
            field: "UInt",
            struct_name: "Str",
        },
        results: R {
            result: "tir::ptr::PtrType",
        },
    }
}

operation! {
    CopyStructOp {
        name: "copy_struct",
        dialect: "cir",
        operands: O {
            destination: "tir::ptr::PtrType",
            source: "tir::ptr::PtrType",
        },
        attributes: A {
            struct_name: "Str",
        },
    }
}

#[derive(TirType)]
#[tir_type(dialect = "cir", name = "varargs")]
pub struct VarArgsType;

impl VarArgsType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for VarArgsType {}

impl Type for VarArgsType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "varargs"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("varargs")
    }

    fn is_variadic_tail(&self) -> bool {
        true
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<VarArgsType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

#[derive(TirType)]
#[tir_type(dialect = "cir", name = "va_list")]
pub struct VaListType;

impl VaListType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for VaListType {}

impl Type for VaListType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "va_list"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("va_list")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<VaListType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

operation! {
    VaStartOp {
        name: "va_start",
        dialect: "cir",
        results: R {
            result: "crate::cir::VaListType",
        },
    }
}

operation! {
    VaArgOp {
        name: "va_arg",
        dialect: "cir",
        operands: O {
            list: "crate::cir::VaListType",
        },
        results: R {
            result: "tir::Any",
        },
    }
}

operation! {
    VaEndOp {
        name: "va_end",
        dialect: "cir",
        operands: O {
            list: "crate::cir::VaListType",
        },
    }
}

// C's loop statements, captured as they are written. The regions are CFG-form and
// carry no ports: dataflow stays in the stack slots codegen emits, so what these ops
// add is structure — which region is the condition, which the step, which the body —
// for the `raise-loops` pass to reason about. Every loop it refuses is flattened to
// the same blocks and branches codegen used to emit directly.

operation! {
    ForOp {
        name: "for",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            condition_region: Region {},
            step_region: Region {},
            body_region: Region {},
        },
        interfaces: [ExitScope],
    }
}

impl ExitScope for ForOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }

    fn label(&self) -> Option<String> {
        loop_label(self)
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_loop_region(context, self, 0, RegionRole::Condition, "cir.for condition")?;
        verify_loop_region(context, self, 1, RegionRole::Step, "cir.for step")?;
        verify_loop_region(context, self, 2, RegionRole::Body, "cir.for body")
    }
}

impl ForOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        print_loop_regions(fmt, self, "cir.for", &["cond", "step", "body"])
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let label = parse_loop_label(parser)?;
        let condition = parse_loop_region(parser, context, "cond")?;
        let step = parse_loop_region(parser, context, "step")?;
        let body = parse_loop_region(parser, context, "body")?;
        let mut builder = ForOpBuilder::new(context)
            .condition_region(condition)
            .step_region(step)
            .body_region(body);
        if let Some(label) = label_attribute(label) {
            builder = builder.attr("label", label);
        }
        Ok(Box::new(builder.build()))
    }
}

operation! {
    WhileOp {
        name: "while",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            condition_region: Region {},
            body_region: Region {},
        },
        interfaces: [ExitScope],
    }
}

impl ExitScope for WhileOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }

    fn label(&self) -> Option<String> {
        loop_label(self)
    }
}

impl tir::Verifiable for WhileOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_loop_region(
            context,
            self,
            0,
            RegionRole::Condition,
            "cir.while condition",
        )?;
        verify_loop_region(context, self, 1, RegionRole::Body, "cir.while body")
    }
}

impl WhileOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        print_loop_regions(fmt, self, "cir.while", &["cond", "body"])
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let label = parse_loop_label(parser)?;
        let condition = parse_loop_region(parser, context, "cond")?;
        let body = parse_loop_region(parser, context, "body")?;
        let mut builder = WhileOpBuilder::new(context)
            .condition_region(condition)
            .body_region(body);
        if let Some(label) = label_attribute(label) {
            builder = builder.attr("label", label);
        }
        Ok(Box::new(builder.build()))
    }
}

operation! {
    DoOp {
        name: "do",
        dialect: "cir",
        format: "custom",
        verifier: "true",
        regions: R {
            body_region: Region {},
            condition_region: Region {},
        },
        interfaces: [ExitScope],
    }
}

impl ExitScope for DoOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }

    fn label(&self) -> Option<String> {
        loop_label(self)
    }
}

impl tir::Verifiable for DoOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_loop_region(context, self, 0, RegionRole::Body, "cir.do body")?;
        verify_loop_region(context, self, 1, RegionRole::Condition, "cir.do condition")
    }
}

impl DoOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        print_loop_regions(fmt, self, "cir.do", &["body", "cond"])
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let label = parse_loop_label(parser)?;
        let body = parse_loop_region(parser, context, "body")?;
        let condition = parse_loop_region(parser, context, "cond")?;
        let mut builder = DoOpBuilder::new(context)
            .body_region(body)
            .condition_region(condition);
        if let Some(label) = label_attribute(label) {
            builder = builder.attr("label", label);
        }
        Ok(Box::new(builder.build()))
    }
}

operation! {
    ConditionOp {
        name: "condition",
        dialect: "cir",
        operands: O {
            condition: "tir::Integer<1>",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ConditionOp {}

operation! {
    YieldOp {
        name: "yield",
        dialect: "cir",
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}

operation! {
    BreakOp {
        name: "break",
        dialect: "cir",
        interfaces: [Terminator, NonLocalExit],
    }
}

impl Terminator for BreakOp {}

/// A `cir.break` leaves the innermost loop, or the loop carrying its `label`;
/// a `switch` break is a plain branch, so no `cir.break` names a switch. C
/// keeps its dataflow in stack slots, so an exit carries no values.
impl NonLocalExit for BreakOp {
    fn target(&self) -> ExitTarget {
        exit_target(self)
    }

    fn values(&self) -> Vec<tir::ValueId> {
        Vec::new()
    }
}

operation! {
    ContinueOp {
        name: "continue",
        dialect: "cir",
        interfaces: [Terminator, NonLocalExit],
    }
}

impl Terminator for ContinueOp {}

impl NonLocalExit for ContinueOp {
    fn target(&self) -> ExitTarget {
        exit_target(self)
    }

    fn values(&self) -> Vec<tir::ValueId> {
        Vec::new()
    }
}

/// The `label` attribute an exit or a loop carries, if any.
fn loop_label(op: &impl Operation) -> Option<String> {
    match op.attr("label")? {
        tir::attributes::AttributeValue::Str(label) => Some(label.to_string()),
        _ => None,
    }
}

fn exit_target(op: &impl Operation) -> ExitTarget {
    match loop_label(op) {
        Some(label) => ExitTarget::Label(label),
        None => ExitTarget::InnermostLoop,
    }
}

/// What a region of a `cir` loop is, and so which terminators may leave it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RegionRole {
    /// Decides whether the next iteration runs; leaves through `cir.condition`.
    Condition,
    /// Runs between iterations; falls through to the condition.
    Step,
    /// Leaves through a fallthrough, or through `break`/`continue`.
    Body,
}

impl RegionRole {
    fn admits(self, exit: &tir::OpHandle) -> bool {
        match self {
            RegionRole::Condition => exit.is::<ConditionOp>(),
            RegionRole::Step => exit.is::<YieldOp>(),
            RegionRole::Body => {
                exit.is::<YieldOp>() || exit.is::<BreakOp>() || exit.is::<ContinueOp>()
            }
        }
    }

    fn expected(self) -> &'static str {
        match self {
            RegionRole::Condition => "cir.condition",
            RegionRole::Step => "cir.yield",
            RegionRole::Body => "cir.yield, cir.break or cir.continue",
        }
    }
}

/// A loop region holds ordinary CFG: a C condition or step spelling `&&` or `?:`
/// branches within the region, so only the blocks that leave it are constrained, and
/// each must leave the way its role says.
fn verify_loop_region(
    context: &Context,
    op: &impl Operation,
    index: usize,
    role: RegionRole,
    label: &str,
) -> Result<(), Error> {
    let region = context.get_region(op.regions().nth(index).unwrap().id());
    let mut exits = 0;
    for block in region.iter(context.clone()) {
        let Some(&last) = block.op_ids().last() else {
            return Err(Error::VerificationError(format!(
                "{label} has a block with no terminator"
            )));
        };
        let last = context.get_op(last);
        if last
            .clone()
            .as_interface::<dyn tir::BranchTerminator>()
            .is_some()
        {
            continue;
        }
        if !role.admits(&last) {
            return Err(Error::VerificationError(format!(
                "{label} must leave through {}",
                role.expected()
            )));
        }
        exits += 1;
    }
    if exits == 0 {
        return Err(Error::VerificationError(format!(
            "{label} must leave through {}",
            role.expected()
        )));
    }
    Ok(())
}

/// Print `cir.for {label = "l"} cond { .. } step { .. } body { .. }`: the
/// label when the loop carries one, then one keyword per region, in the order
/// the op declares them.
fn print_loop_regions(
    fmt: &mut IRFormatter,
    op: &impl Operation,
    mnemonic: &str,
    keywords: &[&str],
) -> Result<(), std::fmt::Error> {
    let context = op.handle().context.clone();
    fmt.write(mnemonic)?;
    if let Some(label) = loop_label(op) {
        fmt.write(format!(" {{label = \"{label}\"}}"))?;
    }
    for (index, keyword) in keywords.iter().enumerate() {
        fmt.write(format!(" {keyword}"))?;
        tir::region_format::print_op_region(fmt, &context, op, index)?;
    }
    Ok(())
}

/// Parse an optional `{label = "l"}` clause into the builder's attribute.
fn parse_loop_label(
    parser: &mut tir::parse::text::Parser,
) -> Result<Option<String>, (Span, Error)> {
    if !parser.parse_token("{") {
        return Ok(None);
    }
    if parser.parse_ident() != Some("label") || !parser.parse_token("=") {
        return Err((parser.span(), Error::ExpectedToken("label")));
    }
    let label = parser
        .parse_string()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("label")))?
        .to_string();
    if !parser.parse_token("}") {
        return Err((parser.span(), Error::ExpectedToken("}")));
    }
    Ok(Some(label))
}

fn label_attribute(label: Option<String>) -> Option<tir::attributes::AttributeValue> {
    label.map(|label| tir::attributes::AttributeValue::Str(label.into()))
}

/// Parse one `<keyword> { .. }` region clause.
fn parse_loop_region(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    keyword: &'static str,
) -> Result<tir::RegionId, (Span, Error)> {
    if !parser.parse_token(keyword) {
        return Err((parser.span(), Error::ExpectedToken(keyword)));
    }
    Ok(parser.parse_region(context)?.id())
}
