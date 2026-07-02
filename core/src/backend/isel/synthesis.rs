//! Discovery of materializer bridges by enumeration, not hand-authoring.
//!
//! For a semantic kind the program may contain but no instruction can root
//! (a sub-word `sext`/`zext`), [`synthesize_bridge`] searches for an
//! equivalent term over the kinds the target *can* realize:
//!
//! 1. Terms are enumerated smallest-first over the target's atomic kinds,
//!    directly in the axiom DSL's language — constant leaves are width
//!    *expressions* (`0`, `1`, `n`, `w`, `(- w n)`), so every candidate is
//!    width-parameterized by construction and needs no later generalization.
//! 2. Each term is fingerprinted by evaluation over sample inputs at several
//!    `(n, w)` width pairs; the bank keeps one representative per behavior
//!    (observational-equivalence pruning), so the space stays small.
//! 3. Terms whose fingerprint matches the goal's register realization (the
//!    low `n` bits of a register extended to `w` — upper junk bits included)
//!    are rendered as axiom text and confirmed by [`Axiom::prove`] at every
//!    sampled width pair. The first (smallest) proved candidate wins.
//!
//! The result is the same artifact a hand-written axiom would be — s-expr
//! text through [`parse_axiom`] — so the compiled rewrite still re-proves
//! each width instantiation it applies (see [`super::axioms`]). Fingerprints
//! only *filter*; the SMT proof is the sole arbiter, so an evaluator
//! divergence can cost a discovery but never admit a false one. Discovery is
//! deterministic.
//!
//! Discovery runs offline: the `tir axioms` developer utility calls
//! [`discover_axioms`] against a backend's rule set whenever instructions
//! change and writes the result next to the backend's sources; the pass loads
//! it back through
//! [`with_axioms`](super::InstructionSelectPass::with_axioms).

use std::collections::{HashMap, HashSet};

use tir::sem::{EXT_WIDTH_SAMPLES, SymKind, op_name, sample_values};

use super::Rule;
use super::axioms::parse_axiom;
use super::pattern::{atomic_kinds, compile_isel_pattern};

/// Kinds candidates may use: cheap fixed-arity bit-vector ops. Division and
/// remainder are excluded — no target bridges through them and their proofs
/// dominate solver time.
const SUPPORTED_KINDS: &[SymKind] = &[
    SymKind::Add,
    SymKind::Sub,
    SymKind::Mul,
    SymKind::And,
    SymKind::Or,
    SymKind::Xor,
    SymKind::ShiftLeft,
    SymKind::ShiftRightLogic,
    SymKind::ShiftRightArithmetic,
];

/// Largest candidate, in operator count. The classic bridges need two; three
/// leaves headroom for targets with unusual kind sets.
const MAX_OPS: usize = 3;

/// Fingerprint collisions kept per behavior class as proof fallbacks.
const CANDIDATES_PER_CLASS: usize = 4;

/// A constant leaf: a width expression evaluated per `(n, w)` instantiation.
#[derive(Clone, Copy, PartialEq)]
enum WLeaf {
    Zero,
    One,
    N,
    W,
    WMinusN,
}

impl WLeaf {
    const ALL: [WLeaf; 5] = [WLeaf::Zero, WLeaf::One, WLeaf::N, WLeaf::W, WLeaf::WMinusN];

    fn eval(self, n: u32, w: u32) -> u64 {
        match self {
            WLeaf::Zero => 0,
            WLeaf::One => 1,
            WLeaf::N => n as u64,
            WLeaf::W => w as u64,
            WLeaf::WMinusN => (w - n) as u64,
        }
    }

    fn render(self) -> &'static str {
        match self {
            WLeaf::Zero => "0",
            WLeaf::One => "1",
            WLeaf::N => "n",
            WLeaf::W => "w",
            WLeaf::WMinusN => "(- w n)",
        }
    }
}

/// A candidate term: the bridged value `x`, a constant leaf, or an operator.
#[derive(Clone)]
enum Term {
    X,
    Const(WLeaf),
    Node(SymKind, Box<Term>, Box<Term>),
}

fn mask(w: u32) -> u64 {
    if w >= 64 { u64::MAX } else { (1u64 << w) - 1 }
}

impl Term {
    fn contains_x(&self) -> bool {
        match self {
            Term::X => true,
            Term::Const(_) => false,
            Term::Node(_, a, b) => a.contains_x() || b.contains_x(),
        }
    }

