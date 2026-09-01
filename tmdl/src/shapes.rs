//! Expanding an encoding expression into shapes.
//!
//! An `encoding` block is an expression: a concatenation of fields, some of
//! them chosen by an `if` over the operands. A *shape* is that expression with
//! every condition decided — a fixed list of fields plus the guard that selects
//! it. An encoding with no condition has exactly one shape whose guard is
//! always true, which is what every fixed-width ISA has.
//!
//! Evaluating the conditions over the operand domains decides which truth
//! assignments are reachable, so an assignment no operand value produces
//! (`base == 0b100` and `base == 0b101` at once) yields no shape, and every
//! sampled operand value picks exactly one shape by construction.
//!
//! The domain of an operand up to 8 bits wide is every value it can hold, so
//! the expansion is exact there. A wider one is sampled: the boundaries, every
//! single-bit value, every constant the conditions name and its neighbours, and
//! for every `x[hi..lo] == k` a condition spells, values that satisfy it with
//! the rest of the operand zero and all-ones. Each condition is therefore
//! satisfiable and falsifiable on its own, but two conditions over disjoint
//! slices of one wide operand can still be missed together, which drops a
//! shape. That is why this is the fast check; the proof that encoding and
//! decoding agree per shape is an SMT obligation.

use std::collections::HashMap;

use crate::Span;
use crate::ast;
use crate::encoding::literal_width;
use crate::sema::expr_span;
use crate::types::Type;
use crate::utils::{OperandConstraint, parse_literal_value};

/// Conditions per encoding, beyond which the expansion is a design smell
/// rather than a limit to raise.
const MAX_CONDITIONS: usize = 8;

/// Operand value combinations the expansion will try before giving up. Reached
/// only by conditions reading many wide operands at once.
const MAX_DOMAIN_POINTS: u64 = 1 << 20;

/// The condition under which a shape is the encoding: a disjunction of
/// conjunctions over the encoding's `if` conditions.
#[derive(Debug, Clone, PartialEq)]
pub struct Guard(pub Vec<Vec<GuardLiteral>>);

/// One `if` condition of the encoding, and the value it takes in a shape.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardLiteral {
    pub cond: ast::Expr,
    pub value: bool,
}

impl Guard {
    /// Whether every operand value satisfies the guard, i.e. the encoding has
    /// one shape and no condition selects it.
    pub fn is_always(&self) -> bool {
        self.0.iter().any(Vec::is_empty)
    }
}

/// A guard as the tests it makes, with nothing left of how they were spelled.
///
/// The encoder, the SMT model and the checker all have to ask the same question
/// of the same operand, so the question is read out of the source once, here,
/// and each of them renders this. A test no encoder can answer is an error at
/// expansion time rather than three separate refusals downstream.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Always,
    Not(Box<Predicate>),
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    /// One bit of an operand's encoded pattern.
    Bit {
        op: String,
        bit: u16,
    },
    SliceEq {
        op: String,
        lo: u16,
        hi: u16,
        value: u128,
    },
    /// The operand against a constant, both read at `cmp_width` bits.
    Cmp {
        op: String,
        width: u16,
        cmp_width: u16,
        cmp: CmpOp,
        value: i128,
    },
    /// Whether the operand's value survives a round trip through `bits`, which
    /// is what an encoding asks before spelling a narrower field. An operand
    /// still waiting for a fixup answers no, so the widest shape takes it.
    Fits {
        op: String,
        width: u16,
        bits: u16,
        signed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    ULt,
    ULe,
    UGt,
    UGe,
}

impl Predicate {
    /// Whether the guard holds when each operand spells the pattern `value`
    /// gives for it. Every value is known here, which is what a verifier
    /// building concrete words has; the encoder's own evaluation is
    /// three-valued because a fixup is not resolved yet.
    pub fn holds(&self, value: &impl Fn(&str) -> u64) -> bool {
        let read = |op: &str, width: u16| value(op) & mask(width);
        match self {
            Predicate::Always => true,
            Predicate::Not(inner) => !inner.holds(value),
            Predicate::And(parts) => parts.iter().all(|part| part.holds(value)),
            Predicate::Or(parts) => parts.iter().any(|part| part.holds(value)),
            Predicate::Bit { op, bit } => shift_right(value(op), u64::from(*bit)) & 1 == 1,
            Predicate::SliceEq { op, lo, hi, value } => {
                u128::from(shift_right(read(op, hi + 1), u64::from(*lo))) == *value
            }
            Predicate::Cmp {
                op,
                width,
                cmp_width,
                cmp,
                value,
            } => {
                let sign = |v: u64| -> i128 {
                    match *cmp_width < 64 && v & (1 << (cmp_width - 1)) != 0 {
                        true => i128::from(v) - (1i128 << cmp_width),
                        false => i128::from(v),
                    }
                };
                let held = read(op, *width);
                let value = *value & (mask(*cmp_width) as i128);
                match cmp {
                    CmpOp::Eq => i128::from(held) == value,
                    CmpOp::Ne => i128::from(held) != value,
                    CmpOp::Lt => sign(held) < sign(value as u64),
                    CmpOp::Le => sign(held) <= sign(value as u64),
                    CmpOp::Gt => sign(held) > sign(value as u64),
                    CmpOp::Ge => sign(held) >= sign(value as u64),
                    CmpOp::ULt => i128::from(held) < value,
                    CmpOp::ULe => i128::from(held) <= value,
                    CmpOp::UGt => i128::from(held) > value,
                    CmpOp::UGe => i128::from(held) >= value,
                }
            }
            Predicate::Fits {
                op,
                width,
                bits,
                signed,
            } => {
                let held = read(op, *width);
                let kept = read(op, *bits);
                let extended = match *signed && kept & (1 << (bits - 1)) != 0 {
                    true => kept | (mask(*width) & !mask(*bits)),
                    false => kept,
                };
                extended == held
            }
        }
    }
}

