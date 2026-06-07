use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::Operation;
use crate::analysis::DominatorTree;
use crate::graph::{Dag, NodeId};
use crate::{
    BlockId, Context, MemoryRead, MemoryWrite, OpId, OperationRef, Pass, PassError, PassTarget,
    PromotableAllocation, Rewriter, ValueId, builtin::FuncOp,
};

#[derive(Default)]
pub struct Mem2RegPass;

/// What we know about one allocated stack slot across the whole function.
#[derive(Default)]
struct SlotState {
    alloca: Option<OpId>,
    stores: Vec<OpId>,
    loads: Vec<OpId>,
    /// The slot's pointer is used somewhere other than a load/store, so its
    /// contents may be observed indirectly and it cannot be promoted.
    escapes: bool,
}

impl Mem2RegPass {
    pub fn new() -> Self {
        Self
    }
}

tir::register_pass!(Mem2RegPass, "mem2reg");

impl Pass for Mem2RegPass {
    fn name(&self) -> &'static str {
        "mem2reg"
    }

    fn target(&self) -> PassTarget {
        PassTarget::Operation(FuncOp::name())
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        if op.as_op::<FuncOp>().is_none() {
            return Ok(());
        }

        // Dominance over the function's whole region tree (including nested
        // structured-control-flow regions) drives the promotion decisions below.
        let dom_tree = DominatorTree::new(context, op.op().id);
        let layout = OpLayout::collect(context, &dom_tree);

        let slots = collect_slots(context, &layout);

        let mut erase: BTreeSet<OpId> = BTreeSet::new();
        for slot in slots.values() {
            if !is_promotable(slot, &layout, &dom_tree) {
                continue;
            }

            if let Some(store) = slot.stores.first() {
                let value = store_value(context, *store);
                for load in &slot.loads {
                    context.replace_value_uses(load_result(context, *load), value);
                    erase.insert(*load);
                }
                erase.insert(*store);
            }
            if let Some(alloca) = slot.alloca {
                erase.insert(alloca);
            }
        }

        for op_id in erase {
            if !context.has_operation(op_id) {
                continue;
            }
            let block = layout.block_of(op_id).map(|id| context.get_block(id));
            let target = OperationRef::new(context.get_op(op_id), block, None);
            rewriter.erase_op(&target)?;
        }

        Ok(())
    }
}

/// Where every operation lives, so dominance can be lifted to operations:
/// within a block, program order decides; across blocks, the dominator tree does.
struct OpLayout {
    position: HashMap<OpId, (BlockId, usize)>,
    blocks: Vec<BlockId>,
}

impl OpLayout {
    fn collect(context: &Context, dom_tree: &DominatorTree) -> Self {
        let mut blocks: Vec<BlockId> = (0..dom_tree.len())
            .map(NodeId::from_index)
            .filter_map(|node| dom_tree.block(node))
            .collect();
        blocks.sort_by_key(BlockId::number);

        let mut position = HashMap::new();
        for &block_id in &blocks {
            for (index, op_id) in context.get_block(block_id).op_ids().into_iter().enumerate() {
                position.insert(op_id, (block_id, index));
            }
        }

        Self { position, blocks }
    }

    fn block_of(&self, op: OpId) -> Option<BlockId> {
        self.position.get(&op).map(|(block, _)| *block)
    }

    /// Whether the operation `a` dominates `b`, reflexively.
    fn dominates(&self, dom_tree: &DominatorTree, a: OpId, b: OpId) -> bool {
        let (Some(&(a_block, a_index)), Some(&(b_block, b_index))) =
            (self.position.get(&a), self.position.get(&b))
        else {
            return false;
        };
        if a_block == b_block {
            a_index <= b_index
        } else {
            dom_tree.dominates(a_block, b_block)
        }
    }

    /// Every operation in dominator-tree block order.
    fn op_ids(&self, context: &Context) -> Vec<OpId> {
        self.blocks
            .iter()
            .flat_map(|&block_id| context.get_block(block_id).op_ids())
            .collect()
    }
}

