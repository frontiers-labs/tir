use crate::builtin::{IntegerType, TokenType};
use crate::{
    Conditional, Context, CountedLoop, Error, GuardedLoop, LoopLike, Operation, Terminator,
    TokenScope, TypeId, ValueId, ValueIds, dialect, operation,
};

use crate as tir;
use crate::Any as AnyConstraint;
use crate::Value;
use crate::parse::common::Cursor;

pub mod ops {
    pub use super::{
        BreakOp, ConditionOp, ContinueOp, ForOp, IfOp, SwitchOp, WhileOp, YieldOp, r#break,
        condition, r#continue, r#for, r#if, switch, r#while, r#yield,
    };
}

dialect! {
    ScfDialect {
        name: "scf",
        operations: [
            ForOp,
            WhileOp,
            IfOp,
            SwitchOp,
            ConditionOp,
            BreakOp,
            ContinueOp,
            YieldOp,
        ],
        types: [],
    }
}

operation! {
    ForOp {
        name: "for",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            lower_bound: "crate::builtin::Counter",
            upper_bound: "crate::builtin::Counter",
            step: "crate::builtin::Counter",
            inits: "*AnyConstraint",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        },
        interfaces: [LoopLike, GuardedLoop, CountedLoop, TokenScope],
    }
}

impl tir::CountedLoop for ForOp {
    fn lower_bound(&self) -> ValueId {
        self.operands()[0]
    }
    fn upper_bound(&self) -> ValueId {
        self.operands()[1]
    }
    fn step(&self) -> ValueId {
        self.operands()[2]
    }
}

/// `scf.for` counts `lb`, `lb + step`, … while the counter is signed-less-than `ub`, so
/// its body runs at all exactly when `lb < ub` — whatever the sign of `step`.
impl tir::GuardedLoop for ForOp {
    fn entry_guard(&self) -> tir::EntryGuard {
        tir::EntryGuard::Less {
            ordering: tir::GuardOrdering::Signed,
            lhs: self.operands()[0],
            rhs: self.operands()[1],
        }
    }
}

impl LoopOp for ForOp {
    fn loop_results(&self) -> Vec<ValueId> {
        self.0.results().to_vec()
    }
    fn init_operands(&self) -> Vec<ValueId> {
        self.operands()[3..].to_vec()
    }
    fn body_block(&self) -> tir::BlockHandle {
        self.body()
    }
    fn iter_args(&self) -> Vec<ValueId> {
        carried_arguments(&self.loop_context(), &self.body_block())
    }
    fn loop_context(&self) -> Context {
        self.0.context.upgrade()
    }
}

impl tir::LoopLike for ForOp {
    fn inits(&self) -> Vec<ValueId> {
        self.init_operands()
    }
    fn carried_args(&self) -> Vec<ValueId> {
        carried_arguments(&self.loop_context(), &self.body_block())
    }
    fn latched(&self) -> Vec<ValueId> {
        latched_values(self)
    }
    fn finals(&self) -> Vec<ValueId> {
        self.loop_results()
    }
}

impl TokenScope for ForOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions()[0]]
    }
}

impl tir::Verifiable for ForOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_counter_type(context, self)?;
        verify_loop_carried(context, self)
    }
}

impl ForOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_result_prefix(fmt, self)?;
        fmt.write(format!(
            "scf.for %{}, %{}, %{}",
            self.operands()[0].number(),
            self.operands()[1].number(),
            self.operands()[2].number()
        ))?;
        print_scope(fmt, &context, self.body_block())?;
        print_loop_tail(fmt, &context, self)?;
        tir::region_format::print_op_region(fmt, &context, self, 0)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let lower_bound = parse_value_id(parser, context)?;
        expect_token(parser, ",")?;
        let upper_bound = parse_value_id(parser, context)?;
        expect_token(parser, ",")?;
        let step = parse_value_id(parser, context)?;
        let scope = parse_scope(parser, context)?;
        let carried = parse_iter_args(parser, context)?;
        let body_carried = carried.iter().map(|c| c.acc.clone()).collect();
        let body = parse_loop_body(parser, context, scope, body_carried)?;

        Ok(Box::new(
            ForOpBuilder::new(context)
                .lower_bound(lower_bound)
                .upper_bound(upper_bound)
                .step(step)
                .body(body)
                .inits(carried.iter().map(|c| c.init).collect())
                .result_types(carried.iter().map(|c| c.ty).collect())
                .build(),
        ))
    }
}

operation! {
    WhileOp {
        name: "while",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            inits: "*AnyConstraint",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            condition_region: Region {
                single_block: true,
            },
            body: Region {
                single_block: true,
            }
        },
        interfaces: [LoopLike, GuardedLoop, TokenScope],
    }
}

