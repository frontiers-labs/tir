use crate::Any;
use crate::attributes::AttributeValue;
use crate::{
    BlockId, BranchGuard, BranchTerminator, CaseGuard, Context, Error, Operation, Terminator,
    ValueId, dialect, operation,
};

use crate as tir;

pub mod ops {
    pub use super::{br, cond_br, switch};
}

dialect! {
    CfgDialect {
        name: "cfg",
        operations: [BranchOp, CondBranchOp, SwitchOp],
        types: [],
    }
}

operation! {
    BranchOp {
        name: "br",
        dialect: "cfg",
        format: "custom",
        operands: O {
            dest_args: "*Any",
        },
        attributes: A {
            dest: "Block",
        },
        interfaces: [Terminator, BranchTerminator],
    }
}

impl Terminator for BranchOp {
    fn successors(&self) -> Vec<BlockId> {
        vec![self.dest()]
    }
}

impl BranchTerminator for BranchOp {
    fn successor_operands(&self) -> Vec<(BlockId, Vec<ValueId>)> {
        vec![(self.dest(), self.dest_args())]
    }
}

impl BranchOp {
    /// The values forwarded to the destination block's arguments.
    pub fn dest_args(&self) -> Vec<ValueId> {
        self.operands().to_vec()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.clone();
        fmt.write("cfg.br ")?;
        print_successor(fmt, &context, self.dest(), &self.dest_args())?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let (dest, dest_args) = parse_successor(parser, context)?;
        let op = BranchOpBuilder::new(context)
            .dest_args(dest_args)
            .dest(dest)
            .build();
        Ok(Box::new(op))
    }
}

operation! {
    CondBranchOp {
        name: "cond_br",
        dialect: "cfg",
        format: "custom",
        operands: O {
            condition: "crate::Integer<1>",
            true_args: "*Any",
            false_args: "*Any",
        },
        attributes: A {
            true_dest: "Block",
            false_dest: "Block",
        },
        interfaces: [Terminator, BranchTerminator, BranchGuard],
    }
}

impl Terminator for CondBranchOp {
    fn successors(&self) -> Vec<BlockId> {
        vec![self.true_dest(), self.false_dest()]
    }
}

impl BranchTerminator for CondBranchOp {
    fn successor_operands(&self) -> Vec<(BlockId, Vec<ValueId>)> {
        vec![
            (self.true_dest(), self.true_args()),
            (self.false_dest(), self.false_args()),
        ]
    }
}

impl BranchGuard for CondBranchOp {
    fn guarded_successors(&self) -> Vec<(BlockId, ValueId, bool)> {
        vec![
            (self.true_dest(), self.condition(), true),
            (self.false_dest(), self.condition(), false),
        ]
    }
}

impl CondBranchOp {
    pub fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    /// The values forwarded to the true successor's block arguments, the
    /// states among them last.
    pub fn true_args(&self) -> Vec<ValueId> {
        self.operands()[self.args_range(1)].to_vec()
    }

    /// The values forwarded to the false successor's block arguments, the
    /// states among them last.
    pub fn false_args(&self) -> Vec<ValueId> {
        self.operands()[self.args_range(2)].to_vec()
    }

    // Operand layout is [condition, true_args.., false_args..], one declared
    // operand group each.
    fn args_range(&self, group: usize) -> std::ops::Range<usize> {
        tir::binding::operand_segments(&self.0, 3)[group].clone()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.clone();
        fmt.write(format!("cfg.cond_br %{}, ", self.condition().number()))?;
        print_successor(fmt, &context, self.true_dest(), &self.true_args())?;
        fmt.write(", ")?;
        print_successor(fmt, &context, self.false_dest(), &self.false_args())?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser, context)?;
        expect_token(parser, ",")?;
        let (true_dest, true_args) = parse_successor(parser, context)?;
        expect_token(parser, ",")?;
        let (false_dest, false_args) = parse_successor(parser, context)?;

        let op = CondBranchOpBuilder::new(context)
            .condition(condition)
            .true_args(true_args)
            .false_args(false_args)
            .true_dest(true_dest)
            .false_dest(false_dest)
            .build();
        Ok(Box::new(op))
    }
}

