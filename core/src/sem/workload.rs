//! Dump a saturation's input as an engine-neutral workload, which the e-graph
//! benchmarks under `utils/relational/benches` replay on every engine they
//! compare. Off unless `TIR_SAT_DUMP` names a directory.
//!
//! The format keeps what any e-graph library can express and erases the rest. A
//! label becomes one symbol, its operator and payload without the type. A rule
//! is kept when its query is a tree of row atoms and its head only inserts and
//! unions, with whatever it computes from types dropped and a constant it
//! builds that way left opaque; a rule that computes with a constant's value or
//! negates a conjunction is not. Commutative operators are listed so a loader can add
//! the rule this engine's matcher applies implicitly.
//!
//! One file per saturation:
//!
//! ```text
//! # pass=instcombine nodes=41 classes=30 rules=12/97
//! n <class> <symbol> <child class>...
//! c <symbol>
//! r <name> <lhs> => <rhs>
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use tir_relational::{
    Atom, Cmp, ColumnId, Expr, Guard, HeadOp, Label as _, Rule, Scalar, Source, Step, Var,
};

use crate::sem::node::field;
use crate::sem::{Kind, SemEGraph, SemNode, SemPayload, SymPayload};

fn directory() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("TIR_SAT_DUMP").map(PathBuf::from))
        .as_ref()
}

/// The operator and payload of `node` as one token.
fn symbol(node: &SemNode) -> String {
    let mut out = match &node.kind {
        Kind::Sym(kind) => format!("{kind:?}"),
        Kind::Ir(op) if op.attrs.is_empty() => format!("{}.{}", op.dialect, op.name),
        Kind::Ir(op) => format!("{}.{}#{:x}", op.dialect, op.name, node.op_key() as u32),
    };
    match &node.payload {
        None => {}
        Some(SemPayload::Expr(SymPayload::Int(value))) => {
            write!(out, ":{}", value.to_i64()).unwrap();
        }
        Some(SemPayload::Expr(SymPayload::Value(value))) => {
            write!(out, ":v{}", value.number()).unwrap();
        }
        Some(SemPayload::Expr(other)) => write!(out, ":{other:?}").unwrap(),
        Some(SemPayload::Opaque(serial)) => write!(out, ":o{serial}").unwrap(),
    }
    out.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._:#-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The pattern `var` stands for on the left-hand side: the atom rooted at it,
/// the literal it is pinned to, or a hole.
fn pattern(
    var: Var,
    atoms: &BTreeMap<Var, &Atom<SemNode>>,
    literals: &BTreeMap<Var, i64>,
) -> String {
    if let Some(literal) = literals.get(&var) {
        return format!("Constant:{literal}");
    }
    match atoms.get(&var) {
        Some(Atom::Node { template, args, .. }) if args.is_empty() => symbol(template),
        Some(Atom::Node { template, args, .. }) => {
            let mut out = format!("({}", symbol(template));
            for &arg in args {
                write!(out, " {}", pattern(arg, atoms, literals)).unwrap();
            }
            out.push(')');
            out
        }
        Some(Atom::Literal { value, .. }) => symbol(value),
        _ => format!("?v{var}"),
    }
}

/// Whether `expr` reads only scalars in `known`.
fn reads_only(expr: &Expr, known: &BTreeSet<Scalar>) -> bool {
    match expr {
        Expr::Lit(_) => true,
        Expr::Scalar(slot) => known.contains(slot),
        Expr::Sub(a, b) | Expr::Add(a, b) | Expr::And(a, b) => {
            reads_only(a, known) && reads_only(b, known)
        }
        Expr::Ones(e) | Expr::IsZero(e) => reads_only(e, known),
    }
}

/// The literal `guard` pins a constant to, and the scalar holding that
/// constant's label, when it is the comparison a rule spells "this operand is
/// the constant `k`" with: the value read off the label against `k`, bare or
/// masked to the constant's own width.
fn pinned(
    guard: &Guard,
    fields: &BTreeMap<Scalar, (Scalar, u32)>,
    lets: &BTreeMap<Scalar, i64>,
) -> Option<(Scalar, i64)> {
    let Guard::Cmp(Cmp::Eq, Expr::Scalar(value), literal) = guard else {
        return None;
    };
    let (label, field::INT_VALUE) = *fields.get(value)? else {
        return None;
    };
    let literal = match literal {
        Expr::Lit(literal) => *literal,
        Expr::Scalar(slot) => *lets.get(slot)?,
        Expr::And(literal, ones) => match (&**literal, &**ones) {
            (Expr::Lit(literal), Expr::Ones(_)) => *literal,
            _ => return None,
        },
        _ => return None,
    };
    Some((label, literal))
}

/// `rule` as `lhs => rhs`, or why it has no neutral spelling.
///
/// Types are erased, so everything a rule computes from a type is dropped: the
/// type facts, the guards over them, and the fills they feed. A constant
/// operand survives only as a literal the rule pins by value.
fn export(eg: &SemEGraph, rule: &Rule<SemNode>) -> Result<String, &'static str> {
    let query = rule.plan.query();
    if !query.nots.is_empty() {
        return Err("negation");
    }
    // Scalars that hold a type or something computed from one.
    let mut typed: BTreeSet<Scalar> = BTreeSet::new();
    // Constant label scalar -> the class it is the constant of.
    let mut constants: BTreeMap<Scalar, Var> = BTreeMap::new();
    let mut atoms: BTreeMap<Var, &Atom<SemNode>> = BTreeMap::new();
    for atom in &query.atoms {
        match atom {
            Atom::Node { template, .. } => {
                // A template that leaves the payload open matches labels the
                // neutral form spells as different symbols.
                let open = eg
                    .classes_with_op(template.op_key())
                    .into_iter()
                    .any(|class| {
                        eg.nodes(class).any(|node| {
                            template.matches_template(node) && symbol(node) != symbol(template)
                        })
                    });
                if open {
                    return Err("wildcard payload");
                }
            }
            Atom::Literal { .. } => {}
            Atom::Fact {
                column: ColumnId::Type,
                value,
                ..
            } => {
                typed.insert(*value);
                continue;
            }
            Atom::Fact {
                column: ColumnId::Const,
                key,
                value,
            } => {
                constants.insert(*value, *key);
                continue;
            }
            _ => return Err("fact atom"),
        }
        if atoms.insert(atom.class(), atom).is_some() {
            return Err("two atoms on one class");
        }
    }
    if !atoms.contains_key(&query.root) {
        return Err("bare root");
    }
    // Scalar read off a constant's label -> that label and the field.
    let mut fields: BTreeMap<Scalar, (Scalar, u32)> = BTreeMap::new();
    let mut literals: BTreeMap<Var, i64> = BTreeMap::new();
    let mut lets: BTreeMap<Scalar, i64> = BTreeMap::new();
    // The plan orders guards by what they read, so one pass in that order sees
    // every scalar bound before it is used.
    for step in rule.plan.steps() {
        let Step::Guard(index) = step else { continue };
        let guard = &query.guards[*index];
        if let Some((label, literal)) = pinned(guard, &fields, &lets) {
            literals.insert(constants[&label], literal);
            continue;
        }
        match guard {
            Guard::Read {
                term: Source::Label(label),
                field,
                out,
            } if constants.contains_key(label) => {
                fields.insert(*out, (*label, *field));
            }
            Guard::Read { field, out, .. } if *field == field::TY => {
                typed.insert(*out);
            }
            Guard::Cmp(_, a, b) if reads_only(a, &typed) && reads_only(b, &typed) => {}
            Guard::Let { out, value } if reads_only(value, &typed) => {
                typed.insert(*out);
                if let Expr::Lit(literal) = value {
                    lets.insert(*out, *literal);
                }
            }
            Guard::Extern {
                terms, args, out, ..
            } if terms.is_empty() && args.iter().all(|arg| reads_only(arg, &typed)) => {
                typed.extend(out.iter().copied());
            }
            _ => return Err("guard"),
        }
    }
    if constants.values().any(|key| !literals.contains_key(key)) {
        return Err("open constant");
    }
    let lhs = |var: Var| pattern(var, &atoms, &literals);
    let mut built: BTreeMap<Var, String> = BTreeMap::new();
    let mut union = None;
    for op in &rule.head {
        match op {
            HeadOp::Insert { label, args, into } => {
                if label.fills.iter().any(|(_, slot)| !typed.contains(slot)) {
                    return Err("computed payload");
                }
                // A payload computed from a type has no spelling once types are
                // gone. It stays a constant, one of its own per site, so two
                // sites that compute different values do not share a class.
                let computed = label.fills.iter().any(|&(field, _)| field != field::TY);
                let mut out = if computed {
                    format!("Constant:k.{}.{}", rule.name.replace(' ', "_"), built.len())
                } else {
                    symbol(&label.template)
                };
                if !args.is_empty() {
                    out.insert(0, '(');
                    for &arg in args {
                        let arg = built.get(&arg).cloned().unwrap_or_else(|| lhs(arg));
                        write!(out, " {arg}").unwrap();
                    }
                    out.push(')');
                }
                built.insert(*into, out);
            }
            HeadOp::Union(a, b) if union.is_none() && (*a == query.root || *b == query.root) => {
                union = Some(if *a == query.root { *b } else { *a });
            }
            _ => return Err("head"),
        }
    }
    let rhs = union.ok_or("head")?;
    let rhs = built.get(&rhs).cloned().unwrap_or_else(|| lhs(rhs));
    Ok(format!("{} => {rhs}", lhs(query.root)))
}

