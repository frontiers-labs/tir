use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tir::analysis::{AnalysisManager, PreservedAnalyses};
use tir::attributes::AttributeValue;
use tir::builtin::{
    BranchOp, CondBranchOp, FuncOp, IntegerType, ModuleOp, ReturnOp, TokenType, ops as b,
};
use tir::ptr::{AllocaOp, PtrType, ops as p};
use tir::{
    Block, BlockId, Context, IRBuilder, Operand, Operation, OperationRef, Pass, PassError,
    PassTarget, RegionId, Rewriter, ValueId, scf,
};

use crate::cir;

#[derive(Clone)]
struct StructFieldLayout {
    ty: tir::TypeId,
    offset: u64,
}

#[derive(Clone)]
struct StructLayout {
    fields: Vec<StructFieldLayout>,
}

pub struct LowerCirStructsPass;

impl LowerCirStructsPass {
    pub fn new() -> Self {
        Self
    }

    fn descendants(context: &Context, root: &Arc<tir::OpInstance>) -> Vec<OperationRef> {
        fn visit(
            context: &Context,
            operation: &Arc<tir::OpInstance>,
            result: &mut Vec<OperationRef>,
        ) {
            for region in &operation.regions {
                for block in context.get_region(*region).iter(context.clone()) {
                    for operation in block.op_ids() {
                        let operation = context.get_op(operation);
                        result.push(OperationRef::new(
                            operation.clone(),
                            Some(block.clone()),
                            None,
                        ));
                        visit(context, &operation, result);
                    }
                }
            }
        }

        let mut result = Vec::new();
        visit(context, root, &mut result);
        result
    }

    fn refresh(context: &Context, operation: &OperationRef) -> OperationRef {
        OperationRef::new(
            context.get_op(operation.op().id),
            operation.block().cloned(),
            operation.position(),
        )
    }

