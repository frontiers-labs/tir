use crate::attributes::AttributeValue;
use crate::{
    BlockId, BranchGuard, BranchTerminator, CaseGuard, Context, Error, Terminator, ValueId, dialect,
};

use crate as tir;

pub mod ops {
    pub use super::{br, cond_br, switch};
}

dialect! {
    CfgDialect {
        name: "cfg",
        operations: [BR, COND_BR, SWITCH],
        types: [],
    }
}

tir::helpers::op_def! {
    file: "defs/converted/dialects_cfg_mod.cdsl",
    hosts: {
        br_iface_Terminator => (Terminator, &crate::cfg::__TIR_IFACE_BranchOp_Terminator),
        br_iface_BranchTerminator => (BranchTerminator, &crate::cfg::__TIR_IFACE_BranchOp_BranchTerminator),
        br_printer => crate::cfg::br_custom_print,
        br_parser => |parser, context, _definition| crate::cfg::br_custom_parse(parser, context),
        cond_br_iface_Terminator => (Terminator, &crate::cfg::__TIR_IFACE_CondBranchOp_Terminator),
        cond_br_iface_BranchTerminator => (BranchTerminator, &crate::cfg::__TIR_IFACE_CondBranchOp_BranchTerminator),
        cond_br_iface_BranchGuard => (BranchGuard, &crate::cfg::__TIR_IFACE_CondBranchOp_BranchGuard),
        cond_br_printer => crate::cfg::cond_br_custom_print,
        cond_br_parser => |parser, context, _definition| crate::cfg::cond_br_custom_parse(parser, context),
        switch_verifier => crate::cfg::switch_verify_impl,
        switch_iface_Terminator => (Terminator, &crate::cfg::__TIR_IFACE_SwitchOp_Terminator),
        switch_iface_BranchTerminator => (BranchTerminator, &crate::cfg::__TIR_IFACE_SwitchOp_BranchTerminator),
        switch_iface_CaseGuard => (CaseGuard, &crate::cfg::__TIR_IFACE_SwitchOp_CaseGuard),
        switch_printer => crate::cfg::switch_custom_print,
        switch_parser => |parser, context, _definition| crate::cfg::switch_custom_parse(parser, context),
    },
}

pub(crate) fn br_terminator_successors(operation: &tir::OpHandle) -> Vec<BlockId> {
    vec![crate::cfg::br_dest(operation)]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_BranchOp_Terminator: crate::TerminatorCallbacks =
    crate::TerminatorCallbacks {
        successors: Some(crate::cfg::br_terminator_successors),
    };

pub(crate) fn br_branchterminator_successor_operand_slots(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, Vec<usize>)> {
    vec![(
        crate::cfg::br_dest(operation),
        (0..operation.operands().len()).collect(),
    )]
}

pub(crate) fn br_branchterminator_successor_operands(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, Vec<ValueId>)> {
    vec![(
        crate::cfg::br_dest(operation),
        crate::cfg::br_dest_args(operation),
    )]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_BranchOp_BranchTerminator: crate::BranchTerminatorCallbacks =
    crate::BranchTerminatorCallbacks {
        successor_operands: crate::cfg::br_branchterminator_successor_operands,
        successor_operand_slots: crate::cfg::br_branchterminator_successor_operand_slots,
    };

/// The values forwarded to the destination block's arguments.
pub fn br_dest_args(operation: &tir::OpHandle) -> Vec<ValueId> {
    operation.operands().to_vec()
}

pub(crate) fn br_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    fmt.write("cfg.br ")?;
    print_successor(
        fmt,
        &context,
        crate::cfg::br_dest(operation),
        &crate::cfg::br_dest_args(operation),
    )?;
    fmt.write("\n")
}

pub(crate) fn br_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    let (dest, dest_args) = parse_successor(parser, context)?;
    let op = r#br(
        context,
        dest_args,
        tir::attributes::AttributeValue::Block(dest),
        Default::default(),
        tir::InsertionPoint::Root,
    );
    Ok(op)
}

