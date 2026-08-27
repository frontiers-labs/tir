//! The affine view: a nest's iteration space, its accesses and their dependence.
//!
//! Built on demand over a maximal counted nest and thrown away again — nothing
//! is interned, no operation or value is created, and no registry holds it. What
//! it reads is what the IR already states: `CountedLoop` for the bounds,
//! `LoopLike` for the carried ports the counters ride on, the `!state` edges for
//! which memory each access is of, and `ptradd` arithmetic for where in that
//! memory it lands.
//!
//! Every reading is refusable on its own. An access whose subscript is not
//! affine is `NonAffine` and the accesses beside it are not; a pair the single
//! equation cannot decide is `Unknown` and the pairs beside it are not. What a
//! consumer may do with a nest is decided from the pairs, never from the view
//! having been built.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::builtin::{
    AddIOp, AndIOp, ExtSIOp, IntegerType, MulIOp, OrIOp, ShlIOp, StateType, SubIOp, TruncIOp,
    XOrIOp,
};
use crate::ptr::PtrAddOp;
use crate::state::{JoinOp, SplitOp};
use crate::{
    BlockId, Conditional, ConstantLike, Context, CountedLoop, DataLayout, LoopLike, MemoryRead,
    MemoryWrite, OpHandle, OpId, TypeId, ValueId, scf,
};

pub(crate) mod build;
mod dependence;
mod form;
mod pairs;
mod print;

pub(crate) use build::counter_port;
pub use dependence::{Component, Sign, distances};
pub use form::AffineForm;

/// How deep a nest the view reads. The dependence test enumerates `3^depth`
/// direction vectors per pair, and no C loop nest anyone writes reaches this.
const MAX_DEPTH: usize = 6;

/// One loop of the nest, outermost first.
pub struct Loop {
    pub op: OpId,
    /// The counter's first value.
    pub lower: AffineForm,
    /// The bound the counter is tested against.
    pub upper: AffineForm,
    /// What the counter gains each iteration.
    pub step: AffineForm,
    /// The width the counter is counted in.
    pub width: u32,
    /// The carried port the body reads the counter through, where the loop has
    /// one. `CountedLoop` names no counter value, so a nest whose body never
    /// looks at its counter has none to name.
    pub counter: Option<ValueId>,
    /// How many iterations the loop runs, where the bounds spell a number.
    pub trip: Option<i128>,
}

/// Where in its object an access lands.
pub enum Offset {
    Affine(AffineForm),
    NonAffine,
}

/// One read or write in the nest's body.
pub struct Access {
    pub op: OpId,
    pub write: bool,
    /// The memory the access is of, named by the state its chain is rooted at.
    pub chain: ValueId,
    /// The object the address was derived from.
    pub base: ValueId,
    pub offset: Offset,
    /// The bytes the access covers.
    pub extent: u64,
    /// The access runs under a `scf.if`, so it may not run every iteration.
    pub guarded: bool,
    /// The subscript arithmetic can leave the region its source width holds, so
    /// the form describes the access only where it does not.
    pub wrapping: bool,
}

/// The bytes one side of a pair may touch, as forms over the nest's symbols.
pub struct Extremes {
    pub base: ValueId,
    pub low: AffineForm,
    pub high: AffineForm,
}

/// What one pair of accesses says about the order the nest may run in.
pub enum Dependence {
    /// No loop carries the pair: whatever one iteration's access meets is that
    /// same iteration's, so the loops may run in any order. What the body does
    /// within an iteration is not the view's to reorder.
    Independent,
    /// The distances the pair admits, per depth.
    Distances(Vec<Component>),
    /// The two are of different objects, so they are independent exactly when
    /// the byte ranges are disjoint.
    Conditional(Box<(Extremes, Extremes)>),
    Unknown,
}

pub struct Pair {
    pub left: usize,
    pub right: usize,
    pub dependence: Dependence,
}

/// The operators a carried port may accumulate under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reduction {
    Add,
    Mul,
    And,
    Or,
    Xor,
}