/// Classify every load/store/escape against the slots opened by `alloca`s.
fn collect_slots(context: &Context, layout: &OpLayout) -> BTreeMap<ValueId, SlotState> {
    let mut slots: BTreeMap<ValueId, SlotState> = BTreeMap::new();
    let op_ids = layout.op_ids(context);

    for &op_id in &op_ids {
        if let Some(alloca) = context
            .get_op(op_id)
            .as_interface::<dyn PromotableAllocation>()
        {
            slots.entry(alloca.promoted_location()).or_default().alloca = Some(op_id);
        }
    }

    for &op_id in &op_ids {
        let instance = context.get_op(op_id);
        if instance
            .clone()
            .as_interface::<dyn PromotableAllocation>()
            .is_some()
        {
            continue;
        }

        if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>()
            && let Some(slot) = slots.get_mut(&read.read_location())
        {
            slot.loads.push(op_id);
            continue;
        }

        if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>()
            && let Some(slot) = slots.get_mut(&write.write_location())
        {
            slot.stores.push(op_id);
            continue;
        }

        for operand in &instance.operands {
            if let Some(slot) = slots.get_mut(operand) {
                slot.escapes = true;
            }
        }
    }

    slots
}

/// A slot promotes only when its single (or absent) store is the unambiguous
/// definition reaching every load — exactly the question dominance answers.
fn is_promotable(slot: &SlotState, layout: &OpLayout, dom_tree: &DominatorTree) -> bool {
    if slot.escapes || slot.alloca.is_none() || slot.stores.len() > 1 {
        return false;
    }

    match slot.stores.first() {
        // A lone store dominating every load gives each load a single reaching
        // value; with no other store there is nothing to merge, so no phi is
        // needed and forwarding is sound.
        Some(store) => slot
            .loads
            .iter()
            .all(|load| layout.dominates(dom_tree, *store, *load)),
        // No store: a load would read an undefined value, so only a store-less,
        // load-less (dead) slot is promotable.
        None => slot.loads.is_empty(),
    }
}

fn store_value(context: &Context, store: OpId) -> ValueId {
    context
        .get_op(store)
        .as_interface::<dyn MemoryWrite>()
        .expect("store op implements MemoryWrite")
        .written_value()
}