impl CmpOp {
    /// The name of the matching `tir::backend::binary::CmpOp` variant.
    pub fn name(self) -> &'static str {
        match self {
            CmpOp::Eq => "Eq",
            CmpOp::Ne => "Ne",
            CmpOp::Lt => "Lt",
            CmpOp::Le => "Le",
            CmpOp::Gt => "Gt",
            CmpOp::Ge => "Ge",
            CmpOp::ULt => "ULt",
            CmpOp::ULe => "ULe",
            CmpOp::UGt => "UGt",
            CmpOp::UGe => "UGe",
        }
    }

    /// Whether the comparison reads both sides as signed.
    pub fn signed(self) -> bool {
        matches!(self, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge)
    }
}

/// The tests a shape's guard makes, or why one of them is not a test the
/// operands answer.
pub fn lower_guard(guard: &Guard, ctx: &Context) -> Result<Predicate, String> {
    let mut clauses = Vec::new();
    for clause in &guard.0 {
        let mut literals = Vec::new();
        for literal in clause {
            let cond = lower_cond(&literal.cond, ctx)?;
            literals.push(match literal.value {
                true => cond,
                false => Predicate::Not(Box::new(cond)),
            });
        }
        // Nothing left to test: this shape holds for every operand tuple.
        if literals.is_empty() {
            return Ok(Predicate::Always);
        }
        clauses.push(match literals.len() {
            1 => literals.remove(0),
            _ => Predicate::And(literals),
        });
    }
    Ok(match clauses.len() {
        0 => Predicate::Always,
        1 => clauses.remove(0),
        _ => Predicate::Or(clauses),
    })
}

fn lower_cond(cond: &ast::Expr, ctx: &Context) -> Result<Predicate, String> {
    let unsupported = || {
        format!(
            "condition in the encoding of '{}' is not a test the encoder can make \
             of an operand",
            ctx.owner
        )
    };
    if let Some(fits) = fit_test(cond, ctx) {
        return Ok(fits);
    }
    if let Some((op, bit, set)) = operand_bit(cond, ctx) {
        let test = Predicate::Bit {
            op: op.to_string(),
            bit,
        };
        return Ok(match set {
            true => test,
            false => Predicate::Not(Box::new(test)),
        });
    }
    match cond {
        ast::Expr::Unary(unary) => Ok(Predicate::Not(Box::new(lower_cond(&unary.x, ctx)?))),
        ast::Expr::Binary(binary) => {
            let (lhs, rhs) = (&*binary.lhs, &*binary.rhs);
            match &binary.op {
                ast::BinOp::BitwiseAnd => Ok(Predicate::And(vec![
                    lower_cond(lhs, ctx)?,
                    lower_cond(rhs, ctx)?,
                ])),
                ast::BinOp::BitwiseOr => Ok(Predicate::Or(vec![
                    lower_cond(lhs, ctx)?,
                    lower_cond(rhs, ctx)?,
                ])),
                op => {
                    let (literal, operand, flipped) = match (int_literal(lhs), int_literal(rhs)) {
                        (Some(literal), None) => (literal, rhs, true),
                        (None, Some(literal)) => (literal, lhs, false),
                        _ => return Err(unsupported()),
                    };
                    lower_comparison(op, operand, literal, flipped, ctx)
                }
            }
        }
        _ => Err(unsupported()),
    }
}

/// A comparison of an operand, or a slice of one, against a constant.
fn lower_comparison(
    op: &ast::BinOp,
    operand: &ast::Expr,
    literal: &ast::LitInt,
    flipped: bool,
    ctx: &Context,
) -> Result<Predicate, String> {
    let unsupported = || {
        format!(
            "condition in the encoding of '{}' compares something other than an \
             operand against a constant",
            ctx.owner
        )
    };
    let value = parse_literal_value(literal);
    // A slice test is a test of its own: `base[hi..lo] == k`.
    if matches!(op, ast::BinOp::Equal | ast::BinOp::NotEqual)
        && let ast::Expr::Slice(slc) = operand
        && let ast::Expr::Ident(id) = &*slc.base
        && ctx.operand_width(&id.name).is_some()
    {
        let test = Predicate::SliceEq {
            op: id.name.clone(),
            lo: slc.lo,
            hi: slc.hi,
            value: u128::from(value),
        };
        return Ok(match op {
            ast::BinOp::NotEqual => Predicate::Not(Box::new(test)),
            _ => test,
        });
    }
    let ast::Expr::Ident(id) = operand else {
        return Err(unsupported());
    };
    let Some(width) = ctx.operand_width(&id.name) else {
        return Err(unsupported());
    };
    let cmp = match (op, flipped) {
        (ast::BinOp::Equal, _) => CmpOp::Eq,
        (ast::BinOp::NotEqual, _) => CmpOp::Ne,
        (ast::BinOp::LessThan, false) | (ast::BinOp::GreaterThan, true) => CmpOp::Lt,
        (ast::BinOp::LessThenEqual, false) | (ast::BinOp::GreaterThanEqual, true) => CmpOp::Le,
        (ast::BinOp::GreaterThan, false) | (ast::BinOp::LessThan, true) => CmpOp::Gt,
        (ast::BinOp::GreaterThanEqual, false) | (ast::BinOp::LessThenEqual, true) => CmpOp::Ge,
        (ast::BinOp::UnsignedLessThan, false) | (ast::BinOp::UnsignedGreaterThan, true) => {
            CmpOp::ULt
        }
        (ast::BinOp::UnsignedLessThenEqual, false)
        | (ast::BinOp::UnsignedGreaterThanEqual, true) => CmpOp::ULe,
        (ast::BinOp::UnsignedGreaterThan, false) | (ast::BinOp::UnsignedLessThan, true) => {
            CmpOp::UGt
        }
        (ast::BinOp::UnsignedGreaterThanEqual, false)
        | (ast::BinOp::UnsignedLessThenEqual, true) => CmpOp::UGe,
        _ => return Err(unsupported()),
    };
    // The operand is read as its declared bit pattern, and both sides are then
    // compared at the width the shape expansion used: the wider of the operand
    // and the literal as spelled (a decimal literal has no width, and is read
    // as 64 bits).
    Ok(Predicate::Cmp {
        op: id.name.clone(),
        width,
        cmp_width: width.max(literal_width(literal.value()).unwrap_or(64)),
        cmp,
        value: i128::from(value),
    })
}

