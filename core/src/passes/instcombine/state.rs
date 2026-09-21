//! The laws of the state algebra the memory terms live in, and the placement
//! facts a memory rewrite reads off them. `docs/design/ir.md` §6.5 states which
//! laws hold, why they are definitional rather than proved, and why dead-store
//! elimination is the commit's rather than a law.
//!
//! An access is placed by its *extent* — the object its address is derived from,
//! the byte offset into it, and the byte count — which is what
//! [`pointer_derivation`] answers.

use smallvec::smallvec;
use tir_relational::ClassId as Id;
use tir_relational::{Atom, Cmp, ColumnId, Expr, Guard, HeadOp, Plan, Query, Source};

use crate::sem::{SemNode as Node, SymKind, node::field};

/// `Load(address, bytes, metadata, state)`.
pub(super) const LOAD_ARITY: usize = 4;
const LOAD_STATE: usize = 3;
/// `Store(address, bytes, value, address_space, state)`.
pub(super) const STORE_ARITY: usize = 5;
const STORE_VALUE: usize = 2;
pub(super) const ADDRESS: usize = 0;
pub(super) const BYTES: usize = 1;

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
        head_vars: 0,
        post_saturation: false,
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
        head_vars: 0,
        post_saturation: false,
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
