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

use smallvec::smallvec;
use tir_relational::{Atom, Cmp, ColumnId, Expr, Guard, HeadOp, Nested, Plan, Query, Source};
use tir_symbolic::egraph::{ENode, Id};

use crate::sem::{SemNode as Node, SymKind, node::field};

/// `Load(address, bytes, metadata, state)`.
const LOAD_ARITY: usize = 4;
const LOAD_STATE: usize = 3;
/// `Store(address, bytes, value, address_space, state)`.
const STORE_ARITY: usize = 5;
const STORE_VALUE: usize = 2;
const STORE_STATE: usize = 4;
const ADDRESS: usize = 0;
const BYTES: usize = 1;

/// The object an address is derived from, one `ptradd` at a time: pointer
/// arithmetic lands in the object it started from, further along by what it
/// added. Stated as a rule raising a column rather than a walk taken on demand,
/// so a chain of any length is read back, and an address the terms derive two
/// ways at once is placed nowhere — which makes a law refuse it rather than pick
/// whichever spelling it met first.
pub(crate) fn pointer_derivation() -> tir_relational::Rule<Node> {
    // Variables: 0 the sum, 1 the address it starts from, 2 the step, 3 the
    // object that address is in.
    tir_relational::Rule {
        name: "pointer-derivation".into(),
        plan: Plan::compile(Query {
            vars: 4,
            scalars: 3,
            root: 0,
            atoms: vec![
                Atom::Node {
                    template: Node::pattern::<crate::ptr::PtrAddOp>(vec![
                        Id::from_raw(1),
                        Id::from_raw(2),
                    ]),
                    args: smallvec![1, 2],
                    class: 0,
                    row: None,
                },
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 2,
                    value: Derivation::STEP_CONST,
                },
                Atom::Object {
                    key: 1,
                    base: 3,
                    offset: Derivation::OFFSET,
                },
            ],
            guards: vec![Guard::Read {
                term: Source::Label(Derivation::STEP_CONST),
                field: field::INT_SIGNED,
                out: Derivation::STEP,
            }],
            nots: Vec::new(),
        }),
        head: vec![HeadOp::RaiseObject {
            key: 0,
            base: 3,
            offset: Expr::Add(
                Box::new(Expr::Scalar(Derivation::OFFSET)),
                Box::new(Expr::Scalar(Derivation::STEP)),
            ),
        }],
    }
}

struct Derivation;

impl Derivation {
    const STEP_CONST: u32 = 0;
    const OFFSET: u32 = 1;
    const STEP: u32 = 2;
}