/// `scf.while` tests its condition region before every iteration, the first one
/// included, so the zero-trip guard is that region read over the init operands.
impl tir::GuardedLoop for WhileOp {
    fn entry_guard(&self) -> tir::EntryGuard {
        let context = self.loop_context();
        let block = self.condition_region();
        let terminator = context.get_op(*block.op_ids().last().unwrap());
        tir::EntryGuard::Region {
            region: self.0.regions()[0],
            arguments: block.arguments().iter().map(Value::id).collect(),
            condition: terminator.operands()[0],
        }
    }
}

impl LoopOp for WhileOp {
    fn loop_results(&self) -> Vec<ValueId> {
        self.0.results().to_vec()
    }
    fn init_operands(&self) -> Vec<ValueId> {
        self.operands().to_vec()
    }
    fn body_block(&self) -> tir::BlockHandle {
        self.body()
    }
    fn iter_args(&self) -> Vec<ValueId> {
        self.condition_region()
            .arguments()
            .iter()
            .map(tir::Value::id)
            .collect()
    }
    fn loop_context(&self) -> Context {
        self.0.context.upgrade()
    }
}

impl tir::LoopLike for WhileOp {
    fn inits(&self) -> Vec<ValueId> {
        self.init_operands()
    }
    fn carried_args(&self) -> Vec<ValueId> {
        carried_arguments(&self.loop_context(), &self.body_block())
    }
    fn latched(&self) -> Vec<ValueId> {
        latched_values(self)
    }
    fn finals(&self) -> Vec<ValueId> {
        self.loop_results()
    }
}

impl TokenScope for WhileOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions()[1]]
    }
}

impl tir::Verifiable for WhileOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_while_condition(context, self)?;
        verify_loop_carried(context, self)
    }
}

impl WhileOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        print_result_prefix(fmt, self)?;
        fmt.write("scf.while")?;
        print_loop_tail(fmt, &context, self)?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" do")?;
        print_scope(fmt, &context, self.body_block())?;
        let body_carried = carried_arguments(&context, &self.body_block());
        if !body_carried.is_empty() {
            fmt.write("(")?;
            print_value_list(fmt, &body_carried)?;
            fmt.write(")")?;
        }
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let carried = parse_iter_args(parser, context)?;
        let condition_args = carried.iter().map(|c| c.acc.clone()).collect();
        let condition_region = parser
            .parse_region_with_entry_args(context, condition_args)?
            .id();
        expect_token(parser, "do")?;
        let scope = parse_scope(parser, context)?;
        let body_carried = parse_body_carried(parser, context, &carried)?;
        let body = parse_loop_body(parser, context, scope, body_carried)?;

        Ok(Box::new(
            WhileOpBuilder::new(context)
                .condition_region(condition_region)
                .body(body)
                .inits(carried.iter().map(|c| c.init).collect())
                .result_types(carried.iter().map(|c| c.ty).collect())
                .build(),
        ))
    }
}

operation! {
    ConditionOp {
        name: "condition",
        dialect: "scf",
        format: "custom",
        operands: O {
            condition: "crate::Integer<1>",
            values: "*AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ConditionOp {}

impl ConditionOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write("scf.condition ")?;
        print_value_list(fmt, &self.operands())?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser, context)?;
        Ok(Box::new(
            ConditionOpBuilder::new(context)
                .condition(condition)
                .values(parse_trailing_values(parser, context)?)
                .build(),
        ))
    }
}

operation! {
    IfOp {
        name: "if",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            condition: "crate::Integer<1>",
            inputs: "*AnyConstraint",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            then_body: Region {
                single_block: true,
            },
            else_body: Region {
                single_block: true,
            }
        },
        interfaces: [Conditional],
    }
}

impl tir::Conditional for IfOp {
    fn decision(&self) -> ValueId {
        self.condition()
    }

    fn region_yields(&self, region: tir::RegionId) -> Vec<ValueId> {
        let context = self.0.context.upgrade();
        region_yielded_values(&context, region)
    }

    fn guarded_regions(&self) -> Vec<(tir::RegionId, ValueId, bool)> {
        vec![
            (self.0.regions()[0], self.condition(), true),
            (self.0.regions()[1], self.condition(), false),
        ]
    }
}

impl tir::Verifiable for IfOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        verify_i1_operand(context, self.condition(), "scf.if condition")?;

        // A value-producing `scf.if` is a γ merge: each arm must yield one value per
        // result, of the matching type; a resultless `scf.if` must yield nothing.
        let result_types = self
            .0
            .results()
            .iter()
            .map(|&r| context.get_value(r).ty())
            .collect::<Vec<_>>();
        verify_region_yield(context, self.then_body(), &result_types, "scf.if then body")?;
        verify_region_yield(context, self.else_body(), &result_types, "scf.if else body")?;

        verify_arm_arguments(
            context,
            self.then_body(),
            &self.inputs(),
            "scf.if then body",
        )?;
        verify_arm_arguments(
            context,
            self.else_body(),
            &self.inputs(),
            "scf.if else body",
        )
    }
}

