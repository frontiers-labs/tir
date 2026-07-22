//! Bit-blast a lowered QF_BV + Bool formula graph to CNF over a [`crate::sat::Solver`].
//! Node bit vectors are little-endian (bit 0 = LSB); operands precede their parents.

mod arith;
mod fp;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};

use tir_graph::{Dag, GenericDag, NodeId};

use crate::lang::{SymKind, SymPayload};
use crate::sat::{Lit, SatResult, Solver};

#[derive(Clone, Copy)]
pub(crate) enum Shift {
    Left,
    Logical,
    Arithmetic,
}

/// Why a formula could not be bit-blasted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitblastError {
    /// Node kind outside the QF_BV + Bool subset (iterators, memory, fp).
    Unsupported(SymKind),
    UnknownWidth(usize),
    BadConstant(usize),
}

impl Display for BitblastError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            BitblastError::Unsupported(k) => write!(f, "cannot bit-blast {k:?}"),
            BitblastError::UnknownWidth(i) => write!(f, "node {i} has no known width"),
            BitblastError::BadConstant(i) => write!(f, "node {i} expected a constant operand"),
        }
    }
}

impl std::error::Error for BitblastError {}

/// CNF encoding: solver, one-bit root literal, per-symbol bits keyed by
/// `SymbolId`, and per-node bits indexed by node id (to read any sub-term back).
pub struct Blasted {
    pub solver: Solver,
    pub root_bit: Lit,
    pub sym_bits: HashMap<u32, Vec<Lit>>,
    pub node_bits: Vec<Vec<Lit>>,
}

impl Blasted {
    /// Assert the root and solve, decoding each symbol's little-endian bits on `Sat`.
    pub fn solve(mut self) -> SolveOutcome {
        self.solver.add_clause(&[self.root_bit]);
        match self.solver.solve() {
            SatResult::Sat(_) => {
                let model = self
                    .sym_bits
                    .iter()
                    .map(|(&id, bits)| {
                        let v: Vec<bool> = bits.iter().map(|&b| self.lit_value(b)).collect();
                        (id, v)
                    })
                    .collect();
                SolveOutcome::Sat(model)
            }
            SatResult::Unsat => SolveOutcome::Unsat,
            SatResult::Unknown => SolveOutcome::Unknown,
        }
    }

    pub fn lit_value(&self, l: Lit) -> bool {
        self.solver.value(l.var()) ^ l.is_negated()
    }
}

/// The result of solving a bit-blasted formula.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SolveOutcome {
    /// Satisfiable; per-symbol little-endian bit assignment keyed by `SymbolId`.
    Sat(HashMap<u32, Vec<bool>>),
    Unsat,
    Unknown,
}

/// Bit-blast a lowered formula graph rooted at its last node.
pub fn blast<V>(
    graph: &GenericDag<SymKind, SymPayload<V>>,
    widths: &[Option<u32>],
) -> Result<Blasted, BitblastError> {
    let mut b = Blaster::new(graph, widths);
    for i in 0..graph.len() {
        let id = NodeId::from_index(i);
        let bits = b.encode(id)?;
        b.bits.push(bits);
    }
    let root = graph.root().expect("non-empty formula graph");
    let root_bit = b.bits[root.index()][0];
    Ok(Blasted {
        solver: b.solver,
        root_bit,
        sym_bits: b.sym_bits,
        node_bits: b.bits,
    })
}

pub(crate) struct Blaster<'g, V> {
    solver: Solver,
    /// A literal fixed to true; its negation is the constant false.
    one: Lit,
    bits: Vec<Vec<Lit>>,
    sym_bits: HashMap<u32, Vec<Lit>>,
    graph: &'g GenericDag<SymKind, SymPayload<V>>,
    widths: &'g [Option<u32>],
}

impl<'g, V> Blaster<'g, V> {
    fn new(graph: &'g GenericDag<SymKind, SymPayload<V>>, widths: &'g [Option<u32>]) -> Self {
        let mut solver = Solver::new();
        let one = Lit::positive(solver.new_var());
        solver.add_clause(&[one]);
        Blaster {
            solver,
            one,
            bits: Vec::with_capacity(graph.len()),
            sym_bits: HashMap::new(),
            graph,
            widths,
        }
    }

