//! The affine view: a nest's iteration space, its accesses and their dependence.
//!
//! Built on demand over a maximal counted nest and thrown away again — nothing
//! is interned, no operation or value is created, and no registry holds it. What
//! it reads is what the IR already states: `CountedLoop` for the bounds,
//! `LoopLike` for the carried ports the counters ride on, the dependency edges for
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
    AddIOp, AndIOp, ExtSIOp, IntegerType, MulIOp, OrIOp, ShlIOp, SubIOp, TruncIOp, XOrIOp,
};
use crate::ptr::PtrAddOp;
use crate::state::{JoinOp, SplitOp};
use crate::{
    BlockId, Conditional, ConstantLike, Context, CountedLoop, DataLayout, Gamma, LoopLike,
    MemoryRead, MemoryWrite, OpHandle, OpId, Theta, TypeId, ValueId, scf,
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
    /// Further carried ports that count the same: entered on the lower bound
    /// and stepped by the step, so the body reads the counter through them too.
    pub counter_aliases: Vec<ValueId>,
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
        if op.has_interface::<dyn CountedLoop>()
            && !covered.contains(&op_id)
            && let Some(view) = AffineView::build(context, op_id)
        {
            covered.extend(view.loop_ops());
            views.push(view);
        }
        for region in op.regions().to_vec() {
            pending.extend(context.get_region(region).op_ids());
        }
    }
    views.sort_by_key(|view| view.root.index());
    views
}

/// The counted nest rooted at `root`: `root` itself, then every loop that is the
/// whole of the body above it, innermost last.
pub fn counted_nest(context: &Context, root: OpId) -> Option<Vec<OpId>> {
    if !context.get_op(root).has_interface::<dyn CountedLoop>() {
        return None;
    }
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
    let mut inner = None;
    for op_id in body_ops(context, outer)? {
        let op = context.get_op(op_id);
        if !op.regions().is_empty() {
            if inner.is_some() || !op.has_interface::<dyn CountedLoop>() {
                return None;
            }
            inner = Some(op_id);
        } else if !(op.is::<JoinOp>() || crate::passes::is_pure_value(&op)) {
            return None;
        }
    }
    inner
}

/// The single block of a loop's ordered body region.
pub(crate) fn body_block(context: &Context, op: OpId) -> Option<BlockId> {
    let region = *context.get_op(op).regions().last()?;
    match context.get_region(region).block_ids()[..] {
        [block] => Some(block),
        _ => None,
    }
}

/// The operations a loop's body computes, whichever kind of region holds it:
/// an unordered body's operations, or an ordered single block's short of its
/// terminator.
pub(crate) fn body_ops(context: &Context, op: OpId) -> Option<Vec<OpId>> {
    let region = context.get_region(*context.get_op(op).regions().last()?);
    if region.is_nodes() {
        return Some(region.op_ids());
    }
    let block = context.get_block(body_block(context, op)?);
    let mut ops = block.op_ids();
    ops.pop()?;
    Some(ops)
}

/// The carried value ports of a loop, however it declares them: the port the
/// body reads each on, the value the next iteration takes, the value the loop
/// is entered on, and the result it produces.
pub(crate) struct Carried {
    pub args: Vec<ValueId>,
    pub latched: Vec<ValueId>,
    pub inits: Vec<ValueId>,
    pub finals: Vec<ValueId>,
}

pub(crate) fn carried(context: &Context, op: &OpHandle) -> Option<Carried> {
    if let Some(theta) = op.clone().as_interface::<dyn Theta>() {
        let binding = theta.carried();
        let region = context.get_region(theta.body());
        let results = region.value_results();
        return Some(Carried {
            args: region.value_arguments()[binding.ports]
                .iter()
                .map(crate::Value::id)
                .collect(),
            latched: results[binding.continue_].to_vec(),
            inits: op.value_operands()[binding.operands].to_vec(),
            finals: op.value_results()[binding.results].to_vec(),
        });
    }
    let carried = op.clone().as_interface::<dyn LoopLike>()?;
    Some(Carried {
        args: carried.carried_args(),
        latched: carried.latched(),
        inits: carried.inits(),
        finals: carried.finals(),
    })
}