/// What a carried port does across iterations.
pub enum Recurrence {
    /// Gains a constant every iteration.
    Induction {
        init: ValueId,
        step: i128,
    },
    /// Accumulates the iteration's value under one operator.
    Reduction(Reduction),
    Other,
}

pub struct Port {
    pub arg: ValueId,
    pub recurrence: Recurrence,
}

/// A counted nest read as an iteration space.
pub struct AffineView {
    pub root: OpId,
    pub loops: Vec<Loop>,
    /// The values the nest was entered with that its forms name, in the order
    /// the build first met them.
    pub symbols: Vec<ValueId>,
    pub accesses: Vec<Access>,
    pub pairs: Vec<Pair>,
    /// The carried ports of the innermost loop.
    pub ports: Vec<Port>,
    /// The body holds something the view cannot describe — a call, a loop that
    /// does not count, an access with no chain to name. Every pair is `Unknown`
    /// while this holds.
    pub opaque: bool,
}

impl AffineView {
    /// Read the maximal counted nest rooted at `root`, or `None` where `root` is
    /// not a counted loop.
    pub fn build(context: &Context, root: OpId) -> Option<Self> {
        build::build(context, root)
    }

    pub fn depth(&self) -> usize {
        self.loops.len()
    }

    /// Whether every bound is decided before the nest runs, so the space is a
    /// box and its loops may be reordered without reshaping it.
    pub fn is_rectangular(&self) -> bool {
        self.loops
            .iter()
            .all(|l| l.lower.is_uniform() && l.upper.is_uniform() && l.step.is_uniform())
    }

    /// Every loop of the nest, outermost first.
    pub fn loop_ops(&self) -> Vec<OpId> {
        self.loops.iter().map(|l| l.op).collect()
    }
}

/// Every maximal counted nest under `root`, outermost first. A loop a view
/// already covers opens no nest of its own.
pub fn nests_under(context: &Context, root: OpId) -> Vec<AffineView> {
    let mut views: Vec<AffineView> = Vec::new();
    let mut covered = HashSet::new();
    let mut pending = vec![root];
    while let Some(op_id) = pending.pop() {
        let op = context.get_op(op_id);
        if op.clone().as_op::<scf::ForOp>().is_some()
            && !covered.contains(&op_id)
            && let Some(view) = AffineView::build(context, op_id)
        {
            covered.extend(view.loop_ops());
            views.push(view);
        }
        for region in op.regions().to_vec() {
            for block in context.get_region(region).block_ids() {
                pending.extend(context.get_block(block).op_ids());
            }
        }
    }
    views.sort_by_key(|view| view.root.index());
    views
}

/// The counted nest rooted at `root`: `root` itself, then every loop that is the
/// whole of the body above it, innermost last.
pub fn counted_nest(context: &Context, root: OpId) -> Option<Vec<OpId>> {
    context.get_op(root).as_op::<scf::ForOp>()?;
    let mut nest = vec![root];
    while nest.len() < MAX_DEPTH {
        let Some(inner) = counted_level(context, *nest.last().expect("a rooted nest")) else {
            break;
        };
        nest.push(inner);
    }
    Some(nest)
}

/// The loop a body holds where the body is nothing but that loop and the
/// bookkeeping around it: the constants its bounds are spelled with, the latch
/// that steps the counter, and the joins state threading left behind.
fn counted_level(context: &Context, outer: OpId) -> Option<OpId> {
    let block = context.get_block(body_block(context, outer)?);
    let ops = block.op_ids();
    let (&_terminator, rest) = ops.split_last()?;
    let mut inner = None;
    for &op_id in rest {
        let op = context.get_op(op_id);
        if !op.regions().is_empty() {
            if inner.is_some() || op.as_op::<scf::ForOp>().is_none() {
                return None;
            }
            inner = Some(op_id);
        } else if !(op.is::<JoinOp>() || crate::passes::is_pure_value(&op)) {
            return None;
        }
    }
    inner
}

/// The single block of a loop's body region.
pub(crate) fn body_block(context: &Context, op: OpId) -> Option<BlockId> {
    let region = *context.get_op(op).regions().last()?;
    match context.get_region(region).block_ids()[..] {
        [block] => Some(block),
        _ => None,
    }
}
