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
//!   Address and byte count are one class on both sides, so the two accesses name
//!   the same extent of the same address. The two must also agree on an IR type:
//!   the vocabulary is bit-level, and a byte count alone would forward the float a
//!   slot was written with into the integer a reader spells it as.
//! * **S2, dead-store elimination.** `Store(a, n, v2, Store(s, a, n, v1))` leaves
//!   memory as `Store(s, a, n, v2)` does, so the inner store *is* the state `s` —
//!   provided nothing else observes it. That proviso is not algebra: a class is
//!   context-free, and two arms of a gate writing one value to one address are one
//!   term, so unioning a class an arm yields with the state before it would take
//!   that arm back before its write. The applier therefore fires only when the
//!   accesses reading the inner state are the overwriting store alone, the seeder
//!   recorded no operation outside the term graph observing it, and the state the
//!   inner store was handed is not a loop's carried port — a class read both at
//!   the head of an iteration and where the loop was left, so a store landing in
//!   it would answer a read after the loop with what the body overwrote — and the
//!   two stores differ somewhere other than the state they take. Two stores of one
//!   value to one address are one node apart from that state, so unioning the
//!   inner one's result with the state before it makes the survivor congruent to
//!   the store it replaced, and the whole chain collapses into the state it
//!   started from: both writes gone, not one. Full unrolling is what makes that
//!   shape ordinary, and `dse` removes the earlier write on the chain anyway.

use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, Rewrite, Rhs, Var};

use super::rules::{Rule, Sym, class_value_type, operand};
use crate::sem::{Prov, SemNode as Node, SymKind};
use crate::{Conditional, Context, LoopLike, OpId, TypeId};

/// `Load(address, bytes, metadata, state)`.
const LOAD_ARITY: usize = 4;
pub(super) const LOAD_STATE: usize = 3;
/// `Store(address, bytes, value, address_space, state)`.
const STORE_ARITY: usize = 5;
const STORE_VALUE: usize = 2;
const STORE_STATE: usize = 4;
pub(super) const ADDRESS: usize = 0;
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
            let Some(value) = stored_value(&context, eg, read[LOAD_STATE], read, ty) else {
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
            let state = written[STORE_STATE];
            if observers(eg, state) != [eg.find(root)] {
                return;
            }
            let Some(before) = overwritten_state(eg, state, written) else {
                return;
            };
            if exported
                .iter()
                .any(|&class| eg.find(class) == state || eg.find(class) == before)
            {
                return;
            }
            // The union runs both ways. Whatever reads the state the overwritten
            // store was handed is handed the memory that store left instead, so a
            // load before the pair would answer with the value written after it.
            // Nothing but the overwritten store may read it.
            if observers(eg, before) != [state] {
                return;
            }
            // A third write to the address would collapse the same way: the union
            // below makes the overwriting store name `before`, and the store that
            // left `before` names the state before *it*, so the two become
            // congruent one round later and the chain folds back to where it
            // started. One elimination per address per round is what the law can
            // carry; `dse` reads the chain itself and needs no such care.
            if writes_to(eg, before, written[ADDRESS]) {
                return;
            }
            // A loop's carried port is one class read at more than one point of the
            // program — the head of an iteration, and where the loop was left. A
            // store node landing in it answers for both, so a read after the loop
            // would forward the value the body overwrote.
            if eg
                .nodes(before)
                .iter()
                .any(|node| node.sym() == Some(SymKind::Theta))
            {
                return;
            }
            eg.union(state, before);
        },
    )
}

