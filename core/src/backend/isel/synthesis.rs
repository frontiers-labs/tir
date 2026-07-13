//! Discovery of materializer bridges by enumeration, not hand-authoring.
//!
//! For a semantic kind the program may contain but no instruction can root
//! (a sub-word `sext`/`zext`, a bare `neg`/`not`), [`synthesize_bridge_texts`]
//! searches for equivalent terms over the kinds the target *can* realize:
//!
//! 1. Terms are enumerated smallest-first over the target's atomic kinds,
//!    directly in the axiom DSL's language — constant leaves are width
//!    *expressions* (`0`, `1`, `n`, `w`, `(- w n)`, `(ones w)`), so every
//!    candidate is width-parameterized by construction and needs no later
//!    generalization.
//! 2. Each term is fingerprinted by evaluation over sample inputs at several
//!    `(n, w)` width pairs; the bank keeps one representative per behavior
//!    (observational-equivalence pruning), so the space stays small.
//! 3. Terms whose fingerprint matches the goal's register realization (for an
//!    extension goal: the low `n` bits of a register extended to `w` — upper
//!    junk bits included) are rendered as axiom text and confirmed by
//!    [`Axiom::prove`] at every sampled width pair. Every proved candidate of
//!    the smallest proving size is emitted — alternatives realize the goal
//!    through different kinds, and the cover picks whichever the target's
//!    operand constraints admit.
//!
//! Extension goals include the `(ones n)` mask leaf: a discovered
//! `zext(x, w) == and(x, 2^n - 1)` beats the shift pair on cost where the
//! mask encodes, and matching's immediate-range legality (an `ImmRange` on
//! the boundary) rejects the `and`-immediate match where it does not (e.g.
//! `0xffff` in a 12-bit field), leaving the shift-pair realization to cover
//! it. The full-register `(ones w)` leaf folds to `-1` as an immediate.
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

use tir::sem::{SymKind, op_name, sample_values};

use super::Rule;
use super::axioms::parse_axiom;
use super::pattern::{atomic_kinds, compile_isel_pattern};
use super::theory::{GoalShape, Leaf as WLeaf, theory};

/// A constant leaf: a width expression evaluated per `(n, w)` instantiation.
impl WLeaf {
    fn eval(self, n: u32, w: u32) -> u64 {
        match self {
            WLeaf::Zero => 0,
            WLeaf::One => 1,
            WLeaf::N => n as u64,
            WLeaf::W => w as u64,
            WLeaf::WMinusN => (w - n) as u64,
            WLeaf::OnesN => mask(n),
            WLeaf::OnesW => mask(w),
        }
    }

    fn render(self) -> &'static str {
        match self {
            WLeaf::Zero => "0",
            WLeaf::One => "1",
            WLeaf::N => "n",
            WLeaf::W => "w",
            WLeaf::WMinusN => "(- w n)",
            WLeaf::OnesN => "(ones n)",
            WLeaf::OnesW => "(ones w)",
        }
    }
}