/// The round-trip test an encoding makes before spelling a narrow field:
/// `sext(x as bits<n>, width(x)) == x` for a signed field, `x as bits<n> == x`
/// or the `zext` spelling for an unsigned one. `x[n-1..0]` stands for the cast.
///
/// The test is written in the model rather than built into the compiler, so
/// this reads the shape of it back out. An equivalent spelling it does not
/// match fails loudly at code generation, never silently.
pub(crate) fn fit_test(cond: &ast::Expr, ctx: &Context) -> Option<Predicate> {
    let ast::Expr::Binary(binary) = cond else {
        return None;
    };
    if binary.op != ast::BinOp::Equal {
        return None;
    }
    let operand = |expr: &ast::Expr| match expr {
        ast::Expr::Ident(id) => ctx
            .operand_width(&id.name)
            .map(|width| (id.name.clone(), width)),
        _ => None,
    };
    let (probe, (name, width)) = match (operand(&binary.lhs), operand(&binary.rhs)) {
        (None, Some(held)) => (&*binary.lhs, held),
        (Some(held), None) => (&*binary.rhs, held),
        _ => return None,
    };
    // The extension back to the operand's own width, if the test spells one.
    let (kept, signed) = match probe {
        ast::Expr::Call(call) => {
            let (ast::Expr::BuiltinFunction(builtin), [kept, to]) =
                (&*call.callee, call.arguments.as_slice())
            else {
                return None;
            };
            if int_literal(to).map(parse_literal_value)? != u64::from(width) {
                return None;
            }
            match builtin {
                ast::BuiltinFunction::SExt => (kept, true),
                ast::BuiltinFunction::ZExt => (kept, false),
                _ => return None,
            }
        }
        kept => (kept, false),
    };
    // The narrow field itself, as a cast or as the low bits.
    let (base, bits) = match kept {
        ast::Expr::Cast(cast) => (&*cast.x, int_literal(&cast.width).map(parse_literal_value)?),
        ast::Expr::Slice(slice) if slice.lo == 0 => (&*slice.base, u64::from(slice.hi) + 1),
        _ => return None,
    };
    let ast::Expr::Ident(id) = base else {
        return None;
    };
    let bits = u16::try_from(bits).ok()?;
    (id.name == name && bits < width).then_some(Predicate::Fits {
        op: name,
        width,
        bits,
        signed,
    })
}

/// The operand, bit and polarity a condition tests, for `x[bit]` and
/// `x[bit] == 0` / `x[bit] == 1`.
fn operand_bit<'a>(cond: &'a ast::Expr, ctx: &Context) -> Option<(&'a str, u16, bool)> {
    let (base, index, set) = match cond {
        ast::Expr::IndexAccess(idx) => (&idx.base, idx.index, true),
        ast::Expr::Binary(binary) if binary.op == ast::BinOp::Equal => {
            let value = int_literal(&binary.rhs).map(parse_literal_value)?;
            match (&*binary.lhs, value) {
                (ast::Expr::IndexAccess(idx), 0) => (&idx.base, idx.index, false),
                (ast::Expr::IndexAccess(idx), 1) => (&idx.base, idx.index, true),
                _ => return None,
            }
        }
        _ => return None,
    };
    match &**base {
        ast::Expr::Ident(id) if ctx.operand_width(&id.name).is_some() => {
            Some((&id.name, index, set))
        }
        _ => None,
    }
}

fn int_literal(expr: &ast::Expr) -> Option<&ast::LitInt> {
    match expr {
        ast::Expr::Lit(ast::Lit::Int(li)) => Some(li),
        _ => None,
    }
}

/// One fixed bit map of an encoding: the fields it spells, high bit first, and
/// the guard that picks it.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub guard: Guard,
    pub fields: Vec<ast::EncodingField>,
}

/// What an encoding expression is resolved against: the instruction's effective
/// operands and parameters, the encoding width of every register class, and the
/// ISA's encoding unit.
pub struct Context {
    pub owner: String,
    pub operands: Vec<(String, Type, OperandConstraint)>,
    pub params: HashMap<String, (Type, Option<ast::Expr>)>,
    pub classes: HashMap<String, RegisterClassInfo>,
    pub unit: Option<u16>,
}

/// What an encoding needs to know about a register class: how many bits an
/// operand of it spells, and which of those bit patterns name a register.
pub struct RegisterClassInfo {
    pub encoding_len: u16,
    /// Encoding indices the class holds. Empty when no register carries one,
    /// in which case every pattern of `encoding_len` bits is assumed possible.
    pub indices: Vec<u64>,
}

/// The `if` conditions of an encoding, keyed by identity, together with one
/// truth assignment of them.
struct Assignment<'a> {
    conditions: &'a [(String, &'a ast::Expr)],
    values: &'a [bool],
}

impl Assignment<'_> {
    fn takes(&self, cond: &ast::Expr) -> bool {
        let key = expr_key(cond);
        self.conditions
            .iter()
            .position(|(held, _)| *held == key)
            .is_some_and(|index| self.values[index])
    }
}

