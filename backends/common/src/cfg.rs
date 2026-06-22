//! Target-independent CFG normalization run before instruction selection.
//!
//! A two-way `builtin.cond_br %c, ^t, ^f` is split into a single-target
//! `asm.condbr %c, ^t` (conditional, fall-through otherwise) followed by an
//! unconditional `builtin.br ^f`. The conditional half then participates in the
//! e-graph cover like any other value-producing op — its condition fuses with a
//! defining comparison — while the unconditional half lowers through the normal
//! branch path. Block arguments on either edge are not yet supported by codegen.

use tir::attributes::AttributeValue;
use tir::builtin::{BranchOpBuilder, CondBranchOp};
use tir::{Context, OperationRef, PassError, Rewriter};

use crate::ops::CondBranchOpBuilder;

/// Split `cond_br` into `asm.condbr` + `br`. An [`OpLowering`](crate::isel::OpLowering)
/// scheduled ahead of instruction selection.
pub fn split_cond_branch(
    context: &Context,
    op: &OperationRef,
    rewriter: &mut Rewriter,
) -> Result<bool, PassError> {
    let Some(cond_br) = op.as_op::<CondBranchOp>() else {
        return Ok(false);
    };

    if !cond_br.true_args().is_empty() || !cond_br.false_args().is_empty() {
        return Err(PassError::InvalidRuleSet(
            "block arguments on conditional branch edges are not supported by codegen yet"
                .to_string(),
        ));
    }

    let condbr = CondBranchOpBuilder::new(context)
        .condition(cond_br.condition())
        .attr("dest", AttributeValue::Block(cond_br.true_dest()))
        .build();
    rewriter.insert_op_before(op, &condbr)?;

    let fallthrough = BranchOpBuilder::new(context)
        .attr("dest", AttributeValue::Block(cond_br.false_dest()))
        .build();
    rewriter.replace_op(op, &fallthrough)?;
    Ok(true)
}
