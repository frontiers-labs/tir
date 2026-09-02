use std::sync::OnceLock;

use crate::sem::{SemExpr, parse};

use super::axioms::{Axiom, axiom_from_expr};

pub(crate) struct Theory {
    pub(crate) axioms: Vec<Axiom>,
}

pub(crate) fn theory() -> &'static Theory {
    static THEORY: OnceLock<Theory> = OnceLock::new();
    THEORY.get_or_init(|| {
        parse_theory(include_str!("../../defs/isel.sexp"))
            .expect("core/defs/isel.sexp must be a valid isel theory")
    })
}

pub(crate) fn axioms() -> Vec<Axiom> {
    theory().axioms.clone()
}

fn atom(expr: &SemExpr) -> Option<&str> {
    match expr {
        SemExpr::Atom(atom) => Some(atom),
        SemExpr::List(_) => None,
    }
}

fn list(expr: &SemExpr) -> Result<&[SemExpr], String> {
    match expr {
        SemExpr::List(items) => Ok(items),
        SemExpr::Atom(_) => Err("expected a list".into()),
    }
}

fn parse_theory(text: &str) -> Result<Theory, String> {
    let text = text
        .lines()
        .filter(|line| !line.trim_start().starts_with(';'))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed = parse(&text).ok_or("malformed s-expression")?;
    let items = list(&parsed)?;
    if items.first().and_then(atom) != Some("theory") {
        return Err("expected a `theory` form".into());
    }
    let mut axioms = Vec::new();
    for section in &items[1..] {
        match list(section)?.first().and_then(atom) {
            Some("axiom") => axioms.push(axiom_from_expr(section)?),
            _ => return Err("unknown theory section".into()),
        }
    }
    Ok(Theory { axioms })
}