operation! {
    SwitchOp {
        name: "switch",
        dialect: "cfg",
        format: "custom",
        verifier: "true",
        operands: O {
            selector: "crate::builtin::IntegerType",
            args: "*Any",
        },
        interfaces: [Terminator, BranchTerminator, CaseGuard],
    }
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

impl SwitchOpBuilder {
    /// Take `cases[i].1` where the selector's bits are `cases[i].0`, and
    /// `default` where they are none of them, each entered on its values.
    pub fn successors(self, cases: Vec<(u64, Successor)>, default: Successor) -> Self {
        let (values, edges): (Vec<u64>, Vec<Successor>) = cases.into_iter().unzip();
        let edges = edges.into_iter().chain([default]).collect::<Vec<_>>();
        let dests = edges.iter().map(|(dest, _)| AttributeValue::Block(*dest));
        let counts = edges.iter().map(|(_, args)| args.len() as u64);
        let args = edges.iter().flat_map(|(_, args)| args.clone()).collect();
        self.args(args)
            .attr(
                DESTS,
                AttributeValue::Array(dests.collect::<Vec<_>>().into()),
            )
            .attr(CASES, unsigned_list(values))
            .attr(ARG_COUNTS, unsigned_list(counts))
    }
}

impl SwitchOp {
    pub fn selector(&self) -> ValueId {
        self.operands()[0]
    }

    fn unsigned(&self, name: &str) -> Vec<u64> {
        match self.attr(name) {
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

    /// Every successor edge, the cases' first and the default last.
    fn edges(&self) -> Vec<Successor> {
        let dests = match self.attr(DESTS) {
            Some(AttributeValue::Array(items)) => items
                .iter()
                .filter_map(|item| match item {
                    AttributeValue::Block(block) => Some(*block),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let operands = self.operands();
        let mut start = 1;
        dests
            .into_iter()
            .zip(self.unsigned(ARG_COUNTS))
            .map(|(dest, count)| {
                let end = (start + count as usize).min(operands.len());
                let args = operands[start.min(end)..end].to_vec();
                start = end;
                (dest, args)
            })
            .collect()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.clone();
        let selector = self.selector();
        fmt.write(format!("cfg.switch %{} : ", selector.number()))?;
        context.print_type(context.get_value(selector).ty(), fmt)?;
        fmt.write(", [\n")?;
        fmt.push();
        let mut edges = self.edges();
        let default = edges.pop();
        if let Some((dest, args)) = default {
            fmt.write("default: ")?;
            print_successor(fmt, &context, dest, &args)?;
        }
        let width = crate::sem::egraph::type_width(&context, context.get_value(selector).ty())
            .unwrap_or(64);
        for (case, (dest, args)) in self.cases().into_iter().zip(edges) {
            fmt.write(",\n")?;
            fmt.write(format!("{}: ", crate::binding::signed_bits(case, width)))?;
            print_successor(fmt, &context, dest, &args)?;
        }
        fmt.write("\n")?;
        fmt.pop();
        fmt.write("]\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;
        let selector = parse_value_id(parser, context)?;
        expect_token(parser, ":")?;
        let ty = parse_arg_type(parser, context)?;
        let width = crate::sem::egraph::type_width(context, ty).unwrap_or(64);
        expect_token(parser, ",")?;
        expect_token(parser, "[")?;
        expect_token(parser, "default")?;
        expect_token(parser, ":")?;
        let default = parse_successor(parser, context)?;
        let mut cases = Vec::new();
        while parser.parse_token(",") {
            let span = parser.span();
            let case = parser
                .parse_number()
                .ok_or((span, Error::ExpectedToken("case value")))?;
            expect_token(parser, ":")?;
            let successor = parse_successor(parser, context)?;
            cases.push((crate::binding::truncate_bits(case, width), successor));
        }
        expect_token(parser, "]")?;
        let op = SwitchOpBuilder::new(context)
            .selector(selector)
            .successors(cases, default)
            .build();
        Ok(Box::new(op))
    }
}

impl tir::Verifiable for SwitchOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let fail = |message: String| Err(Error::VerificationError(format!("cfg.switch {message}")));
        for name in [CASES, ARG_COUNTS] {
            if let Some(AttributeValue::Array(items)) = self.attr(name)
                && items
                    .iter()
                    .any(|item| !matches!(item, AttributeValue::UInt(_)))
            {
                return fail(format!("{name} must be unsigned integers"));
            }
        }
        let edges = self.edges();
        let cases = self.cases();
        let counts = self.unsigned(ARG_COUNTS);
        if edges.len() != cases.len() + 1 || counts.len() != edges.len() {
            return fail(format!(
                "names {} successors for {} cases: each case and the default has one",
                edges.len(),
                cases.len()
            ));
        }
        if counts.iter().sum::<u64>() as usize + 1 != self.operands().len() {
            return fail("forwards values its successors do not take".to_string());
        }
        let width =
            crate::sem::egraph::type_width(context, context.get_value(self.selector()).ty())
                .unwrap_or(64);
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
}

impl Terminator for SwitchOp {
    fn successors(&self) -> Vec<BlockId> {
        self.edges().into_iter().map(|(dest, _)| dest).collect()
    }
}

impl BranchTerminator for SwitchOp {
    fn successor_operands(&self) -> Vec<(BlockId, Vec<ValueId>)> {
        self.edges()
    }
}

impl CaseGuard for SwitchOp {
    fn selector(&self) -> ValueId {
        SwitchOp::selector(self)
    }

    fn cases(&self) -> Vec<u64> {
        self.unsigned(CASES)
    }
}

/// Print a successor as `^bbN` followed by an optional MLIR-style argument list
/// `(%a, %b : t1, t2)` when the branch forwards block arguments.
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
            expect_token(parser, ":")?;
            loop {
                arg_types.push(parse_arg_type(parser, context)?);
                if parser.parse_token(",") {
                    continue;
                }
                break;
            }
        }
        expect_token(parser, ")")?;
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

fn expect_token(
    parser: &mut tir::parse::text::Parser,
    token: &'static str,
) -> Result<(), (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}
