//! Discovery of algebraic identities by *testing and proving*, not hand-authoring.
//!
//! Instruction selection needs target-independent bit-vector lemmas to bridge IR
//! operators that no single instruction implements (e.g. a sub-word sign extension)
//! to sequences that the target *does* have. Rather than writing those lemmas by
//! hand, we propose a candidate shape and confirm it against an
//! [`EquivalenceOracle`]: [`FuzzOracle`] evaluates both sides on many inputs with
//! the reference interpreter ([`super::execute`]), [`SmtOracle`] proves the
//! equivalence unsatisfiable-to-refute with [`tir_symbolic`]'s QF_BV pipeline.
//! Confirmed shapes become e-graph rewrites at the call site.

use std::collections::HashMap;

use tir_adt::APInt;
use tir_symbolic::bitblast::{SolveOutcome, blast};
use tir_symbolic::lang::infer_widths;

use crate::ValueId;
use crate::graph::{Dag, GenericDag, MutDag, NodeId};
use crate::sem::{SemGraph, SymKind, SymPayload, Value, execute};

/// Decides whether two single-output expression graphs (over the same symbols)
/// compute the same value for every input of the given symbol widths.
pub trait EquivalenceOracle {
    fn equivalent(&self, lhs: &SemGraph, rhs: &SemGraph, symbol_widths: &[u32]) -> bool;
}

/// Property-testing oracle: evaluates both graphs on boundary values plus a
/// deterministic pseudo-random spread per symbol. Sound enough to bootstrap the
/// standard bit-vector idioms; not a proof. Deterministic, so discovery is stable.
pub struct FuzzOracle {
    pub samples_per_symbol: usize,
}

impl Default for FuzzOracle {
    fn default() -> Self {
        Self {
            samples_per_symbol: 16,
        }
    }
}

impl EquivalenceOracle for FuzzOracle {
    fn equivalent(&self, lhs: &SemGraph, rhs: &SemGraph, symbol_widths: &[u32]) -> bool {
        let value_sets: Vec<Vec<APInt>> = symbol_widths
            .iter()
            .map(|&w| sample_values(w, self.samples_per_symbol))
            .collect();

        let mut assignment = vec![0usize; symbol_widths.len()];
        loop {
            let inputs: Vec<Value> = assignment
                .iter()
                .enumerate()
                .map(|(i, &j)| Value::Int(value_sets[i][j].clone()))
                .collect();
            if !values_bit_eq(&execute(lhs, &inputs), &execute(rhs, &inputs)) {
                return false;
            }
            if !advance(&mut assignment, &value_sets) {
                return true;
            }
        }
    }
}

/// Proving oracle: bit-blasts `lhs != rhs` over shared symbols and reports
/// equivalence iff the SAT backend returns unsat — a proof, not a sampling.
/// Anything the pipeline cannot handle (unsupported node kinds, unknown or
/// mismatched root widths, an `Unknown` verdict) conservatively reports
/// non-equivalence.
#[derive(Default)]
pub struct SmtOracle;

type OracleGraph = GenericDag<SymKind, SymPayload<ValueId>>;

impl EquivalenceOracle for SmtOracle {
    fn equivalent(&self, lhs: &SemGraph, rhs: &SemGraph, symbol_widths: &[u32]) -> bool {
        let (Some(lhs_root), Some(rhs_root)) = (lhs.root(), rhs.root()) else {
            return false;
        };
        let mut g = OracleGraph::new();
        let mut symbols = HashMap::new();
        let l = copy_reachable(lhs, lhs_root, &mut g, &mut symbols, &mut HashMap::new());
        let r = copy_reachable(rhs, rhs_root, &mut g, &mut symbols, &mut HashMap::new());
        let ne = g.add_node(SymKind::Ne);
        g.add_edge(ne, l);
        g.add_edge(ne, r);

        let widths = infer_widths(&g, |id| match g.get_leaf_data(id) {
            Some(SymPayload::SymbolId(id)) => symbol_widths.get(*id as usize).copied(),
            _ => None,
        });
        match (widths[l.index()], widths[r.index()]) {
            (Some(lw), Some(rw)) if lw == rw => {}
            _ => return false,
        }
        match blast(&g, &widths) {
            Ok(b) => matches!(b.solve(), SolveOutcome::Unsat),
            Err(_) => false,
        }
    }
}

