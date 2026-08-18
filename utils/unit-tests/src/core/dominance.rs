//! Dominator- and post-dominator-tree construction.

use std::collections::HashSet;

use tir::{
    analysis::DominatorTree,
    builtin::{IntegerType, UnitType},
    cfg::ops as cfg_ops,
    func::ops as func_ops,
    graph::Dag,
    BlockHandle, BlockId, Context, OpId, Operand, Operation, RegionId,
};

fn block_succs(tree: &DominatorTree, block: BlockId) -> HashSet<BlockId> {
    let node = tree.node_of(block).unwrap();
    tree.children(node)
        .filter_map(|child| tree.block(child))
        .collect()
}

fn yield_region(context: &Context) -> RegionId {
    let region = context.create_region();
    let block = context.create_block(vec![]);
    region.add_block(block.id());
    block.append_op(tir::scf::ops::r#yield(context, vec![]).build());
    region.id()
}

fn func_with_region(context: &Context, region: RegionId) -> OpId {
    func_ops::func(context, "f", UnitType::new(context), Some(region))
        .build()
        .id()
}

fn terminate(block: &BlockHandle, op: impl Operation) {
    block.append_op(op);
}

#[test]
fn diamond_dominators() {
    let context = Context::with_default_dialects();
    let i1 = IntegerType::new(&context, 1);
    let cond = context.create_value(i1, None);
    let cond_id = cond.id();

    let region = context.create_region();
    let entry = context.create_block(vec![cond]);
    let t = context.create_block(vec![]);
    let f = context.create_block(vec![]);
    let merge = context.create_block(vec![]);
    for block in [&entry, &t, &f, &merge] {
        region.add_block(block.id());
    }

    terminate(
        &entry,
        cfg_ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build(),
    );
    terminate(&t, cfg_ops::br(&context, vec![], merge.id()).build());
    terminate(&f, cfg_ops::br(&context, vec![], merge.id()).build());
    terminate(
        &merge,
        func_ops::r#return(&context, Operand::none()).build(),
    );

    let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));

    assert_eq!(dt.len(), 4);
    assert_eq!(dt.idom(entry.id()), None);
    assert_eq!(dt.idom(t.id()), Some(entry.id()));
    assert_eq!(dt.idom(f.id()), Some(entry.id()));
    assert_eq!(dt.idom(merge.id()), Some(entry.id()));

    assert!(dt.dominates(entry.id(), entry.id()));
    assert!(dt.dominates(entry.id(), merge.id()));
    assert!(!dt.dominates(t.id(), merge.id()));

    let root = dt.root().unwrap();
    assert_eq!(dt.block(root), Some(entry.id()));
    assert_eq!(
        block_succs(&dt, entry.id()),
        HashSet::from([t.id(), f.id(), merge.id()])
    );
}

#[test]
fn loop_back_edge_dominators() {
    let context = Context::with_default_dialects();
    let i1 = IntegerType::new(&context, 1);
    let cond = context.create_value(i1, None);
    let cond_id = cond.id();

    let region = context.create_region();
    let entry = context.create_block(vec![cond]);
    let header = context.create_block(vec![]);
    let body = context.create_block(vec![]);
    let exit = context.create_block(vec![]);
    for block in [&entry, &header, &body, &exit] {
        region.add_block(block.id());
    }

    terminate(&entry, cfg_ops::br(&context, vec![], header.id()).build());
    terminate(
        &header,
        cfg_ops::cond_br(&context, cond_id, vec![], vec![], body.id(), exit.id()).build(),
    );
    terminate(&body, cfg_ops::br(&context, vec![], header.id()).build());
    terminate(&exit, func_ops::r#return(&context, Operand::none()).build());

    let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));

    assert_eq!(dt.idom(header.id()), Some(entry.id()));
    assert_eq!(dt.idom(body.id()), Some(header.id()));
    assert_eq!(dt.idom(exit.id()), Some(header.id()));
    assert!(dt.dominates(header.id(), body.id()));
    assert!(!dt.dominates(body.id(), exit.id()));
}

