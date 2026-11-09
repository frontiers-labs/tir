use tir::parse::common::Cursor;
use tir::{
    Context, Ctor, Error, ExitScope, ExitScopeKind, ExitTarget, IRFormatter, NonLocalExit,
    OpHandle, Terminator, Terms, TypeId, TypeSpelling, dialect, parse::Span,
};
use tir_symbolic::term::{no_carrier, valid};

pub mod ops {
    pub use super::{
        BREAK, CONDITION, CONTINUE, DO, FOR, VA_ARG, VA_END, VA_START, WHILE, YIELD, r#break,
        condition, r#continue, r#do, r#for, va_arg, va_end, va_start, r#while, r#yield,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
            VA_START,
            VA_ARG,
            VA_END,
            FOR,
            WHILE,
            DO,
            CONDITION,
            YIELD,
            BREAK,
            CONTINUE,
        ],
        types: [VarArgsType, VaListType],
    }
}

static VARARGS: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "../core/defs/types/fcc_src_cir.cdsl", binding: constructors::VARARGS, hosts: { VARARGS_validate => valid, VARARGS_carrier => no_carrier } },
);
#[doc = "The tail of a variadic signature, written `!cir.varargs`."]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarArgsType;
impl VarArgsType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context
            .terms()
            .intern(VARARGS.id(context), vec![])
            .expect("varargs")
    }
    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        VARARGS.is(terms, ty).then_some(Self)
    }
    pub fn spelling(context: &Context) -> TypeSpelling {
        ::tir::helpers::type_spelling! { file: "../core/defs/types/fcc_src_cir.cdsl", binding: spellings::VarArgsType, context: context, hosts: { VarArgsType_parse => |_, _, context| Ok(VarArgsType::new(context)), VarArgsType_print => |_, _, fmt| fmt.write(concat!("cir.", "varargs")), VarArgsType_ctor_id => |context| { VARARGS.id(context) } } }
    }
}
static VA_LIST: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "../core/defs/types/fcc_src_cir.cdsl", binding: constructors::VA_LIST, hosts: { VA_LIST_validate => valid, VA_LIST_carrier => no_carrier } },
);
#[doc = "A C `va_list`, written `!cir.va_list`."]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaListType;
impl VaListType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context
            .terms()
            .intern(VA_LIST.id(context), vec![])
            .expect("va_list")
    }
    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        VA_LIST.is(terms, ty).then_some(Self)
    }
    pub fn spelling(context: &Context) -> TypeSpelling {
        ::tir::helpers::type_spelling! { file: "../core/defs/types/fcc_src_cir.cdsl", binding: spellings::VaListType, context: context, hosts: { VaListType_parse => |_, _, context| Ok(VaListType::new(context)), VaListType_print => |_, _, fmt| fmt.write(concat!("cir.", "va_list")), VaListType_ctor_id => |context| { VA_LIST.id(context) } } }
    }
}

tir::helpers::op_def! {
    file: "../core/defs/converted-cir/cir.cdsl",
    hosts: {
        for_verifier => for_verify,
        for_iface_ExitScope => (ExitScope, &CIR_LOOP_EXIT_SCOPE),
        for_printer => for_print,
        for_parser => |parser, context, definition| parse_loop(parser, context, definition, &["cond", "step", "body"]),
        while_verifier => while_verify,
        while_iface_ExitScope => (ExitScope, &CIR_LOOP_EXIT_SCOPE),
        while_printer => while_print,
        while_parser => |parser, context, definition| parse_loop(parser, context, definition, &["cond", "body"]),
        do_verifier => do_verify,
        do_iface_ExitScope => (ExitScope, &CIR_LOOP_EXIT_SCOPE),
        do_printer => do_print,
        do_parser => |parser, context, definition| parse_loop(parser, context, definition, &["body", "cond"]),
        condition_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        yield_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        break_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        break_iface_NonLocalExit => (NonLocalExit, &CIR_NON_LOCAL_EXIT),
        continue_iface_Terminator => (Terminator, &tir::TerminatorCallbacks::DEFAULT),
        continue_iface_NonLocalExit => (NonLocalExit, &CIR_NON_LOCAL_EXIT),
    },
}

