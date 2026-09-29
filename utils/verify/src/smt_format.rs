// Adapted from isla-lib's BSD-2-Clause `simplify` SMT writer. Kept local
// because the upstream formatter is private and writes text, while verifier
// traces must remain structured.

use anyhow::anyhow;
use isla_lib::bitvector::BV;
use isla_lib::ir::{SharedState, Symtab};
use isla_lib::smt::{smtlib, Sym};
use isla_lib::zencode;
use tir_symbolic::smtlib::ast::{
    Identifier, Index, QualIdentifier, Sort, SpecConstant, Symbol, Term,
};

/// The SMT name of an Isla variable.
pub fn var(sym: Sym) -> String {
    format!("v{sym}")
}

pub fn sort(ty: &smtlib::Ty, symtab: &Symtab) -> Sort {
    use smtlib::Ty::*;
    match ty {
        Bool => Sort::bool(),
        BitVec(width) => Sort::bitvec(*width),
        Enum(id) => Sort::simple(Identifier::simple(zencode::decode(
            symtab.to_str(id.to_name()),
        ))),
        Array(domain, range) => Sort {
            id: Identifier::simple("Array"),
            params: vec![sort(domain, symtab), sort(range, symtab)],
        },
        Float(ebits, sbits) => Sort::simple(indexed("FloatingPoint", &[*ebits, *sbits])),
        RoundingMode => Sort::simple(Identifier::simple("RoundingMode")),
    }
}

fn indexed(name: &str, indices: &[u32]) -> Identifier {
    Identifier {
        symbol: Symbol(name.into()),
        indices: indices
            .iter()
            .map(|&index| Index::Numeral(u128::from(index)))
            .collect(),
    }
}

/// A `#x`/`#b` literal as Isla prints bit-vectors.
pub fn bits_literal(text: &str) -> anyhow::Result<Term> {
    let constant = if let Some(digits) = text.strip_prefix("#x") {
        SpecConstant::Hexadecimal(digits.into())
    } else if let Some(digits) = text.strip_prefix("#b") {
        SpecConstant::Binary(digits.into())
    } else {
        anyhow::bail!("unrecognized Isla bit-vector literal {text}")
    };
    Ok(Term::Constant(constant))
}