#[test]
fn structured_if_dominators() {
    let context = Context::with_default_dialects();
    let i1 = IntegerType::new(&context, 1);
    let cond = context.create_value(i1, None);
    let cond_id = cond.id();

    let region = context.create_region();
    let entry = context.create_block(vec![cond]);
    region.add_block(entry.id());

    let then_region = yield_region(&context);
    let else_region = yield_region(&context);
    let then_entry = context
        .get_region(then_region)
        .iter(context.clone())
        .next()
        .unwrap()
        .id();
    let else_entry = context
        .get_region(else_region)
        .iter(context.clone())
        .next()
        .unwrap()
        .id();

    let if_op = tir::scf::ops::r#if(
        &context,
        cond_id,
        vec![],
        vec![],
        Some(then_region),
        Some(else_region),
    )
    .build();

    entry.append_op(if_op);
    entry.append_op(func_ops::r#return(&context, Operand::none()).build());

    let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));

    // The block holding scf.if dominates the entries of both nested regions.
    assert_eq!(dt.idom(then_entry), Some(entry.id()));
    assert_eq!(dt.idom(else_entry), Some(entry.id()));
    assert!(dt.dominates(entry.id(), then_entry));
    assert!(dt.dominates(entry.id(), else_entry));
    assert!(!dt.dominates(then_entry, else_entry));
}

#[test]
fn diamond_post_dominators() {
    let context = Context::with_default_dialects();
    let i1 = IntegerType::new(&context, 1);
    let cond = context.create_value(i1, None);
    let cond_id = cond.id();

    let region = context.create_region();
    let entry = context.create_block(vec![cond]);
    let t = context.create_block(vec![]);
    let f = context.create_block(vec![]);
    let merge = context.create_block(vec![]);
    for block in [&entry, &t, &f, &merge] {
        region.add_block(block.id());
    }

    terminate(
        &entry,
        cfg_ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build(),
    );
    terminate(&t, cfg_ops::br(&context, vec![], merge.id()).build());
    terminate(&f, cfg_ops::br(&context, vec![], merge.id()).build());
    terminate(
        &merge,
        func_ops::r#return(&context, Operand::none()).build(),
    );

    let pdt = DominatorTree::post_dominator(&context, func_with_region(&context, region.id()));

    // The merge block post-dominates every block; the root is the virtual exit.
    let root = pdt.root().unwrap();
    assert_eq!(pdt.block(root), None);
    assert_eq!(pdt.idom(merge.id()), None);
    assert_eq!(pdt.idom(entry.id()), Some(merge.id()));
    assert_eq!(pdt.idom(t.id()), Some(merge.id()));
    assert_eq!(pdt.idom(f.id()), Some(merge.id()));

    assert!(pdt.dominates(merge.id(), entry.id()));
    assert!(pdt.dominates(merge.id(), t.id()));
    assert!(!pdt.dominates(t.id(), entry.id()));
}

#[test]
fn single_block_tree() {
    let context = Context::with_default_dialects();
    let region = context.create_region();
    let entry = context.create_block(vec![]);
    region.add_block(entry.id());
    terminate(
        &entry,
        func_ops::r#return(&context, Operand::none()).build(),
    );

    let dt = DominatorTree::new(&context, func_with_region(&context, region.id()));
    assert_eq!(dt.len(), 1);
    assert_eq!(dt.block(dt.root().unwrap()), Some(entry.id()));
    assert_eq!(dt.idom(entry.id()), None);
    assert!(dt.dominates(entry.id(), entry.id()));
}

#[test]
fn for_loop_as_root() {
    let context = Context::with_default_dialects();
    let index = tir::builtin::IndexType::new(&context);
    let lb = context.create_value(index, None);
    let ub = context.create_value(index, None);
    let step = context.create_value(index, None);

    let body = yield_region(&context);
    let body_entry = context
        .get_region(body)
        .iter(context.clone())
        .next()
        .unwrap()
        .id();

    let for_op = tir::scf::ops::r#for(
        &context,
        lb.id(),
        ub.id(),
        step.id(),
        vec![],
        vec![],
        Some(body),
    )
    .build();

    // An scf.for can itself be the root: its single body region is the tree.
    let dt = DominatorTree::new(&context, for_op.id());
    assert_eq!(dt.len(), 1);
    assert_eq!(dt.block(dt.root().unwrap()), Some(body_entry));
}
