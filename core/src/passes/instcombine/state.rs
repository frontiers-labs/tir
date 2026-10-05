//! The laws of the state algebra the memory terms live in, and the placement
//! facts a memory rewrite reads off them. `docs/design/ir.md` §6.5 states which
//! laws hold, why they are definitional rather than proved, and why dead-store
//! elimination is the commit's rather than a law.
//!
//! An access is placed by its *extent* — the class its address is an offset
//! of, the byte offset from it, and the byte count. Pointer arithmetic by a
//! constant is an offset law, so the address reference is the extent: a
//! `ptradd` chain of any length lands on its base at the summed offset.

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

/// S1: a load whose state a matching store left reads that store's value. The
/// store is a node of the state class the load names, and the two extents are
/// one base, one offset and one byte count — the base through the shared
/// variable, the rest through the guards.
pub(crate) fn forward_load() -> tir_relational::Rule<Node> {
    // Variables: 0 the load, 1..4 its operands, 5 the base both address, 6..10
    // the store's operands.
    tir_relational::Rule {
        name: "store-to-load".into(),
        plan: Plan::compile(Query {
            vars: 11,
            scalars: 7,
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
                Atom::Offset {
                    key: 1 + ADDRESS as u32,
                    base: 5,
                    offset: Access::OFFSET,
                },
                Atom::Const {
                    key: 1 + BYTES as u32,
                    value: Access::BYTES,
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
                Atom::Offset {
                    key: 6 + ADDRESS as u32,
                    base: 5,
                    offset: Access::WRITTEN_OFFSET,
                },
                Atom::Const {
                    key: 6 + BYTES as u32,
                    value: Access::WRITTEN_BYTES,
                },
                Atom::Fact {
                    column: ColumnId::Type,
                    key: 6 + STORE_VALUE as u32,
                    value: Access::VALUE_TY,
                },
            ],
            guards: vec![
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
    const BYTES: u32 = 2;
    const WRITTEN_OFFSET: u32 = 3;
    const WRITTEN_BYTES: u32 = 4;
    const LOAD_TY: u32 = 5;
    const VALUE_TY: u32 = 6;
}
