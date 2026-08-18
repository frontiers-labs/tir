//! `Display` impls rendering the AST to canonical (not byte-identical) SMT-LIB 2.7 text.

use std::fmt::{self, Display, Formatter};

use super::ast::*;

fn join<T: Display>(f: &mut Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            f.write_str(" ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl Display for SpecConstant {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SpecConstant::Numeral(n) => write!(f, "{n}"),
            SpecConstant::Decimal(s) => write!(f, "{s}"),
            SpecConstant::Hexadecimal(s) => write!(f, "#x{s}"),
            SpecConstant::Binary(s) => write!(f, "#b{s}"),
            SpecConstant::String(s) => write!(f, "\"{}\"", s.replace('"', "\"\"")),
        }
    }
}

impl Display for Symbol {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if is_simple_symbol(&self.0) {
            f.write_str(&self.0)
        } else {
            write!(f, "|{}|", self.0)
        }
    }
}

impl Display for Keyword {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, ":{}", self.0)
    }
}

impl Display for Index {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Index::Numeral(n) => write!(f, "{n}"),
            Index::Symbol(s) => write!(f, "{s}"),
        }
    }
}

impl Display for Identifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.indices.is_empty() {
            write!(f, "{}", self.symbol)
        } else {
            write!(f, "(_ {} ", self.symbol)?;
            join(f, &self.indices)?;
            f.write_str(")")
        }
    }
}

impl Display for Sort {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.params.is_empty() {
            write!(f, "{}", self.id)
        } else {
            write!(f, "({} ", self.id)?;
            join(f, &self.params)?;
            f.write_str(")")
        }
    }
}

impl Display for QualIdentifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            QualIdentifier::Plain(id) => write!(f, "{id}"),
            QualIdentifier::Annotated(id, sort) => write!(f, "(as {id} {sort})"),
        }
    }
}

impl Display for SExpr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SExpr::Constant(c) => write!(f, "{c}"),
            SExpr::Symbol(s) => write!(f, "{s}"),
            SExpr::Keyword(k) => write!(f, "{k}"),
            SExpr::List(items) => {
                f.write_str("(")?;
                join(f, items)?;
                f.write_str(")")
            }
        }
    }
}

impl Display for AttributeValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            AttributeValue::Constant(c) => write!(f, "{c}"),
            AttributeValue::Symbol(s) => write!(f, "{s}"),
            AttributeValue::List(items) => {
                f.write_str("(")?;
                join(f, items)?;
                f.write_str(")")
            }
        }
    }
}

impl Display for Attribute {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.value {
            None => write!(f, "{}", self.keyword),
            Some(value) => write!(f, "{} {value}", self.keyword),
        }
    }
}

impl Display for VarBinding {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({} {})", self.var, self.term)
    }
}

impl Display for SortedVar {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({} {})", self.var, self.sort)
    }
}

impl Display for Pattern {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Pattern::Var(s) => write!(f, "{s}"),
            Pattern::Constructor(ctor, vars) => {
                write!(f, "({ctor} ")?;
                join(f, vars)?;
                f.write_str(")")
            }
        }
    }
}

impl Display for MatchCase {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({} {})", self.pattern, self.body)
    }
}

impl Display for Term {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Term::Constant(c) => write!(f, "{c}"),
            Term::Ident(id) => write!(f, "{id}"),
            Term::App(head, args) => {
                write!(f, "({head} ")?;
                join(f, args)?;
                f.write_str(")")
            }
            Term::Let(binds, body) => {
                f.write_str("(let (")?;
                join(f, binds)?;
                write!(f, ") {body})")
            }
            Term::Forall(vars, body) => {
                f.write_str("(forall (")?;
                join(f, vars)?;
                write!(f, ") {body})")
            }
            Term::Exists(vars, body) => {
                f.write_str("(exists (")?;
                join(f, vars)?;
                write!(f, ") {body})")
            }
            Term::Match(scrutinee, cases) => {
                write!(f, "(match {scrutinee} (")?;
                join(f, cases)?;
                f.write_str("))")
            }
            Term::Annotated(term, attrs) => {
                write!(f, "(! {term} ")?;
                join(f, attrs)?;
                f.write_str(")")
            }
        }
    }
}

impl Display for FunctionDef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} (", self.name)?;
        join(f, &self.params)?;
        write!(f, ") {} {}", self.return_sort, self.body)
    }
}

impl Display for FunctionDec {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({} (", self.name)?;
        join(f, &self.params)?;
        write!(f, ") {})", self.return_sort)
    }
}

impl Display for PropLiteral {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.negated {
            write!(f, "(not {})", self.symbol)
        } else {
            write!(f, "{}", self.symbol)
        }
    }
}

impl Display for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Command::SetLogic(s) => write!(f, "(set-logic {s})"),
            Command::SetOption(a) => write!(f, "(set-option {a})"),
            Command::SetInfo(a) => write!(f, "(set-info {a})"),
            Command::DeclareSort(s, n) => write!(f, "(declare-sort {s} {n})"),
            Command::DefineSort(name, params, def) => {
                write!(f, "(define-sort {name} (")?;
                join(f, params)?;
                write!(f, ") {def})")
            }
            Command::DeclareConst(name, sort) => write!(f, "(declare-const {name} {sort})"),
            Command::DeclareFun(name, args, ret) => {
                write!(f, "(declare-fun {name} (")?;
                join(f, args)?;
                write!(f, ") {ret})")
            }
            Command::DefineFun(def) => write!(f, "(define-fun {def})"),
            Command::DefineFunRec(def) => write!(f, "(define-fun-rec {def})"),
            Command::DefineFunsRec(decs, bodies) => {
                f.write_str("(define-funs-rec (")?;
                join(f, decs)?;
                f.write_str(") (")?;
                join(f, bodies)?;
                f.write_str("))")
            }
            Command::Assert(t) => write!(f, "(assert {t})"),
            Command::CheckSat => f.write_str("(check-sat)"),
            Command::CheckSatAssuming(lits) => {
                f.write_str("(check-sat-assuming (")?;
                join(f, lits)?;
                f.write_str("))")
            }
            Command::GetAssertions => f.write_str("(get-assertions)"),
            Command::GetModel => f.write_str("(get-model)"),
            Command::GetValue(terms) => {
                f.write_str("(get-value (")?;
                join(f, terms)?;
                f.write_str("))")
            }
            Command::GetProof => f.write_str("(get-proof)"),
            Command::GetUnsatCore => f.write_str("(get-unsat-core)"),
            Command::GetUnsatAssumptions => f.write_str("(get-unsat-assumptions)"),
            Command::GetAssignment => f.write_str("(get-assignment)"),
            Command::GetInfo(k) => write!(f, "(get-info {k})"),
            Command::GetOption(k) => write!(f, "(get-option {k})"),
            Command::Push(n) => write!(f, "(push {n})"),
            Command::Pop(n) => write!(f, "(pop {n})"),
            Command::Reset => f.write_str("(reset)"),
            Command::ResetAssertions => f.write_str("(reset-assertions)"),
            Command::Echo(s) => write!(f, "(echo \"{}\")", s.replace('"', "\"\"")),
            Command::Exit => f.write_str("(exit)"),
        }
    }
}

impl Display for Script {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for command in &self.0 {
            writeln!(f, "{command}")?;
        }
        Ok(())
    }
}
