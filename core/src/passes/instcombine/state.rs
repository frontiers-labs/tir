//! The laws of the state algebra the memory terms live in.
//!
//! They are definitional, not proved: the axiom prover is QF_BV and has no array
//! model to quantify a memory over, so a read and a write mean here what addition
//! means for `addi`. What keeps them honest is that they are narrow. Both read
//! the state operand the seeder threads, which is the whole of memory identity in
//! the term graph: a chain reaches an access only through the writes that
//! actually happened on it, so a law that fires has already been told the two
//! accesses alias exactly.
//!
//! * **S1, store-to-load forwarding.** `Load(a, n, m, Store(s, a, n, v))` is `v`.
//!   The two accesses name one extent of one object, so the read covers exactly
//!   the bytes the write left. They must also agree on an IR type: the vocabulary
//!   is bit-level, and a byte count alone would forward the float a slot was
//!   written with into the integer a reader spells it as.
//! * **S2, dead-store elimination.** `Store(a, n, v2, Store(s, a, n, v1))` leaves
//!   memory as `Store(s, a, n, v2)` does, so the inner store *is* the state `s` —
//!   provided nothing that reads the bytes it wrote can tell. That proviso is not
//!   algebra: a class is context-free, and the union runs both ways, so whatever
//!   reads the memory the dead store left is handed the memory before it and
//!   whatever reads the memory before it is handed what the store left. Both are
//!   questions about bytes, and the answer is the extent: an access naming bytes
//!   the pair does not cover cannot tell the two states apart, however it is
//!   ordered against them. What the law refuses is therefore an access on either
//!   state that overlaps the extent — the reads Spec 05's fork leaves on the
//!   dead store's own state among them — a second write to the same extent on
//!   the state before, which would become congruent to the survivor one round
//!   later and fold the chain back to where it started, a state something
//!   outside the term graph observes, and a loop's carried port, a class read
//!   both at the head of an iteration and where the loop was left. Two stores
//!   agreeing on everything but the state they take are one node apart from it,
//!   so the law leaves them alone: the term does not say `s` already holds `v`.
//!
//! Both laws read the *extent* an access names — the object its address is
//! derived from, the byte offset into it, and the byte count — rather than the
//! address class alone, so `p + 4` and `p + 2 + 2` are the one extent they are.

use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, Rewrite, Rhs, Var};

use super::rules::{Rule, Sym, class_value_type, operand};
use crate::sem::{SemNode as Node, SymKind};
use crate::{Context, TypeId};

/// `Load(address, bytes, metadata, state)`.
const LOAD_ARITY: usize = 4;
const LOAD_STATE: usize = 3;
/// `Store(address, bytes, value, address_space, state)`.
const STORE_ARITY: usize = 5;
const STORE_VALUE: usize = 2;
const STORE_STATE: usize = 4;
const ADDRESS: usize = 0;
const BYTES: usize = 1;

/// S1: a load whose state a matching store left reads that store's value.
pub(crate) fn forward_load(context: Context) -> Rule {
    access_law(
        "store-to-load",
        SymKind::LoadMemory,
        LOAD_ARITY,
        move |eg, read, root| {
            let Some(ty) = loaded_type(eg, root, read) else {
                return;
            };
            let Some(over) = extent(eg, read[ADDRESS], read[BYTES]) else {
                return;
            };
            let Some(value) = stored_value(&context, eg, read[LOAD_STATE], over, ty) else {
                return;
            };
            eg.union(root, value);
        },
    )
}

