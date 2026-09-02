//! Lowering of the no-alias fact.
//!
//! `ptr.disjoint` is the mid-end's spelling of "these two ranges share no
//! byte": the analyses read the fact off the op by name, and no machine has an
//! instruction for it. At the backend boundary it becomes the unsigned range
//! check it stands for — `lhs + lhs_size <= rhs || rhs + rhs_size <= lhs` —
//! for the same reason the state chains are dropped there.

use crate::Operation;
use crate::analysis::AnalysisManager;
use crate::builtin::{IntegerType, ops as b};
use crate::func::FuncOp;
use crate::ptr::{CmpOpBuilder, DisjointOp, ops as p};
use crate::{Context, OperationRef, Pass, PassError, PassTarget, Rewriter, ValueId};

#[derive(Default)]
pub struct LowerPtrDisjointPass;

impl LowerPtrDisjointPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(LowerPtrDisjointPass, "lower-ptr-disjoint");

impl Pass for LowerPtrDisjointPass {
    fn name(&self) -> &'static str {
        "lower-ptr-disjoint"
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
        for operation in facts(context, op) {
            let fact = operation
                .as_op::<DisjointOp>()
                .expect("operation was collected as ptr.disjoint");
            let [lhs, lhs_size, rhs, rhs_size] = fact.operands()[..] else {
                unreachable!("the verifier fixes ptr.disjoint's arity");
            };
            let before = precedes(context, rewriter, &operation, lhs, lhs_size, rhs)?;
            let after = precedes(context, rewriter, &operation, rhs, rhs_size, lhs)?;
            let disjoint = b::ori(context, before, after, IntegerType::new(context, 1)).build();
            rewriter.replace_op(&operation, &disjoint)?;
        }
        Ok(())
    }
}

/// `[start, start + size)` ends at or before `other` starts, addresses being
/// unsigned.
fn precedes(
    context: &Context,
    rewriter: &mut Rewriter,
    before: &OperationRef,
    start: ValueId,
    size: ValueId,
    other: ValueId,
) -> Result<ValueId, PassError> {
    let end = p::ptradd(context, start, size, context.get_value(start).ty()).build();
    rewriter.insert_op_before(before, &end)?;
    let compare = CmpOpBuilder::new(context)
        .lhs(end.result())
        .rhs(other)
        .predicate("ule")
        .result_type(IntegerType::new(context, 1))
        .build();
    rewriter.insert_op_before(before, &compare)?;
    Ok(compare.result())
}

/// Every `ptr.disjoint` under `op`, outermost region first.
fn facts(context: &Context, op: &OperationRef) -> Vec<OperationRef> {
    let mut found = Vec::new();
    for region in crate::passes::regions_under(context, op.op().id) {
        for block in context.get_region(region).iter(context.clone()) {
            for id in block.op_ids() {
                let instance = context.get_op(id);
                if instance.is::<DisjointOp>() {
                    found.push(OperationRef::new(instance));
                }
            }
        }
    }
    found
}