    /// Evaluate at register width `w`; semantics mirror the bit-blaster
    /// (shifts of `>= w` saturate to the fill pattern).
    fn eval(&self, x: u64, n: u32, w: u32) -> u64 {
        let m = mask(w);
        match self {
            Term::X => x & m,
            Term::Const(l) => l.eval(n, w) & m,
            Term::Node(kind, a, b) => {
                let (a, b) = (a.eval(x, n, w), b.eval(x, n, w));
                match kind {
                    SymKind::Add => a.wrapping_add(b) & m,
                    SymKind::Sub => a.wrapping_sub(b) & m,
                    SymKind::Mul => a.wrapping_mul(b) & m,
                    SymKind::And => a & b,
                    SymKind::Or => a | b,
                    SymKind::Xor => a ^ b,
                    SymKind::ShiftLeft => {
                        if b >= w as u64 {
                            0
                        } else {
                            (a << b) & m
                        }
                    }
                    SymKind::ShiftRightLogic => {
                        if b >= w as u64 {
                            0
                        } else {
                            a >> b
                        }
                    }
                    SymKind::ShiftRightArithmetic => {
                        let negative = (a >> (w - 1)) & 1 == 1;
                        if b >= w as u64 {
                            if negative { m } else { 0 }
                        } else if negative && b > 0 {
                            ((a >> b) | (m << (w as u64 - b))) & m
                        } else {
                            a >> b
                        }
                    }
                    other => unreachable!("kind {other:?} is not enumerated"),
                }
            }
        }
    }

    fn render(&self) -> String {
        match self {
            Term::X => "x".to_string(),
            Term::Const(l) => l.render().to_string(),
            Term::Node(kind, a, b) => {
                let name = op_name(*kind).expect("enumerated kind is in the vocabulary");
                format!("({name} {} {})", a.render(), b.render())
            }
        }
    }
}

/// Per width pair: `(n, w, register-wide input samples)`. The samples carry
/// junk above bit `n`, so a candidate must tolerate undefined upper bits to
/// match the goal.
fn build_samples() -> Vec<(u32, u32, Vec<u64>)> {
    EXT_WIDTH_SAMPLES
        .iter()
        .map(|&(n, w)| {
            let xs = sample_values(w, 2)
                .iter()
                .map(|v| v.to_u64() & mask(w))
                .collect();
            (n, w, xs)
        })
        .collect()
}

fn fingerprint(term: &Term, samples: &[(u32, u32, Vec<u64>)]) -> Vec<u64> {
    samples
        .iter()
        .flat_map(|&(n, w, ref xs)| xs.iter().map(move |&x| term.eval(x, n, w)))
        .collect()
}

/// The bridged kind's register realization: extend the low `n` bits of the
/// register to `w`.
fn goal_eval(goal: SymKind, x: u64, n: u32, w: u32) -> u64 {
    let low = x & mask(n);
    match goal {
        SymKind::ZExt => low,
        SymKind::SExt => {
            if (low >> (n - 1)) & 1 == 1 {
                low | (mask(w) & !mask(n))
            } else {
                low
            }
        }
        other => unreachable!("kind {other:?} is not a bridge goal"),
    }
}

/// Enumerate terms smallest-first with observational-equivalence pruning:
/// per behavior class, the first few terms found (ordered by size).
fn enumerate(kinds: &[SymKind], samples: &[(u32, u32, Vec<u64>)]) -> HashMap<Vec<u64>, Vec<Term>> {
    let mut classes: HashMap<Vec<u64>, Vec<Term>> = HashMap::new();
    let mut by_size: Vec<Vec<Term>> = Vec::with_capacity(MAX_OPS + 1);

    let leaves: Vec<Term> = std::iter::once(Term::X)
        .chain(WLeaf::ALL.into_iter().map(Term::Const))
        .collect();
    for leaf in &leaves {
        classes
            .entry(fingerprint(leaf, samples))
            .or_default()
            .push(leaf.clone());
    }
    by_size.push(leaves);

    for size in 1..=MAX_OPS {
        let mut level = Vec::new();
        for &kind in kinds {
            for left_size in 0..size {
                let right_size = size - 1 - left_size;
                for a in &by_size[left_size] {
                    for b in &by_size[right_size] {
                        // A constant-only composite is never a bridge and is
                        // not renderable as a leaf width expression.
                        if !a.contains_x() && !b.contains_x() {
                            continue;
                        }
                        let term = Term::Node(kind, Box::new(a.clone()), Box::new(b.clone()));
                        let fp = fingerprint(&term, samples);
                        let class = classes.entry(fp).or_default();
                        if class.is_empty() {
                            level.push(term.clone());
                        }
                        if class.len() < CANDIDATES_PER_CLASS {
                            class.push(term);
                        }
                    }
                }
            }
        }
        by_size.push(level);
    }
    classes
}

fn render_axiom(goal: SymKind, rhs: &Term) -> String {
    let goal_name = op_name(goal).expect("bridge goal is in the vocabulary");
    format!(
        "(axiom {goal_name}-bridge (vars (x n)) (root w) (where (< n w)) \
         (lhs ({goal_name} x w)) (rhs {}))",
        rhs.render()
    )
}

