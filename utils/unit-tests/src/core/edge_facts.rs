//! Guarded-edge facts inherited down the dominator tree.

use std::rc::Rc;

use tir::{
    analysis::{DominatingEdgeFacts, EdgeFact},
    builtin::{IntegerType, UnitType},
    cfg::ops as cfg_ops,
    func::ops as func_ops,
    AnalysisManager, BlockHandle, Context, Operand, Operation, RegionId, ValueId,
};

fn analyze(context: &Context, region: RegionId) -> Rc<DominatingEdgeFacts> {
    let root = func_ops::func(context, "f", UnitType::new(context), Some(region))
        .build()
        .id();
    AnalysisManager::new().get::<DominatingEdgeFacts>(context, root)
}

fn cond(context: &Context) -> ValueId {
    let i1 = IntegerType::new(context, 1);
    context.create_value(i1, None).id()
}

fn terminate(block: &BlockHandle, op: impl Operation) {
    block.append_op(op);
}

#[test]
fn diamond_then_else_get_fact_join_does_not() {
    let context = Context::with_default_dialects();
    let c = cond(&context);

    let region = context.create_region();
    let entry = context.create_block(vec![]);
    let t = context.create_block(vec![]);
    let f = context.create_block(vec![]);
    let merge = context.create_block(vec![]);
    for block in [&entry, &t, &f, &merge] {
        region.add_block(block.id());
    }

    terminate(
        &entry,
        cfg_ops::cond_br(&context, c, vec![], vec![], t.id(), f.id()).build(),
    );
    terminate(&t, cfg_ops::br(&context, vec![], merge.id()).build());
    terminate(&f, cfg_ops::br(&context, vec![], merge.id()).build());
    terminate(
        &merge,
        func_ops::r#return(&context, Operand::none()).build(),
    );

    let facts = analyze(&context, region.id());

    assert_eq!(
        facts.own_fact(t.id()),
        Some(EdgeFact {
            condition: c,
            holds: true
        })
    );
    assert_eq!(
        facts.own_fact(f.id()),
        Some(EdgeFact {
            condition: c,
            holds: false
        })
    );
    assert_eq!(
        facts.facts(t.id()),
        &[EdgeFact {
            condition: c,
            holds: true
        }]
    );
    assert_eq!(facts.own_fact(merge.id()), None);
    assert!(facts.facts(merge.id()).is_empty());
    // Entry has an implicit incoming edge, so no fact.
    assert_eq!(facts.own_fact(entry.id()), None);
}

#[test]
fn nested_diamond_inherits_outer_then_own_ordered() {
    let context = Context::with_default_dialects();
    let c1 = cond(&context);
    let c2 = cond(&context);

    let region = context.create_region();
    let entry = context.create_block(vec![]);
    let outer_t = context.create_block(vec![]);
    let outer_f = context.create_block(vec![]);
    let inner_t = context.create_block(vec![]);
    let inner_f = context.create_block(vec![]);
    for block in [&entry, &outer_t, &outer_f, &inner_t, &inner_f] {
        region.add_block(block.id());
    }

    terminate(
        &entry,
        cfg_ops::cond_br(&context, c1, vec![], vec![], outer_t.id(), outer_f.id()).build(),
    );
    terminate(
        &outer_t,
        cfg_ops::cond_br(&context, c2, vec![], vec![], inner_t.id(), inner_f.id()).build(),
    );
    terminate(
        &outer_f,
        func_ops::r#return(&context, Operand::none()).build(),
    );
    terminate(
        &inner_t,
        func_ops::r#return(&context, Operand::none()).build(),
    );
    terminate(
        &inner_f,
        func_ops::r#return(&context, Operand::none()).build(),
    );

    let facts = analyze(&context, region.id());

    assert_eq!(
        facts.facts(inner_t.id()),
        &[
            EdgeFact {
                condition: c1,
                holds: true
            },
            EdgeFact {
                condition: c2,
                holds: true
            },
        ]
    );
    assert_eq!(
        facts.own_fact(inner_t.id()),
        Some(EdgeFact {
            condition: c2,
            holds: true
        })
    );
}

#[test]
fn loop_header_back_edge_gets_no_fact() {
    let context = Context::with_default_dialects();
    let c = cond(&context);

    let region = context.create_region();
    let entry = context.create_block(vec![]);
    let header = context.create_block(vec![]);
    let body = context.create_block(vec![]);
    let exit = context.create_block(vec![]);
    for block in [&entry, &header, &body, &exit] {
        region.add_block(block.id());
    }

    terminate(&entry, cfg_ops::br(&context, vec![], header.id()).build());
    terminate(
        &header,
        cfg_ops::cond_br(&context, c, vec![], vec![], body.id(), exit.id()).build(),
    );
    terminate(&body, cfg_ops::br(&context, vec![], header.id()).build());
    terminate(&exit, func_ops::r#return(&context, Operand::none()).build());

    let facts = analyze(&context, region.id());

    // Two incoming edges (entry, back edge) disqualify the header.
    assert_eq!(facts.own_fact(header.id()), None);
    assert!(facts.facts(header.id()).is_empty());
}

#[test]
fn single_pred_unguarded_edge_gets_no_fact() {
    let context = Context::with_default_dialects();

    let region = context.create_region();
    let entry = context.create_block(vec![]);
    let next = context.create_block(vec![]);
    for block in [&entry, &next] {
        region.add_block(block.id());
    }

    terminate(&entry, cfg_ops::br(&context, vec![], next.id()).build());
    terminate(&next, func_ops::r#return(&context, Operand::none()).build());

    let facts = analyze(&context, region.id());
    assert_eq!(facts.own_fact(next.id()), None);
    assert!(facts.facts(next.id()).is_empty());
}

#[test]
fn region_entry_excluded() {
    let context = Context::with_default_dialects();
    let c = cond(&context);

    let region = context.create_region();
    let entry = context.create_block(vec![]);
    region.add_block(entry.id());

    let then_region = context.create_region();
    let then_block = context.create_block(vec![]);
    then_region.add_block(then_block.id());
    terminate(
        &then_block,
        tir::scf::ops::r#yield(&context, vec![]).build(),
    );
    let then_entry = then_block.id();

    let if_op =
        tir::scf::ops::r#if(&context, c, vec![], vec![], Some(then_region.id()), None).build();
    entry.append_op(if_op);
    entry.append_op(func_ops::r#return(&context, Operand::none()).build());

    let facts = analyze(&context, region.id());
    // The nested region's entry has an implicit incoming edge.
    assert_eq!(facts.own_fact(then_entry), None);
    assert!(facts.facts(then_entry).is_empty());
}

#[test]
fn cond_br_identical_successors_gets_no_fact() {
    let context = Context::with_default_dialects();
    let c = cond(&context);

    let region = context.create_region();
    let entry = context.create_block(vec![]);
    let target = context.create_block(vec![]);
    for block in [&entry, &target] {
        region.add_block(block.id());
    }

    terminate(
        &entry,
        cfg_ops::cond_br(&context, c, vec![], vec![], target.id(), target.id()).build(),
    );
    terminate(
        &target,
        func_ops::r#return(&context, Operand::none()).build(),
    );

    let facts = analyze(&context, region.id());
    // Both guarded edges land on `target`: two in-edges, so no single fact.
    assert_eq!(facts.own_fact(target.id()), None);
    assert!(facts.facts(target.id()).is_empty());
}