// C's loop statements, captured as they are written. The regions are CFG-form and
// carry no ports: dataflow stays in the stack slots codegen emits, so what these ops
// add is structure — which region is the condition, which the step, which the body —
// for the `raise-loops` pass to reason about. Every loop it refuses is flattened to
// the same blocks and branches codegen used to emit directly.

fn for_verify(operation: &OpHandle, context: &Context) -> Result<(), Error> {
    verify_loop_region(
        context,
        operation,
        0,
        RegionRole::Condition,
        "cir.for condition",
    )?;
    verify_loop_region(context, operation, 1, RegionRole::Step, "cir.for step")?;
    verify_loop_region(context, operation, 2, RegionRole::Body, "cir.for body")
}

fn for_print(operation: &OpHandle, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
    print_loop_regions(fmt, operation, "cir.for", &["cond", "step", "body"])
}

fn while_verify(operation: &OpHandle, context: &Context) -> Result<(), Error> {
    verify_loop_region(
        context,
        operation,
        0,
        RegionRole::Condition,
        "cir.while condition",
    )?;
    verify_loop_region(context, operation, 1, RegionRole::Body, "cir.while body")
}

fn while_print(operation: &OpHandle, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
    print_loop_regions(fmt, operation, "cir.while", &["cond", "body"])
}

fn do_verify(operation: &OpHandle, context: &Context) -> Result<(), Error> {
    verify_loop_region(context, operation, 0, RegionRole::Body, "cir.do body")?;
    verify_loop_region(
        context,
        operation,
        1,
        RegionRole::Condition,
        "cir.do condition",
    )
}

fn do_print(operation: &OpHandle, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
    print_loop_regions(fmt, operation, "cir.do", &["body", "cond"])
}

fn parse_loop(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    definition: &'static tir::OpDefSpec,
    labels: &[&'static str],
) -> Result<OpHandle, (Span, Error)> {
    let label = parse_loop_label(parser)?;
    let regions = labels
        .iter()
        .map(|name| parse_loop_region(parser, context, name))
        .collect::<Result<Vec<_>, _>>()?;
    let attributes = label_attribute(label)
        .map(|label| context.named_attribute("label", label))
        .into_iter()
        .collect();
    let operation = tir::NewOp::for_definition(
        definition,
        context.clone(),
        vec![],
        vec![],
        regions,
        tir::NewOpParts { attributes },
    );
    Ok(context.add_operation(tir::InsertionPoint::Root, operation))
}

/// A `cir.break` leaves the innermost loop, or the loop carrying its `label`;
/// a `switch` break is a plain branch, so no `cir.break` names a switch. C
/// keeps its dataflow in stack slots, so an exit carries no values.
const CIR_NON_LOCAL_EXIT: tir::NonLocalExitCallbacks = tir::NonLocalExitCallbacks {
    target: exit_target,
    values: |_| Vec::new(),
};

const CIR_LOOP_EXIT_SCOPE: tir::ExitScopeCallbacks = tir::ExitScopeCallbacks {
    exit_scope: |_| ExitScopeKind::Loop,
    label: Some(loop_label),
};

fn loop_label(op: &OpHandle) -> Option<String> {
    match op.attr("label")? {
        tir::attributes::AttributeValue::Str(label) => Some(label.to_string()),
        _ => None,
    }
}

fn exit_target(op: &OpHandle) -> ExitTarget {
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
            RegionRole::Condition => exit.is_operation(&CONDITION),
            RegionRole::Step => exit.is_operation(&YIELD),
            RegionRole::Body => {
                exit.is_operation(&YIELD)
                    || exit.is_operation(&BREAK)
                    || exit.is_operation(&CONTINUE)
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

fn verify_loop_region(
    context: &Context,
    op: &OpHandle,
    index: usize,
    role: RegionRole,
    label: &str,
) -> Result<(), Error> {
    let region = context.get_region(op.regions()[index]);
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

fn print_loop_regions(
    fmt: &mut IRFormatter,
    op: &OpHandle,
    mnemonic: &str,
    keywords: &[&str],
) -> Result<(), std::fmt::Error> {
    let context = op.context.clone();
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