/// Search for a proved bridge realizing `goal` over `kinds`; the axiom text of
/// the smallest fingerprint-matching candidate that survives [`Axiom::prove`]
/// at every sampled width pair.
fn discover_bridge_text(goal: SymKind, kinds: &[SymKind]) -> Option<String> {
    if kinds.is_empty() {
        return None;
    }
    let samples = build_samples();
    let classes = enumerate(kinds, &samples);
    let goal_fp: Vec<u64> = samples
        .iter()
        .flat_map(|&(n, w, ref xs)| xs.iter().map(move |&x| goal_eval(goal, x, n, w)))
        .collect();
    for candidate in classes.get(&goal_fp)? {
        if !candidate.contains_x() {
            continue;
        }
        let text = render_axiom(goal, candidate);
        let axiom = parse_axiom(&text).expect("rendered axiom must parse");
        // Width order matches the rendered declarations: `n` (vars), `w` (root).
        if EXT_WIDTH_SAMPLES
            .iter()
            .all(|&(n, w)| axiom.prove(&[n as u64, w as u64]))
        {
            return Some(text);
        }
    }
    None
}

/// The proved bridge axiom text realizing `goal` over the target's atomic
/// kinds, if discovery finds one. Deterministic.
pub(crate) fn synthesize_bridge_text(goal: SymKind, atomics: &HashSet<SymKind>) -> Option<String> {
    let mut kinds: Vec<SymKind> = SUPPORTED_KINDS
        .iter()
        .copied()
        .filter(|k| atomics.contains(k))
        .collect();
    kinds.sort();
    discover_bridge_text(goal, &kinds)
}

/// Discover every bridge axiom the rule set supports: the `tir axioms`
/// utility's entry point. Deterministic over a fixed rule set, so its output
/// is committed next to the backend and checked for freshness by a test.
pub fn discover_axioms(rules: &[Rule]) -> Vec<String> {
    let compiled: Vec<_> = rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            compile_isel_pattern(
                index,
                &rule.pattern,
                &rule.operand_constraints,
                &rule.operand_widths,
            )
        })
        .collect();
    let atomics = atomic_kinds(&compiled);
    [SymKind::SExt, SymKind::ZExt]
        .into_iter()
        .filter_map(|goal| synthesize_bridge_text(goal, &atomics))
        .collect()
}

/// Render discovered axioms as the committed `isel.axioms` file.
pub fn render_axioms_file(axioms: &[String]) -> String {
    let mut out = String::from(
        "; Materializer bridges discovered over this target's instruction set.\n\
         ; Generated by `tir axioms`; regenerate after adding instructions.\n\
         ; Every width instantiation is re-proved at selection time.\n",
    );
    for axiom in axioms {
        out.push('\n');
        out.push_str(axiom);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(list: &[SymKind]) -> HashSet<SymKind> {
        list.iter().copied().collect()
    }

    #[test]
    fn discovers_the_sign_extension_shift_pair() {
        let text = discover_bridge_text(
            SymKind::SExt,
            &[SymKind::ShiftLeft, SymKind::ShiftRightArithmetic],
        )
        .expect("sext bridge must be discovered");
        assert!(
            text.contains("(ashr (shl x (- w n)) (- w n))"),
            "unexpected discovery: {text}"
        );
    }

    #[test]
    fn discovers_the_zero_extension_shift_pair() {
        let text = discover_bridge_text(
            SymKind::ZExt,
            &[SymKind::ShiftLeft, SymKind::ShiftRightLogic],
        )
        .expect("zext bridge must be discovered");
        assert!(
            text.contains("(lshr (shl x (- w n)) (- w n))"),
            "unexpected discovery: {text}"
        );
    }

    #[test]
    fn insufficient_kinds_discover_nothing() {
        assert!(discover_bridge_text(SymKind::SExt, &[]).is_none());
        assert!(discover_bridge_text(SymKind::SExt, &[SymKind::Add, SymKind::Xor]).is_none());
    }

    #[test]
    fn synthesized_bridge_is_a_full_axiom() {
        let text = synthesize_bridge_text(
            SymKind::SExt,
            &kinds(&[SymKind::ShiftLeft, SymKind::ShiftRightArithmetic]),
        )
        .expect("bridge");
        let axiom = parse_axiom(&text).unwrap();
        assert_eq!(
            axiom.rhs_kinds(),
            kinds(&[SymKind::ShiftLeft, SymKind::ShiftRightArithmetic])
        );
        // Guarded instantiations prove; an inverted one must not.
        assert!(axiom.prove(&[16, 64]));
        assert!(!axiom.prove(&[64, 16]));
    }

    #[test]
    fn irrelevant_atomics_do_not_change_the_discovery() {
        let text = synthesize_bridge_text(
            SymKind::ZExt,
            &kinds(&[
                SymKind::ShiftLeft,
                SymKind::ShiftRightLogic,
                SymKind::Add,
                SymKind::Xor,
            ]),
        )
        .expect("bridge");
        assert!(
            text.contains("(lshr (shl x (- w n)) (- w n))"),
            "unexpected discovery: {text}"
        );
    }
}