/// S2: a store the next one overwrites unobserved leaves the state it was handed.
pub(crate) fn eliminate_dead_store(exported: Vec<Id>) -> Rule {
    access_law(
        "dead-store",
        SymKind::StoreMemory,
        STORE_ARITY,
        move |eg, written, root| {
            let Some(over) = extent(eg, written[ADDRESS], written[BYTES]) else {
                return;
            };
            let state = written[STORE_STATE];
            let Some(before) = overwritten_state(eg, state, written, over) else {
                return;
            };
            if exported
                .iter()
                .any(|&class| eg.find(class) == state || eg.find(class) == before)
            {
                return;
            }
            // Nothing but the overwriting store may read the bytes the dead
            // store left: the union hands such a read the memory before that
            // write, and a store there covering other bytes too would answer it.
            if observed(eg, state, over, eg.find(root), Extent::overlaps) {
                return;
            }
            // And nothing but the dead store may read those bytes on the state it
            // was handed: forwarding answers an access naming exactly the extent
            // a store left, so that is the reading the union would change.
            if observed(eg, before, over, state, Extent::reads) {
                return;
            }
            // A second write to the same extent on `before` becomes congruent to
            // the survivor one round later — the survivor names `before`, and the
            // store that left `before` names the state before *it* — and the
            // chain folds back to where it started: both writes gone, not one.
            if writes_over(eg, before, over) {
                return;
            }
            // A loop's carried port is one class read at more than one point of the
            // program — the head of an iteration, and where the loop was left. A
            // store node landing in it answers for both, so a read after the loop
            // would forward the value the body overwrote.
            if eg
                .nodes(before)
                .any(|node| node.sym() == Some(SymKind::Theta))
            {
                return;
            }
            eg.union(state, before);
        },
    )
}

/// A law over every access of `kind`, handed the canonical classes of the term's
/// `arity` operands and the class the access itself stands for.
fn access_law(
    name: &'static str,
    kind: SymKind,
    arity: usize,
    fire: impl Fn(&mut EGraph<Node>, &[Id], Id) + Send + Sync + 'static,
) -> Rule {
    let mut lhs = Pattern::new();
    let args = (0..arity)
        .map(|index| lhs.var(Var::Symbol(index as Sym)))
        .collect();
    lhs.add(Node::sym_pattern(kind, args));
    Rewrite::new(
        name,
        lhs,
        Rhs::Apply(Box::new(move |eg, substitution, root| {
            let operands: Vec<Id> = (0..arity)
                .map(|index| eg.find(operand(substitution, index as Sym)))
                .collect();
            fire(eg, &operands, root);
        })),
    )
}

/// The bytes an access names: the object its address is derived from, the byte
/// offset into it, and how many bytes it covers.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Extent {
    base: Id,
    offset: i64,
    bytes: i64,
}

impl Extent {
    /// Whether the two name a byte in common. Two extents of different objects
    /// are not comparable here — the term graph does not know an allocation from
    /// a parameter, and an address it cannot read back to `other`'s object is its
    /// own object — so they are taken to overlap. What tells objects apart is the
    /// chain they sit on, and a law only ever sees one chain.
    fn overlaps(self, other: Extent) -> bool {
        !self.comparable(other)
            || (self.offset < other.offset + other.bytes && other.offset < self.offset + self.bytes)
    }

    /// Whether an access naming these bytes reads what a write of `other` left.
    /// Forwarding answers exactly the extent a store covers, so that is the one
    /// reading a law changes — but only where the two are placed on one object
    /// at all. An address the walk cannot read back to `other`'s is its own
    /// object *for now*: an offset that becomes a literal later moves it onto
    /// `other`'s, and a law that has already fired cannot be taken back.
    fn reads(self, other: Extent) -> bool {
        !self.comparable(other) || self == other
    }

    fn comparable(self, other: Extent) -> bool {
        self.base == other.base
    }
}

/// How far a chain of `ptradd`s is read back for the object an address names.
const DERIVATION_LIMIT: usize = 8;

/// The extent an access over `address` covering `bytes` names.
fn extent(eg: &EGraph<Node>, address: Id, bytes: Id) -> Option<Extent> {
    let bytes = eg.nodes(bytes).find_map(Node::int)?.to_i64();
    let (base, offset) = derivation(eg, eg.find(address), 0, 0);
    Some(Extent {
        base,
        offset,
        bytes,
    })
}