/// Write `eg` and the neutral subset of `rules` to the dump directory.
pub(crate) fn dump(pass: &str, eg: &SemEGraph, rules: &[Rule<SemNode>]) {
    let Some(directory) = directory() else {
        return;
    };
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let mut body = String::new();
    let mut commutative = BTreeSet::new();
    for class in eg.class_ids() {
        for node in eg.nodes(class) {
            write!(body, "n {} {}", class.index(), symbol(node)).unwrap();
            for &child in node.children() {
                write!(body, " {}", eg.find(child).index()).unwrap();
            }
            body.push('\n');
            if node.commutative() && node.children().len() == 2 {
                commutative.insert(symbol(node));
            }
        }
    }
    for symbol in &commutative {
        writeln!(body, "c {symbol}").unwrap();
    }
    let mut exported = 0;
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    for rule in rules.iter().filter(|rule| !rule.post_saturation) {
        match export(eg, rule) {
            Ok(text) => {
                exported += 1;
                writeln!(body, "r {} {text}", rule.name.replace(' ', "_")).unwrap();
            }
            Err(why) => *skipped.entry(why).or_default() += 1,
        }
    }
    let header = format!(
        "# pass={pass} nodes={} classes={} rules={exported}/{} skipped={skipped:?}\n",
        eg.total_size(),
        eg.num_classes(),
        rules.len(),
    );
    let name = format!(
        "{pass}-{}-{:05}.wl",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    // A dump that cannot be written is a diagnostic lost, not a compile failed.
    let _ = std::fs::create_dir_all(directory);
    let _ = std::fs::write(directory.join(name), header + &body);
}