    fn zero(&self) -> Lit {
        self.one.negate()
    }

    fn fresh(&mut self) -> Lit {
        Lit::positive(self.solver.new_var())
    }

    fn width(&self, id: NodeId) -> Result<usize, BitblastError> {
        self.widths
            .get(id.index())
            .copied()
            .flatten()
            .map(|w| w as usize)
            .ok_or(BitblastError::UnknownWidth(id.index()))
    }

    fn const_u64(&self, id: NodeId) -> Result<u64, BitblastError> {
        match self.graph.get_leaf_data(id) {
            Some(SymPayload::Int(v)) => Ok(v.to_u64()),
            _ => Err(BitblastError::BadConstant(id.index())),
        }
    }

    fn child_bits(&self, id: NodeId, k: usize) -> Vec<Lit> {
        let child = self.graph.children(id).nth(k).expect("child exists");
        self.bits[child.index()].clone()
    }

    fn binop_bits(&self, id: NodeId) -> (Vec<Lit>, Vec<Lit>) {
        (self.child_bits(id, 0), self.child_bits(id, 1))
    }

    fn encode(&mut self, id: NodeId) -> Result<Vec<Lit>, BitblastError> {
        use SymKind::*;
        let kind = *self.graph.get_kind(id);
        match kind {
            Symbol => self.encode_symbol(id),
            Constant => self.encode_constant(id),
            Not => Ok(self.child_bits(id, 0).iter().map(|l| l.negate()).collect()),
            And => self.bitwise(id, |s, a, b| s.gate_and(a, b)),
            Or => self.bitwise(id, |s, a, b| s.gate_or(a, b)),
            Xor => self.bitwise(id, |s, a, b| s.gate_xor(a, b)),
            Add => {
                let (a, b) = self.binop_bits(id);
                let z = self.zero();
                Ok(self.adder(&a, &b, z).0)
            }
            Sub => {
                let (a, b) = self.binop_bits(id);
                Ok(self.subtract(&a, &b))
            }
            Neg => {
                let a = self.child_bits(id, 0);
                Ok(self.negate(&a))
            }
            Mul => {
                let (a, b) = self.binop_bits(id);
                Ok(self.multiply(&a, &b))
            }
            UDiv => self.divrem(id, false, true),
            URem => self.divrem(id, false, false),
            Div => self.divrem(id, true, true),
            SRem => self.divrem(id, true, false),
            ShiftLeft => self.shift(id, Shift::Left),
            ShiftRightLogic => self.shift(id, Shift::Logical),
            ShiftRightArithmetic => self.shift(id, Shift::Arithmetic),
            Eq => {
                let (a, b) = self.binop_bits(id);
                Ok(vec![self.eq_bits(&a, &b)])
            }
            Ne => {
                let (a, b) = self.binop_bits(id);
                let eq = self.eq_bits(&a, &b);
                Ok(vec![eq.negate()])
            }
            ULt | ULe | UGt | UGe | Lt | Le | Gt | Ge => self.compare(id, kind),
            Concat => {
                // Operand 0 is the high half; little-endian result is low bits then high.
                let mut out = self.child_bits(id, 1);
                out.extend(self.child_bits(id, 0));
                Ok(out)
            }
            Bitcast => Ok(self.child_bits(id, 0)),
            Extract => self.encode_extract(id),
            ZExt => self.encode_extend(id, false),
            SExt => self.encode_extend(id, true),
            If => self.encode_ite(id),
            SIToFP => self.encode_int_to_float(id, true),
            UIToFP => self.encode_int_to_float(id, false),
            other => Err(BitblastError::Unsupported(other)),
        }
    }

    fn encode_symbol(&mut self, id: NodeId) -> Result<Vec<Lit>, BitblastError> {
        let w = self.width(id)?;
        let bits: Vec<Lit> = (0..w).map(|_| self.fresh()).collect();
        if let Some(SymPayload::SymbolId(sid)) = self.graph.get_leaf_data(id) {
            self.sym_bits.insert(*sid, bits.clone());
        }
        Ok(bits)
    }

