//! Loop scheduling: the arranger's first instance.
//!
//! Per counted nest: read the affine view, ask the arranger which
//! `(permutation, tiling)` the dependence vectors admit at least modelled cost,
//! and build that nest. Refusal is the default — a nest with one pair the view
//! could not decide, a bound decided inside the nest, a port that is neither the
//! counter nor a memory chain, all get the identity placement and are left
//! byte-identical.
//!
//! The arranger ranks the schedules; the cheapest few are built, each in a
//! fork of the overlay, and the nest the model charges least once it stands
//! is the one kept ([`crate::variants`]). A tie keeps the earlier one, so the
//! arranger's order decides it.
//!
//! Full unroll of a short counted loop runs after the scheduling, on whatever
//! nest is left: it is a plain rewrite, not a placement, and folding is what
//! turns the copies into straight-line code.

mod lower;
mod recurrence;
mod schedule;
mod strip_mine;
mod unroll;

use crate::analysis::affine::{AffineView, nests_under};
use crate::func::FuncOp;
use crate::{
    AnalysisManager, Context, DataLayout, OpId, OperationRef, Pass, PassError, PassTarget, variants,
};

pub use strip_mine::strip_mine;

/// How many of the arranger's cheapest schedules are built and scored per
/// nest. The model's order says which; the built nests decide among them.
const VARIANTS: usize = 3;

#[derive(Clone, Default)]
pub struct AffineSchedulePass;

impl AffineSchedulePass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(AffineSchedulePass, "affine");

impl Pass for AffineSchedulePass {
    fn name(&self) -> &'static str {
        "affine"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<FuncOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let line = line_bytes(context, op);
        for view in nests_under(context, op.op().id) {
            schedule_nest(context, op.op().id, &view, line)?;
        }
        unroll::run(context, op.op().id)?;
        recurrence::run(context, op.op().id);
        Ok(())
    }
}

/// Build the cheapest schedules of `view`'s nest and keep the one that
/// costs least once built, or leave the nest alone where the identity is
/// what the model wants or nothing built beats it.
fn schedule_nest(
    context: &Context,
    root: OpId,
    view: &AffineView,
    line: i64,
) -> Result<(), PassError> {
    let candidates = schedule::schedule(view, line, VARIANTS);
    if candidates[0].is_identity() {
        return Ok(());
    }
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| !candidate.is_identity())
        .collect();
    variants::choose(
        context,
        &candidates,
        |fork, candidate| {
            let Some(nest) = lower::Nest::read(fork, view).filter(|nest| nest.admits(candidate))
            else {
                return Ok(false);
            };
            lower::Lowering::new(fork, nest, candidate.clone()).run(view)?;
            Ok(true)
        },
        |fork| {
            nests_under(fork, root).iter().fold(0i64, |sum, nest| {
                sum.saturating_add(schedule::standing_cost(nest, line))
            })
        },
    )
    .map(drop)
}

/// The cache line the cost model measures locality against.
fn line_bytes(context: &Context, op: &OperationRef) -> i64 {
    DataLayout::for_op(context, op.op().id)
        .and_then(|layout| layout.cache_line())
        .filter(|&bytes| bytes > 0)
        .map_or(schedule::DEFAULT_LINE_BYTES, i64::from)
}