/// Copy the subgraph under `node` into `dst`. Symbol leaves are shared through
/// `symbols` across *both* sides of the equivalence — the bit-blaster allocates
/// fresh literals per node, so a symbol duplicated per side would leave the two
/// occurrences unconstrained against each other.
fn copy_reachable(
    src: &SemGraph,
    node: NodeId,
    dst: &mut OracleGraph,
    symbols: &mut HashMap<u32, NodeId>,
    memo: &mut HashMap<NodeId, NodeId>,
) -> NodeId {
    if let Some(&copied) = memo.get(&node) {
        return copied;
    }
    let copied = if let Some(SymPayload::SymbolId(id)) = src.get_leaf_data(node) {
        *symbols.entry(*id).or_insert_with(|| {
            let n = dst.add_node(SymKind::Symbol);
            dst.set_leaf_data(n, SymPayload::SymbolId(*id));
            n
        })
    } else {
        let children: Vec<NodeId> = src
            .children(node)
            .map(|c| copy_reachable(src, c, dst, symbols, memo))
            .collect();
        let n = dst.add_node(*src.get_kind(node));
        if let Some(data) = src.get_leaf_data(node) {
            dst.set_leaf_data(n, data.clone());
        }
        for child in children {
            dst.add_edge(n, child);
        }
        n
    };
    memo.insert(node, copied);
    copied
}

/// Mixed-radix odometer over the per-symbol value sets; returns false when wrapped.
fn advance(assignment: &mut [usize], value_sets: &[Vec<APInt>]) -> bool {
    for (slot, set) in assignment.iter_mut().zip(value_sets.iter()) {
        *slot += 1;
        if *slot < set.len() {
            return true;
        }
        *slot = 0;
    }
    false
}

fn values_bit_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => {
            // Compare bit patterns over the common width, ignoring how each side's
            // signedness flag would sign- vs zero-extend `to_u64` past that width.
            let width = a.width();
            let mask = if width >= 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            width == b.width() && (a.to_u64() & mask) == (b.to_u64() & mask)
        }
        _ => false,
    }
}

/// Boundary values (0, 1, all-ones, sign bit, alternating patterns) plus a small
/// deterministic LCG spread, all masked to `width` bits.
pub(crate) fn sample_values(width: u32, extra: usize) -> Vec<APInt> {
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    let mut raw = vec![
        0u64,
        1,
        mask,
        1u64 << (width - 1),
        0x5555_5555_5555_5555 & mask,
        0xAAAA_AAAA_AAAA_AAAA & mask,
    ];
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..extra {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        raw.push(state & mask);
    }
    raw.sort_unstable();
    raw.dedup();
    // Flag the samples signed so the interpreter's arithmetic right shift performs a
    // true (sign-extending) shift, matching hardware `sra`; the logical shifts
    // (`srl`) and the masked bit-pattern comparison are unaffected by the flag.
    raw.into_iter()
        .map(|v| APInt::new(width, v).with_signed(true))
        .collect()
}

pub(crate) fn sym(g: &mut SemGraph, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

pub(crate) fn con(g: &mut SemGraph, value: u64, width: u32) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}

pub(crate) fn op(g: &mut SemGraph, kind: SymKind, children: &[NodeId]) -> NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}

/// The candidate realization of an extension as a shift pair, parameterized by the
/// source width `n` and register width `w`: `ext_kind(extract(x, n-1, 0), w)`.
fn ext_of_low_bits(ext_kind: SymKind, n: u32, w: u32) -> SemGraph {
    let mut g = SemGraph::new();
    let x = sym(&mut g, 0);
    let hi = con(&mut g, (n - 1) as u64, 16);
    let lo = con(&mut g, 0, 16);
    let extract = op(&mut g, SymKind::Extract, &[x, hi, lo]);
    let width = con(&mut g, w as u64, 16);
    op(&mut g, ext_kind, &[extract, width]);
    g
}

/// `shr_kind(shl(x, k), k)` over `w`-bit values.
fn shift_pair(shr_kind: SymKind, k: u32, w: u32) -> SemGraph {
    let mut g = SemGraph::new();
    let x = sym(&mut g, 0);
    let amount = con(&mut g, k as u64, w);
    let shl = op(&mut g, SymKind::ShiftLeft, &[x, amount]);
    let amount2 = con(&mut g, k as u64, w);
    op(&mut g, shr_kind, &[shl, amount2]);
    g
}

/// Representative `(source_width, register_width)` pairs width-parameterized
/// identities are sampled at, spanning several source widths per register width.
pub(crate) const EXT_WIDTH_SAMPLES: &[(u32, u32)] =
    &[(8, 32), (16, 32), (8, 64), (16, 64), (32, 64)];