impl GoalShape {
    /// The `prove` argument for one pair, in width-name declaration order.
    fn prove_widths(self, n: u32, w: u32) -> Vec<u64> {
        match self {
            GoalShape::Extension => vec![n as u64, w as u64],
            GoalShape::Unary => vec![w as u64],
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

    /// Whether the term folds a `ones` mask into an immediate. Such a
    /// realization is conditional: matching rejects it where the mask does
    /// not encode, so it never terminates the search on its own.
    fn contains_ones_mask(&self) -> bool {
        match self {
            Term::X => false,
            Term::Const(l) => matches!(l, WLeaf::OnesN | WLeaf::OnesW),
            Term::Node(_, a, b) => a.contains_ones_mask() || b.contains_ones_mask(),
        }
    }

    fn size(&self) -> usize {
        match self {
            Term::X | Term::Const(_) => 0,
            Term::Node(_, a, b) => 1 + a.size() + b.size(),
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
fn build_samples(width_pairs: &[(u32, u32)]) -> Vec<(u32, u32, Vec<u64>)> {
    width_pairs
        .iter()
        .copied()
        .map(|(n, w)| {
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

/// The bridged kind's register realization: extensions widen the low `n` bits
/// of the register to `w`; unary goals operate on the whole register.
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
        SymKind::Neg => x.wrapping_neg() & mask(w),
        SymKind::Not => !x & mask(w),
        other => unreachable!("kind {other:?} is not a bridge goal"),
    }
}

/// Enumerate terms smallest-first with observational-equivalence pruning:
/// per behavior class, the first few terms found (ordered by size).
fn enumerate(
    kinds: &[SymKind],
    leaves: &[WLeaf],
    samples: &[(u32, u32, Vec<u64>)],
) -> HashMap<Vec<u64>, Vec<Term>> {
    let mut classes: HashMap<Vec<u64>, Vec<Term>> = HashMap::new();
    let config = theory();
    let mut by_size: Vec<Vec<Term>> = Vec::with_capacity(config.max_ops + 1);

    let leaves: Vec<Term> = std::iter::once(Term::X)
        .chain(leaves.iter().copied().map(Term::Const))
        .collect();
    for leaf in &leaves {
        classes
            .entry(fingerprint(leaf, samples))
            .or_default()
            .push(leaf.clone());
    }
    by_size.push(leaves);

    for size in 1..=config.max_ops {
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
                        // One operand order suffices: the match engine handles
                        // commutativity, so the swap is a redundant candidate.
                        if kind.is_commutative() && a.render() > b.render() {
                            continue;
                        }
                        let term = Term::Node(kind, Box::new(a.clone()), Box::new(b.clone()));
                        let fp = fingerprint(&term, samples);
                        let class = classes.entry(fp).or_default();
                        if class.is_empty() {
                            level.push(term.clone());
                        }
                        if class.len() < config.candidates_per_class {
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

fn render_axiom(goal: SymKind, shape: GoalShape, rhs: &Term, index: usize) -> String {
    let goal_name = op_name(goal).expect("bridge goal is in the vocabulary");
    let suffix = if index == 0 {
        String::new()
    } else {
        format!("-{}", index + 1)
    };
    match shape {
        GoalShape::Extension => format!(
            "(axiom {goal_name}-bridge{suffix} (vars (x n)) (root w) (where (< n w)) \
             (lhs ({goal_name} x w)) (rhs {}))",
            rhs.render()
        ),
        GoalShape::Unary => format!(
            "(axiom {goal_name}-bridge{suffix} (vars (x w)) (root w) \
             (lhs ({goal_name} x)) (rhs {}))",
            rhs.render()
        ),
    }
}

/// Search for proved bridges realizing `goal` over `kinds`. The main pass runs
/// over the shape's full leaf vocabulary; when every realization it finds
/// folds a `ones` mask into an immediate (`zext(x, w) == and(x, 2^n - 1)`,
/// conditional: matching rejects it where the mask does not encode), a second
/// pass without the mask leaves discovers the unconditional fallback (the
/// shift pair) and both are emitted — the cover picks the mask where it
/// encodes, the fallback everywhere else.
fn discover_bridge_texts(goal: SymKind, shape: GoalShape, kinds: &[SymKind]) -> Vec<String> {
    if kinds.is_empty() {
        return Vec::new();
    }
    let config = theory()
        .goals
        .iter()
        .find(|config| config.kind == goal && config.shape == shape)
        .expect("requested bridge goal is declared in the theory");
    let mut terms = proved_bridge_terms(goal, shape, kinds, &config.leaves, &config.widths);
    if !terms.is_empty() && terms.iter().all(Term::contains_ones_mask) {
        let mask_free: Vec<WLeaf> = config
            .leaves
            .iter()
            .copied()
            .filter(|leaf| !matches!(leaf, WLeaf::OnesN | WLeaf::OnesW))
            .collect();
        for term in proved_bridge_terms(goal, shape, kinds, &mask_free, &config.widths) {
            if !terms.iter().any(|t| t.render() == term.render()) {
                terms.push(term);
            }
        }
    }
    terms
        .iter()
        .enumerate()
        .map(|(index, term)| render_axiom(goal, shape, term, index))
        .collect()
}

/// The proved candidates of the smallest proving size over `leaves`: every
/// candidate whose fingerprint matches the goal and that survives
/// [`Axiom::prove`] at every sampled width pair. Same-size alternatives are
/// all kept — they realize the goal through different kinds and operand
/// shapes, and the cover picks whichever the target's operand constraints
/// admit.
fn proved_bridge_terms(
    goal: SymKind,
    shape: GoalShape,
    kinds: &[SymKind],
    leaves: &[WLeaf],
    width_pairs: &[(u32, u32)],
) -> Vec<Term> {
    let samples = build_samples(width_pairs);
    let classes = enumerate(kinds, leaves, &samples);
    let goal_fp: Vec<u64> = samples
        .iter()
        .flat_map(|&(n, w, ref xs)| xs.iter().map(move |&x| goal_eval(goal, x, n, w)))
        .collect();
    let Some(candidates) = classes.get(&goal_fp) else {
        return Vec::new();
    };
    let mut terms = Vec::new();
    let mut proved_size = None;
    for candidate in candidates {
        if !candidate.contains_x() {
            continue;
        }
        // Candidates arrive size-ascending; stop after the minimal proved size.
        if proved_size.is_some_and(|s| candidate.size() > s) {
            break;
        }
        let text = render_axiom(goal, shape, candidate, 0);
        let axiom = parse_axiom(&text).expect("rendered axiom must parse");
        if width_pairs
            .iter()
            .all(|&(n, w)| axiom.prove(&shape.prove_widths(n, w)))
        {
            proved_size = Some(candidate.size());
            terms.push(candidate.clone());
        }
    }
    terms
}

/// The proved bridge axiom texts realizing `goal` over the target's atomic
/// kinds; empty if discovery finds none. Deterministic.
pub(crate) fn synthesize_bridge_texts(goal: SymKind, atomics: &HashSet<SymKind>) -> Vec<String> {
    let Some(config) = theory().goals.iter().find(|config| config.kind == goal) else {
        return Vec::new();
    };
    let mut kinds: Vec<SymKind> = theory()
        .operators
        .iter()
        .copied()
        .filter(|k| atomics.contains(k))
        .collect();
    kinds.sort();
    discover_bridge_texts(goal, config.shape, &kinds)
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
                &rule.operand_registers,
                &rule.operand_imm_ranges,
                rule.result_register,
            )
        })
        .collect();
    let atomics = atomic_kinds(&compiled);
    theory()
        .goals
        .iter()
        .flat_map(|goal| synthesize_bridge_texts(goal.kind, &atomics))
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
        let texts = discover_bridge_texts(
            SymKind::SExt,
            GoalShape::Extension,
            &[SymKind::ShiftLeft, SymKind::ShiftRightArithmetic],
        );
        assert_eq!(texts.len(), 1, "unexpected discoveries: {texts:?}");
        assert!(
            texts[0].contains("(ashr (shl x (- w n)) (- w n))"),
            "unexpected discovery: {}",
            texts[0]
        );
    }

    #[test]
    fn discovers_the_zero_extension_shift_pair() {
        let texts = discover_bridge_texts(
            SymKind::ZExt,
            GoalShape::Extension,
            &[SymKind::ShiftLeft, SymKind::ShiftRightLogic],
        );
        assert_eq!(texts.len(), 1, "unexpected discoveries: {texts:?}");
        assert!(
            texts[0].contains("(lshr (shl x (- w n)) (- w n))"),
            "unexpected discovery: {}",
            texts[0]
        );
    }

    #[test]
    fn discovers_negation_via_subtraction() {
        let texts = discover_bridge_texts(
            SymKind::Neg,
            GoalShape::Unary,
            &[SymKind::Sub, SymKind::Add],
        );
        assert!(
            texts.iter().any(|t| t.contains("(sub 0 x)")),
            "unexpected discoveries: {texts:?}"
        );
    }

    #[test]
    fn discovers_complement_alternatives() {
        // Both same-size realizations are emitted: the cover picks whichever
        // the target's operand constraints admit (xori folds `-1`; sub needs
        // the all-ones value in a register). Both fold a `ones` mask, so the
        // mask-free pass adds an unconditional fallback (`-x - 1`); xor's
        // commutative twin `(xor x (ones w))` must still be pruned.
        let texts = discover_bridge_texts(
            SymKind::Not,
            GoalShape::Unary,
            &[SymKind::Sub, SymKind::Xor],
        );
        assert!(
            texts.iter().any(|t| t.contains("(xor (ones w) x)")),
            "unexpected discoveries: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("(sub (ones w) x)")),
            "unexpected discoveries: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("(sub (sub 0 x) 1)")),
            "unexpected discoveries: {texts:?}"
        );
        assert_eq!(texts.len(), 3, "commutative twin must be pruned: {texts:?}");
    }

    #[test]
    fn insufficient_kinds_discover_nothing() {
        assert!(discover_bridge_texts(SymKind::SExt, GoalShape::Extension, &[]).is_empty());
        assert!(
            discover_bridge_texts(
                SymKind::SExt,
                GoalShape::Extension,
                &[SymKind::Add, SymKind::Xor]
            )
            .is_empty()
        );
    }

    #[test]
    fn synthesized_bridge_is_a_full_axiom() {
        let texts = synthesize_bridge_texts(
            SymKind::SExt,
            &kinds(&[SymKind::ShiftLeft, SymKind::ShiftRightArithmetic]),
        );
        let axiom = parse_axiom(&texts[0]).unwrap();
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
        let texts = synthesize_bridge_texts(
            SymKind::ZExt,
            &kinds(&[
                SymKind::ShiftLeft,
                SymKind::ShiftRightLogic,
                SymKind::Add,
                SymKind::Xor,
            ]),
        );
        assert_eq!(texts.len(), 1, "unexpected discoveries: {texts:?}");
        assert!(
            texts[0].contains("(lshr (shl x (- w n)) (- w n))"),
            "unexpected discovery: {}",
            texts[0]
        );
    }
}
