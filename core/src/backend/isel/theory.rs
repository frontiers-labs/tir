use std::sync::OnceLock;

use tir::sem::{SemExpr, SymKind, op_kind, parse};

use super::axioms::{Axiom, parse_axiom};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum GoalShape {
    Extension,
    Unary,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Leaf {
    Zero,
    One,
    N,
    W,
    WMinusN,
    OnesN,
    OnesW,
}

pub(crate) struct Goal {
    pub(crate) kind: SymKind,
    pub(crate) shape: GoalShape,
    pub(crate) leaves: Vec<Leaf>,
    pub(crate) widths: Vec<(u32, u32)>,
}

pub(crate) struct Family {
    pub(crate) requires: Vec<SymKind>,
    pub(crate) axioms: Vec<String>,
}

pub(crate) struct Theory {
    pub(crate) max_ops: usize,
    pub(crate) candidates_per_class: usize,
    pub(crate) operators: Vec<SymKind>,
    pub(crate) goals: Vec<Goal>,
    pub(crate) families: Vec<Family>,
}

pub(crate) fn theory() -> &'static Theory {
    static THEORY: OnceLock<Theory> = OnceLock::new();
    THEORY.get_or_init(|| {
        parse_theory(include_str!("../../../defs/isel.sexp"))
            .expect("core/defs/isel.sexp must be a valid isel theory")
    })
}

pub(crate) fn enabled_axioms(rooted: impl Fn(SymKind) -> bool) -> Vec<Axiom> {
    theory()
        .families
        .iter()
        .filter(|family| family.requires.iter().copied().all(&rooted))
        .flat_map(|family| {
            family
                .axioms
                .iter()
                .map(|text| parse_axiom(text).expect("checked theory axiom must parse"))
        })
        .collect()
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

fn kind(expr: &SemExpr) -> Result<SymKind, String> {
    let name = atom(expr).ok_or("operator must be an atom")?;
    op_kind(name).ok_or_else(|| format!("unknown semantic operator `{name}`"))
}

fn number(expr: &SemExpr) -> Result<u32, String> {
    atom(expr)
        .ok_or("number must be an atom")?
        .parse()
        .map_err(|_| "expected an unsigned integer".to_string())
}

fn render(expr: &SemExpr) -> String {
    match expr {
        SemExpr::Atom(atom) => atom.clone(),
        SemExpr::List(items) => {
            let body = items.iter().map(render).collect::<Vec<_>>().join(" ");
            format!("({body})")
        }
    }
}

fn parse_leaf(expr: &SemExpr) -> Result<Leaf, String> {
    match atom(expr) {
        Some("zero") => Ok(Leaf::Zero),
        Some("one") => Ok(Leaf::One),
        Some("n") => Ok(Leaf::N),
        Some("w") => Ok(Leaf::W),
        Some("w-minus-n") => Ok(Leaf::WMinusN),
        Some("ones-n") => Ok(Leaf::OnesN),
        Some("ones-w") => Ok(Leaf::OnesW),
        Some(other) => Err(format!("unknown search leaf `{other}`")),
        None => Err("search leaf must be an atom".into()),
    }
}

fn parse_goal(items: &[SemExpr]) -> Result<Goal, String> {
    let [_, goal_kind, shape, sections @ ..] = items else {
        return Err("expected (goal <operator> <shape> <section>...)".into());
    };
    let shape = match atom(shape) {
        Some("extension") => GoalShape::Extension,
        Some("unary") => GoalShape::Unary,
        _ => return Err("goal shape must be `extension` or `unary`".into()),
    };
    let mut leaves = None;
    let mut widths = None;
    for section in sections {
        let section = list(section)?;
        match section.first().and_then(atom) {
            Some("leaves") => {
                leaves = Some(
                    section[1..]
                        .iter()
                        .map(parse_leaf)
                        .collect::<Result<_, _>>()?,
                )
            }
            Some("widths") => {
                widths = Some(
                    section[1..]
                        .iter()
                        .map(|pair| {
                            let [n, w] = list(pair)? else {
                                return Err("width sample must be `(n w)`".into());
                            };
                            Ok((number(n)?, number(w)?))
                        })
                        .collect::<Result<_, String>>()?,
                )
            }
            _ => return Err("unknown goal section".into()),
        }
    }
    Ok(Goal {
        kind: kind(goal_kind)?,
        shape,
        leaves: leaves.ok_or("goal is missing leaves")?,
        widths: widths.ok_or("goal is missing widths")?,
    })
}

fn parse_search(items: &[SemExpr]) -> Result<(usize, usize, Vec<SymKind>, Vec<Goal>), String> {
    let mut max_ops = None;
    let mut candidates = None;
    let mut operators = None;
    let mut goals = Vec::new();
    for section in &items[1..] {
        let section = list(section)?;
        match section.first().and_then(atom) {
            Some("max-ops") => max_ops = Some(number(&section[1])? as usize),
            Some("candidates-per-class") => candidates = Some(number(&section[1])? as usize),
            Some("operators") => {
                operators = Some(section[1..].iter().map(kind).collect::<Result<_, _>>()?)
            }
            Some("goal") => goals.push(parse_goal(section)?),
            _ => return Err("unknown search section".into()),
        }
    }
    Ok((
        max_ops.ok_or("search is missing max-ops")?,
        candidates.ok_or("search is missing candidates-per-class")?,
        operators.ok_or("search is missing operators")?,
        goals,
    ))
}

fn parse_family(items: &[SemExpr]) -> Result<Family, String> {
    let [_, _name, sections @ ..] = items else {
        return Err("expected (family <name> <section>...)".into());
    };
    let mut requires = None;
    let mut axioms = Vec::new();
    for section in sections {
        let section_items = list(section)?;
        match section_items.first().and_then(atom) {
            Some("requires") => {
                requires = Some(
                    section_items[1..]
                        .iter()
                        .map(kind)
                        .collect::<Result<_, _>>()?,
                )
            }
            Some("axiom") => {
                let text = render(section);
                parse_axiom(&text)?;
                axioms.push(text);
            }
            _ => return Err("unknown family section".into()),
        }
    }
    Ok(Family {
        requires: requires.ok_or("family is missing requires")?,
        axioms,
    })
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
    let mut search = None;
    let mut families = Vec::new();
    for section in &items[1..] {
        let section = list(section)?;
        match section.first().and_then(atom) {
            Some("search") => search = Some(parse_search(section)?),
            Some("family") => families.push(parse_family(section)?),
            _ => return Err("unknown theory section".into()),
        }
    }
    let (max_ops, candidates_per_class, operators, goals) =
        search.ok_or("theory is missing search")?;
    Ok(Theory {
        max_ops,
        candidates_per_class,
        operators,
        goals,
        families,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_theory_declares_search_and_builtin_families() {
        let theory = theory();

        assert_eq!(theory.max_ops, 3);
        assert_eq!(theory.candidates_per_class, 4);
        assert!(theory.operators.contains(&SymKind::ShiftLeft));
        assert!(theory.goals.iter().any(|goal| goal.kind == SymKind::SExt));
        assert_eq!(theory.families.len(), 3);
    }
}