/// Confirm that extending the low `n` bits of a register (`ext_kind` ∈ {`SExt`,
/// `ZExt`}) equals `shr_kind(shl(x, w - n), w - n)` for every sampled width pair.
/// On success the caller may emit a width-parameterized rewrite
/// `ext_kind(v, w) -> shr_kind(shl(v, w - n), w - n)` with `n = width(v)`.
pub fn confirm_extension_via_shifts(
    ext_kind: SymKind,
    shr_kind: SymKind,
    oracle: &dyn EquivalenceOracle,
) -> bool {
    EXT_WIDTH_SAMPLES.iter().all(|&(n, w)| {
        n < w
            && oracle.equivalent(
                &ext_of_low_bits(ext_kind, n, w),
                &shift_pair(shr_kind, w - n, w),
                &[w],
            )
    })
}

/// Confirm the width-1 identity `c == If(c, zext(1, 1), zext(0, 1))` — the shape
/// TMDL derives for `slt`-style instructions — so the caller may bridge bare
/// boolean classes to `If`-rooted materializer patterns.
pub fn confirm_bool_via_if(oracle: &dyn EquivalenceOracle) -> bool {
    let mut lhs = SemGraph::new();
    sym(&mut lhs, 0);

    let mut rhs = SemGraph::new();
    let c = sym(&mut rhs, 0);
    let one = con(&mut rhs, 1, 1);
    let zero = con(&mut rhs, 0, 1);
    let then_branch = op(&mut rhs, SymKind::ZExt, &[one, one]);
    let else_branch = op(&mut rhs, SymKind::ZExt, &[zero, one]);
    op(&mut rhs, SymKind::If, &[c, then_branch, else_branch]);

    oracle.equivalent(&lhs, &rhs, &[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_extension_is_a_left_then_arithmetic_right_shift() {
        assert!(confirm_extension_via_shifts(
            SymKind::SExt,
            SymKind::ShiftRightArithmetic,
            &FuzzOracle::default(),
        ));
    }

    #[test]
    fn zero_extension_is_a_left_then_logical_right_shift() {
        assert!(confirm_extension_via_shifts(
            SymKind::ZExt,
            SymKind::ShiftRightLogic,
            &FuzzOracle::default(),
        ));
    }

    #[test]
    fn sign_extension_is_not_a_logical_right_shift() {
        // The oracle must reject the wrong pairing (srl can't sign-extend).
        assert!(!confirm_extension_via_shifts(
            SymKind::SExt,
            SymKind::ShiftRightLogic,
            &FuzzOracle::default(),
        ));
    }

    #[test]
    fn smt_oracle_proves_extension_identities() {
        assert!(confirm_extension_via_shifts(
            SymKind::SExt,
            SymKind::ShiftRightArithmetic,
            &SmtOracle,
        ));
        assert!(confirm_extension_via_shifts(
            SymKind::ZExt,
            SymKind::ShiftRightLogic,
            &SmtOracle,
        ));
    }

    #[test]
    fn smt_oracle_refutes_wrong_pairing() {
        assert!(!confirm_extension_via_shifts(
            SymKind::SExt,
            SymKind::ShiftRightLogic,
            &SmtOracle,
        ));
    }

    #[test]
    fn smt_oracle_shares_symbols_across_sides() {
        // `x ^ x == 0` holds only if both sides constrain the same `x`.
        let mut lhs = SemGraph::new();
        let x = sym(&mut lhs, 0);
        op(&mut lhs, SymKind::Xor, &[x, x]);
        let mut rhs = SemGraph::new();
        con(&mut rhs, 0, 32);
        assert!(SmtOracle.equivalent(&lhs, &rhs, &[32]));
    }

    #[test]
    fn smt_oracle_finds_counterexamples_over_two_symbols() {
        // `x + y != x - y` whenever `2*y != 0`.
        let mut lhs = SemGraph::new();
        let x = sym(&mut lhs, 0);
        let y = sym(&mut lhs, 1);
        op(&mut lhs, SymKind::Add, &[x, y]);
        let mut rhs = SemGraph::new();
        let x = sym(&mut rhs, 0);
        let y = sym(&mut rhs, 1);
        op(&mut rhs, SymKind::Sub, &[x, y]);
        assert!(!SmtOracle.equivalent(&lhs, &rhs, &[32, 32]));
    }

    #[test]
    fn bool_via_if_identity_is_proved() {
        assert!(confirm_bool_via_if(&SmtOracle));
    }
}