    fn string_attribute(operation: &impl Operation, name: &str) -> String {
        operation
            .attributes()
            .iter()
            .find(|attribute| attribute.name == name)
            .and_then(|attribute| match &attribute.value {
                AttributeValue::Str(value) => Some(value.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn uint_attribute(operation: &impl Operation, name: &str) -> u64 {
        operation
            .attributes()
            .iter()
            .find(|attribute| attribute.name == name)
            .and_then(|attribute| match attribute.value {
                AttributeValue::UInt(value) => Some(value),
                _ => None,
            })
            .unwrap()
    }

    fn layouts(descendants: &[OperationRef]) -> HashMap<String, StructLayout> {
        descendants
            .iter()
            .filter_map(|operation| operation.as_op::<cir::DefineStructOp>())
            .map(|definition| {
                let name = Self::string_attribute(&definition, "sym_name");
                let fields = definition
                    .attributes()
                    .iter()
                    .find(|attribute| attribute.name == "fields")
                    .and_then(|attribute| match &attribute.value {
                        AttributeValue::Array(fields) => Some(fields),
                        _ => None,
                    })
                    .unwrap()
                    .iter()
                    .map(|field| {
                        let AttributeValue::Dict(field) = field else {
                            unreachable!();
                        };
                        let AttributeValue::Type(ty) = field["type"] else {
                            unreachable!();
                        };
                        let AttributeValue::UInt(offset) = field["offset"] else {
                            unreachable!();
                        };
                        StructFieldLayout { ty, offset }
                    })
                    .collect();
                (name, StructLayout { fields })
            })
            .collect()
    }

    fn offset_pointer(
        context: &Context,
        rewriter: &mut Rewriter,
        target: &OperationRef,
        base: ValueId,
        offset: u64,
        result_type: tir::TypeId,
    ) -> Result<ValueId, PassError> {
        let offset = b::constant(context, offset as i64, IntegerType::new(context, 64)).build();
        rewriter.insert_op_before(target, &offset)?;
        let pointer = p::ptradd(context, base, offset.result(), result_type).build();
        let result = pointer.result();
        rewriter.insert_op_before(target, &pointer)?;
        Ok(result)
    }

    fn insert_copy(
        context: &Context,
        rewriter: &mut Rewriter,
        target: &OperationRef,
        layouts: &HashMap<String, StructLayout>,
        name: &str,
        destination: ValueId,
        source: ValueId,
    ) -> Result<(), PassError> {
        for field in &layouts[name].fields {
            let pointer_type = PtrType::typed(context, field.ty);
            let destination = Self::offset_pointer(
                context,
                rewriter,
                target,
                destination,
                field.offset,
                pointer_type,
            )?;
            let source = Self::offset_pointer(
                context,
                rewriter,
                target,
                source,
                field.offset,
                pointer_type,
            )?;
            let field_type = context.get_type_data(field.ty);
            if let Some(structure) =
                (field_type.as_ref() as &dyn std::any::Any).downcast_ref::<cir::StructType>()
            {
                Self::insert_copy(
                    context,
                    rewriter,
                    target,
                    layouts,
                    structure.name(),
                    destination,
                    source,
                )?;
            } else {
                let load = p::load(context, source, field.ty).build();
                let value = load.result();
                rewriter.insert_op_before(target, &load)?;
                let store = p::store(context, value, destination).build();
                rewriter.insert_op_before(target, &store)?;
            }
        }
        Ok(())
    }
}

impl Default for LowerCirStructsPass {
    fn default() -> Self {
        Self::new()
    }
}

impl Pass for LowerCirStructsPass {
    fn name(&self) -> &'static str {
        "lower-cir-structs"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<ModuleOp>()
    }

    fn run(
        &mut self,
        operation: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError> {
        if operation.as_op::<ModuleOp>().is_none() {
            return Ok(PreservedAnalyses::all());
        }
        let descendants = Self::descendants(context, operation.op());
        let layouts = Self::layouts(&descendants);
        if layouts.is_empty() {
            return Ok(PreservedAnalyses::all());
        }

        for target in &descendants {
            let target = Self::refresh(context, target);
            let Some(member) = target.as_op::<cir::GetMemberOp>() else {
                continue;
            };
            let name = Self::string_attribute(&member, "struct_name");
            let field = Self::uint_attribute(&member, "field") as usize;
            let offset = layouts[&name].fields[field].offset;
            let result_type = context.get_value(member.result()).ty();
            let offset_value =
                b::constant(context, offset as i64, IntegerType::new(context, 64)).build();
            rewriter.insert_op_before(&target, &offset_value)?;
            let pointer = p::ptradd(
                context,
                member.operands()[0],
                offset_value.result(),
                result_type,
            )
            .build();
            rewriter.replace_op(&target, &pointer)?;
        }

        for target in &descendants {
            if target.as_op::<cir::CopyStructOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            let copy = target.as_op::<cir::CopyStructOp>().unwrap();
            Self::insert_copy(
                context,
                rewriter,
                &target,
                &layouts,
                &Self::string_attribute(&copy, "struct_name"),
                copy.operands()[0],
                copy.operands()[1],
            )?;
            rewriter.erase_op(&target)?;
        }

        for target in &descendants {
            if target.as_op::<AllocaOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            if let Some(allocation) = target.as_op::<AllocaOp>() {
                let result_type = context.get_value(allocation.result()).ty();
                let result_type = context.get_type_data(result_type);
                let Some(pointer) =
                    (result_type.as_ref() as &dyn std::any::Any).downcast_ref::<PtrType>()
                else {
                    continue;
                };
                let Some(pointee) = pointer.pointee(context) else {
                    continue;
                };
                let pointee = context.get_type_data(pointee);
                if (pointee.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<cir::StructType>()
                    .is_none()
                {
                    continue;
                }
                let replacement = p::alloca(
                    context,
                    allocation.size(),
                    allocation.align(),
                    PtrType::opaque(context),
                )
                .build();
                rewriter.replace_op(&target, &replacement)?;
            }
        }

        for target in &descendants {
            if target.as_op::<cir::DefineStructOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            if target.as_op::<cir::DefineStructOp>().is_some() {
                rewriter.erase_op(&target)?;
            }
        }
        Ok(PreservedAnalyses::none())
    }
}

#[derive(Clone, Copy)]
struct LoopTargets {
    break_dest: BlockId,
    continue_dest: BlockId,
}

pub struct LowerCirControlFlowPass;

impl LowerCirControlFlowPass {
    pub fn new() -> Self {
        Self
    }

    fn single_block(context: &Context, region: RegionId) -> Option<Arc<Block>> {
        let blocks = context
            .get_region(region)
            .iter(context.clone())
            .collect::<Vec<_>>();
        (blocks.len() == 1).then(|| blocks[0].clone())
    }

    fn entry_block(context: &Context, region: RegionId) -> Arc<Block> {
        context
            .get_region(region)
            .iter(context.clone())
            .next()
            .unwrap()
    }

    fn marker_label(operation: &impl Operation) -> String {
        operation
            .attributes()
            .iter()
            .find(|attribute| attribute.name == "label")
            .and_then(|attribute| match &attribute.value {
                AttributeValue::Str(label) => Some(label.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn region_has_goto(context: &Context, region: RegionId) -> bool {
        context
            .get_region(region)
            .iter(context.clone())
            .any(|block| {
                block.op_ids().into_iter().any(|op_id| {
                    let op = context.get_op(op_id);
                    op.clone().as_op::<cir::GotoOp>().is_some()
                        || op.clone().as_op::<cir::LabelOp>().is_some()
                        || op
                            .regions
                            .iter()
                            .any(|region| Self::region_has_goto(context, *region))
                })
            })
    }

    fn resolve_gotos(
        context: &Context,
        rewriter: &mut Rewriter,
        function_region: RegionId,
    ) -> Result<bool, PassError> {
        let region = context.get_region(function_region);
        let labels = region
            .iter(context.clone())
            .flat_map(|block| block.op_ids())
            .filter(|op_id| context.get_op(*op_id).as_op::<cir::LabelOp>().is_some())
            .collect::<Vec<_>>();
        let mut destinations = HashMap::new();
        for label_id in labels {
            let label = context.get_op(label_id);
            let name = Self::marker_label(&label.clone().as_op::<cir::LabelOp>().unwrap());
            let block = context.get_block(label.parent_block().unwrap());
            let op_ids = block.op_ids();
            let position = op_ids.iter().position(|id| *id == label_id).unwrap();
            let destination = context.create_block(vec![]);
            for op_id in op_ids.into_iter().skip(position + 1) {
                block.remove_op(op_id);
                destination.insert(destination.len(), op_id);
            }
            rewriter.erase_op(&OperationRef::new(label, Some(block.clone()), None))?;
            region.add_block(destination.id());
            let terminated = block.op_ids().last().is_some_and(|op_id| {
                context
                    .get_op(*op_id)
                    .as_interface::<dyn tir::Terminator>()
                    .is_some()
            });
            if !terminated {
                IRBuilder::new(block).insert(b::br(context, vec![], destination.id()).build());
            }
            destinations.insert(name, destination.id());
        }

        let gotos = region
            .iter(context.clone())
            .flat_map(|block| block.op_ids())
            .filter(|op_id| context.get_op(*op_id).as_op::<cir::GotoOp>().is_some())
            .collect::<Vec<_>>();
        let changed = !destinations.is_empty() || !gotos.is_empty();
        for goto_id in gotos {
            let goto = context.get_op(goto_id);
            let destination =
                destinations[&Self::marker_label(&goto.clone().as_op::<cir::GotoOp>().unwrap())];
            let block = context.get_block(goto.parent_block().unwrap());
            let op_ids = block.op_ids();
            let position = op_ids.iter().position(|id| *id == goto_id).unwrap();
            if position + 1 < op_ids.len() {
                let continuation = context.create_block(vec![]);
                for op_id in op_ids.into_iter().skip(position + 1) {
                    block.remove_op(op_id);
                    continuation.insert(continuation.len(), op_id);
                }
                region.add_block(continuation.id());
            }
            let branch = b::br(context, vec![], destination).build();
            rewriter.replace_op(&OperationRef::new(goto, Some(block), None), &branch)?;
        }
        Ok(changed)
    }

    fn remove_unreachable_blocks(context: &Context, function_region: RegionId) -> bool {
        let region = context.get_region(function_region);
        let blocks = region.iter(context.clone()).collect::<Vec<_>>();
        let Some(entry) = blocks.first() else {
            return false;
        };
        let mut reachable = HashSet::new();
        let mut pending = vec![entry.id()];
        while let Some(block_id) = pending.pop() {
            if !reachable.insert(block_id) {
                continue;
            }
            let block = context.get_block(block_id);
            let Some(terminator) = block.op_ids().last().map(|id| context.get_op(*id)) else {
                continue;
            };
            if let Some(branch) = terminator.clone().as_op::<BranchOp>() {
                pending.push(branch.dest());
            } else if let Some(branch) = terminator.clone().as_op::<CondBranchOp>() {
                pending.push(branch.true_dest());
                pending.push(branch.false_dest());
            }
        }
        let mut changed = false;
        for block in blocks {
            if !reachable.contains(&block.id()) {
                changed |= region.remove_block(block.id());
            }
        }
        changed
    }

    fn condition_operand(context: &Context, region: RegionId) -> Option<ValueId> {
        let block = Self::single_block(context, region)?;
        let op_ids = block.op_ids();
        let last = *op_ids.last()?;
        context
            .get_op(last)
            .as_op::<cir::ConditionOp>()
            .map(|condition| condition.operands()[0])
    }

    fn body_is_structured(context: &Context, region: RegionId) -> bool {
        let Some(block) = Self::single_block(context, region) else {
            return false;
        };
        let op_ids = block.op_ids();
        let Some(last) = op_ids.last() else {
            return false;
        };
        let terminator = context.get_op(*last);
        if terminator.clone().as_op::<cir::YieldOp>().is_none()
            && terminator.clone().as_op::<cir::BreakOp>().is_none()
            && terminator.clone().as_op::<cir::ContinueOp>().is_none()
        {
            return false;
        }
        op_ids[..op_ids.len() - 1].iter().all(|op_id| {
            let op = context.get_op(*op_id);
            if op.is::<cir::IfOp>() {
                return op
                    .regions
                    .iter()
                    .all(|region| Self::body_is_structured(context, *region));
            }
            if op.is::<cir::WhileOp>() {
                return Self::while_is_structured(context, &op);
            }
            if op.is::<cir::ForOp>() {
                return Self::for_is_structured(context, &op);
            }
            op.regions.is_empty()
        })
    }

    fn while_is_structured(context: &Context, op: &tir::OpInstance) -> bool {
        Self::condition_operand(context, op.regions[0]).is_some()
            && Self::body_is_structured(context, op.regions[1])
    }

    fn for_is_structured(context: &Context, op: &tir::OpInstance) -> bool {
        Self::condition_operand(context, op.regions[0]).is_some()
            && Self::body_is_structured(context, op.regions[1])
            && Self::body_is_structured(context, op.regions[2])
            && !Self::loop_scope_is_used(context, op.regions[1])
    }

    fn loop_scope_is_used(context: &Context, body: RegionId) -> bool {
        let scope = Self::entry_block(context, body).arguments()[0].id();
        context.is_value_used(scope)
    }

    fn next_control_op_in_region(
        context: &Context,
        region: RegionId,
    ) -> Option<(Arc<Block>, Arc<tir::OpInstance>)> {
        for block in context.get_region(region).iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                if op.is::<cir::WhileOp>()
                    || op.is::<cir::ForOp>()
                    || op.is::<cir::DoOp>()
                    || op.is::<cir::IfOp>()
                {
                    return Some((block, op));
                }
                for child in &op.regions {
                    if let Some(found) = Self::next_control_op_in_region(context, *child) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }

    fn region_uses_direct_loop_target(
        context: &Context,
        region: RegionId,
        loop_targets: &HashMap<ValueId, LoopTargets>,
    ) -> bool {
        context
            .get_region(region)
            .iter(context.clone())
            .any(|block| {
                block.op_ids().into_iter().any(|op_id| {
                    let op = context.get_op(op_id);
                    let exits_direct_loop = (op.clone().as_op::<cir::BreakOp>().is_some()
                        || op.clone().as_op::<cir::ContinueOp>().is_some())
                        && loop_targets.contains_key(&op.operands[0]);
                    exits_direct_loop
                        || op.regions.iter().any(|region| {
                            Self::region_uses_direct_loop_target(context, *region, loop_targets)
                        })
                })
            })
    }

    fn split_after(context: &Context, block: &Arc<Block>, op: &Arc<tir::OpInstance>) -> Arc<Block> {
        let continuation = context.create_block(vec![]);
        let op_ids = block.op_ids();
        let position = op_ids.iter().position(|id| *id == op.id).unwrap();
        for op_id in op_ids.into_iter().skip(position + 1) {
            block.remove_op(op_id);
            continuation.insert(continuation.len(), op_id);
        }
        continuation
    }

    fn erase_control_op(
        rewriter: &mut Rewriter,
        block: &Arc<Block>,
        op: Arc<tir::OpInstance>,
    ) -> Result<(), PassError> {
        rewriter.erase_op(&OperationRef::new(op, Some(block.clone()), None))
    }

    fn add_blocks(context: &Context, region: RegionId, blocks: &[Arc<Block>]) {
        let region = context.get_region(region);
        for block in blocks {
            region.add_block(block.id());
        }
    }

    fn rewrite_internal_branches(
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[Arc<Block>],
        mapping: &HashMap<BlockId, BlockId>,
    ) -> Result<(), PassError> {
        for block in blocks {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                let replacement: Option<Box<dyn Operation>> =
                    if let Some(branch) = op.clone().as_op::<BranchOp>() {
                        mapping.get(&branch.dest()).map(|destination| {
                            Box::new(b::br(context, branch.dest_args(), *destination).build())
                                as Box<dyn Operation>
                        })
                    } else if let Some(branch) = op.clone().as_op::<CondBranchOp>() {
                        let true_dest = mapping
                            .get(&branch.true_dest())
                            .copied()
                            .unwrap_or(branch.true_dest());
                        let false_dest = mapping
                            .get(&branch.false_dest())
                            .copied()
                            .unwrap_or(branch.false_dest());
                        (true_dest != branch.true_dest() || false_dest != branch.false_dest()).then(
                            || {
                                Box::new(
                                    b::cond_br(
                                        context,
                                        branch.condition(),
                                        branch.true_args(),
                                        branch.false_args(),
                                        true_dest,
                                        false_dest,
                                    )
                                    .build(),
                                ) as Box<dyn Operation>
                            },
                        )
                    } else {
                        None
                    };
                if let Some(replacement) = replacement {
                    rewriter.replace_op(
                        &OperationRef::new(op, Some(block.clone()), None),
                        replacement.as_ref(),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn extract_region(
        context: &Context,
        rewriter: &mut Rewriter,
        region: RegionId,
    ) -> Result<Vec<Arc<Block>>, PassError> {
        let old_blocks = context
            .get_region(region)
            .iter(context.clone())
            .collect::<Vec<_>>();
        let token = TokenType::new(context);
        let mut mapping = HashMap::new();
        let mut blocks = Vec::with_capacity(old_blocks.len());
        for old in &old_blocks {
            let mut arguments = Vec::new();
            for argument in old.arguments() {
                if argument.ty() == token {
                    continue;
                }
                let replacement = context.create_value(argument.ty(), None);
                context.replace_value_uses(argument.id(), replacement.id());
                arguments.push(replacement);
            }
            let new = context.create_block(arguments);
            mapping.insert(old.id(), new.id());
            blocks.push(new);
        }
        for (old, new) in old_blocks.iter().zip(&blocks) {
            for op_id in old.op_ids() {
                old.remove_op(op_id);
                new.insert(new.len(), op_id);
            }
        }
        Self::rewrite_internal_branches(context, rewriter, &blocks, &mapping)?;
        Ok(blocks)
    }

    fn replace_with_branch(
        context: &Context,
        rewriter: &mut Rewriter,
        block: &Arc<Block>,
        destination: BlockId,
    ) -> Result<(), PassError> {
        let terminator = context.get_op(*block.op_ids().last().unwrap());
        let branch = b::br(context, vec![], destination).build();
        rewriter.replace_op(
            &OperationRef::new(terminator, Some(block.clone()), None),
            &branch,
        )
    }

    fn rewrite_condition(
        context: &Context,
        rewriter: &mut Rewriter,
        block: &Arc<Block>,
        true_dest: BlockId,
        false_dest: BlockId,
    ) -> Result<(), PassError> {
        let terminator = context.get_op(*block.op_ids().last().unwrap());
        let condition = terminator
            .clone()
            .as_op::<cir::ConditionOp>()
            .ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "CIR condition region must end with cir.condition".to_string(),
                )
            })?
            .operands()[0];
        let branch = b::cond_br(context, condition, vec![], vec![], true_dest, false_dest).build();
        rewriter.replace_op(
            &OperationRef::new(terminator, Some(block.clone()), None),
            &branch,
        )
    }

    fn rewrite_region_exits(
        context: &Context,
        rewriter: &mut Rewriter,
        blocks: &[Arc<Block>],
        normal_dest: BlockId,
        loop_targets: &HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        for block in blocks {
            let terminator = context.get_op(*block.op_ids().last().unwrap());
            let destination = if terminator.clone().as_op::<cir::YieldOp>().is_some() {
                Some(normal_dest)
            } else if let Some(exit) = terminator.clone().as_op::<cir::BreakOp>() {
                Some(
                    loop_targets
                        .get(&exit.operands()[0])
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet("cir.break has no enclosing loop".to_string())
                        })?
                        .break_dest,
                )
            } else if let Some(exit) = terminator.clone().as_op::<cir::ContinueOp>() {
                Some(
                    loop_targets
                        .get(&exit.operands()[0])
                        .ok_or_else(|| {
                            PassError::InvalidRuleSet(
                                "cir.continue has no enclosing loop".to_string(),
                            )
                        })?
                        .continue_dest,
                )
            } else if terminator.clone().as_op::<BranchOp>().is_some()
                || terminator.clone().as_op::<CondBranchOp>().is_some()
                || terminator.clone().as_op::<ReturnOp>().is_some()
            {
                None
            } else {
                return Err(PassError::InvalidRuleSet(
                    "CIR region has an unsupported exit".to_string(),
                ));
            };
            if let Some(destination) = destination {
                Self::replace_with_branch(context, rewriter, block, destination)?;
            }
        }
        Ok(())
    }

    fn structured_body(
        context: &Context,
        rewriter: &mut Rewriter,
        source: RegionId,
    ) -> Result<RegionId, PassError> {
        let source = Self::single_block(context, source).unwrap();
        let region = context.create_region();
        let arguments = source
            .arguments()
            .iter()
            .map(|argument| {
                let replacement = context.create_value(argument.ty(), None);
                context.replace_value_uses(argument.id(), replacement.id());
                replacement
            })
            .collect();
        let block = context.create_block(arguments);
        region.add_block(block.id());
        for op_id in source.op_ids() {
            source.remove_op(op_id);
            block.insert(block.len(), op_id);
        }
        let terminator = context.get_op(*block.op_ids().last().unwrap());
        let replacement: Box<dyn Operation> =
            if terminator.clone().as_op::<cir::BreakOp>().is_some() {
                Box::new(scf::ops::r#break(context, terminator.operands[0]).build())
            } else if terminator.clone().as_op::<cir::ContinueOp>().is_some() {
                Box::new(scf::ops::r#continue(context, terminator.operands[0]).build())
            } else {
                Box::new(scf::ops::r#yield(context, Operand::none()).build())
            };
        rewriter.replace_op(
            &OperationRef::new(terminator, Some(block), None),
            replacement.as_ref(),
        )?;
        Ok(region.id())
    }

    fn structured_condition(
        context: &Context,
        rewriter: &mut Rewriter,
        source: RegionId,
    ) -> Result<RegionId, PassError> {
        let source = Self::single_block(context, source).unwrap();
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        for op_id in source.op_ids() {
            source.remove_op(op_id);
            block.insert(block.len(), op_id);
        }
        let terminator = context.get_op(*block.op_ids().last().unwrap());
        let condition = terminator.operands[0];
        let replacement = scf::ops::condition(context, condition, Operand::none()).build();
        rewriter.replace_op(
            &OperationRef::new(terminator, Some(block), None),
            &replacement,
        )?;
        Ok(region.id())
    }

    fn structured_for_body(
        context: &Context,
        body_region: RegionId,
        step_region: RegionId,
    ) -> Result<RegionId, PassError> {
        let body = Self::single_block(context, body_region).unwrap();
        let step = Self::single_block(context, step_region).unwrap();
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        for source in [body, step] {
            let op_ids = source.op_ids();
            for op_id in op_ids.iter().take(op_ids.len() - 1) {
                source.remove_op(*op_id);
                block.insert(block.len(), *op_id);
            }
        }
        IRBuilder::new(block).insert(scf::ops::r#yield(context, Operand::none()).build());
        Ok(region.id())
    }

    fn lower_structured(
        context: &Context,
        rewriter: &mut Rewriter,
        block: Arc<Block>,
        op: Arc<tir::OpInstance>,
    ) -> Result<(), PassError> {
        let replacement: Box<dyn Operation> = if op.clone().as_op::<cir::WhileOp>().is_some() {
            let condition = Self::structured_condition(context, rewriter, op.regions[0])?;
            let body = Self::structured_body(context, rewriter, op.regions[1])?;
            Box::new(
                scf::ops::r#while(context, Operand::none(), None, Some(condition), Some(body))
                    .build(),
            )
        } else if op.clone().as_op::<cir::ForOp>().is_some() {
            let condition = Self::structured_condition(context, rewriter, op.regions[0])?;
            let body = Self::structured_for_body(context, op.regions[1], op.regions[2])?;
            Box::new(
                scf::ops::r#while(context, Operand::none(), None, Some(condition), Some(body))
                    .build(),
            )
        } else {
            let then_body = Self::structured_body(context, rewriter, op.regions[0])?;
            let else_body = Self::structured_body(context, rewriter, op.regions[1])?;
            Box::new(
                scf::ops::r#if(
                    context,
                    op.operands[0],
                    None,
                    Some(then_body),
                    Some(else_body),
                )
                .build(),
            )
        };
        rewriter.replace_op(
            &OperationRef::new(op, Some(block), None),
            replacement.as_ref(),
        )
    }

    fn lower_direct(
        context: &Context,
        rewriter: &mut Rewriter,
        function_region: RegionId,
        block: Arc<Block>,
        op: Arc<tir::OpInstance>,
        loop_targets: &mut HashMap<ValueId, LoopTargets>,
    ) -> Result<(), PassError> {
        if op.clone().as_op::<cir::WhileOp>().is_some() {
            let scope = Self::entry_block(context, op.regions[1]).arguments()[0].id();
            let condition = Self::extract_region(context, rewriter, op.regions[0])?;
            let body = Self::extract_region(context, rewriter, op.regions[1])?;
            let continuation = Self::split_after(context, &block, &op);
            loop_targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: condition[0].id(),
                },
            );
            Self::add_blocks(context, function_region, &condition);
            Self::add_blocks(context, function_region, &body);
            Self::add_blocks(
                context,
                function_region,
                std::slice::from_ref(&continuation),
            );
            Self::erase_control_op(rewriter, &block, op)?;
            IRBuilder::new(block).insert(b::br(context, vec![], condition[0].id()).build());
            Self::rewrite_condition(
                context,
                rewriter,
                &condition[0],
                body[0].id(),
                continuation.id(),
            )?;
            Self::rewrite_region_exits(context, rewriter, &body, condition[0].id(), loop_targets)?;
        } else if op.clone().as_op::<cir::ForOp>().is_some() {
            let scope = Self::entry_block(context, op.regions[1]).arguments()[0].id();
            let condition = Self::extract_region(context, rewriter, op.regions[0])?;
            let body = Self::extract_region(context, rewriter, op.regions[1])?;
            let step = Self::extract_region(context, rewriter, op.regions[2])?;
            let continuation = Self::split_after(context, &block, &op);
            loop_targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: step[0].id(),
                },
            );
            Self::add_blocks(context, function_region, &condition);
            Self::add_blocks(context, function_region, &body);
            Self::add_blocks(context, function_region, &step);
            Self::add_blocks(
                context,
                function_region,
                std::slice::from_ref(&continuation),
            );
            Self::erase_control_op(rewriter, &block, op)?;
            IRBuilder::new(block).insert(b::br(context, vec![], condition[0].id()).build());
            Self::rewrite_condition(
                context,
                rewriter,
                &condition[0],
                body[0].id(),
                continuation.id(),
            )?;
            Self::rewrite_region_exits(context, rewriter, &body, step[0].id(), loop_targets)?;
            Self::rewrite_region_exits(context, rewriter, &step, condition[0].id(), loop_targets)?;
        } else if op.clone().as_op::<cir::DoOp>().is_some() {
            let scope = Self::entry_block(context, op.regions[0]).arguments()[0].id();
            let body = Self::extract_region(context, rewriter, op.regions[0])?;
            let condition = Self::extract_region(context, rewriter, op.regions[1])?;
            let continuation = Self::split_after(context, &block, &op);
            loop_targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: condition[0].id(),
                },
            );
            Self::add_blocks(context, function_region, &body);
            Self::add_blocks(context, function_region, &condition);
            Self::add_blocks(
                context,
                function_region,
                std::slice::from_ref(&continuation),
            );
            Self::erase_control_op(rewriter, &block, op)?;
            IRBuilder::new(block).insert(b::br(context, vec![], body[0].id()).build());
            Self::rewrite_region_exits(context, rewriter, &body, condition[0].id(), loop_targets)?;
            Self::rewrite_condition(
                context,
                rewriter,
                &condition[0],
                body[0].id(),
                continuation.id(),
            )?;
        } else {
            let then_blocks = Self::extract_region(context, rewriter, op.regions[0])?;
            let else_blocks = Self::extract_region(context, rewriter, op.regions[1])?;
            let continuation = Self::split_after(context, &block, &op);
            Self::add_blocks(context, function_region, &then_blocks);
            Self::add_blocks(context, function_region, &else_blocks);
            Self::add_blocks(
                context,
                function_region,
                std::slice::from_ref(&continuation),
            );
            let condition = op.operands[0];
            Self::erase_control_op(rewriter, &block, op)?;
            IRBuilder::new(block).insert(
                b::cond_br(
                    context,
                    condition,
                    vec![],
                    vec![],
                    then_blocks[0].id(),
                    else_blocks[0].id(),
                )
                .build(),
            );
            Self::rewrite_region_exits(
                context,
                rewriter,
                &then_blocks,
                continuation.id(),
                loop_targets,
            )?;
            Self::rewrite_region_exits(
                context,
                rewriter,
                &else_blocks,
                continuation.id(),
                loop_targets,
            )?;
        }
        Ok(())
    }
}

impl Default for LowerCirControlFlowPass {
    fn default() -> Self {
        Self::new()
    }
}

impl Pass for LowerCirControlFlowPass {
    fn name(&self) -> &'static str {
        "lower-cir-control-flow"
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
        let function_region = op.op().regions[0];
        let has_goto = Self::region_has_goto(context, function_region);
        let mut loop_targets = HashMap::new();
        let mut changed = false;
        while let Some((block, control)) = Self::next_control_op_in_region(context, function_region)
        {
            let structured = if control.clone().as_op::<cir::WhileOp>().is_some() {
                Self::while_is_structured(context, &control)
            } else if control.clone().as_op::<cir::ForOp>().is_some() {
                Self::for_is_structured(context, &control)
            } else if control.clone().as_op::<cir::IfOp>().is_some() {
                control
                    .regions
                    .iter()
                    .all(|region| Self::body_is_structured(context, *region))
            } else {
                false
            } && !has_goto
                && !control.regions.iter().any(|region| {
                    Self::region_uses_direct_loop_target(context, *region, &loop_targets)
                });
            if structured {
                Self::lower_structured(context, rewriter, block, control)?;
            } else {
                Self::lower_direct(
                    context,
                    rewriter,
                    function_region,
                    block,
                    control,
                    &mut loop_targets,
                )?;
            }
            changed = true;
        }
        if has_goto {
            changed |= Self::resolve_gotos(context, rewriter, function_region)?;
            changed |= Self::remove_unreachable_blocks(context, function_region);
        }
        Ok(if changed {
            PreservedAnalyses::none()
        } else {
            PreservedAnalyses::all()
        })
    }
}