/// S1: a load whose state a matching store left reads that store's value. The
/// store is a node of the state class the load names, and the two extents are
/// one object, one offset and one byte count — the object through the shared
/// variable, the rest through the guards.
pub(crate) fn forward_load() -> tir_relational::Rule<Node> {
    // Variables: 0 the load, 1..4 its operands, 5 the object both address, 6..10
    // the store's operands.
    tir_relational::Rule {
        name: "store-to-load".into(),
        plan: Plan::compile(Query {
            vars: 11,
            scalars: 9,
            root: 0,
            atoms: vec![
                Atom::Node {
                    template: Node::sym_pattern(
                        SymKind::LoadMemory,
                        (1..=LOAD_ARITY as u32).map(Id::from_raw).collect(),
                    ),
                    args: (1..=LOAD_ARITY as u32).collect(),
                    class: 0,
                    row: Some(Access::LOAD_ROW),
                },
                Atom::Object {
                    key: 1 + ADDRESS as u32,
                    base: 5,
                    offset: Access::OFFSET,
                },
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 1 + BYTES as u32,
                    value: Access::BYTES_CONST,
                },
                Atom::Node {
                    template: Node::sym_pattern(
                        SymKind::StoreMemory,
                        (6..6 + STORE_ARITY as u32).map(Id::from_raw).collect(),
                    ),
                    args: (6..6 + STORE_ARITY as u32).collect(),
                    class: 1 + LOAD_STATE as u32,
                    row: None,
                },
                Atom::Object {
                    key: 6 + ADDRESS as u32,
                    base: 5,
                    offset: Access::WRITTEN_OFFSET,
                },
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 6 + BYTES as u32,
                    value: Access::WRITTEN_BYTES_CONST,
                },
                Atom::Fact {
                    column: ColumnId::Type,
                    key: 6 + STORE_VALUE as u32,
                    value: Access::VALUE_TY,
                },
            ],
            guards: vec![
                Guard::Read {
                    term: Source::Label(Access::BYTES_CONST),
                    field: field::INT_SIGNED,
                    out: Access::BYTES,
                },
                Guard::Read {
                    term: Source::Label(Access::WRITTEN_BYTES_CONST),
                    field: field::INT_SIGNED,
                    out: Access::WRITTEN_BYTES,
                },
                Guard::Cmp(
                    Cmp::Eq,
                    Expr::Scalar(Access::OFFSET),
                    Expr::Scalar(Access::WRITTEN_OFFSET),
                ),
                Guard::Cmp(
                    Cmp::Eq,
                    Expr::Scalar(Access::BYTES),
                    Expr::Scalar(Access::WRITTEN_BYTES),
                ),
                // The vocabulary is bit-level, so a byte count alone would
                // forward the float a slot was written with into the integer a
                // reader spells it as.
                Guard::Read {
                    term: Source::Row(Access::LOAD_ROW),
                    field: field::TY,
                    out: Access::LOAD_TY,
                },
                Guard::Cmp(
                    Cmp::Eq,
                    Expr::Scalar(Access::LOAD_TY),
                    Expr::Scalar(Access::VALUE_TY),
                ),
            ],
            nots: Vec::new(),
        }),
        head: vec![HeadOp::Union(0, 6 + STORE_VALUE as u32)],
    }
}

/// The scalar slots [`forward_load`] names.
struct Access;

impl Access {
    const LOAD_ROW: u32 = 0;
    const OFFSET: u32 = 1;
    const BYTES_CONST: u32 = 2;
    const BYTES: u32 = 3;
    const WRITTEN_OFFSET: u32 = 4;
    const WRITTEN_BYTES_CONST: u32 = 5;
    const WRITTEN_BYTES: u32 = 6;
    const LOAD_TY: u32 = 7;
    const VALUE_TY: u32 = 8;
}

/// Where a rule's next variable and next scalar come from, so a law of twenty
/// negated conjunctions can name what it needs without counting slots by hand.
#[derive(Default)]
struct Slots {
    vars: u32,
    scalars: u32,
}

impl Slots {
    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    fn vars(&mut self, count: usize) -> Vec<u32> {
        (0..count).map(|_| self.var()).collect()
    }

    fn scalar(&mut self) -> u32 {
        self.scalars += 1;
        self.scalars - 1
    }
}

/// The tag the pass puts on a state something outside the term graph observes.
pub(crate) const EXPORTED: u64 = 1;

/// What an access on a state has to name for the two memories a law would merge
/// to be tellable apart.
#[derive(Clone, Copy)]
enum Tells {
    /// Any byte in common — what a read of the memory the dead store left can
    /// notice.
    Overlaps,
    /// Exactly the bytes a store covers — what forwarding answers, and so the
    /// one reading the union would change on the state the dead store was
    /// handed.
    Same,
}

/// An access of `kind` reached through the state class it names.
fn access(arity: usize, state_slot: usize, on: u32, slots: &mut Slots) -> (u32, Vec<u32>) {
    let class = slots.var();
    let mut operands = slots.vars(arity);
    operands[state_slot] = on;
    (class, operands)
}

fn access_atom(kind: SymKind, class: u32, operands: &[u32]) -> Atom<Node> {
    Atom::Node {
        template: Node::sym_pattern(
            kind,
            operands.iter().map(|&var| Id::from_raw(var)).collect(),
        ),
        args: operands.iter().copied().collect(),
        class,
        row: None,
    }
}

fn read_signed(label: u32, out: u32) -> Guard {
    Guard::Read {
        term: Source::Label(label),
        field: field::INT_SIGNED,
        out,
    }
}