/// γ-distribution: a load of the state a gate chose is the gate's choice between
/// the loads of what each arm left. The law only restates which state the read
/// observes, so it is as definitional as S1 — but only where the address names a
/// slot the graph sees every access of, since a value reconstructed for an
/// address the term graph cannot follow would answer for memory it never read.
pub(crate) fn distribute_load_over_gamma(context: Context, promotable: Vec<Id>) -> Rule {
    access_law(
        "gamma-load",
        SymKind::LoadMemory,
        LOAD_ARITY,
        move |eg, read, root| {
            if !promotable
                .iter()
                .any(|&class| eg.find(class) == read[ADDRESS])
            {
                return;
            }
            let Some((ty, load)) = read_form(&context, eg, root, read) else {
                return;
            };
            let Some((gate, decision, arms)) = state_gamma(&context, eg, read[LOAD_STATE]) else {
                return;
            };
            let mut children = vec![decision];
            for state in arms {
                let mut operands = read.to_vec();
                operands[LOAD_STATE] = state;
                children.push(
                    eg.add(Node::introduced_at(SymKind::LoadMemory, load, operands).typed(ty)),
                );
            }
            let gamma = eg.add(Node::introduced_at(SymKind::If, gate, children));
            eg.union(root, gamma);
        },
    )
}

/// θ-distribution: a load of the state a loop carries is the loop's carry of the
/// loads of what it started with and what its body left. Like γ-distribution it
/// only restates which state the read observes, and it fires under the same
/// reading of the address.
pub(crate) fn distribute_load_over_theta(context: Context, promotable: Vec<Id>) -> Rule {
    access_law(
        "theta-load",
        SymKind::LoadMemory,
        LOAD_ARITY,
        move |eg, read, root| {
            if !promotable
                .iter()
                .any(|&class| eg.find(class) == read[ADDRESS])
            {
                return;
            }
            let Some((ty, load)) = read_form(&context, eg, root, read) else {
                return;
            };
            let Some((loop_op, carried)) = state_theta(&context, eg, read[LOAD_STATE]) else {
                return;
            };
            let carried: Vec<Id> = carried
                .iter()
                .map(|&state| {
                    let mut operands = read.to_vec();
                    operands[LOAD_STATE] = state;
                    eg.add(Node::introduced_at(SymKind::LoadMemory, load, operands).typed(ty))
                })
                .collect();
            let theta = eg.add(Node::introduced_at(SymKind::Theta, loop_op, carried));
            eg.union(root, theta);
        },
    )
}

/// The carry `state` is: the loop publishing it, the state it was entered with,
/// and the one each edge back into the port carries.
fn state_theta(context: &Context, eg: &EGraph<Node>, state: Id) -> Option<(OpId, Vec<Id>)> {
    eg.nodes(state).iter().find_map(|node| {
        if node.sym() != Some(SymKind::Theta) {
            return None;
        }
        let [init, edges @ ..] = &node.children[..] else {
            return None;
        };
        let init = eg.find(*init);
        let edges: Vec<Id> = edges.iter().map(|&edge| eg.find(edge)).collect();
        // A loop entered on the state it carries, or leaving every edge on it,
        // carries nothing to read.
        if init == state || edges.is_empty() || edges.iter().all(|&edge| edge == state) {
            return None;
        }
        let loop_op = live_op(context, node)?;
        context
            .get_op(loop_op)
            .as_interface::<dyn LoopLike>()
            .map(|_| (loop_op, std::iter::once(init).chain(edges).collect()))
    })
}

/// The gate `state` is the choice of: the conditional publishing it, its
/// decision, and the state each of its arms leaves — one per arm in the order
/// [`Conditional::case_values`] reports, which the commit reads back the same
/// way. A two-armed gate is the case `N == 2`.
fn state_gamma(context: &Context, eg: &EGraph<Node>, state: Id) -> Option<(OpId, Id, Vec<Id>)> {
    eg.nodes(state).iter().find_map(|node| {
        if node.sym() != Some(SymKind::If) {
            return None;
        }
        let [decision, arms @ ..] = &node.children[..] else {
            return None;
        };
        let arms: Vec<Id> = arms.iter().map(|&arm| eg.find(arm)).collect();
        if arms.len() < 2 || arms.contains(&state) {
            return None;
        }
        let gate = live_op(context, node)?;
        let conditional = context.get_op(gate).as_interface::<dyn Conditional>()?;
        (conditional.case_values().len() == arms.len()).then_some((gate, eg.find(*decision), arms))
    })
}