impl IfOp {
    fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    /// The values the gate forwards to every arm as its entry arguments.
    fn inputs(&self) -> ValueIds {
        self.operands()[1..].into()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        if !self.0.results().is_empty() {
            print_value_list(fmt, &self.0.results())?;
            fmt.write(" = ")?;
        }
        fmt.write(format!("scf.if %{}", self.condition().number()))?;
        print_gamma_inputs(fmt, &self.inputs())?;
        print_result_types(fmt, &context, &self.0.results())?;
        print_arm_arguments(fmt, self.then_body())?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" else")?;
        print_arm_arguments(fmt, self.else_body())?;
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let condition = parse_value_id(parser, context)?;
        let inputs = parse_gamma_inputs(parser, context)?;
        let result_types = parse_result_types(parser, context)?;
        let then_body = parse_arm(parser, context, &inputs)?;
        expect_token(parser, "else")?;
        let else_body = parse_arm(parser, context, &inputs)?;

        Ok(Box::new(
            IfOpBuilder::new(context)
                .condition(condition)
                .inputs(inputs)
                .then_body(then_body)
                .else_body(else_body)
                .result_types(result_types)
                .build(),
        ))
    }
}

// An n-ary γ: the arm whose case value equals the integer predicate runs, and the last
// arm — the default — runs when none does. The default arm is mandatory rather than
// optional so the op is total: every predicate value selects exactly one arm, which is
// what a value-producing switch needs to define its results and what destruction needs
// as the last destination of its comparison chain. A C `switch` without a `default:`
// label lowers to an empty default arm.
operation! {
    SwitchOp {
        name: "switch",
        dialect: "scf",
        format: "custom",
        verifier: "true",
        operands: O {
            predicate: "crate::builtin::IntegerType",
            inputs: "*AnyConstraint",
        },
        attributes: A {
            cases: "Array",
        },
        results: R {
            results: "*AnyConstraint",
        },
        regions: R {
            arms: Region {
                single_block: true,
                variadic: true,
            }
        },
        interfaces: [Conditional],
    }
}

impl SwitchOpBuilder {
    pub fn cases(self, values: Vec<i64>) -> Self {
        self.attr(
            "cases",
            tir::attributes::AttributeValue::Array(
                values
                    .into_iter()
                    .map(tir::attributes::AttributeValue::Int)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }
}

impl tir::Conditional for SwitchOp {
    fn decision(&self) -> ValueId {
        self.predicate()
    }

    fn region_yields(&self, region: tir::RegionId) -> Vec<ValueId> {
        let context = self.0.context.upgrade();
        region_yielded_values(&context, region)
    }

    /// No arm is entered under a boolean fact about an existing value: an arm runs when
    /// the predicate equals its case value, which [`Conditional::case_values`] reports.
    fn guarded_regions(&self) -> Vec<(tir::RegionId, ValueId, bool)> {
        vec![]
    }

    fn case_values(&self) -> Vec<(tir::RegionId, Option<i64>)> {
        let cases = self.cases();
        self.arms()
            .iter()
            .enumerate()
            .map(|(index, &region)| (region, cases.get(index).copied()))
            .collect()
    }
}

impl tir::Verifiable for SwitchOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let cases = self.cases();
        let arms = self.arms();
        if arms.len() != cases.len() + 1 {
            return Err(Error::VerificationError(format!(
                "scf.switch has {} case values but {} arms; every case needs an arm, plus a default",
                cases.len(),
                arms.len()
            )));
        }
        for (index, case) in cases.iter().enumerate() {
            if cases[..index].contains(case) {
                return Err(Error::VerificationError(format!(
                    "scf.switch case value {case} appears more than once"
                )));
            }
        }

        let result_types = self
            .0
            .results()
            .iter()
            .map(|&r| context.get_value(r).ty())
            .collect::<Vec<_>>();
        for (index, &region) in arms.iter().enumerate() {
            let label = match cases.get(index) {
                Some(case) => format!("scf.switch case {case} arm"),
                None => "scf.switch default arm".to_string(),
            };
            let block = context
                .get_region(region)
                .iter(context.clone())
                .next()
                .ok_or_else(|| Error::VerificationError(format!("{label} must contain a block")))?;
            verify_region_yield(context, block.clone(), &result_types, &label)?;
            verify_arm_arguments(context, block, &self.inputs(), &label)?;
        }
        Ok(())
    }
}

