//! Discovery of algebraic identities by *testing and proving*, not hand-authoring.
//!
//! Instruction selection needs target-independent bit-vector lemmas to bridge IR
//! operators that no single instruction implements (e.g. a sub-word sign extension)
//! to sequences that the target *does* have. Rather than writing those lemmas by
//! hand, we propose a candidate shape and confirm it against an
//! [`EquivalenceOracle`]: [`FuzzOracle`] evaluates both sides on many inputs with
//! the reference interpreter ([`crate::lang::execute`]), [`SmtOracle`] proves the
//! equivalence unsatisfiable-to-refute with this crate's QF_BV pipeline.
//! Confirmed shapes become e-graph rewrites at the call site.

use std::collections::HashMap;

use tir_adt::APInt;
use tir_graph::{Dag, GenericDag, MutDag, NodeId};

use super::{SemGraph, ValueId};
use crate::bitblast::{SolveOutcome, blast, blast_with_types};
use crate::lang::{SemType, SymKind, SymPayload, Value, Width, execute, infer_types, infer_widths};

/// Decides whether two single-output expression graphs (over the same symbols)
/// compute the same value for every input of the given symbol widths.
pub trait EquivalenceOracle<A = ()> {
    fn equivalent(&self, lhs: &SemGraph<A>, rhs: &SemGraph<A>, symbol_widths: &[u32]) -> bool;

    /// Decides whether `rhs` is defined and equal to `lhs` whenever `lhs` is
    /// defined. Total operations reduce this obligation to equivalence.
    fn refines(&self, lhs: &SemGraph<A>, rhs: &SemGraph<A>, symbol_widths: &[u32]) -> bool {
        self.equivalent(lhs, rhs, symbol_widths)
    }
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

impl<A> EquivalenceOracle<A> for FuzzOracle {
    fn equivalent(&self, lhs: &SemGraph<A>, rhs: &SemGraph<A>, symbol_widths: &[u32]) -> bool {
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

impl<A> EquivalenceOracle<A> for SmtOracle {
    fn equivalent(&self, lhs: &SemGraph<A>, rhs: &SemGraph<A>, symbol_widths: &[u32]) -> bool {
        self.prove(lhs, rhs, symbol_widths, false)
    }

    fn refines(&self, lhs: &SemGraph<A>, rhs: &SemGraph<A>, symbol_widths: &[u32]) -> bool {
        self.prove(lhs, rhs, symbol_widths, true)
    }
}

impl SmtOracle {
    /// Prove equivalence while preserving the semantic domains of shared symbols.
    pub fn equivalent_typed<A>(
        &self,
        lhs: &SemGraph<A>,
        rhs: &SemGraph<A>,
        symbol_types: &[SemType],
    ) -> bool {
        let Some((g, l, r)) = disequality(lhs, rhs) else {
            return false;
        };
        let symbol_type = |id: NodeId| match g.get_leaf_data(id) {
            Some(SymPayload::SymbolId(id)) => symbol_types.get(*id as usize).cloned(),
            _ => None,
        };
        let Ok(types) = infer_types(&g, symbol_type) else {
            return false;
        };
        let widths = infer_widths(&g, |id| symbol_type(id).and_then(semantic_width));
        same_root_widths(&widths, l, r)
            && matches!(
                blast_with_types(&g, &widths, &types).map(|blasted| blasted.solve()),
                Ok(SolveOutcome::Unsat)
            )
    }

    fn prove<A>(
        &self,
        lhs: &SemGraph<A>,
        rhs: &SemGraph<A>,
        symbol_widths: &[u32],
        defined_refinement: bool,
    ) -> bool {
        let Some((g, l, r)) = disequality(lhs, rhs) else {
            return false;
        };
        let widths = infer_widths(&g, |id| match g.get_leaf_data(id) {
            Some(SymPayload::SymbolId(id)) => symbol_widths.get(*id as usize).copied(),
            _ => None,
        });
        if !same_root_widths(&widths, l, r) {
            return false;
        }
        match blast(&g, &widths) {
            Ok(b) if defined_refinement => {
                matches!(b.solve_defined_equivalence(l, r), SolveOutcome::Unsat)
            }
            Ok(b) => matches!(b.solve(), SolveOutcome::Unsat),
            Err(_) => false,
        }
    }
}

/// The `lhs != rhs` graph both sides share, with the copied roots. `None` if
/// either side is empty.
fn disequality<A>(lhs: &SemGraph<A>, rhs: &SemGraph<A>) -> Option<(OracleGraph, NodeId, NodeId)> {
    let (lhs_root, rhs_root) = (lhs.root()?, rhs.root()?);
    let mut g = OracleGraph::new();
    let mut symbols = HashMap::new();
    let l = copy_reachable(lhs, lhs_root, &mut g, &mut symbols, &mut HashMap::new());
    let r = copy_reachable(rhs, rhs_root, &mut g, &mut symbols, &mut HashMap::new());
    let ne = g.add_node(SymKind::Ne);
    g.add_edge(ne, l);
    g.add_edge(ne, r);
    Some((g, l, r))
}

fn same_root_widths(widths: &[Option<u32>], l: NodeId, r: NodeId) -> bool {
    matches!((widths[l.index()], widths[r.index()]), (Some(l), Some(r)) if l == r)
}

fn semantic_width(ty: SemType) -> Option<u32> {
    match ty {
        SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width)) => Some(width),
        SemType::Float(format) => match (format.exponent, format.mantissa) {
            (Width::Const(exponent), Width::Const(mantissa)) => Some(1 + exponent + mantissa),
            _ => None,
        },
        _ => None,
    }
}

/// Copy the subgraph under `node` into `dst`. Symbol leaves are shared through
/// `symbols` across *both* sides of the equivalence — the bit-blaster allocates
/// fresh literals per node, so a symbol duplicated per side would leave the two
/// occurrences unconstrained against each other.
fn copy_reachable<A>(
    src: &SemGraph<A>,
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
fn sample_values(width: u32, extra: usize) -> Vec<APInt> {
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

/// A symbol leaf for `id`.
pub fn sym<A>(g: &mut SemGraph<A>, id: u32) -> NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

/// A `width`-bit constant leaf.
pub fn con<A>(g: &mut SemGraph<A>, value: u64, width: u32) -> NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(APInt::new(width, value)));
    node
}

/// An interior node of `kind` over `children`.
pub fn op<A>(g: &mut SemGraph<A>, kind: SymKind, children: &[NodeId]) -> NodeId {
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
const EXT_WIDTH_SAMPLES: &[(u32, u32)] = &[(8, 32), (16, 32), (8, 64), (16, 64), (32, 64)];

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
