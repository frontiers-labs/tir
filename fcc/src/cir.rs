use tir::parse::common::Cursor;
use tir::{
    Context, Ctor, Error, ExitScope, ExitScopeKind, ExitTarget, IRFormatter, Kind, NonLocalExit,
    Operation, Terminator, Terms, TypeCtor, TypeId, TypeSpelling, dialect, parse::Span,
};
use tir_symbolic::term::{no_carrier, valid};

include!(concat!(env!("OUT_DIR"), "/cir.rs"));

pub mod ops {
    pub use super::{
        BreakOp, ConditionOp, ContinueOp, DoOp, ForOp, VaArgOp, VaEndOp, VaStartOp, WhileOp,
        YieldOp, r#break, condition, r#continue, r#do, r#for, va_arg, va_end, va_start, r#while,
        r#yield,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
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
        types: [VarArgsType, VaListType],
    }
}

macro_rules! unit_type {
    ($name:ident, $ctor:ident, $mnemonic:literal, $variadic_tail:literal, $doc:literal) => {
        static $ctor: Ctor = Ctor::new(TypeCtor {
            dialect: "cir",
            name: $mnemonic,
            params: &[],
            kind: Kind::Data,
            validate: valid,
            carrier: no_carrier,
        });

        #[doc = $doc]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name;

        impl $name {
            #[allow(clippy::new_ret_no_self)]
            pub fn new(context: &Context) -> TypeId {
                context
                    .terms()
                    .intern($ctor.id(context), vec![])
                    .expect($mnemonic)
            }

            pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
                $ctor.is(terms, ty).then_some(Self)
            }

            pub fn spelling(context: &Context) -> TypeSpelling {
                TypeSpelling {
                    ctor: $ctor.id(context),
                    keys: &[$mnemonic],
                    parse: |_, _, context| Ok(Self::new(context)),
                    print: |_, _, fmt| fmt.write(concat!("cir.", $mnemonic)),
                    variadic_tail: $variadic_tail,
                }
            }
        }
    };
}

unit_type!(
    VarArgsType,
    VARARGS,
    "varargs",
    true,
    "The tail of a variadic signature, written `!cir.varargs`."
);
unit_type!(
    VaListType,
    VA_LIST,
    "va_list",
    false,
    "A C `va_list`, written `!cir.va_list`."
);

__cdsl_cir_VaStartOp!();

__cdsl_cir_VaArgOp!();

__cdsl_cir_VaEndOp!();

// C's loop statements, captured as they are written. The regions are CFG-form and
// carry no ports: dataflow stays in the stack slots codegen emits, so what these ops
// add is structure — which region is the condition, which the step, which the body —
// for the `raise-loops` pass to reason about. Every loop it refuses is flattened to
// the same blocks and branches codegen used to emit directly.

__cdsl_cir_ForOp!();

impl ExitScope for ForOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }

    fn label(&self) -> Option<String> {
        loop_label(self)
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

__cdsl_cir_WhileOp!();

impl ExitScope for WhileOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }

    fn label(&self) -> Option<String> {
        loop_label(self)
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

__cdsl_cir_DoOp!();

impl ExitScope for DoOp {
    fn exit_scope(&self) -> ExitScopeKind {
        ExitScopeKind::Loop
    }

    fn label(&self) -> Option<String> {
        loop_label(self)
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

__cdsl_cir_ConditionOp!();

impl Terminator for ConditionOp {}

__cdsl_cir_YieldOp!();

impl Terminator for YieldOp {}

__cdsl_cir_BreakOp!();

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

__cdsl_cir_ContinueOp!();

impl Terminator for ContinueOp {}

impl NonLocalExit for ContinueOp {
    fn target(&self) -> ExitTarget {
        exit_target(self)
    }

    fn values(&self) -> Vec<tir::ValueId> {
        Vec::new()
    }
}

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