pub(crate) fn cond_br_terminator_successors(operation: &tir::OpHandle) -> Vec<BlockId> {
    vec![
        crate::cfg::cond_br_true_dest(operation),
        crate::cfg::cond_br_false_dest(operation),
    ]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CondBranchOp_Terminator: crate::TerminatorCallbacks =
    crate::TerminatorCallbacks {
        successors: Some(crate::cfg::cond_br_terminator_successors),
    };

pub(crate) fn cond_br_branchterminator_successor_operand_slots(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, Vec<usize>)> {
    vec![
        (
            crate::cfg::cond_br_true_dest(operation),
            crate::cfg::cond_br_args_range(operation, 1).collect(),
        ),
        (
            crate::cfg::cond_br_false_dest(operation),
            crate::cfg::cond_br_args_range(operation, 2).collect(),
        ),
    ]
}

pub(crate) fn cond_br_branchterminator_successor_operands(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, Vec<ValueId>)> {
    vec![
        (
            crate::cfg::cond_br_true_dest(operation),
            crate::cfg::cond_br_true_args(operation),
        ),
        (
            crate::cfg::cond_br_false_dest(operation),
            crate::cfg::cond_br_false_args(operation),
        ),
    ]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CondBranchOp_BranchTerminator: crate::BranchTerminatorCallbacks =
    crate::BranchTerminatorCallbacks {
        successor_operands: crate::cfg::cond_br_branchterminator_successor_operands,
        successor_operand_slots: crate::cfg::cond_br_branchterminator_successor_operand_slots,
    };

pub(crate) fn cond_br_branchguard_guarded_successors(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, ValueId, bool)> {
    vec![
        (
            crate::cfg::cond_br_true_dest(operation),
            crate::cfg::cond_br_condition(operation),
            true,
        ),
        (
            crate::cfg::cond_br_false_dest(operation),
            crate::cfg::cond_br_condition(operation),
            false,
        ),
    ]
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CondBranchOp_BranchGuard: crate::BranchGuardCallbacks =
    crate::BranchGuardCallbacks {
        guarded_successors: crate::cfg::cond_br_branchguard_guarded_successors,
    };

pub fn cond_br_condition(operation: &tir::OpHandle) -> ValueId {
    operation.operands()[0]
}

/// The values forwarded to the true successor's block arguments, the
/// states among them last.
pub fn cond_br_true_args(operation: &tir::OpHandle) -> Vec<ValueId> {
    operation.operands()[crate::cfg::cond_br_args_range(operation, 1)].to_vec()
}

/// The values forwarded to the false successor's block arguments, the
/// states among them last.
pub fn cond_br_false_args(operation: &tir::OpHandle) -> Vec<ValueId> {
    operation.operands()[crate::cfg::cond_br_args_range(operation, 2)].to_vec()
}

pub(crate) fn cond_br_args_range(
    operation: &tir::OpHandle,
    group: usize,
) -> std::ops::Range<usize> {
    tir::binding::operand_segments(operation, 3)[group].clone()
}

pub(crate) fn cond_br_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    fmt.write(format!(
        "cfg.cond_br %{}, ",
        crate::cfg::cond_br_condition(operation).number()
    ))?;
    print_successor(
        fmt,
        &context,
        crate::cfg::cond_br_true_dest(operation),
        &crate::cfg::cond_br_true_args(operation),
    )?;
    fmt.write(", ")?;
    print_successor(
        fmt,
        &context,
        crate::cfg::cond_br_false_dest(operation),
        &crate::cfg::cond_br_false_args(operation),
    )?;
    fmt.write("\n")
}

pub(crate) fn cond_br_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    let condition = parse_value_id(parser, context)?;
    parser.expect_token(",")?;
    let (true_dest, true_args) = parse_successor(parser, context)?;
    parser.expect_token(",")?;
    let (false_dest, false_args) = parse_successor(parser, context)?;

    let op = r#cond_br(
        context,
        condition,
        true_args,
        false_args,
        tir::attributes::AttributeValue::Block(true_dest),
        tir::attributes::AttributeValue::Block(false_dest),
        Default::default(),
        tir::InsertionPoint::Root,
    );
    Ok(op)
}

/// The successor blocks, one per case and the default last.
const DESTS: &str = "dests";
/// The selector bits taking each case edge.
const CASES: &str = "cases";
/// How many of the forwarded values each successor takes, in successor order.
const ARG_COUNTS: &str = "arg_counts";

fn unsigned_list(values: impl IntoIterator<Item = u64>) -> AttributeValue {
    AttributeValue::Array(
        values
            .into_iter()
            .map(AttributeValue::UInt)
            .collect::<Vec<_>>()
            .into(),
    )
}

