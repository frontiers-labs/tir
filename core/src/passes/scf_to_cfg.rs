use std::collections::HashMap;
use std::sync::Arc;

use crate::analysis::{AnalysisManager, PreservedAnalyses};
use crate::builtin::{FuncOp, IntegerType, ReturnOp, TokenType, ops as b};
use crate::scf;
use crate::{
    Block, Context, IRBuilder, OperationRef, Pass, PassError, PassTarget, RegionId, Rewriter,
    Value, ValueId,
};

#[derive(Clone, Copy)]
struct LoopTargets {
    break_dest: crate::BlockId,
    continue_dest: crate::BlockId,
}

pub struct ScfToCfgPass;

impl ScfToCfgPass {
    pub fn new() -> Self {
        Self
    }

    fn next_scf_op(
        context: &Context,
        region: RegionId,
    ) -> Option<(Arc<Block>, Arc<crate::OpInstance>)> {
        for block in context.get_region(region).iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                if op.is::<scf::ForOp>() || op.is::<scf::WhileOp>() || op.is::<scf::IfOp>() {
                    return Some((block, op));
                }
            }
        }
        None
    }

    fn split_after(
        context: &Context,
        block: &Arc<Block>,
        op: &Arc<crate::OpInstance>,
    ) -> Arc<Block> {
        let result_args = op
            .results
            .iter()
            .map(|result| context.create_value(context.get_value(*result).ty(), None))
            .collect();
        let continuation = context.create_block(result_args);
        let op_ids = block.op_ids();
        let position = op_ids.iter().position(|id| *id == op.id).unwrap();
        for op_id in op_ids.into_iter().skip(position + 1) {
            block.remove_op(op_id);
            continuation.insert(continuation.len(), op_id);
        }
        for (result, argument) in op.results.iter().zip(continuation.arguments()) {
            context.replace_value_uses(*result, argument.id());
        }
        continuation
    }

    fn move_body(context: &Context, region: RegionId) -> Arc<Block> {
        context
            .get_region(region)
            .iter(context.clone())
            .next()
            .unwrap()
    }

    fn move_loop_body(context: &Context, region: RegionId) -> (Arc<Block>, Option<ValueId>) {
        let source = Self::move_body(context, region);
        let token = TokenType::new(context);
        let scope = source
            .arguments()
            .iter()
            .find(|argument| argument.ty() == token)
            .map(Value::id);
        let arguments = source
            .arguments()
            .iter()
            .filter(|argument| argument.ty() != token)
            .map(|argument| {
                let replacement = context.create_value(argument.ty(), None);
                context.replace_value_uses(argument.id(), replacement.id());
                replacement
            })
            .collect();
        let body = context.create_block(arguments);
        for op_id in source.op_ids() {
            source.remove_op(op_id);
            body.insert(body.len(), op_id);
        }
        (body, scope)
    }

    fn replace_terminator_with_branch(
        context: &Context,
        rewriter: &mut Rewriter,
        block: &Arc<Block>,
        destination: crate::BlockId,
        loop_targets: &HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        let terminator_id = *block.op_ids().last().unwrap();
        let terminator = context.get_op(terminator_id);
        let (operands, destination) = if terminator.is::<scf::YieldOp>() {
            (terminator.operands.clone(), destination)
        } else if terminator.is::<scf::BreakOp>() {
            let target = loop_targets.get(&terminator.operands[0]).ok_or_else(|| {
                PassError::InvalidRuleSet("scf.break has no enclosing loop".to_string())
            })?;
            (vec![], target.break_dest)
        } else if terminator.is::<scf::ContinueOp>() {
            let target = loop_targets.get(&terminator.operands[0]).ok_or_else(|| {
                PassError::InvalidRuleSet("scf.continue has no enclosing loop".to_string())
            })?;
            (vec![], target.continue_dest)
        } else if terminator.clone().as_op::<ReturnOp>().is_some() {
            return Ok(());
        } else {
            return Err(PassError::InvalidRuleSet(
                "SCF region has an unsupported exit".to_string(),
            ));
        };
        let branch = b::br(context, operands, destination).build();
        rewriter.replace_op(
            &OperationRef::new(terminator, Some(block.clone()), None),
            &branch,
        )
    }

    fn erase(
        rewriter: &mut Rewriter,
        block: &Arc<Block>,
        op: Arc<crate::OpInstance>,
    ) -> Result<(), PassError> {
        rewriter.erase_op(&OperationRef::new(op, Some(block.clone()), None))
    }

    fn lower_while(
        context: &Context,
        rewriter: &mut Rewriter,
        function_region: RegionId,
        block: Arc<Block>,
        op: Arc<crate::OpInstance>,
        loop_targets: &mut HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        let init = op.operands.first().copied();
        let condition = Self::move_body(context, op.regions[0]);
        let (body, scope) = Self::move_loop_body(context, op.regions[1]);
        let continuation = Self::split_after(context, &block, &op);
        let region = context.get_region(function_region);
        region.add_block(condition.id());
        region.add_block(body.id());
        region.add_block(continuation.id());
        if let Some(scope) = scope {
            loop_targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: condition.id(),
                },
            );
        }

        Self::erase(rewriter, &block, op)?;
        IRBuilder::new(block)
            .insert(b::br(context, init.into_iter().collect(), condition.id()).build());
        let terminator = context.get_op(*condition.op_ids().last().unwrap());
        if !terminator.is::<scf::ConditionOp>() {
            return Err(PassError::InvalidRuleSet(
                "SCF while condition must end with scf.condition".to_string(),
            ));
        }
        let branch_condition = terminator.operands[0];
        let forwarded = terminator.operands[1..].to_vec();
        let branch = b::cond_br(
            context,
            branch_condition,
            forwarded.clone(),
            forwarded,
            body.id(),
            continuation.id(),
        )
        .build();
        rewriter.replace_op(
            &OperationRef::new(terminator, Some(condition.clone()), None),
            &branch,
        )?;
        Self::replace_terminator_with_branch(context, rewriter, &body, condition.id(), loop_targets)
    }

    fn lower_for(
        context: &Context,
        rewriter: &mut Rewriter,
        function_region: RegionId,
        block: Arc<Block>,
        op: Arc<crate::OpInstance>,
        loop_targets: &mut HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        let lower = op.operands[0];
        let upper = op.operands[1];
        let step = op.operands[2];
        let init = op.operands.get(3).copied();
        let (body, scope) = Self::move_loop_body(context, op.regions[0]);
        let index_type = context.get_value(lower).ty();
        let mut header_values = vec![context.create_value(index_type, None)];
        header_values.extend(
            body.arguments()
                .iter()
                .map(|argument| context.create_value(argument.ty(), None)),
        );
        let header = context.create_block(header_values);
        let latch_values = body
            .arguments()
            .iter()
            .map(|argument| context.create_value(argument.ty(), None))
            .collect();
        let latch = context.create_block(latch_values);
        let continuation = Self::split_after(context, &block, &op);
        let region = context.get_region(function_region);
        region.add_block(header.id());
        region.add_block(body.id());
        region.add_block(latch.id());
        region.add_block(continuation.id());
        if let Some(scope) = scope {
            loop_targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: latch.id(),
                },
            );
        }

        Self::erase(rewriter, &block, op)?;
        let mut entry_args = vec![lower];
        entry_args.extend(init);
        IRBuilder::new(block).insert(b::br(context, entry_args, header.id()).build());

        let induction = header.arguments()[0].id();
        let carried = header.arguments()[1..]
            .iter()
            .map(Value::id)
            .collect::<Vec<_>>();
        let mut header_builder = IRBuilder::new(header.clone());
        let comparison = header_builder
            .insert(
                b::CmpIOpBuilder::new(context)
                    .lhs(induction)
                    .rhs(upper)
                    .predicate("slt")
                    .result_type(IntegerType::new(context, 1))
                    .build(),
            )
            .result();
        header_builder.insert(
            b::cond_br(
                context,
                comparison,
                carried.clone(),
                carried,
                body.id(),
                continuation.id(),
            )
            .build(),
        );
        Self::replace_terminator_with_branch(context, rewriter, &body, latch.id(), loop_targets)?;

        let mut latch_builder = IRBuilder::new(latch.clone());
        let next = latch_builder
            .insert(b::addi(context, induction, step, index_type).build())
            .result();
        let mut backedge = vec![next];
        backedge.extend(latch.arguments().iter().map(Value::id));
        latch_builder.insert(b::br(context, backedge, header.id()).build());
        Ok(())
    }

    fn lower_if(
        context: &Context,
        rewriter: &mut Rewriter,
        function_region: RegionId,
        block: Arc<Block>,
        op: Arc<crate::OpInstance>,
        loop_targets: &HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        let condition = op.operands[0];
        let then_block = Self::move_body(context, op.regions[0]);
        let else_block = Self::move_body(context, op.regions[1]);
        let continuation = Self::split_after(context, &block, &op);
        let region = context.get_region(function_region);
        region.add_block(then_block.id());
        region.add_block(else_block.id());
        region.add_block(continuation.id());

        Self::erase(rewriter, &block, op)?;
        IRBuilder::new(block).insert(
            b::cond_br(
                context,
                condition,
                vec![],
                vec![],
                then_block.id(),
                else_block.id(),
            )
            .build(),
        );
        Self::replace_terminator_with_branch(
            context,
            rewriter,
            &then_block,
            continuation.id(),
            loop_targets,
        )?;
        Self::replace_terminator_with_branch(
            context,
            rewriter,
            &else_block,
            continuation.id(),
            loop_targets,
        )
    }

    fn lower_one(
        context: &Context,
        rewriter: &mut Rewriter,
        function_region: RegionId,
        block: Arc<Block>,
        op: Arc<crate::OpInstance>,
        loop_targets: &mut HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        if op.clone().as_op::<scf::WhileOp>().is_some() {
            Self::lower_while(context, rewriter, function_region, block, op, loop_targets)
        } else if op.clone().as_op::<scf::ForOp>().is_some() {
            Self::lower_for(context, rewriter, function_region, block, op, loop_targets)
        } else {
            Self::lower_if(context, rewriter, function_region, block, op, loop_targets)
        }
    }
}

impl Default for ScfToCfgPass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(ScfToCfgPass, "scf-to-cfg");

impl Pass for ScfToCfgPass {
    fn name(&self) -> &'static str {
        "scf-to-cfg"
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
    ) -> Result<PreservedAnalyses, PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(PreservedAnalyses::all());
        }
        let region = op.op().regions[0];
        let mut loop_targets = HashMap::new();
        let mut changed = false;
        while let Some((block, structured)) = Self::next_scf_op(context, region) {
            Self::lower_one(
                context,
                rewriter,
                region,
                block,
                structured,
                &mut loop_targets,
            )?;
            changed = true;
        }
        Ok(if changed {
            PreservedAnalyses::none()
        } else {
            PreservedAnalyses::all()
        })
    }
}
