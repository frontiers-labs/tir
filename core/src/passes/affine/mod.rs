//! Loop scheduling: the arranger's first instance.
//!
//! Per counted nest: read the affine view, ask the arranger which
//! `(permutation, tiling)` the dependence vectors admit at least modelled cost,
//! and build that nest. Refusal is the default — a nest with one pair the view
//! could not decide, a bound decided inside the nest, a port that is neither the
//! counter nor a memory chain, all get the identity placement and are left
//! byte-identical.
//!
//! Full unroll of a short counted loop runs after the scheduling, on whatever
//! nest is left: it is a plain rewrite, not a placement, and folding is what
//! turns the copies into straight-line code.

mod lower;
mod schedule;
mod strip_mine;
mod unroll;

use crate::analysis::affine::{AffineView, nests_under};
use crate::func::FuncOp;
use crate::{
    AnalysisManager, Context, DataLayout, OperationRef, Pass, PassError, PassTarget, Rewriter,
};

pub use strip_mine::strip_mine;

#[derive(Default)]
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
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let line = line_bytes(context, op);
        for view in nests_under(context, op.op().id) {
            schedule_nest(context, rewriter, &view, line)?;
        }
        unroll::run(context, rewriter, op.op().id)
    }
}

fn schedule_nest(
    context: &Context,
    rewriter: &mut Rewriter,
    view: &AffineView,
    line: i64,
) -> Result<(), PassError> {
    let candidate = schedule::schedule(view, line);
    if candidate.is_identity() {
        return Ok(());
    }
    let Some(nest) = lower::Nest::read(context, view).filter(|nest| nest.admits(&candidate)) else {
        return Ok(());
    };
    lower::Lowering::new(context, nest, candidate).run(rewriter, view)
}

/// The cache line the cost model measures locality against.
fn line_bytes(context: &Context, op: &OperationRef) -> i64 {
    DataLayout::for_op(context, op.op().id)
        .and_then(|layout| layout.cache_line())
        .filter(|&bytes| bytes > 0)
        .map_or(schedule::DEFAULT_LINE_BYTES, i64::from)
}