/// The object an address is derived from and the offset into it: arithmetic on a
/// pointer points into the object it started from, so the `ptradd`s over
/// literals read back to that object and one offset. An address the walk cannot
/// read back is its own object at offset zero, which is what makes two
/// unrelated addresses overlap rather than not.
fn derivation(eg: &EGraph<Node>, address: Id, offset: i64, depth: usize) -> (Id, i64) {
    if depth == DERIVATION_LIMIT {
        return (address, offset);
    }
    for node in eg.nodes(address) {
        let Some(op) = node.kind.ir() else {
            continue;
        };
        if (op.dialect, op.name) != ("ptr", "ptradd") {
            continue;
        }
        let [base, added] = node.children[..] else {
            continue;
        };
        let Some(step) = eg.nodes(eg.find(added)).find_map(Node::int) else {
            continue;
        };
        return derivation(eg, eg.find(base), offset + step.to_i64(), depth + 1);
    }
    (address, offset)
}

/// The state a store in `state` was handed, when it wrote the extent `over` and
/// is not the very store overwriting it.
fn overwritten_state(eg: &EGraph<Node>, state: Id, written: &[Id], over: Extent) -> Option<Id> {
    eg.nodes(state)
        .filter(|node| node.sym() == Some(SymKind::StoreMemory))
        .find_map(|node| {
            let dead = canonical(eg, node);
            (extent(eg, dead[ADDRESS], dead[BYTES]) == Some(over)
                && dead[..STORE_STATE] != written[..STORE_STATE])
                .then_some(dead[STORE_STATE])
        })
}

/// Whether `state` is the memory a store to the extent `over` names left.
fn writes_over(eg: &EGraph<Node>, state: Id, over: Extent) -> bool {
    eg.nodes(state).any(|node| {
        node.sym() == Some(SymKind::StoreMemory) && {
            let written = canonical(eg, node);
            extent(eg, written[ADDRESS], written[BYTES]) == Some(over)
        }
    })
}

/// Whether an access other than `spared` reads `state` at bytes `tells` says it
/// can tell the memory `over` names apart by. An access whose extent the terms
/// cannot place may name any of them.
fn observed(
    eg: &EGraph<Node>,
    state: Id,
    over: Extent,
    spared: Id,
    tells: impl Fn(Extent, Extent) -> bool,
) -> bool {
    for (kind, slot) in [
        (SymKind::LoadMemory, LOAD_STATE),
        (SymKind::StoreMemory, STORE_STATE),
    ] {
        let key = Node::sym_pattern(kind, Vec::new()).op_key();
        for class in eg.classes_with_op(key) {
            if class == spared {
                continue;
            }
            for node in eg.nodes(class) {
                if node.sym() != Some(kind) || eg.find(node.children[slot]) != state {
                    continue;
                }
                let read = canonical(eg, node);
                match extent(eg, read[ADDRESS], read[BYTES]) {
                    Some(read) if !tells(read, over) => {}
                    _ => return true,
                }
            }
        }
    }
    false
}

/// The type the load standing for `read` in `class` yields.
fn loaded_type(eg: &EGraph<Node>, class: Id, read: &[Id]) -> Option<TypeId> {
    eg.nodes(class)
        .find(|node| node.sym() == Some(SymKind::LoadMemory) && canonical(eg, node) == read)
        .and_then(|node| node.ty)
}

/// The class a store in `state` left at the extent `over`, when it holds a value
/// of type `ty`.
fn stored_value(
    context: &Context,
    eg: &EGraph<Node>,
    state: Id,
    over: Extent,
    ty: TypeId,
) -> Option<Id> {
    eg.nodes(state)
        .filter(|node| node.sym() == Some(SymKind::StoreMemory))
        .find_map(|node| {
            let written = canonical(eg, node);
            let value = written[STORE_VALUE];
            (extent(eg, written[ADDRESS], written[BYTES]) == Some(over)
                && class_value_type(context, eg, value) == Some(ty))
            .then_some(value)
        })
}

fn canonical(eg: &EGraph<Node>, node: &Node) -> Vec<Id> {
    node.children.iter().map(|&child| eg.find(child)).collect()
}