/// Expand `encoding` into its shapes, with any diagnostic the expansion found.
/// Shapes are in first-reached order; identical field lists are one shape whose
/// guard is the disjunction of the assignments that reach it.
pub fn expand(encoding: &ast::Expr, ctx: &Context) -> (Vec<Shape>, Vec<(Span, String)>) {
    let mut errors = Vec::new();
    let encoding = &normalize(encoding, ctx);
    let conditions = collect_conditions(encoding);

    if conditions.len() > MAX_CONDITIONS {
        errors.push((
            expr_span(encoding),
            format!(
                "encoding of instruction '{}' has {} conditions, more than the {MAX_CONDITIONS} \
                 a shape expansion admits",
                ctx.owner,
                conditions.len()
            ),
        ));
        return (Vec::new(), errors);
    }

    let mut shapes: Vec<Shape> = Vec::new();
    for values in reachable_assignments(&conditions, ctx, &mut errors) {
        let clause = conditions
            .iter()
            .zip(&values)
            .map(|((_, cond), value)| GuardLiteral {
                cond: (*cond).clone(),
                value: *value,
            })
            .collect();
        let assignment = Assignment {
            conditions: &conditions,
            values: &values,
        };
        let mut fields = Vec::new();
        flatten(encoding, &assignment, ctx, 0, &mut fields, &mut errors);
        // Two assignments that spell the same bits are one shape, whatever the
        // spans of the branches they came from.
        let key = field_key(&fields);
        match shapes
            .iter_mut()
            .find(|shape| field_key(&shape.fields) == key)
        {
            Some(shape) => shape.guard.0.push(clause),
            None => shapes.push(Shape {
                guard: Guard(vec![clause]),
                fields,
            }),
        }
    }

    // One mistake in an encoding is reached once per truth assignment.
    let mut seen = std::collections::HashSet::new();
    errors.retain(|(span, message)| seen.insert((span.start, span.end, message.clone())));

    if shapes.is_empty() && errors.is_empty() {
        errors.push((
            expr_span(encoding),
            format!(
                "encoding of instruction '{}' has no satisfiable shape",
                ctx.owner
            ),
        ));
    }
    (shapes, errors)
}

/// An encoding expression in the form the shapes are computed over: `let`
/// bindings substituted into their uses, the block an inlined `fn` body leaves
/// behind collapsed to the value it produces, and every condition the
/// instruction's parameters already answer taken.
fn normalize(expr: &ast::Expr, ctx: &Context) -> ast::Expr {
    fn collapse(expr: &ast::Expr) -> ast::Expr {
        let expr = crate::utils::map_child_exprs(expr, &mut collapse);
        match &expr {
            ast::Expr::Block(block) if block.last_expr_return && block.stmts.len() == 1 => {
                block.stmts[0].clone()
            }
            _ => expr,
        }
    }
    let expr = collapse(&crate::utils::inline_let_bindings(expr));
    decide(&resolve_widths(&expr, ctx), ctx)
}

/// Replace `width(x)` with the width `x` spells in an encoding, which for a
/// register operand is its class's `ENCODING_LEN` rather than the value width a
/// behavior would read. `fn signed_fits(x, n)` is written over it, so this has
/// to happen before the conditions are evaluated.
fn resolve_widths(expr: &ast::Expr, ctx: &Context) -> ast::Expr {
    let expr = crate::utils::map_child_exprs(expr, &mut |child| resolve_widths(child, ctx));
    let ast::Expr::Call(call) = &expr else {
        return expr;
    };
    if !matches!(
        &*call.callee,
        ast::Expr::BuiltinFunction(ast::BuiltinFunction::Width)
    ) {
        return expr;
    }
    let Some(ast::Expr::Ident(id)) = call.arguments.first() else {
        return expr;
    };
    match ctx.named_width(&id.name) {
        // Decimal: this is a count of bits, not a field, and it reads as one
        // in the guard a document or a diagnostic prints.
        Ok(width) => ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(
            width.to_string(),
            call.span,
        ))),
        // Left as it was: the condition then reports what it could not read.
        Err(_) => expr,
    }
}

/// Take every branch the parameters decide. One template writes the encoding of
/// every width it serves, so a condition an instruction's parameters answer
/// (`REXW | reg[3] | rm[3]`, with `REXW` set) is not a test the encoder repeats
/// at run time: the branch it selects replaces the `if`, and only the tests the
/// operands answer are left to guard a shape.
fn decide(expr: &ast::Expr, ctx: &Context) -> ast::Expr {
    let expr = crate::utils::map_child_exprs(expr, &mut |child| decide(child, ctx));
    let ast::Expr::If(if_) = &expr else {
        return expr;
    };
    let cond = fold(&if_.cond, ctx);
    match int_value(&cond) {
        Some(0) => if_
            .else_
            .as_deref()
            .cloned()
            .unwrap_or(ast::Expr::Tuple(ast::Tuple {
                elements: Vec::new(),
                span: if_.span,
            })),
        Some(_) => (*if_.then).clone(),
        None => ast::Expr::If(ast::If {
            cond: Box::new(cond),
            then: if_.then.clone(),
            else_: if_.else_.clone(),
            span: if_.span,
        }),
    }
}

/// A condition with everything the operands do not decide replaced by its
/// value, and the identities that leaves behind (`0 | x`, `1 & x`) applied, so
/// what remains reads operands only.
fn fold(cond: &ast::Expr, ctx: &Context) -> ast::Expr {
    let folded = crate::utils::map_child_exprs(cond, &mut |child| fold(child, ctx));
    if !matches!(folded, ast::Expr::Lit(_))
        && let Ok((value, width)) = eval(&folded, ctx, &HashMap::new())
        && (1..=64).contains(&width)
    {
        return ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(
            format!("0b{value:0width$b}", width = usize::from(width)),
            expr_span(&folded),
        )));
    }
    simplify(folded, ctx)
}

/// `x | 0`, `x | 1`, `x & 0` and `x & 1`, where the constant side is what a
/// decided parameter left behind. Both sides are the width of the result, so
/// dropping one keeps the condition's width.
fn simplify(expr: ast::Expr, ctx: &Context) -> ast::Expr {
    let ast::Expr::Binary(binary) = &expr else {
        return expr;
    };
    if !matches!(binary.op, ast::BinOp::BitwiseOr | ast::BinOp::BitwiseAnd) {
        return expr;
    }
    let (Some(width), Some(rhs_width)) = (
        static_width(&binary.lhs, ctx),
        static_width(&binary.rhs, ctx),
    ) else {
        return expr;
    };
    if width != rhs_width {
        return expr;
    }
    let ones = mask(width);
    let absorbing = match binary.op {
        ast::BinOp::BitwiseOr => ones,
        _ => 0,
    };
    for (side, other) in [(&binary.lhs, &binary.rhs), (&binary.rhs, &binary.lhs)] {
        match int_value(side).map(|value| value & ones) {
            Some(value) if value == absorbing => return (**side).clone(),
            Some(_) => return (**other).clone(),
            None => {}
        }
    }
    expr
}