/// The four ways an access on `on` other than `spared` can tell apart the two
/// memories a law would merge: it cannot be placed, its byte count is not known,
/// it names another object, or it names bytes `tells` says it notices.
///
/// Each is a conjunction the law must have no solution of, and the four together
/// are the negation of one disjunction — which is what `observed` computed by
/// sweeping every class holding a memory operator.
#[allow(clippy::too_many_arguments)]
fn undisturbed(
    on: u32,
    spared: u32,
    object: u32,
    offset: u32,
    bytes: u32,
    tells: Tells,
    slots: &mut Slots,
) -> Vec<Nested<Node>> {
    let mut out = Vec::new();
    for (kind, arity, state_slot) in [
        (SymKind::LoadMemory, LOAD_ARITY, LOAD_STATE),
        (SymKind::StoreMemory, STORE_ARITY, STORE_STATE),
    ] {
        let elsewhere = |class: u32| Guard::Distinct(smallvec![(class, spared)]);

        let (class, operands) = access(arity, state_slot, on, slots);
        out.push(Nested {
            atoms: vec![
                access_atom(kind, class, &operands),
                Atom::Unplaceable {
                    key: operands[ADDRESS],
                },
            ],
            guards: vec![elsewhere(class)],
        });

        let (class, operands) = access(arity, state_slot, on, slots);
        out.push(Nested {
            atoms: vec![
                access_atom(kind, class, &operands),
                Atom::Unknown {
                    column: ColumnId::Const,
                    key: operands[BYTES],
                },
            ],
            guards: vec![elsewhere(class)],
        });

        let (class, operands) = access(arity, state_slot, on, slots);
        let other = slots.var();
        out.push(Nested {
            atoms: vec![
                access_atom(kind, class, &operands),
                Atom::Object {
                    key: operands[ADDRESS],
                    base: other,
                    offset: slots.scalar(),
                },
            ],
            guards: vec![
                elsewhere(class),
                Guard::Distinct(smallvec![(other, object)]),
            ],
        });

        let (class, operands) = access(arity, state_slot, on, slots);
        let (read_offset, read_label, read_bytes) =
            (slots.scalar(), slots.scalar(), slots.scalar());
        let noticed = match tells {
            Tells::Overlaps => vec![
                Guard::Cmp(
                    Cmp::Lt,
                    Expr::Scalar(read_offset),
                    Expr::Add(
                        Box::new(Expr::Scalar(offset)),
                        Box::new(Expr::Scalar(bytes)),
                    ),
                ),
                Guard::Cmp(
                    Cmp::Lt,
                    Expr::Scalar(offset),
                    Expr::Add(
                        Box::new(Expr::Scalar(read_offset)),
                        Box::new(Expr::Scalar(read_bytes)),
                    ),
                ),
            ],
            Tells::Same => vec![
                Guard::Cmp(Cmp::Eq, Expr::Scalar(read_offset), Expr::Scalar(offset)),
                Guard::Cmp(Cmp::Eq, Expr::Scalar(read_bytes), Expr::Scalar(bytes)),
            ],
        };
        let mut guards = vec![elsewhere(class), read_signed(read_label, read_bytes)];
        guards.extend(noticed);
        out.push(Nested {
            atoms: vec![
                access_atom(kind, class, &operands),
                Atom::Object {
                    key: operands[ADDRESS],
                    base: object,
                    offset: read_offset,
                },
                Atom::Fact {
                    column: ColumnId::Const,
                    key: operands[BYTES],
                    value: read_label,
                },
            ],
            guards,
        });
    }
    out
}