impl SwitchOp {
    fn predicate(&self) -> ValueId {
        self.operands()[0]
    }

    /// The values the gate forwards to every arm as its entry arguments.
    fn inputs(&self) -> ValueIds {
        self.operands()[1..].into()
    }

    /// The case value selecting each non-default arm, in arm order.
    fn cases(&self) -> Vec<i64> {
        self.attr("cases")
            .and_then(|value| match value {
                tir::attributes::AttributeValue::Array(items) => items
                    .iter()
                    .map(|item| match item {
                        tir::attributes::AttributeValue::Int(value) => Some(*value),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>(),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        if !self.0.results().is_empty() {
            print_value_list(fmt, &self.0.results())?;
            fmt.write(" = ")?;
        }
        fmt.write(format!("scf.switch %{}", self.predicate().number()))?;
        print_gamma_inputs(fmt, &self.inputs())?;
        print_result_types(fmt, &context, &self.0.results())?;
        let arms = self.arms().to_vec();
        for (index, case) in self.cases().iter().enumerate() {
            fmt.write(format!(" case {case}"))?;
            print_arm_arguments(fmt, arm_block(&context, arms[index]))?;
            tir::region_format::print_op_region(fmt, &context, self, index)?;
        }
        fmt.write(" default")?;
        let default = arms.len() - 1;
        print_arm_arguments(fmt, arm_block(&context, arms[default]))?;
        tir::region_format::print_op_region(fmt, &context, self, default)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let predicate = parse_value_id(parser, context)?;
        let inputs = parse_gamma_inputs(parser, context)?;
        let result_types = parse_result_types(parser, context)?;
        let mut cases = vec![];
        let mut arms = vec![];
        while parser.parse_token("case") {
            let case = parser
                .parse_number()
                .ok_or_else(|| (parser.span(), Error::ExpectedToken("case value")))?;
            cases.push(case);
            arms.push(parse_arm(parser, context, &inputs)?);
        }
        expect_token(parser, "default")?;
        arms.push(parse_arm(parser, context, &inputs)?);

        Ok(Box::new(
            SwitchOpBuilder::new(context)
                .predicate(predicate)
                .inputs(inputs)
                .cases(cases)
                .arms(arms)
                .result_types(result_types)
                .build(),
        ))
    }
}

operation! {
    BreakOp {
        name: "break",
        dialect: "scf",
        format: "custom",
        operands: O {
            scope: "crate::builtin::TokenType",
            values: "*AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for BreakOp {}

impl BreakOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        print_exit(fmt, "scf.break", &self.operands())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let scope = parse_value_id(parser, context)?;
        Ok(Box::new(
            BreakOpBuilder::new(context)
                .scope(scope)
                .values(parse_trailing_values(parser, context)?)
                .build(),
        ))
    }
}

operation! {
    ContinueOp {
        name: "continue",
        dialect: "scf",
        format: "custom",
        operands: O {
            scope: "crate::builtin::TokenType",
            values: "*AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ContinueOp {}

impl ContinueOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        print_exit(fmt, "scf.continue", &self.operands())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let scope = parse_value_id(parser, context)?;
        Ok(Box::new(
            ContinueOpBuilder::new(context)
                .scope(scope)
                .values(parse_trailing_values(parser, context)?)
                .build(),
        ))
    }
}

/// Print `scf.break %scope, %v0, %v1`: the scope token, then one value per carried port.
fn print_exit(
    fmt: &mut tir::IRFormatter,
    mnemonic: &'static str,
    operands: &[ValueId],
) -> Result<(), std::fmt::Error> {
    fmt.write(format!("{mnemonic} "))?;
    print_value_list(fmt, operands)?;
    fmt.write("\n")
}

operation! {
    YieldOp {
        name: "yield",
        dialect: "scf",
        format: "custom",
        operands: O {
            values: "*AnyConstraint",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}

impl YieldOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        fmt.write("scf.yield")?;
        if !self.operands().is_empty() {
            fmt.write(" ")?;
            print_value_list(fmt, &self.operands())?;
        }
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        let mut values = vec![];
        while let Some(name) = parser.parse_value_ref() {
            values.push(parser.resolve_value(context, name));
            if !parser.parse_token(",") {
                break;
            }
        }
        Ok(Box::new(
            YieldOpBuilder::new(context).values(values).build(),
        ))
    }
}

fn parse_value_id(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(parser.resolve_value(context, value_ref))
}

fn expect_token(
    parser: &mut tir::parse::text::Parser,
    token: &'static str,
) -> Result<(), (tir::parse::Span, Error)> {
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}

/// The per-op specifics a structured loop exposes for μ-gate construction, printing,
/// and verification: its results, init operands, and body block.
trait LoopOp: Operation {
    fn loop_results(&self) -> Vec<ValueId>;
    fn init_operands(&self) -> Vec<ValueId>;
    fn body_block(&self) -> tir::BlockHandle;
    /// The values bound by the printed `iter_args` clause: where the loop observes the
    /// carried values on entry to an iteration.
    fn iter_args(&self) -> Vec<ValueId>;
    fn loop_context(&self) -> Context;
}

/// A parsed `iter_args(%acc = %init) -> <ty>` port: the created carried block
/// argument, its init operand, and the carried type.
struct Carried {
    acc: Value,
    init: ValueId,
    ty: TypeId,
}

/// The values a loop body yields on its back edge: the next iteration's carried values.
/// A body that leaves through an exit terminator reports the values that exit carries,
/// which follow its scope token.
fn latched_values(op: &impl LoopOp) -> Vec<ValueId> {
    let context = op.loop_context();
    let body = op.body_block();
    exit_values(&context.get_op(*body.op_ids().last().unwrap())).to_vec()
}

/// The carried values a structured terminator transfers: everything a `scf.break` or
/// `scf.continue` carries past its scope token, and everything else's operands.
fn exit_values(terminator: &tir::OpHandle) -> ValueIds {
    let operands = terminator.operands();
    if terminator.is::<BreakOp>() || terminator.is::<ContinueOp>() {
        operands[1..].into()
    } else {
        operands
    }
}

/// Print a comma-separated `%a, %b` list.
fn print_value_list(fmt: &mut tir::IRFormatter, values: &[ValueId]) -> Result<(), std::fmt::Error> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}", value.number()))?;
    }
    Ok(())
}

/// Print the ` -> <ty>, <ty>` clause of a value-producing structured op.
fn print_result_types(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    results: &[ValueId],
) -> Result<(), std::fmt::Error> {
    if results.is_empty() {
        return Ok(());
    }
    fmt.write(" -> ")?;
    for (index, &result) in results.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        context.print_type(context.get_value(result).ty(), fmt)?;
    }
    Ok(())
}

/// Print a `%r0, %r1 = ` binding for a value-producing loop, nothing for a
/// side-effecting one.
fn print_result_prefix(
    fmt: &mut tir::IRFormatter,
    op: &impl LoopOp,
) -> Result<(), std::fmt::Error> {
    let results = op.loop_results();
    if results.is_empty() {
        return Ok(());
    }
    print_value_list(fmt, &results)?;
    fmt.write(" = ")
}

/// Print the `iter_args(%acc = %init, ...) -> <ty>, <ty>` clause of a carrying loop.
fn print_loop_tail(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    op: &impl LoopOp,
) -> Result<(), std::fmt::Error> {
    let results = op.loop_results();
    if results.is_empty() {
        return Ok(());
    }
    fmt.write(" iter_args(")?;
    for (index, (acc, init)) in op.iter_args().iter().zip(op.init_operands()).enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{} = %{}", acc.number(), init.number()))?;
    }
    fmt.write(")")?;
    print_result_types(fmt, context, &results)
}

fn print_scope(
    fmt: &mut tir::IRFormatter,
    context: &Context,
    body: tir::BlockHandle,
) -> Result<(), std::fmt::Error> {
    if let Some(scope) = body
        .arguments()
        .iter()
        .find(|argument| argument.ty() == TokenType::new(context))
    {
        fmt.write(format!(" scope(%{})", scope.id().number()))?;
    }
    Ok(())
}

fn parse_scope(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Option<Value>, (tir::parse::Span, Error)> {
    if !parser.parse_token("scope") {
        return Ok(None);
    }
    expect_token(parser, "(")?;
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
        .to_string();
    expect_token(parser, ")")?;
    let scope = context.create_value(TokenType::new(context), None);
    parser.define_value(&name, scope.id());
    Ok(Some(scope))
}

/// Print a γ's `args(%a, %b)` clause: the values every arm receives as its entry
/// arguments. A gate that forwards nothing prints no clause.
fn print_gamma_inputs(
    fmt: &mut tir::IRFormatter,
    inputs: &[ValueId],
) -> Result<(), std::fmt::Error> {
    if inputs.is_empty() {
        return Ok(());
    }
    fmt.write(" args(")?;
    print_value_list(fmt, inputs)?;
    fmt.write(")")
}

/// Print the `(%a, %b)` binding an arm puts before its body: the names that arm
/// gives the forwarded inputs.
fn print_arm_arguments(
    fmt: &mut tir::IRFormatter,
    arm: tir::BlockHandle,
) -> Result<(), std::fmt::Error> {
    let arguments = arm.arguments();
    if arguments.is_empty() {
        return Ok(());
    }
    fmt.write(" (")?;
    print_value_list(fmt, &arguments.iter().map(Value::id).collect::<Vec<_>>())?;
    fmt.write(")")
}

/// The single block of a γ arm.
fn arm_block(context: &Context, arm: tir::RegionId) -> tir::BlockHandle {
    context.get_block(context.get_region(arm).block_ids()[0])
}

/// Parse an optional `args(%a, %b)` clause.
fn parse_gamma_inputs(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Vec<ValueId>, (tir::parse::Span, Error)> {
    if !parser.parse_token("args") {
        return Ok(vec![]);
    }
    expect_token(parser, "(")?;
    let mut inputs = vec![parse_value_id(parser, context)?];
    while parser.parse_token(",") {
        inputs.push(parse_value_id(parser, context)?);
    }
    expect_token(parser, ")")?;
    Ok(inputs)
}

/// Parse a γ arm, seeding its entry block with one argument per forwarded input,
/// bound to the name the arm gives it.
fn parse_arm(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    inputs: &[ValueId],
) -> Result<tir::RegionId, (tir::parse::Span, Error)> {
    if inputs.is_empty() {
        return Ok(parser.parse_region(context)?.id());
    }
    expect_token(parser, "(")?;
    let mut arguments = vec![];
    for &input in inputs {
        if !arguments.is_empty() {
            expect_token(parser, ",")?;
        }
        let name = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
            .to_string();
        let argument = context.create_value(context.get_value(input).ty(), None);
        parser.define_value(&name, argument.id());
        arguments.push(argument);
    }
    expect_token(parser, ")")?;
    Ok(parser
        .parse_region_with_entry_args(context, arguments)?
        .id())
}

/// Verify a γ arm's entry signature: it takes one argument per forwarded input,
/// matching in type, so every arm reads the same values under names of its own.
fn verify_arm_arguments(
    context: &Context,
    arm: tir::BlockHandle,
    inputs: &[ValueId],
    label: &str,
) -> Result<(), Error> {
    let arguments = arm.arguments();
    if arguments.len() != inputs.len() {
        return Err(Error::VerificationError(format!(
            "{label} must take {} arguments, but takes {}",
            inputs.len(),
            arguments.len()
        )));
    }
    for (index, (argument, &input)) in arguments.iter().zip(inputs).enumerate() {
        if argument.ty() != context.get_value(input).ty() {
            return Err(Error::VerificationError(format!(
                "{label} argument {index} type must match the type of input {index}"
            )));
        }
    }
    Ok(())
}

/// Parse an optional `iter_args(%acc = %init, ...) -> <ty>, <ty>` clause, creating one
/// carried block argument per port, bound to its `%acc` name.
fn parse_iter_args(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Vec<Carried>, (tir::parse::Span, Error)> {
    if !parser.parse_token("iter_args") {
        return Ok(vec![]);
    }
    expect_token(parser, "(")?;
    let mut ports = vec![];
    loop {
        let acc_name = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
            .to_string();
        expect_token(parser, "=")?;
        ports.push((acc_name, parse_value_id(parser, context)?));
        if !parser.parse_token(",") {
            break;
        }
    }
    expect_token(parser, ")")?;
    let types = parse_result_types(parser, context)?;
    if types.len() != ports.len() {
        return Err((parser.span(), Error::ExpectedType));
    }
    Ok(ports
        .into_iter()
        .zip(types)
        .map(|((acc_name, init), ty)| {
            let acc = context.create_value(ty, None);
            parser.define_value(&acc_name, acc.id());
            Carried { acc, init, ty }
        })
        .collect())
}

/// Parse an optional `, %a, %b` trailing operand list.
fn parse_trailing_values(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Vec<ValueId>, (tir::parse::Span, Error)> {
    let mut values = vec![];
    while parser.parse_token(",") {
        values.push(parse_value_id(parser, context)?);
    }
    Ok(values)
}

/// Parse a loop body, seeding its entry block with the carried arguments.
fn parse_loop_body(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    scope: Option<Value>,
    carried: Vec<Value>,
) -> Result<tir::RegionId, (tir::parse::Span, Error)> {
    let entry_args = scope.into_iter().chain(carried).collect();
    Ok(parser
        .parse_region_with_entry_args(context, entry_args)?
        .id())
}

/// Parse `scf.while`'s `do(%b0, %b1)` clause, creating one body argument per carried
/// port; a non-carrying loop has no clause.
fn parse_body_carried(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
    carried: &[Carried],
) -> Result<Vec<Value>, (tir::parse::Span, Error)> {
    if carried.is_empty() {
        return Ok(vec![]);
    }
    expect_token(parser, "(")?;
    let mut values = vec![];
    for port in carried {
        if !values.is_empty() {
            expect_token(parser, ",")?;
        }
        let name = parser
            .parse_value_ref()
            .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
            .to_string();
        let value = context.create_value(port.ty, None);
        parser.define_value(&name, value.id());
        values.push(value);
    }
    expect_token(parser, ")")?;
    Ok(values)
}

/// The loop body's carried arguments: every entry argument but the token scope.
fn carried_arguments(context: &Context, body: &tir::BlockHandle) -> Vec<ValueId> {
    let token = TokenType::new(context);
    body.arguments()
        .iter()
        .filter(|argument| argument.ty() != token)
        .map(Value::id)
        .collect()
}

/// Verify a counted loop counts through one type: whatever `!index` or integer width
/// the lower bound names, the upper bound and step name too. Mixing widths would leave
/// the counter's arithmetic — and so its trip count — undefined.
fn verify_counter_type(context: &Context, op: &ForOp) -> Result<(), Error> {
    let [lower, upper, step] = op.operands()[..3] else {
        unreachable!("scf.for takes three bounds");
    };
    let expected = context.get_value(lower).ty();
    for (bound, name) in [(upper, "upper bound"), (step, "step")] {
        let found = context.get_value(bound).ty();
        if found != expected {
            return Err(Error::VerificationError(format!(
                "scf.for {name} counts through a different type than its lower bound"
            )));
        }
    }
    Ok(())
}

/// Verify a loop's carried ports: the init operands, the body's carried arguments, the
/// yielded values and the results must all have the same arity, matching pairwise in
/// type.
fn verify_loop_carried<T: LoopOp + Operation>(context: &Context, op: &T) -> Result<(), Error> {
    let label = format!("{}.{}", T::dialect(), T::name());
    let body = op.body_block();
    let terminator = context.get_op(*body.op_ids().last().unwrap());
    let yielded = exit_values(&terminator).to_vec();
    let token = TokenType::new(context);
    let arguments = body.arguments();
    let scope_args = arguments
        .iter()
        .filter(|argument| argument.ty() == token)
        .collect::<Vec<_>>();
    let body_args = arguments
        .iter()
        .filter(|argument| argument.ty() != token)
        .collect::<Vec<_>>();
    if scope_args.len() > 1 {
        return Err(Error::VerificationError(format!(
            "{label} body must have at most one token scope"
        )));
    }
    if !terminator.is::<YieldOp>() && !terminator.is::<BreakOp>() && !terminator.is::<ContinueOp>()
    {
        return Err(Error::VerificationError(format!(
            "{label} body must end with scf.yield, scf.break, or scf.continue"
        )));
    }
    if (terminator.is::<BreakOp>() || terminator.is::<ContinueOp>())
        && (scope_args.len() != 1 || terminator.operands()[0] != scope_args[0].id())
    {
        return Err(Error::VerificationError(format!(
            "{label} exit must consume its body token scope"
        )));
    }

    let results = op.loop_results();
    let inits = op.init_operands();
    let arity = results.len();
    if inits.len() != arity {
        return Err(Error::VerificationError(format!(
            "{label} has {} results but {} init operands",
            arity,
            inits.len()
        )));
    }
    if body_args.len() != arity {
        return Err(Error::VerificationError(format!(
            "{label} has {} results but its body carries {} arguments",
            arity,
            body_args.len()
        )));
    }
    if yielded.len() != arity {
        return Err(Error::VerificationError(format!(
            "{label} has {} results but its body yields {} values",
            arity,
            yielded.len()
        )));
    }

    for (port, &result) in results.iter().enumerate() {
        let ty = context.get_value(result).ty();
        if context.get_value(inits[port]).ty() != ty {
            return Err(Error::VerificationError(format!(
                "{label} init {port} type must match the type of result {port}"
            )));
        }
        if body_args[port].ty() != ty {
            return Err(Error::VerificationError(format!(
                "{label} body argument {port} type must match the type of result {port}"
            )));
        }
        if context.get_value(yielded[port]).ty() != ty {
            return Err(Error::VerificationError(format!(
                "{label} yielded value {port} type must match the type of result {port}"
            )));
        }
    }

    match scope_args.first() {
        Some(scope) => verify_scope_exits(context, &body, scope.id(), &results, &label),
        None => Ok(()),
    }
}

/// Verify every `scf.break`/`scf.continue` leaving through `scope`: an exit edge feeds
/// the loop's carried ports, so it carries one value per port, matching in type.
fn verify_scope_exits(
    context: &Context,
    block: &tir::BlockHandle,
    scope: ValueId,
    results: &[ValueId],
    label: &str,
) -> Result<(), Error> {
    for op_id in block.op_ids() {
        let op = context.get_op(op_id);
        for region in op.regions() {
            for nested in context.get_region(region).iter(context.clone()) {
                verify_scope_exits(context, &nested, scope, results, label)?;
            }
        }
        let is_exit = op.is::<BreakOp>() || op.is::<ContinueOp>();
        if !is_exit || op.operands()[0] != scope {
            continue;
        }
        let carried = &op.operands()[1..];
        if carried.len() != results.len() {
            return Err(Error::VerificationError(format!(
                "{label} carries {} values but its {}.{} carries {}",
                results.len(),
                op.dialect(),
                op.name(),
                carried.len()
            )));
        }
        for (port, (&value, &result)) in carried.iter().zip(results).enumerate() {
            if context.get_value(value).ty() != context.get_value(result).ty() {
                return Err(Error::VerificationError(format!(
                    "{}.{} value {port} type must match the type of {label} result {port}",
                    op.dialect(),
                    op.name()
                )));
            }
        }
    }
    Ok(())
}

fn verify_while_condition(context: &Context, op: &WhileOp) -> Result<(), Error> {
    let block = op.condition_region();
    let terminator = context.get_op(*block.op_ids().last().unwrap());
    if !terminator.is::<ConditionOp>() {
        return Err(Error::VerificationError(
            "scf.while condition must end with scf.condition".to_string(),
        ));
    }
    let arguments = block.arguments();
    let results = &op.0.results();
    if arguments.len() != results.len() {
        return Err(Error::VerificationError(format!(
            "scf.while has {} results but its condition carries {} arguments",
            results.len(),
            arguments.len()
        )));
    }
    // `scf.condition` forwards the deciding boolean plus one value per carried port.
    if terminator.operands().len() != results.len() + 1 {
        return Err(Error::VerificationError(format!(
            "scf.while has {} results but its scf.condition forwards {} values",
            results.len(),
            terminator.operands().len().saturating_sub(1)
        )));
    }
    for (port, &result) in results.iter().enumerate() {
        let ty = context.get_value(result).ty();
        if arguments[port].ty() != ty {
            return Err(Error::VerificationError(format!(
                "scf.while condition argument {port} type must match the type of result {port}"
            )));
        }
        if context.get_value(terminator.operands()[port + 1]).ty() != ty {
            return Err(Error::VerificationError(format!(
                "scf.condition forwarded value {port} type must match the type of result {port}"
            )));
        }
    }
    Ok(())
}

/// Parse an optional `-> <type>, <type>` result-type clause.
fn parse_result_types(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Vec<tir::TypeId>, (tir::parse::Span, Error)> {
    if !parser.parse_token("->") {
        return Ok(vec![]);
    }
    let mut types = vec![];
    loop {
        types.push(
            parser
                .parse_type(context)?
                .ok_or_else(|| (parser.span(), Error::ExpectedType))?,
        );
        if !parser.parse_token(",") {
            return Ok(types);
        }
    }
}

/// The values a structured region's single block yields through its terminator. A region
/// that leaves through an early exit never reaches the join, so it yields nothing there.
fn region_yielded_values(context: &Context, region: tir::RegionId) -> Vec<ValueId> {
    let Some(block) = context.get_region(region).iter(context.clone()).next() else {
        return vec![];
    };
    let Some(&terminator) = block.op_ids().last() else {
        return vec![];
    };
    let terminator = context.get_op(terminator);
    if terminator.is::<YieldOp>() {
        terminator.operands().to_vec()
    } else {
        vec![]
    }
}

/// Check that a structured region's terminator yields one value per result, matching in
/// type. Assumes the terminator already exists.
fn verify_region_yield(
    context: &Context,
    block: tir::BlockHandle,
    expected: &[tir::TypeId],
    label: &str,
) -> Result<(), Error> {
    let terminator = context.get_op(*block.op_ids().last().unwrap());
    // An arm that leaves through `scf.break`/`scf.continue` never reaches the join, so
    // it owes it no value; what it carries is checked against the loop it exits.
    if !terminator.is::<YieldOp>() {
        return Ok(());
    }
    let operands = &terminator.operands();
    if operands.len() != expected.len() {
        return Err(Error::VerificationError(format!(
            "{label} must yield {} values, but yields {}",
            expected.len(),
            operands.len()
        )));
    }
    for (index, (&operand, &ty)) in operands.iter().zip(expected).enumerate() {
        if context.get_value(operand).ty() != ty {
            return Err(Error::VerificationError(format!(
                "{label} yielded value {index} type must match the type of result {index}"
            )));
        }
    }
    Ok(())
}

fn verify_i1_operand(context: &Context, value: ValueId, label: &str) -> Result<(), Error> {
    let ty = context.get_value(value).ty();
    if ty != IntegerType::new(context, 1) {
        return Err(Error::VerificationError(format!(
            "{label} must have type i1"
        )));
    }
    Ok(())
}