/// The width an encoding condition has whatever its operands hold.
fn static_width(expr: &ast::Expr, ctx: &Context) -> Option<u16> {
    let env: HashMap<&str, (u64, u16)> = ctx
        .operands
        .iter()
        .filter_map(|(name, _, _)| Some((name.as_str(), (0, ctx.named_width(name).ok()?))))
        .collect();
    eval(expr, ctx, &env).ok().map(|(_, width)| width)
}

fn int_value(expr: &ast::Expr) -> Option<u64> {
    match expr {
        ast::Expr::Lit(ast::Lit::Int(li)) => Some(parse_literal_value(li)),
        _ => None,
    }
}

/// The distinct `if` conditions of an encoding, keyed by identity and in source
/// order. The same test spelled in two places is one condition, so the shapes
/// it selects agree.
fn collect_conditions(expr: &ast::Expr) -> Vec<(String, &ast::Expr)> {
    fn walk<'a>(expr: &'a ast::Expr, out: &mut Vec<(String, &'a ast::Expr)>) {
        match expr {
            ast::Expr::Tuple(tuple) => tuple.elements.iter().for_each(|e| walk(e, out)),
            ast::Expr::If(if_) => {
                let key = expr_key(&if_.cond);
                if !out.iter().any(|(held, _)| *held == key) {
                    out.push((key, &if_.cond));
                }
                walk(&if_.then, out);
                if let Some(else_) = &if_.else_ {
                    walk(else_, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

/// The bits a shape spells, independent of where they were written.
fn field_key(fields: &[ast::EncodingField]) -> Vec<(u16, String)> {
    fields
        .iter()
        .map(|field| (field.width, expr_key(&field.value)))
        .collect()
}

/// An expression's identity, independent of where it was written. A form no
/// encoder can decide keys on its span, so nothing merges by accident.
fn expr_key(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Ident(id) => id.name.clone(),
        ast::Expr::Lit(ast::Lit::Int(li)) => format!(
            "{}:{}",
            parse_literal_value(li),
            literal_width(li.value()).unwrap_or(0)
        ),
        ast::Expr::Slice(slc) => format!("{}[{}..{}]", expr_key(&slc.base), slc.hi, slc.lo),
        ast::Expr::IndexAccess(idx) => format!("{}[{}]", expr_key(&idx.base), idx.index),
        ast::Expr::Unary(unary) => format!("{:?} {}", unary.op, expr_key(&unary.x)),
        ast::Expr::Binary(binary) => format!(
            "({} {:?} {})",
            expr_key(&binary.lhs),
            binary.op,
            expr_key(&binary.rhs)
        ),
        ast::Expr::Cast(cast) => format!("({} as {})", expr_key(&cast.x), expr_key(&cast.width)),
        ast::Expr::Tuple(tuple) => format!(
            "({})",
            tuple
                .elements
                .iter()
                .map(expr_key)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ast::Expr::If(if_) => format!(
            "if {} {{ {} }} else {{ {} }}",
            expr_key(&if_.cond),
            expr_key(&if_.then),
            if_.else_.as_deref().map(expr_key).unwrap_or_default()
        ),
        other => format!("@{:?}", expr_span(other)),
    }
}

/// Flatten `expr` under one truth assignment into the fields it spells,
/// returning their total width. A nested concatenation is a group the ISA's
/// manual draws as whole encoding units, so it must fill them.
fn flatten(
    expr: &ast::Expr,
    assignment: &Assignment<'_>,
    ctx: &Context,
    depth: usize,
    fields: &mut Vec<ast::EncodingField>,
    errors: &mut Vec<(Span, String)>,
) -> u16 {
    match expr {
        ast::Expr::Tuple(tuple) => {
            let width: u16 = tuple
                .elements
                .iter()
                .map(|e| flatten(e, assignment, ctx, depth + 1, fields, errors))
                .sum();
            if let Some(unit) = ctx.unit
                && depth > 0
                && !width.is_multiple_of(unit)
            {
                errors.push((
                    tuple.span,
                    format!(
                        "concatenation in the encoding of '{}' is {width} bits, \
                         which is not a whole number of {unit}-bit encoding units",
                        ctx.owner
                    ),
                ));
            }
            width
        }
        ast::Expr::If(if_) => {
            let branch = match assignment.takes(&if_.cond) {
                true => Some(&*if_.then),
                false => if_.else_.as_deref(),
            };
            match branch {
                Some(branch) => flatten(branch, assignment, ctx, depth, fields, errors),
                None => 0,
            }
        }
        leaf => match ctx.leaf_width(leaf) {
            Ok(width) => {
                fields.push(ast::EncodingField {
                    value: leaf.clone(),
                    width,
                    span: expr_span(leaf),
                });
                width
            }
            Err(message) => {
                errors.push((expr_span(leaf), message));
                0
            }
        },
    }
}

impl Context {
    /// The width a leaf of an encoding expression contributes. A register
    /// operand contributes its class's `ENCODING_LEN`; anything whose width is
    /// an ISA-parameter expression has no single width and must be sliced.
    fn leaf_width(&self, value: &ast::Expr) -> Result<u16, String> {
        match value {
            ast::Expr::Lit(ast::Lit::Int(li)) => literal_width(li.value()),
            ast::Expr::Ident(id) => self.named_width(&id.name),
            ast::Expr::Slice(slc) => self.sliced(&slc.base).map(|()| slc.hi - slc.lo + 1),
            ast::Expr::IndexAccess(idx) => self.sliced(&idx.base).map(|()| 1),
            // `x as bits<n>`: the low n bits, the same field `x[n-1..0]` names.
            ast::Expr::Cast(cast) => match int_literal(&cast.width).map(parse_literal_value) {
                Some(width) if (1..=128).contains(&width) => Ok(width as u16),
                _ => Err(format!(
                    "cast in the encoding of '{}' must name a constant width",
                    self.owner
                )),
            },
            _ => Err(format!(
                "encoding field in '{}' must be a literal, parameter, operand or bit slice",
                self.owner
            )),
        }
    }

    /// A field may take bits out of an operand, whose value the encoder reads
    /// at encode time. Taking them out of a parameter instead spells a constant
    /// no consumer reads back as one, so the parameter is spelled whole.
    fn sliced(&self, base: &ast::Expr) -> Result<(), String> {
        match base {
            ast::Expr::Ident(id) if self.operand(&id.name).is_none() => Err(format!(
                "encoding of '{}' takes bits out of '{}', which is a parameter; \
                 spell the parameter whole",
                self.owner, id.name
            )),
            _ => Ok(()),
        }
    }

    /// The declared width of an operand: its `bits<N>`, or the `ENCODING_LEN`
    /// of a register operand's class. The width a condition over that operand
    /// is evaluated at, so a runtime guard can read the same pattern.
    pub fn operand_width(&self, name: &str) -> Option<u16> {
        self.operand(name)?;
        self.named_width(name).ok()
    }

    fn named_width(&self, name: &str) -> Result<u16, String> {
        let ty = self
            .operand(name)
            .map(|(ty, _)| ty)
            .or_else(|| self.params.get(name).map(|(ty, _)| ty))
            .ok_or_else(|| {
                format!(
                    "Unknown '{name}' in encoding of instruction '{}': \
                     not a parameter or operand",
                    self.owner
                )
            })?;
        match ty {
            Type::Bits(width) => Ok(*width),
            Type::Struct(class) => self
                .classes
                .get(class)
                .map(|info| info.encoding_len)
                .ok_or_else(|| {
                    format!(
                        "register class '{class}' of operand '{name}' in '{}' \
                         declares no ENCODING_LEN",
                        self.owner
                    )
                }),
            Type::BitsExpr(_) => Err(format!(
                "width of '{name}' in '{}' depends on an ISA parameter; \
                 give the encoding field an explicit bit range",
                self.owner
            )),
            _ => Err(format!(
                "'{name}' in encoding of '{}' has no bit width",
                self.owner
            )),
        }
    }

    fn operand(&self, name: &str) -> Option<(&Type, OperandConstraint)> {
        self.operands
            .iter()
            .find(|(held, _, _)| held == name)
            .map(|(_, ty, constraint)| (ty, *constraint))
    }

    /// The width and the values an operand may take: its bit domain, minus what
    /// its `#[align]` and `#[nonzero]` constraints exclude and, for a register,
    /// minus the patterns its class does not name. A wide operand is sampled
    /// rather than enumerated (see the module docs). `None` when the operand has
    /// no domain the expansion can enumerate.
    fn domain(&self, name: &str, probes: &Probes) -> Option<(u16, Vec<u64>)> {
        let (ty, constraint) = self.operand(name)?;
        let mut held: Option<&[u64]> = None;
        let width = match ty {
            Type::Bits(width) => *width,
            Type::Struct(class) => {
                let info = self.classes.get(class)?;
                if !info.indices.is_empty() {
                    held = Some(&info.indices);
                }
                info.encoding_len
            }
            _ => return None,
        };
        if width == 0 || width > 64 {
            return None;
        }
        let domain_mask = mask(width);
        let mut values: Vec<u64> = if width <= 8 {
            (0..=domain_mask).collect()
        } else {
            let mut values = vec![0, 1, domain_mask, domain_mask >> 1, (domain_mask >> 1) + 1];
            values.extend((0..width).map(|bit| 1u64 << bit));
            values.extend(
                probes
                    .literals
                    .iter()
                    .flat_map(|v| [*v, v.wrapping_sub(1), v.wrapping_add(1)]),
            );
            // Values that satisfy each slice test a condition spells, with the
            // rest of the operand zero and all-ones: without them a test like
            // `x[7..4] == 0b0110` is never true anywhere in the sample and
            // loses its shape. Falsifying it needs nothing extra, since the
            // base values above already differ from any one slice value.
            // The edges of each fit test: the widest value the narrow field
            // holds and the first one it does not, on both sides of zero.
            for bits in probes.fits.get(name).into_iter().flatten() {
                values.extend([
                    mask(*bits),
                    mask(*bits) + 1,
                    mask(bits - 1),
                    mask(bits - 1) + 1,
                    domain_mask & !mask(bits - 1),
                    domain_mask & !mask(*bits),
                ]);
            }
            for (lo, hi, value) in probes.slices.get(name).into_iter().flatten() {
                let field = shift_left(mask(hi - lo + 1), u64::from(*lo));
                let placed = shift_left(*value, u64::from(*lo)) & field;
                values.push(placed);
                values.push(placed | !field);
            }
            values.iter_mut().for_each(|v| *v &= domain_mask);
            values
        };
        if let Some(held) = held {
            values.retain(|v| held.contains(v));
        }
        values.retain(|v| {
            v.is_multiple_of(u64::from(constraint.align)) && (!constraint.nonzero || *v != 0)
        });
        values.sort_unstable();
        values.dedup();
        Some((width, values))
    }
}

/// What the conditions of an encoding test, so the operand domains can cover
/// it: the operands they read, the constants they name, and the slice
/// comparisons they spell.
#[derive(Default)]
struct Probes {
    names: Vec<String>,
    literals: Vec<u64>,
    /// Operand name -> `(lo, hi, value)` of each `x[hi..lo] == value` spelled.
    slices: HashMap<String, Vec<(u16, u16, u64)>>,
    /// Operand name -> the field widths a fit test asks about.
    fits: HashMap<String, Vec<u16>>,
}

impl Probes {
    fn collect(&mut self, cond: &ast::Expr, ctx: &Context) {
        crate::utils::visit_exprs(cond, &mut |node| {
            if let Some(Predicate::Fits { op, bits, .. }) = fit_test(node, ctx) {
                self.fits.entry(op).or_default().push(bits);
            }
            match node {
                ast::Expr::Ident(id) if ctx.operand(&id.name).is_some() => {
                    if !self.names.contains(&id.name) {
                        self.names.push(id.name.clone());
                    }
                }
                ast::Expr::Lit(ast::Lit::Int(li)) => self.literals.push(parse_literal_value(li)),
                _ => {}
            }
            let ast::Expr::Binary(binary) = node else {
                return;
            };
            if !matches!(binary.op, ast::BinOp::Equal | ast::BinOp::NotEqual) {
                return;
            }
            let (slice, literal) = match (&*binary.lhs, &*binary.rhs) {
                (slice, ast::Expr::Lit(ast::Lit::Int(li))) => (slice, li),
                (ast::Expr::Lit(ast::Lit::Int(li)), slice) => (slice, li),
                _ => return,
            };
            let (base, lo, hi) = match slice {
                ast::Expr::Slice(slc) => (&slc.base, slc.lo, slc.hi),
                ast::Expr::IndexAccess(idx) => (&idx.base, idx.index, idx.index),
                _ => return,
            };
            if let ast::Expr::Ident(id) = &**base
                && ctx.operand(&id.name).is_some()
            {
                self.slices.entry(id.name.clone()).or_default().push((
                    lo,
                    hi,
                    parse_literal_value(literal),
                ));
            }
        });
    }
}

/// The truth assignments of `conditions` some operand value produces. Operands
/// no condition reads are irrelevant, so only the rest are enumerated.
fn reachable_assignments(
    conditions: &[(String, &ast::Expr)],
    ctx: &Context,
    errors: &mut Vec<(Span, String)>,
) -> Vec<Vec<bool>> {
    if conditions.is_empty() {
        return vec![Vec::new()];
    }

    let mut probes = Probes::default();
    for (_, cond) in conditions {
        probes.collect(cond, ctx);
    }
    let names = probes.names.clone();

    let mut domains = Vec::new();
    for name in &names {
        match ctx.domain(name, &probes) {
            Some(domain) => domains.push(domain),
            None => {
                errors.push((
                    expr_span(conditions[0].1),
                    format!(
                        "operand '{name}' in a condition of the encoding of '{}' \
                         has no domain the shape expansion can enumerate",
                        ctx.owner
                    ),
                ));
                return Vec::new();
            }
        }
    }

    // An operand its constraints leave no value for makes every shape
    // unreachable; `expand` reports the encoding, not the enumeration.
    if domains.iter().any(|(_, values)| values.is_empty()) {
        return Vec::new();
    }

    let points = domains.iter().try_fold(1u64, |acc, (_, values)| {
        acc.checked_mul(values.len() as u64)
            .filter(|points| *points <= MAX_DOMAIN_POINTS)
    });
    if points.is_none() {
        errors.push((
            expr_span(conditions[0].1),
            format!(
                "conditions in the encoding of '{}' read more operand values than \
                 the {MAX_DOMAIN_POINTS} combinations a shape expansion tries",
                ctx.owner
            ),
        ));
        return Vec::new();
    }

    let mut assignments: Vec<Vec<bool>> = Vec::new();
    let mut cursor = vec![0usize; names.len()];
    loop {
        let env: HashMap<&str, (u64, u16)> = names
            .iter()
            .zip(&domains)
            .zip(&cursor)
            .map(|((name, (width, values)), index)| (name.as_str(), (values[*index], *width)))
            .collect();

        let mut assignment = Vec::with_capacity(conditions.len());
        for (_, cond) in conditions {
            match eval(cond, ctx, &env) {
                // A condition is a single bit; anything wider is a value the
                // encoding meant to spell, not a test (see `docs/tmdl/syntax.md`).
                Ok((_, width)) if width != 1 => {
                    errors.push((
                        expr_span(cond),
                        format!(
                            "condition in the encoding of '{}' is bits<{width}>; \
                             a condition is bits<1>",
                            ctx.owner
                        ),
                    ));
                    return Vec::new();
                }
                Ok((value, _)) => assignment.push(value != 0),
                Err(reason) => {
                    errors.push((
                        expr_span(cond),
                        format!("condition in the encoding of '{}' {reason}", ctx.owner),
                    ));
                    return Vec::new();
                }
            }
        }
        if !assignments.contains(&assignment) {
            assignments.push(assignment);
        }

        let mut level = names.len();
        loop {
            if level == 0 {
                return assignments;
            }
            level -= 1;
            cursor[level] += 1;
            if cursor[level] < domains[level].1.len() {
                break;
            }
            cursor[level] = 0;
        }
    }
}

/// Shifts that saturate to zero instead of overflowing at 64 bits.
fn shift_left(value: u64, by: u64) -> u64 {
    u32::try_from(by)
        .ok()
        .and_then(|by| value.checked_shl(by))
        .unwrap_or(0)
}

fn shift_right(value: u64, by: u64) -> u64 {
    u32::try_from(by)
        .ok()
        .and_then(|by| value.checked_shr(by))
        .unwrap_or(0)
}

/// The low `width` bits set.
fn mask(width: u16) -> u64 {
    match width >= 64 {
        true => u64::MAX,
        false => (1u64 << width) - 1,
    }
}

/// Evaluate an encoding condition to a value and its width, or say why the
/// operands do not determine it.
fn eval(
    expr: &ast::Expr,
    ctx: &Context,
    env: &HashMap<&str, (u64, u16)>,
) -> Result<(u64, u16), String> {
    match expr {
        ast::Expr::Lit(ast::Lit::Int(li)) => {
            let width = literal_width(li.value()).unwrap_or(64);
            Ok((parse_literal_value(li), width))
        }
        ast::Expr::Ident(id) => {
            if let Some(value) = env.get(id.name.as_str()) {
                return Ok(*value);
            }
            let Some((ty, value)) = ctx.params.get(&id.name) else {
                return Err(format!(
                    "reads '{}', which is not one of its operands or parameters",
                    id.name
                ));
            };
            let width = match ty {
                Type::Bits(width) => *width,
                _ => 64,
            };
            match value {
                Some(ast::Expr::Lit(ast::Lit::Int(li))) => Ok((parse_literal_value(li), width)),
                _ => Err(format!(
                    "reads parameter '{}', which holds no literal value",
                    id.name
                )),
            }
        }
        ast::Expr::Slice(slc) => {
            let (value, _) = eval(&slc.base, ctx, env)?;
            let width = slc.hi - slc.lo + 1;
            Ok((shift_right(value, u64::from(slc.lo)) & mask(width), width))
        }
        ast::Expr::IndexAccess(idx) => {
            let (value, _) = eval(&idx.base, ctx, env)?;
            Ok((shift_right(value, u64::from(idx.index)) & 1, 1))
        }
        ast::Expr::Unary(unary) => {
            let (value, width) = eval(&unary.x, ctx, env)?;
            match unary.op {
                ast::UnOp::BitwiseNot => Ok((!value & mask(width), width)),
            }
        }
        ast::Expr::Tuple(tuple) => {
            let mut acc = (0u64, 0u16);
            for element in &tuple.elements {
                let (value, width) = eval(element, ctx, env)?;
                acc.1 += width;
                if acc.1 > 64 {
                    return Err(
                        "concatenates more than 64 bits, which no condition can decide".to_string(),
                    );
                }
                acc.0 = shift_left(acc.0, u64::from(width)) | value;
            }
            Ok(acc)
        }
        ast::Expr::If(if_) => {
            let (cond, _) = eval(&if_.cond, ctx, env)?;
            match (cond != 0, &if_.else_) {
                (true, _) => eval(&if_.then, ctx, env),
                (false, Some(else_)) => eval(else_, ctx, env),
                (false, None) => Ok((0, 0)),
            }
        }
        ast::Expr::Binary(binary) => eval_binary(binary, ctx, env),
        // `x as bits<n>` keeps the low n bits, which is what a narrow encoding
        // field spells of a wider operand.
        ast::Expr::Cast(cast) => {
            let (value, _) = eval(&cast.x, ctx, env)?;
            let width = eval_width(&cast.width, ctx, env)?;
            Ok((value & mask(width), width))
        }
        ast::Expr::Call(call) => {
            let (ast::Expr::BuiltinFunction(builtin), [value, width]) =
                (&*call.callee, call.arguments.as_slice())
            else {
                return Err("calls a function no encoder can evaluate".to_string());
            };
            let (value, from) = eval(value, ctx, env)?;
            let to = eval_width(width, ctx, env)?;
            match builtin {
                ast::BuiltinFunction::SExt => Ok((sign_extend(value, from) & mask(to), to)),
                ast::BuiltinFunction::ZExt => Ok((value & mask(to), to)),
                other => Err(format!("calls '{other:?}', which no encoder can evaluate")),
            }
        }
        _ => Err("is not an expression over the operands".to_string()),
    }
}

/// A width an encoding names: a constant between 1 and 64.
fn eval_width(
    expr: &ast::Expr,
    ctx: &Context,
    env: &HashMap<&str, (u64, u16)>,
) -> Result<u16, String> {
    let (value, _) = eval(expr, ctx, env)?;
    u16::try_from(value)
        .ok()
        .filter(|width| (1..=64).contains(width))
        .ok_or_else(|| format!("names the width {value}, which no encoder can spell"))
}

/// `value`, read as a signed number of `width` bits, in 64.
fn sign_extend(value: u64, width: u16) -> u64 {
    match width > 0 && width < 64 && value & (1 << (width - 1)) != 0 {
        true => value | !mask(width),
        false => value,
    }
}

fn eval_binary(
    binary: &ast::Binary,
    ctx: &Context,
    env: &HashMap<&str, (u64, u16)>,
) -> Result<(u64, u16), String> {
    let (lhs, lhs_width) = eval(&binary.lhs, ctx, env)?;
    let (rhs, rhs_width) = eval(&binary.rhs, ctx, env)?;
    let width = lhs_width.max(rhs_width);
    // A narrower side widens by zero-extension first, so both are read signed
    // at the same width: `imm < 0b1000` with `imm: bits<8>` compares against 8.
    let signed = |value: u64| -> i64 {
        match width != 0 && width < 64 && value & (1 << (width - 1)) != 0 {
            true => (value | !mask(width)) as i64,
            false => value as i64,
        }
    };
    let bit = |b: bool| Ok((u64::from(b), 1u16));
    match &binary.op {
        ast::BinOp::Add => Ok((lhs.wrapping_add(rhs) & mask(width), width)),
        ast::BinOp::Sub => Ok((lhs.wrapping_sub(rhs) & mask(width), width)),
        ast::BinOp::Mul => Ok((lhs.wrapping_mul(rhs) & mask(width), width)),
        ast::BinOp::BitwiseAnd => Ok((lhs & rhs, width)),
        ast::BinOp::BitwiseOr => Ok((lhs | rhs, width)),
        ast::BinOp::BitwiseXor => Ok((lhs ^ rhs, width)),
        ast::BinOp::ShiftLeftLogical => Ok((shift_left(lhs, rhs) & mask(width), width)),
        ast::BinOp::ShiftRightLogical => Ok((shift_right(lhs, rhs), width)),
        ast::BinOp::Equal => bit(lhs == rhs),
        ast::BinOp::NotEqual => bit(lhs != rhs),
        ast::BinOp::UnsignedLessThan => bit(lhs < rhs),
        ast::BinOp::UnsignedLessThenEqual => bit(lhs <= rhs),
        ast::BinOp::UnsignedGreaterThan => bit(lhs > rhs),
        ast::BinOp::UnsignedGreaterThanEqual => bit(lhs >= rhs),
        ast::BinOp::LessThan => bit(signed(lhs) < signed(rhs)),
        ast::BinOp::LessThenEqual => bit(signed(lhs) <= signed(rhs)),
        ast::BinOp::GreaterThan => bit(signed(lhs) > signed(rhs)),
        ast::BinOp::GreaterThanEqual => bit(signed(lhs) >= signed(rhs)),
        op => Err(format!("applies '{op:?}', which no encoder can decide")),
    }
}