fn load_result(context: &Context, load: OpId) -> ValueId {
    context
        .get_op(load)
        .as_interface::<dyn MemoryRead>()
        .expect("load op implements MemoryRead")
        .read_value()
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, IRBuilder, IRFormatter, OpId, Operand, Operation, PassManager,
        builtin::{IntegerType, ops as b},
        ptr::{PtrType, ops as p},
    };

    use super::Mem2RegPass;

    fn run_mem2reg(context: &Context, func: OpId) {
        let mut pm = PassManager::new();
        pm.add_pass(Mem2RegPass::new());
        pm.run(context, context.get_op(func)).expect("mem2reg");
    }

    fn print_func(func: &impl Operation) -> String {
        let mut out = String::new();
        let mut fmt = IRFormatter::new(&mut out);
        func.print(&mut fmt).expect("print");
        out
    }

    #[test]
    fn promotes_linear_stack_slot() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let param = context.create_value(i32_ty, None);
        let param_id = param.id();
        let region = context.create_region();
        let block = context.create_block(vec![param]);
        region.add_block(block.id());
        let func = b::func(&context, "id", i32_ty, Some(region.id())).build();

        let mut builder = IRBuilder::new(func.body());
        let slot = builder.insert(p::alloca(&context, PtrType::typed(&context, i32_ty)).build());
        builder.insert(p::store(&context, param_id, slot.result()).build());
        let loaded = builder
            .insert(p::load(&context, slot.result(), i32_ty).build())
            .result();
        builder.insert(b::r#return(&context, loaded).build());

        run_mem2reg(&context, func.id());

        let out = print_func(&func);
        assert!(!out.contains("ptr.alloca"));
        assert!(!out.contains("ptr.store"));
        assert!(!out.contains("ptr.load"));
        assert!(out.contains(&format!("return %{}", param_id.number())));
    }

    /// A store in the entry block dominates a load in a successor block, so the
    /// value is forwarded across the branch. The `FuncOp` printer only shows the
    /// entry block, so these multi-block cases inspect the IR directly.
    #[test]
    fn promotes_across_unstructured_branch() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let param = context.create_value(i32_ty, None);
        let param_id = param.id();

        let region = context.create_region();
        let entry = context.create_block(vec![param]);
        let next = context.create_block(vec![]);
        region.add_block(entry.id());
        region.add_block(next.id());
        let func = b::func(&context, "fwd", i32_ty, Some(region.id())).build();

        let mut entry_b = IRBuilder::new(entry.clone());
        let slot = entry_b.insert(p::alloca(&context, PtrType::typed(&context, i32_ty)).build());
        let slot_ptr = slot.result();
        let alloca_id = slot.id();
        let store_id = entry_b
            .insert(p::store(&context, param_id, slot_ptr).build())
            .id();
        entry_b.insert(b::br(&context, vec![], next.id()).build());

        let mut next_b = IRBuilder::new(next.clone());
        let load = next_b.insert(p::load(&context, slot_ptr, i32_ty).build());
        let load_id = load.id();
        let ret_id = next_b
            .insert(b::r#return(&context, load.result()).build())
            .id();

        run_mem2reg(&context, func.id());

        assert!(!context.has_operation(alloca_id));
        assert!(!context.has_operation(store_id));
        assert!(!context.has_operation(load_id));
        // The load's consumer now reads the stored value directly.
        assert_eq!(context.get_op(ret_id).operands, vec![param_id]);
    }

    /// A store on only one side of a branch does not dominate a load placed after
    /// the join, so the slot must be left in memory.
    #[test]
    fn keeps_slot_when_store_does_not_dominate_load() {
        let context = Context::with_default_dialects();
        let i1_ty = IntegerType::new(&context, 1);
        let i32_ty = IntegerType::new(&context, 32);
        let cond = context.create_value(i1_ty, None);
        let param = context.create_value(i32_ty, None);
        let cond_id = cond.id();
        let param_id = param.id();

        let region = context.create_region();
        let entry = context.create_block(vec![cond, param]);
        let then = context.create_block(vec![]);
        let join = context.create_block(vec![]);
        for block in [&entry, &then, &join] {
            region.add_block(block.id());
        }
        let func = b::func(&context, "maybe", i32_ty, Some(region.id())).build();

        let mut entry_b = IRBuilder::new(entry.clone());
        let slot = entry_b.insert(p::alloca(&context, PtrType::typed(&context, i32_ty)).build());
        let slot_ptr = slot.result();
        let alloca_id = slot.id();
        entry_b.insert(b::cond_br(&context, cond_id, vec![], vec![], then.id(), join.id()).build());

        let mut then_b = IRBuilder::new(then.clone());
        let store_id = then_b
            .insert(p::store(&context, param_id, slot_ptr).build())
            .id();
        then_b.insert(b::br(&context, vec![], join.id()).build());

        let mut join_b = IRBuilder::new(join.clone());
        let load_id = join_b
            .insert(p::load(&context, slot_ptr, i32_ty).build())
            .id();
        join_b.insert(b::r#return(&context, context.get_op(load_id).results[0]).build());

        run_mem2reg(&context, func.id());

        assert!(context.has_operation(alloca_id));
        assert!(context.has_operation(store_id));
        assert!(context.has_operation(load_id));
    }

    /// Two stores to the same slot need a merge (a phi) that this pass does not
    /// build, so the slot is conservatively kept.
    #[test]
    fn keeps_slot_with_multiple_stores() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let a = context.create_value(i32_ty, None);
        let bb = context.create_value(i32_ty, None);
        let a_id = a.id();
        let b_id = bb.id();

        let region = context.create_region();
        let block = context.create_block(vec![a, bb]);
        region.add_block(block.id());
        let func = b::func(&context, "twostores", i32_ty, Some(region.id())).build();

        let mut builder = IRBuilder::new(func.body());
        let slot = builder.insert(p::alloca(&context, PtrType::typed(&context, i32_ty)).build());
        let slot_ptr = slot.result();
        builder.insert(p::store(&context, a_id, slot_ptr).build());
        builder.insert(p::store(&context, b_id, slot_ptr).build());
        let loaded = builder
            .insert(p::load(&context, slot_ptr, i32_ty).build())
            .result();
        builder.insert(b::r#return(&context, loaded).build());

        run_mem2reg(&context, func.id());

        let out = print_func(&func);
        assert!(out.contains("ptr.store"), "{out}");
        assert!(out.contains("ptr.load"), "{out}");
    }

    #[test]
    fn erases_dead_unused_alloca() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());
        let func = b::func(
            &context,
            "dead",
            crate::builtin::UnitType::new(&context),
            Some(region.id()),
        )
        .build();

        let mut builder = IRBuilder::new(func.body());
        builder.insert(p::alloca(&context, PtrType::typed(&context, i32_ty)).build());
        builder.insert(b::r#return(&context, Operand::none()).build());

        run_mem2reg(&context, func.id());

        let out = print_func(&func);
        assert!(!out.contains("ptr.alloca"), "{out}");
    }
}