/// Take `cases[i].1` where the selector's bits are `cases[i].0`, and
/// `default` where they are none of them, each entered on its values.
pub fn switch_with_successors(
    context: &Context,
    selector: ValueId,
    cases: Vec<(u64, Successor)>,
    default: Successor,
    mut parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> tir::OpHandle {
    let (values, edges): (Vec<u64>, Vec<Successor>) = cases.into_iter().unzip();
    let edges = edges.into_iter().chain([default]).collect::<Vec<_>>();
    let dests = edges.iter().map(|(dest, _)| AttributeValue::Block(*dest));
    let counts = edges.iter().map(|(_, args)| args.len() as u64);
    let args = edges.iter().flat_map(|(_, args)| args.clone()).collect();
    parts.attributes.splice(
        ..0,
        [
            context.named_attribute(
                DESTS,
                AttributeValue::Array(dests.collect::<Vec<_>>().into()),
            ),
            context.named_attribute(CASES, unsigned_list(values)),
            context.named_attribute(ARG_COUNTS, unsigned_list(counts)),
        ],
    );
    crate::cfg::r#switch(context, selector, args, parts, insertion)
}

pub fn switch_selector(operation: &tir::OpHandle) -> ValueId {
    operation.operands()[0]
}

pub(crate) fn switch_unsigned(operation: &tir::OpHandle, name: &str) -> Vec<u64> {
    match operation.attr(name) {
        Some(AttributeValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                AttributeValue::UInt(value) => *value,
                _ => 0,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn switch_edges(operation: &tir::OpHandle) -> Vec<Successor> {
    let dests = match operation.attr(DESTS) {
        Some(AttributeValue::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                AttributeValue::Block(block) => Some(*block),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let operands = operation.operands();
    let mut start = 1;
    dests
        .into_iter()
        .zip(crate::cfg::switch_unsigned(operation, ARG_COUNTS))
        .map(|(dest, count)| {
            let end = (start + count as usize).min(operands.len());
            let args = operands[start.min(end)..end].to_vec();
            start = end;
            (dest, args)
        })
        .collect()
}

pub(crate) fn switch_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut tir::IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    let selector = crate::cfg::switch_selector(operation);
    fmt.write(format!("cfg.switch %{} : ", selector.number()))?;
    context.print_type(context.get_value(selector).ty(), fmt)?;
    fmt.write(", [\n")?;
    fmt.push();
    let mut edges = crate::cfg::switch_edges(operation);
    let default = edges.pop();
    if let Some((dest, args)) = default {
        fmt.write("default: ")?;
        print_successor(fmt, &context, dest, &args)?;
    }
    let width = context
        .terms()
        .bit_width(context.get_value(selector).ty())
        .map_or(64, |width| width as u32);
    for (case, (dest, args)) in crate::cfg::switch_caseguard_cases(operation)
        .into_iter()
        .zip(edges)
    {
        fmt.write(",\n")?;
        fmt.write(format!("{}: ", crate::binding::signed_bits(case, width)))?;
        print_successor(fmt, &context, dest, &args)?;
    }
    fmt.write("\n")?;
    fmt.pop();
    fmt.write("]\n")
}

pub(crate) fn switch_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let selector = parse_value_id(parser, context)?;
    parser.expect_token(":")?;
    let ty = parse_arg_type(parser, context)?;
    let width = context
        .terms()
        .bit_width(ty)
        .map_or(64, |width| width as u32);
    parser.expect_token(",")?;
    parser.expect_token("[")?;
    parser.expect_token("default")?;
    parser.expect_token(":")?;
    let default = parse_successor(parser, context)?;
    let mut cases = Vec::new();
    while parser.parse_token(",") {
        let span = parser.span();
        let case = parser
            .parse_number()
            .ok_or((span, Error::ExpectedToken("case value")))?;
        parser.expect_token(":")?;
        let successor = parse_successor(parser, context)?;
        cases.push((crate::binding::truncate_bits(case, width), successor));
    }
    parser.expect_token("]")?;
    let op = crate::cfg::switch_with_successors(
        context,
        selector,
        cases,
        default,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    Ok(op)
}

pub(crate) fn switch_verify_impl(
    operation: &tir::OpHandle,
    context: &Context,
) -> Result<(), Error> {
    let fail = |message: String| Err(Error::VerificationError(format!("cfg.switch {message}")));
    for name in [CASES, ARG_COUNTS] {
        if let Some(AttributeValue::Array(items)) = operation.attr(name)
            && items
                .iter()
                .any(|item| !matches!(item, AttributeValue::UInt(_)))
        {
            return fail(format!("{name} must be unsigned integers"));
        }
    }
    let edges = crate::cfg::switch_edges(operation);
    let cases = crate::cfg::switch_caseguard_cases(operation);
    let counts = crate::cfg::switch_unsigned(operation, ARG_COUNTS);
    if edges.len() != cases.len() + 1 || counts.len() != edges.len() {
        return fail(format!(
            "names {} successors for {} cases: each case and the default has one",
            edges.len(),
            cases.len()
        ));
    }
    if counts.iter().sum::<u64>() as usize + 1 != operation.operands().len() {
        return fail("forwards values its successors do not take".to_string());
    }
    let width = context
        .terms()
        .bit_width(
            context
                .get_value(crate::cfg::switch_selector(operation))
                .ty(),
        )
        .map_or(64, |width| width as u32);
    let mut seen = std::collections::HashSet::new();
    for &case in &cases {
        if crate::binding::truncate_bits(case as i64, width) != case {
            return fail(format!(
                "case {case:#x} does not fit its {width}-bit selector"
            ));
        }
        if !seen.insert(case) {
            return fail(format!("names case {case:#x} twice"));
        }
    }
    Ok(())
}

pub(crate) fn switch_terminator_successors(operation: &tir::OpHandle) -> Vec<BlockId> {
    crate::cfg::switch_edges(operation)
        .into_iter()
        .map(|(dest, _)| dest)
        .collect()
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SwitchOp_Terminator: crate::TerminatorCallbacks =
    crate::TerminatorCallbacks {
        successors: Some(crate::cfg::switch_terminator_successors),
    };

pub(crate) fn switch_branchterminator_successor_operand_slots(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, Vec<usize>)> {
    let mut start = 1;
    crate::cfg::switch_edges(operation)
        .into_iter()
        .map(|(dest, args)| {
            let end = start + args.len();
            let slots = (start..end).collect();
            start = end;
            (dest, slots)
        })
        .collect()
}

pub(crate) fn switch_branchterminator_successor_operands(
    operation: &tir::OpHandle,
) -> Vec<(BlockId, Vec<ValueId>)> {
    crate::cfg::switch_edges(operation)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SwitchOp_BranchTerminator: crate::BranchTerminatorCallbacks =
    crate::BranchTerminatorCallbacks {
        successor_operands: crate::cfg::switch_branchterminator_successor_operands,
        successor_operand_slots: crate::cfg::switch_branchterminator_successor_operand_slots,
    };

pub(crate) fn switch_caseguard_selector(operation: &tir::OpHandle) -> ValueId {
    crate::cfg::switch_selector(operation)
}

pub(crate) fn switch_caseguard_cases(operation: &tir::OpHandle) -> Vec<u64> {
    crate::cfg::switch_unsigned(operation, CASES)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SwitchOp_CaseGuard: crate::CaseGuardCallbacks =
    crate::CaseGuardCallbacks {
        selector: crate::cfg::switch_caseguard_selector,
        cases: crate::cfg::switch_caseguard_cases,
    };

fn print_successor(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    block: BlockId,
    args: &[ValueId],
) -> Result<(), std::fmt::Error> {
    fmt.write(format!("^bb{}", fmt.region_block_number(block)))?;
    if args.is_empty() {
        return Ok(());
    }
    fmt.write("(")?;
    tir::region_format::print_value_list(fmt, args)?;
    fmt.write(" : ")?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            fmt.write(", ")?;
        }
        context.print_type(context.get_value(*arg).ty(), fmt)?;
    }
    fmt.write(")")
}

/// A successor: its block and the values it is entered on.
pub type Successor = (BlockId, Vec<ValueId>);

fn parse_successor(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Successor, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let label = parser
        .parse_block_label()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("^label")))?
        .to_string();

    let mut args = vec![];
    let mut arg_types = vec![];
    if parser.parse_token("(") {
        if parser.peek_char() == Some('%') {
            loop {
                args.push(parse_value_id(parser, context)?);
                if parser.parse_token(",") {
                    continue;
                }
                break;
            }
            parser.expect_token(":")?;
            loop {
                arg_types.push(parse_arg_type(parser, context)?);
                if parser.parse_token(",") {
                    continue;
                }
                break;
            }
        }
        parser.expect_token(")")?;
    }

    let block = parser.resolve_region_block_label(context, &label, &arg_types)?;
    Ok((block, args))
}

fn parse_value_id(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(parser.resolve_value(context, value_ref))
}

fn parse_arg_type(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::TypeId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    parser
        .parse_type(context)?
        .ok_or_else(|| (parser.span(), Error::ExpectedType))
}
