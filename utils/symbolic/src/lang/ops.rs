use tir_adt::APInt;

use super::SymKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidthRule {
    First,
    Bool,
    Sum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtTemplate {
    Binary(&'static str),
    Compare(&'static str),
    Shift(&'static str),
    Unary(&'static str),
    Concat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvalRule {
    Add,
    Sub,
    Mul,
    SDiv,
    UDiv,
    SRem,
    URem,
    Neg,
    Eq,
    Ne,
    SLt,
    SLe,
    SGt,
    SGe,
    ULt,
    ULe,
    UGt,
    UGe,
    Shl,
    AShr,
    LShr,
    Or,
    And,
    Xor,
    Xnor,
    Not,
    Concat,
}

#[derive(Clone, Copy, Debug)]
pub struct ScalarOp {
    pub kind: SymKind,
    pub name: &'static str,
    pub arity: usize,
    pub commutative: bool,
    pub width: WidthRule,
    pub smt: SmtTemplate,
    /// Rust spelling of the [`SymKind`] variant used by graph printers.
    pub rust: &'static str,
    eval: EvalRule,
}

macro_rules! op {
    ($kind:ident, $name:literal, $arity:literal, $commutative:literal, $width:ident,
     $smt:expr, $rust:literal, $eval:ident) => {
        ScalarOp {
            kind: SymKind::$kind,
            name: $name,
            arity: $arity,
            commutative: $commutative,
            width: WidthRule::$width,
            smt: $smt,
            rust: $rust,
            eval: EvalRule::$eval,
        }
    };
}

pub const SCALAR_OPS: &[ScalarOp] = &[
    op!(
        Add,
        "add",
        2,
        true,
        First,
        SmtTemplate::Binary("bvadd"),
        "Add",
        Add
    ),
    op!(
        Sub,
        "sub",
        2,
        false,
        First,
        SmtTemplate::Binary("bvsub"),
        "Sub",
        Sub
    ),
    op!(
        Mul,
        "mul",
        2,
        true,
        First,
        SmtTemplate::Binary("bvmul"),
        "Mul",
        Mul
    ),
    op!(
        Div,
        "div",
        2,
        false,
        First,
        SmtTemplate::Binary("bvsdiv"),
        "Div",
        SDiv
    ),
    op!(
        UDiv,
        "udiv",
        2,
        false,
        First,
        SmtTemplate::Binary("bvudiv"),
        "UDiv",
        UDiv
    ),
    op!(
        SRem,
        "srem",
        2,
        false,
        First,
        SmtTemplate::Binary("bvsrem"),
        "SRem",
        SRem
    ),
    op!(
        URem,
        "urem",
        2,
        false,
        First,
        SmtTemplate::Binary("bvurem"),
        "URem",
        URem
    ),
    op!(
        Neg,
        "neg",
        1,
        false,
        First,
        SmtTemplate::Unary("bvneg"),
        "Neg",
        Neg
    ),
    op!(Eq, "eq", 2, true, Bool, SmtTemplate::Compare("="), "Eq", Eq),
    op!(
        Ne,
        "ne",
        2,
        true,
        Bool,
        SmtTemplate::Compare("distinct"),
        "Ne",
        Ne
    ),
    op!(
        Lt,
        "lt",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvslt"),
        "Lt",
        SLt
    ),
    op!(
        Le,
        "le",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvsle"),
        "Le",
        SLe
    ),
    op!(
        Gt,
        "gt",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvsgt"),
        "Gt",
        SGt
    ),
    op!(
        Ge,
        "ge",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvsge"),
        "Ge",
        SGe
    ),
    op!(
        ULt,
        "ult",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvult"),
        "ULt",
        ULt
    ),
    op!(
        ULe,
        "ule",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvule"),
        "ULe",
        ULe
    ),
    op!(
        UGt,
        "ugt",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvugt"),
        "UGt",
        UGt
    ),
    op!(
        UGe,
        "uge",
        2,
        false,
        Bool,
        SmtTemplate::Compare("bvuge"),
        "UGe",
        UGe
    ),
    op!(
        ShiftLeft,
        "shl",
        2,
        false,
        First,
        SmtTemplate::Shift("bvshl"),
        "ShiftLeft",
        Shl
    ),
    op!(
        ShiftRightArithmetic,
        "ashr",
        2,
        false,
        First,
        SmtTemplate::Shift("bvashr"),
        "ShiftRightArithmetic",
        AShr
    ),
    op!(
        ShiftRightLogic,
        "lshr",
        2,
        false,
        First,
        SmtTemplate::Shift("bvlshr"),
        "ShiftRightLogic",
        LShr
    ),
    op!(
        Or,
        "or",
        2,
        true,
        First,
        SmtTemplate::Binary("bvor"),
        "Or",
        Or
    ),
    op!(
        And,
        "and",
        2,
        true,
        First,
        SmtTemplate::Binary("bvand"),
        "And",
        And
    ),
    op!(
        Xor,
        "xor",
        2,
        true,
        First,
        SmtTemplate::Binary("bvxor"),
        "Xor",
        Xor
    ),
    op!(
        Xnor,
        "xnor",
        2,
        true,
        First,
        SmtTemplate::Binary("bvxnor"),
        "Xnor",
        Xnor
    ),
    op!(
        Not,
        "not",
        1,
        false,
        First,
        SmtTemplate::Unary("bvnot"),
        "Not",
        Not
    ),
    op!(
        Concat,
        "concat",
        2,
        false,
        Sum,
        SmtTemplate::Concat,
        "Concat",
        Concat
    ),
];

pub fn scalar_op(kind: SymKind) -> Option<&'static ScalarOp> {
    SCALAR_OPS.iter().find(|op| op.kind == kind)
}

pub fn scalar_op_named(name: &str) -> Option<&'static ScalarOp> {
    SCALAR_OPS.iter().find(|op| op.name == name)
}

fn widen(value: APInt, width: u32) -> APInt {
    if value.width() >= width {
        value
    } else if value.is_signed() {
        value.sign_extend(width)
    } else {
        value.zero_extend(width)
    }
}

fn coerce(lhs: APInt, rhs: APInt) -> (APInt, APInt) {
    let width = lhs.width().max(rhs.width());
    (widen(lhs, width), widen(rhs, width))
}

impl ScalarOp {
    pub fn eval_int(&self, operands: &[APInt]) -> APInt {
        assert_eq!(operands.len(), self.arity);
        if self.arity == 1 {
            let value = operands[0].clone();
            return match self.eval {
                EvalRule::Neg => value.neg(),
                EvalRule::Not => value.not(),
                _ => unreachable!(),
            };
        }
        let (lhs, rhs) = coerce(operands[0].clone(), operands[1].clone());
        let boolean = |value| APInt::new(1, u64::from(value));
        match self.eval {
            EvalRule::Add => lhs.add(&rhs),
            EvalRule::Sub => lhs.sub(&rhs),
            EvalRule::Mul => lhs.mul(&rhs),
            EvalRule::SDiv => lhs.sdiv(&rhs),
            EvalRule::UDiv => lhs.udiv(&rhs),
            EvalRule::SRem => lhs.srem(&rhs),
            EvalRule::URem => lhs.urem(&rhs),
            EvalRule::Eq => boolean(lhs.with_signed(false) == rhs.with_signed(false)),
            EvalRule::Ne => boolean(lhs.with_signed(false) != rhs.with_signed(false)),
            EvalRule::SLt => boolean(lhs.slt(&rhs)),
            EvalRule::SLe => boolean(lhs.sle(&rhs)),
            EvalRule::SGt => boolean(lhs.sgt(&rhs)),
            EvalRule::SGe => boolean(lhs.sge(&rhs)),
            EvalRule::ULt => boolean(lhs.ult(&rhs)),
            EvalRule::ULe => boolean(lhs.ule(&rhs)),
            EvalRule::UGt => boolean(lhs.ugt(&rhs)),
            EvalRule::UGe => boolean(lhs.uge(&rhs)),
            EvalRule::Shl => lhs.shl(rhs.to_u64() as u32),
            EvalRule::AShr => lhs.with_signed(true).ashr(rhs.to_u64() as u32),
            EvalRule::LShr => lhs.lshr(rhs.to_u64() as u32),
            EvalRule::Or => lhs.or(&rhs),
            EvalRule::And => lhs.and(&rhs),
            EvalRule::Xor => lhs.xor(&rhs),
            EvalRule::Xnor => lhs.xor(&rhs).not(),
            EvalRule::Concat => {
                let width = operands[0].width() + operands[1].width();
                operands[0]
                    .zero_extend(width)
                    .shl(operands[1].width())
                    .or(&operands[1].zero_extend(width))
            }
            EvalRule::Neg | EvalRule::Not => unreachable!(),
        }
    }
}