/// S2: a store the next one overwrites unobserved leaves the state it was handed.
pub(crate) fn eliminate_dead_store() -> tir_relational::Rule<Node> {
    let mut slots = Slots::default();
    let root = slots.var();
    let store = slots.vars(STORE_ARITY);
    let object = slots.var();
    let dead = slots.vars(STORE_ARITY);
    let state = store[STORE_STATE];
    let before = dead[STORE_STATE];

    let (offset, bytes_label, bytes) = (slots.scalar(), slots.scalar(), slots.scalar());
    let (dead_offset, dead_bytes_label, dead_bytes) =
        (slots.scalar(), slots.scalar(), slots.scalar());
    let tag = slots.scalar();

    let atoms = vec![
        access_atom(SymKind::StoreMemory, root, &store),
        Atom::Object {
            key: store[ADDRESS],
            base: object,
            offset,
        },
        Atom::Fact {
            column: ColumnId::Const,
            key: store[BYTES],
            value: bytes_label,
        },
        access_atom(SymKind::StoreMemory, state, &dead),
        Atom::Object {
            key: dead[ADDRESS],
            base: object,
            offset: dead_offset,
        },
        Atom::Fact {
            column: ColumnId::Const,
            key: dead[BYTES],
            value: dead_bytes_label,
        },
    ];
    let guards = vec![
        read_signed(bytes_label, bytes),
        read_signed(dead_bytes_label, dead_bytes),
        Guard::Cmp(Cmp::Eq, Expr::Scalar(offset), Expr::Scalar(dead_offset)),
        Guard::Cmp(Cmp::Eq, Expr::Scalar(bytes), Expr::Scalar(dead_bytes)),
        // Two stores agreeing on everything but the state they take are one node
        // apart from it, so the law leaves them alone: the term does not say the
        // state before already holds the value.
        Guard::Distinct(
            (0..STORE_STATE)
                .map(|slot| (dead[slot], store[slot]))
                .collect(),
        ),
    ];

    let mut nots = vec![
        // A state something outside the term graph observes is not ours to
        // rewrite, on either side of the merge.
        Nested {
            atoms: vec![Atom::Fact {
                column: ColumnId::Mark,
                key: state,
                value: tag,
            }],
            guards: Vec::new(),
        },
        Nested {
            atoms: vec![Atom::Fact {
                column: ColumnId::Mark,
                key: before,
                value: tag,
            }],
            guards: Vec::new(),
        },
        // A loop's carried port is one class read at more than one point of the
        // program — the head of an iteration, and where the loop was left. A
        // store node landing in it answers for both, so a read after the loop
        // would forward the value the body overwrote.
        Nested {
            atoms: vec![Atom::Holds {
                key: before,
                op: Node::sym_pattern(SymKind::Theta, Vec::new()).op_key(),
            }],
            guards: Vec::new(),
        },
    ];

    // A second write to the same extent on the state before becomes congruent to
    // the survivor one round later — the survivor names it, and the store that
    // left it names the state before *that* — and the chain folds back to where
    // it started: both writes gone, not one.
    let over = slots.vars(STORE_ARITY);
    let (over_offset, over_label, over_bytes) = (slots.scalar(), slots.scalar(), slots.scalar());
    nots.push(Nested {
        atoms: vec![
            access_atom(SymKind::StoreMemory, before, &over),
            Atom::Object {
                key: over[ADDRESS],
                base: object,
                offset: over_offset,
            },
            Atom::Fact {
                column: ColumnId::Const,
                key: over[BYTES],
                value: over_label,
            },
        ],
        guards: vec![
            read_signed(over_label, over_bytes),
            Guard::Cmp(Cmp::Eq, Expr::Scalar(over_offset), Expr::Scalar(offset)),
            Guard::Cmp(Cmp::Eq, Expr::Scalar(over_bytes), Expr::Scalar(bytes)),
        ],
    });

    // Nothing but the overwriting store may read the bytes the dead store left:
    // the union hands such a read the memory before that write.
    nots.extend(undisturbed(
        state,
        root,
        object,
        offset,
        bytes,
        Tells::Overlaps,
        &mut slots,
    ));
    // And nothing but the dead store may read those bytes on the state it was
    // handed: forwarding answers an access naming exactly the extent a store
    // left, so that is the reading the union would change.
    nots.extend(undisturbed(
        before,
        state,
        object,
        offset,
        bytes,
        Tells::Same,
        &mut slots,
    ));

    tir_relational::Rule {
        name: "dead-store".into(),
        plan: Plan::compile(Query {
            vars: slots.vars,
            scalars: slots.scalars,
            root,
            atoms,
            guards,
            nots,
        }),
        head: vec![HeadOp::Union(state, before)],
    }
}