    fn encode_constant(&mut self, id: NodeId) -> Result<Vec<Lit>, BitblastError> {
        let w = self.width(id)?;
        let value = self.const_u64(id)?;
        let (one, zero) = (self.one, self.zero());
        Ok((0..w)
            .map(|i| if (value >> i) & 1 == 1 { one } else { zero })
            .collect())
    }

    fn bitwise(
        &mut self,
        id: NodeId,
        gate: impl Fn(&mut Self, Lit, Lit) -> Lit,
    ) -> Result<Vec<Lit>, BitblastError> {
        let (a, b) = self.binop_bits(id);
        Ok(a.iter().zip(&b).map(|(&x, &y)| gate(self, x, y)).collect())
    }

    fn encode_extract(&mut self, id: NodeId) -> Result<Vec<Lit>, BitblastError> {
        let value = self.child_bits(id, 0);
        let children: Vec<NodeId> = self.graph.children(id).collect();
        let hi = self.const_u64(children[1])? as usize;
        let lo = self.const_u64(children[2])? as usize;
        Ok(value[lo..=hi].to_vec())
    }

    fn encode_extend(&mut self, id: NodeId, signed: bool) -> Result<Vec<Lit>, BitblastError> {
        let value = self.child_bits(id, 0);
        let target = self.width(id)?;
        let fill = if signed {
            *value.last().expect("non-empty operand")
        } else {
            self.zero()
        };
        let mut out = value;
        out.resize(target, fill);
        Ok(out)
    }

    fn encode_ite(&mut self, id: NodeId) -> Result<Vec<Lit>, BitblastError> {
        let cond = self.child_bits(id, 0)[0];
        let then = self.child_bits(id, 1);
        let els = self.child_bits(id, 2);
        Ok(self.mux_bits(cond, &then, &els))
    }

    // ----- Tseitin gate primitives -----

    fn gate_and(&mut self, a: Lit, b: Lit) -> Lit {
        let y = self.fresh();
        self.solver.add_clause(&[a.negate(), b.negate(), y]);
        self.solver.add_clause(&[a, y.negate()]);
        self.solver.add_clause(&[b, y.negate()]);
        y
    }

    fn gate_or(&mut self, a: Lit, b: Lit) -> Lit {
        let y = self.fresh();
        self.solver.add_clause(&[a, b, y.negate()]);
        self.solver.add_clause(&[a.negate(), y]);
        self.solver.add_clause(&[b.negate(), y]);
        y
    }

    fn gate_xor(&mut self, a: Lit, b: Lit) -> Lit {
        let y = self.fresh();
        self.solver
            .add_clause(&[a.negate(), b.negate(), y.negate()]);
        self.solver.add_clause(&[a, b, y.negate()]);
        self.solver.add_clause(&[a, b.negate(), y]);
        self.solver.add_clause(&[a.negate(), b, y]);
        y
    }

    /// `y = s ? a : b`.
    fn gate_mux(&mut self, s: Lit, a: Lit, b: Lit) -> Lit {
        let y = self.fresh();
        self.solver.add_clause(&[s.negate(), a.negate(), y]);
        self.solver.add_clause(&[s.negate(), a, y.negate()]);
        self.solver.add_clause(&[s, b.negate(), y]);
        self.solver.add_clause(&[s, b, y.negate()]);
        y
    }

    fn mux_bits(&mut self, s: Lit, a: &[Lit], b: &[Lit]) -> Vec<Lit> {
        a.iter()
            .zip(b)
            .map(|(&x, &y)| self.gate_mux(s, x, y))
            .collect()
    }

    /// One bit equal to whether the two equal-width vectors are bitwise equal.
    fn eq_bits(&mut self, a: &[Lit], b: &[Lit]) -> Lit {
        let mut acc = self.one;
        for (&x, &y) in a.iter().zip(b) {
            let xor = self.gate_xor(x, y);
            acc = self.gate_and(acc, xor.negate());
        }
        acc
    }
}
