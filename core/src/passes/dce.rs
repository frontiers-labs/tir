//! Dead code elimination shared by SSA functions and machine symbols: a
//! worklist over [`DefUse`] chains erases pure ops whose every virtual def
//! (SSA result or Def-role register attribute) is unused, retiring the erased
//! op's reads so newly dead producers are revisited without rescanning.
//!
//! In backend pipelines it must run before register allocation — a
//! physical-register write counts as a side effect, so nothing is eligible
//! after allocation.

use std::collections::HashMap;
use std::sync::Arc;

use crate::analysis::{DefUse, DominatorTree, RegRef, op_regs};
use crate::backend::SymbolOp;
use crate::{
    AnalysisManager, ConstantLike, Context, MemoryWrite, OpInstance, OperationRef, Pass, PassError,
    PassTarget, PreservedAnalyses, Rewriter, Terminator, builtin::FuncOp,
};

#[derive(Default)]
pub struct DeadCodeEliminationPass;

impl DeadCodeEliminationPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(DeadCodeEliminationPass, "dce");

impl Pass for DeadCodeEliminationPass {
    fn name(&self) -> &'static str {
        "dce"
    }

    // Anchors on both SSA functions and machine symbols; a target can name only
    // one op, so the match happens in `run`.
    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        if op.as_op::<FuncOp>().is_none() && op.as_op::<SymbolOp>().is_none() {
            return Ok(PreservedAnalyses::all());
        }

        let defuse = analyses.get::<DefUse>(context, op.op().id);
        // Live read counts, retired as dead readers are erased.
        let mut use_counts = defuse.use_counts();
        // LIFO over walk order visits consumers before their producers.
        let mut queue: Vec<_> = defuse.ops().to_vec();
        let mut erased = false;

        while let Some(op_id) = queue.pop() {
            if !context.has_operation(op_id) {
                continue;
            }
            let instance = context.get_op(op_id);
            if !is_erasable(&instance, &use_counts) {
                continue;
            }

            let block = instance.parent_block().map(|b| context.get_block(b));
            rewriter.erase_op(&OperationRef::new(instance.clone(), block, None))?;
            erased = true;

            for used in op_regs(&instance).uses {
                let RegRef::Virtual { id, .. } = used else {
                    continue;
                };
                if let Some(count) = use_counts.get_mut(&id) {
                    *count -= 1;
                    if *count == 0 {
                        queue.extend_from_slice(defuse.defs_of(id));
                    }
                }
            }
        }

        if !erased {
            return Ok(PreservedAnalyses::all());
        }
        // Only non-terminator ops without regions were erased, so the block
        // graph — and with it dominance — is intact.
        Ok(PreservedAnalyses::none().preserve::<DominatorTree>())
    }
}

/// A pure value-producing op whose every virtual def is unused. Nested regions,
/// a terminator, a memory write, or any physical-register write keep it; an op
/// with SSA results must additionally declare pure semantics, so effectful ops
/// like calls survive even when their result is unread.
fn is_erasable(instance: &Arc<OpInstance>, use_counts: &HashMap<u32, usize>) -> bool {
    if !instance.regions.is_empty()
        || instance.clone().as_interface::<dyn Terminator>().is_some()
        || instance.clone().as_interface::<dyn MemoryWrite>().is_some()
    {
        return false;
    }
    if !instance.results.is_empty() && !is_pure_value(instance) {
        return false;
    }

    let regs = op_regs(instance);
    if regs
        .defs
        .iter()
        .any(|r| matches!(r, RegRef::Physical { .. }))
    {
        return false;
    }

    let mut defines = false;
    for def in &regs.defs {
        if let RegRef::Virtual { id, .. } = def {
            defines = true;
            if use_counts.get(id).is_some_and(|&count| count > 0) {
                return false;
            }
        }
    }
    // Only a value-producing op is a DCE candidate; a def-less pure op is left alone.
    defines
}

fn is_pure_value(instance: &Arc<OpInstance>) -> bool {
    instance
        .clone()
        .as_interface::<dyn ConstantLike>()
        .is_some()
        || instance
            .clone()
            .as_dyn_op()
            .semantic_expr(&mut crate::sem::SemGraph::new())
            .is_some()
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, IRBuilder, Operation, PassManager,
        builtin::{IntegerType, ops},
    };

    use super::DeadCodeEliminationPass;

    #[test]
    fn erases_dead_ssa_chain_but_keeps_calls() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);

        let region = context.create_region();
        let arg = context.create_value(i32, None);
        let arg_id = arg.id();
        let block = context.create_block(vec![arg]);
        region.add_block(block.id());
        let func = ops::func(&context, "f", i32, Some(region.id())).build();

        let mut b = IRBuilder::new(block);
        // A dead chain: the constant feeds only the add, which feeds nothing.
        let c = b.insert(ops::constant(&context, 3, i32).build());
        let dead = b.insert(ops::addi(&context, arg_id, c.result(), i32).build());
        // An effectful op with an unread result must survive.
        let call = b.insert(
            crate::builtin::CallOpBuilder::new(&context)
                .args(vec![arg_id])
                .attr("callee", crate::attributes::AttributeValue::Str("g".into()))
                .result_type(i32)
                .build(),
        );
        b.insert(ops::r#return(&context, arg_id).build());

        let mut pm = PassManager::new();
        pm.add_pass(DeadCodeEliminationPass::new());
        pm.run(&context, context.get_op(func.id())).expect("dce");

        assert!(!context.has_operation(dead.id()));
        assert!(!context.has_operation(c.id()));
        assert!(context.has_operation(call.id()));
    }
}