/// The type and the operation of the load standing for `read` in `class`.
fn read_form(
    context: &Context,
    eg: &EGraph<Node>,
    class: Id,
    read: &[Id],
) -> Option<(TypeId, OpId)> {
    eg.nodes(class)
        .iter()
        .find(|node| node.sym() == Some(SymKind::LoadMemory) && canonical(eg, node) == read)
        .and_then(|node| Some((node.ty?, live_op(context, node)?)))
}

/// The operation `node` stands for, if it is still in the tree: the one defining
/// the value a seeded term was read from, or the one a law named to rebuild an
/// introduced term. A term the graph keeps outlives the rewrite that erased
/// either.
fn live_op(context: &Context, node: &Node) -> Option<OpId> {
    let op = match node.prov {
        Prov::Value(value) => {
            if !context.has_value(value) {
                return None;
            }
            context.get_value(value).defining_op()?
        }
        Prov::Op(op) => op,
        _ => return None,
    };
    context.has_operation(op).then_some(op)
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

/// The state a store in `state` was handed, when it wrote the extent `written`
/// names.
fn overwritten_state(eg: &EGraph<Node>, state: Id, written: &[Id]) -> Option<Id> {
    eg.nodes(state)
        .iter()
        .filter(|node| node.sym() == Some(SymKind::StoreMemory))
        .find_map(|node| {
            let dead = canonical(eg, node);
            (dead[ADDRESS] == written[ADDRESS]
                && dead[BYTES] == written[BYTES]
                && dead[..STORE_STATE] != written[..STORE_STATE])
                .then_some(dead[STORE_STATE])
        })
}

/// Whether `state` is the memory a store to `address` left.
fn writes_to(eg: &EGraph<Node>, state: Id, address: Id) -> bool {
    eg.nodes(state).iter().any(|node| {
        node.sym() == Some(SymKind::StoreMemory) && canonical(eg, node)[ADDRESS] == address
    })
}

/// The classes of the accesses reading `state`, in the graph's own order.
fn observers(eg: &EGraph<Node>, state: Id) -> Vec<Id> {
    let mut classes = Vec::new();
    for (kind, slot) in [
        (SymKind::LoadMemory, LOAD_STATE),
        (SymKind::StoreMemory, STORE_STATE),
    ] {
        let key = Node::sym_pattern(kind, Vec::new()).op_key();
        for class in eg.classes_with_op(key) {
            let reads = eg
                .nodes(class)
                .iter()
                .any(|node| node.sym() == Some(kind) && eg.find(node.children[slot]) == state);
            if reads {
                classes.push(class);
            }
        }
    }
    classes
}

/// The type the load standing for `read` in `class` yields.
fn loaded_type(eg: &EGraph<Node>, class: Id, read: &[Id]) -> Option<TypeId> {
    eg.nodes(class)
        .iter()
        .find(|node| node.sym() == Some(SymKind::LoadMemory) && canonical(eg, node) == read)
        .and_then(|node| node.ty)
}

/// The class a store in `state` left at the extent `read` names, when it holds a
/// value of type `ty`.
fn stored_value(
    context: &Context,
    eg: &EGraph<Node>,
    state: Id,
    read: &[Id],
    ty: TypeId,
) -> Option<Id> {
    eg.nodes(state)
        .iter()
        .filter(|node| node.sym() == Some(SymKind::StoreMemory))
        .find_map(|node| {
            let written = canonical(eg, node);
            let value = written[STORE_VALUE];
            (written[ADDRESS] == read[ADDRESS]
                && written[BYTES] == read[BYTES]
                && class_value_type(context, eg, value) == Some(ty))
            .then_some(value)
        })
}

fn canonical(eg: &EGraph<Node>, node: &Node) -> Vec<Id> {
    node.children.iter().map(|&child| eg.find(child)).collect()
}