pub fn term<B: BV>(exp: &smtlib::Exp<Sym>, shared: &SharedState<B>) -> anyhow::Result<Term> {
    use smtlib::Exp::*;
    let app = |name: &str, args: &[&smtlib::Exp<Sym>]| -> anyhow::Result<Term> {
        let args = args
            .iter()
            .map(|arg| term(arg, shared))
            .collect::<anyhow::Result<_>>()?;
        Ok(Term::app(name, args))
    };
    let indexed_app = |name: &str, indices: &[u32], args: &[&smtlib::Exp<Sym>]| {
        let args = args
            .iter()
            .map(|arg| term(arg, shared))
            .collect::<anyhow::Result<_>>()?;
        anyhow::Ok(Term::App(
            QualIdentifier::Plain(indexed(name, indices)),
            args,
        ))
    };
    Ok(match exp {
        Var(value) => Term::ident(var(*value)),
        Bits(bits) => Term::Constant(SpecConstant::Binary(
            bits.iter()
                .rev()
                .map(|bit| if *bit { '1' } else { '0' })
                .collect(),
        )),
        Bits64(bits) => bits_literal(&bits.to_string())?,
        Enum(member) => {
            let members = shared
                .type_info
                .enums
                .get(&member.enum_id.to_name())
                .ok_or_else(|| anyhow!("missing Isla enum"))?;
            Term::ident(zencode::decode(
                shared.symtab.to_str(members[member.member]),
            ))
        }
        Bool(value) => Term::bool(*value),
        Eq(lhs, rhs) => app("=", &[lhs, rhs])?,
        Neq(lhs, rhs) => Term::app("not", vec![app("=", &[lhs, rhs])?]),
        And(lhs, rhs) => app("and", &[lhs, rhs])?,
        Or(lhs, rhs) => app("or", &[lhs, rhs])?,
        Not(value) => app("not", &[value])?,
        Bvnot(value) => app("bvnot", &[value])?,
        Bvand(lhs, rhs) => app("bvand", &[lhs, rhs])?,
        Bvor(lhs, rhs) => app("bvor", &[lhs, rhs])?,
        Bvxor(lhs, rhs) => app("bvxor", &[lhs, rhs])?,
        Bvnand(lhs, rhs) => app("bvnand", &[lhs, rhs])?,
        Bvnor(lhs, rhs) => app("bvnor", &[lhs, rhs])?,
        Bvxnor(lhs, rhs) => app("bvxnor", &[lhs, rhs])?,
        Bvneg(value) => app("bvneg", &[value])?,
        Bvadd(lhs, rhs) => app("bvadd", &[lhs, rhs])?,
        Bvsub(lhs, rhs) => app("bvsub", &[lhs, rhs])?,
        Bvmul(lhs, rhs) => app("bvmul", &[lhs, rhs])?,
        Bvudiv(lhs, rhs) => app("bvudiv", &[lhs, rhs])?,
        Bvsdiv(lhs, rhs) => app("bvsdiv", &[lhs, rhs])?,
        Bvurem(lhs, rhs) => app("bvurem", &[lhs, rhs])?,
        Bvsrem(lhs, rhs) => app("bvsrem", &[lhs, rhs])?,
        Bvsmod(lhs, rhs) => app("bvsmod", &[lhs, rhs])?,
        Bvult(lhs, rhs) => app("bvult", &[lhs, rhs])?,
        Bvslt(lhs, rhs) => app("bvslt", &[lhs, rhs])?,
        Bvule(lhs, rhs) => app("bvule", &[lhs, rhs])?,
        Bvsle(lhs, rhs) => app("bvsle", &[lhs, rhs])?,
        Bvuge(lhs, rhs) => app("bvuge", &[lhs, rhs])?,
        Bvsge(lhs, rhs) => app("bvsge", &[lhs, rhs])?,
        Bvugt(lhs, rhs) => app("bvugt", &[lhs, rhs])?,
        Bvsgt(lhs, rhs) => app("bvsgt", &[lhs, rhs])?,
        Extract(high, low, value) => indexed_app("extract", &[*high, *low], &[value])?,
        ZeroExtend(amount, value) => indexed_app("zero_extend", &[*amount], &[value])?,
        SignExtend(amount, value) => indexed_app("sign_extend", &[*amount], &[value])?,
        Bvshl(lhs, rhs) => app("bvshl", &[lhs, rhs])?,
        Bvlshr(lhs, rhs) => app("bvlshr", &[lhs, rhs])?,
        Bvashr(lhs, rhs) => app("bvashr", &[lhs, rhs])?,
        Concat(lhs, rhs) => app("concat", &[lhs, rhs])?,
        Ite(cond, then_value, else_value) => app("ite", &[cond, then_value, else_value])?,
        App(function, args) => app(&var(*function), &args.iter().collect::<Vec<_>>())?,
        Select(array, index) => app("select", &[array, index])?,
        Store(array, index, value) => app("store", &[array, index, value])?,
        Distinct(values) => app("distinct", &values.iter().collect::<Vec<_>>())?,
        FPConstant(value, ebits, sbits) => {
            use smtlib::FPConstant::*;
            let name = match value {
                NaN => "NaN",
                Inf { negative: false } => "+oo",
                Inf { negative: true } => "-oo",
                Zero { negative: false } => "+zero",
                Zero { negative: true } => "-zero",
            };
            Term::Ident(QualIdentifier::Plain(indexed(name, &[*ebits, *sbits])))
        }
        FPRoundingMode(mode) => {
            use smtlib::FPRoundingMode::*;
            Term::ident(match mode {
                RoundNearestTiesToEven => "roundNearestTiesToEven",
                RoundNearestTiesToAway => "roundNearestTiesToAway",
                RoundTowardPositive => "roundTowardPositive",
                RoundTowardNegative => "roundTowardNegative",
                RoundTowardZero => "roundTowardZero",
            })
        }
        FPUnary(op, value) => {
            use smtlib::FPUnary::*;
            match op {
                Abs => app("fp.abs", &[value])?,
                Neg => app("fp.neg", &[value])?,
                IsNormal => app("fp.isNormal", &[value])?,
                IsSubnormal => app("fp.isSubnormal", &[value])?,
                IsZero => app("fp.isZero", &[value])?,
                IsInfinite => app("fp.isInfinite", &[value])?,
                IsNaN => app("fp.isNaN", &[value])?,
                IsNegative => app("fp.isNegative", &[value])?,
                IsPositive => app("fp.isPositive", &[value])?,
                FromIEEE(ebits, sbits) => indexed_app("to_fp", &[*ebits, *sbits], &[value])?,
            }
        }
        FPRoundingUnary(op, mode, value) => {
            use smtlib::FPRoundingUnary::*;
            let args = &[mode.as_ref(), value.as_ref()];
            match op {
                Sqrt => app("fp.sqrt", args)?,
                RoundToIntegral => app("fp.roundToIntegral", args)?,
                Convert(ebits, sbits) | FromSigned(ebits, sbits) => {
                    indexed_app("to_fp", &[*ebits, *sbits], args)?
                }
                FromUnsigned(ebits, sbits) => {
                    indexed_app("to_fp_unsigned", &[*ebits, *sbits], args)?
                }
                ToSigned(width) => indexed_app("fp.to_sbv", &[*width], args)?,
                ToUnsigned(width) => indexed_app("fp.to_ubv", &[*width], args)?,
            }
        }
        FPBinary(op, lhs, rhs) => {
            use smtlib::FPBinary::*;
            let name = match op {
                Rem => "fp.rem",
                Min => "fp.min",
                Max => "fp.max",
                Leq => "fp.leq",
                Lt => "fp.lt",
                Geq => "fp.geq",
                Gt => "fp.gt",
                Eq => "fp.eq",
            };
            app(name, &[lhs, rhs])?
        }
        FPRoundingBinary(op, mode, lhs, rhs) => {
            use smtlib::FPRoundingBinary::*;
            let name = match op {
                Add => "fp.add",
                Sub => "fp.sub",
                Mul => "fp.mul",
                Div => "fp.div",
            };
            app(name, &[mode, lhs, rhs])?
        }
        FPfma(mode, x, y, z) => app("fp.fma", &[mode, x, y, z])?,
    })
}
