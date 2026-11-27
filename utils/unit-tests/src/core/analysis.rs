//! CFG memoization and def-use analysis.

use tir::{
    analysis::{cfg, DefUse},
    builtin::{ops, IntegerType, UnitType},
    func::ops as func_ops,
    Context, Operation,
};

#[test]
fn chains_over_ssa_ops() {
    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);

    let region = context.create_region();
    let arg = context.create_value(i32, None);
    let arg_id = arg.id();
    let block = context.create_block(vec![arg]);
    region.add_block(block.id());
    let func = func_ops::lambda(&context, "f", UnitType::new(&context), &region).build();

    let b = block;
    let dead = ops::constant(&context, 7, i32).at(b.end()).build();
    let sum = ops::addi(&context, arg_id, arg_id, i32).at(b.end()).build();
    let sum_val = sum.result();
    let call = tir::func::CallOpBuilder::new(&context)
        .args(vec![sum_val])
        .attr("callee", tir::attributes::AttributeValue::Str("g".into()))
        .result_type(i32)
        .at(b.end())
        .build();
    let call_result = call.result();
    let ret = func_ops::r#return(&context, call_result)
        .at(b.end())
        .build();

    let du = DefUse::new(&context, func.id());

    // The block argument is read twice by the add and defined by no op.
    assert_eq!(context.users_of(arg_id), [sum.id(), sum.id()]);
    assert!(du.defs_of(arg_id.number()).is_empty());

    assert_eq!(du.defs_of(sum_val.number()), [sum.id()]);
    assert_eq!(context.users_of(sum_val), [call.id()]);

    // Call arguments count as uses, and its result chains on.
    assert!(context.is_used(sum_val));
    assert_eq!(du.defs_of(call_result.number()), [call.id()]);
    assert_eq!(context.users_of(call_result), [ret.id()]);

    assert!(!context.is_used(dead.result()));
    assert_eq!(context.use_count(arg_id), 2);
}

#[test]
fn cfg_scope_tracks_moved_operations_without_invalidating_unrelated_functions() {
    use std::collections::BTreeSet;

    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(
        &context,
        r#"module {
  %a = func.func @a(%0: !i32) -> !i32 {
    %1 = constant {value = 7} : !i32
    func.return %0
  }
  %b = func.func @b(%0: !i32) -> !i32 {
    %1 = constant {value = 9} : !i32
    func.return %0
  }
  module_end
}"#,
    )
    .unwrap();
    let functions: Vec<_> = module
        .body()
        .op_ids()
        .into_iter()
        .filter_map(|id| context.get_op(id).as_op::<tir::func::FuncOp>())
        .collect();
    let [a, b] = functions.as_slice() else {
        panic!("expected two functions");
    };
    let [moved, a_return] = a.body().op_ids()[..] else {
        panic!("expected a spare constant and a return");
    };
    let [b_spare, b_return] = b.body().op_ids()[..] else {
        panic!("expected a spare constant and a return");
    };
    let scope = |op| cfg::scope(&context, op).unwrap();
    let before_a = scope(a.id());
    let ops = |facts: &cfg::ScopeFacts, db: &tir_egglog::Database| {
        facts
            .ops(db)
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap()
    };
    let keys = |ids: &[tir::OpId]| {
        ids.iter()
            .map(|id| (i64::from(id.number()),))
            .collect::<BTreeSet<_>>()
    };

    assert!(b.body().remove_op(b_spare));
    let before_b = scope(b.id());
    let after_unrelated_edit = scope(a.id());
    {
        let engine = context.egraph();
        let db = engine.database();
        assert_eq!(
            before_a.table_ops().id(db).unwrap(),
            after_unrelated_edit.table_ops().id(db).unwrap()
        );
        assert_eq!(ops(&before_a, db), keys(&[a.id(), moved, a_return]));
        assert_eq!(ops(&before_b, db), keys(&[b.id(), b_return]));
        assert_eq!(before_a.modules(db).unwrap().count(), 0);
    }

    context.insert(
        tir::InsertionPoint::Block {
            block: b.body().id(),
            position: 0,
        },
        moved,
    );
    let after_a = scope(a.id());
    let after_b = scope(b.id());
    {
        let engine = context.egraph();
        let db = engine.database();
        assert_eq!(ops(&after_a, db), keys(&[a.id(), a_return]));
        assert_eq!(ops(&after_b, db), keys(&[b.id(), moved, b_return]));
        assert_eq!(ops(&before_a, db), keys(&[a.id(), moved, a_return]));
        assert_eq!(ops(&before_b, db), keys(&[b.id(), b_return]));
    }

    let destination = a.body().id();
    {
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = db.table("Operation").unwrap();
        let id = db.base(i64::from(moved.number()));
        let old: Vec<_> = db
            .rows(table)
            .find(|row| row[0] == id)
            .unwrap()
            .values()
            .collect();
        let mut new = old.clone();
        new[3] = db.base(i64::from(destination.number()));
        db.remove(table, &old);
        db.insert(table, &new).unwrap();
    }

    let restored_a = scope(a.id());
    let restored_b = scope(b.id());
    let engine = context.egraph();
    let db = engine.database();
    assert_eq!(ops(&restored_a, db), keys(&[a.id(), moved, a_return]));
    assert_eq!(ops(&restored_b, db), keys(&[b.id(), b_return]));
}

#[test]
fn cfg_analysis_identifies_loops_branches_and_memory_chains() {
    let context = Context::with_default_dialects();
    let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(
        &context,
        r#"module {
  %f = func.func @irr(%0: !i1, %1: !i32) -> !i32 {
    cfg.cond_br %0, ^bb1, ^bb2
  ^bb1:
    %2 = addi %1, %1 : !i32
    cfg.br ^bb2
  ^bb2:
    %3 = subi %1, %1 : !i32
    cfg.cond_br %0, ^bb1, ^bb3
  ^bb3:
    func.return %1
  }
  %g = func.func @gis(%0: !i1, %1: !i32, %2: !i32) -> !i32 {
    %3 = constant {value = 1} : !i32
    cfg.cond_br %0, ^bb3, ^bb1
  ^bb1:
    %4 = cmpi %1, %3 {predicate = "eq"} : !i1
    cfg.cond_br %4, ^bb2, ^bb3
  ^bb2:
    %5 = addi %2, %2 : !i32
    cfg.br ^bb4(%5 : !i32)
  ^bb3:
    %6 = subi %2, %3 : !i32
    cfg.br ^bb4(%6 : !i32)
  ^bb4(%7: !i32):
    func.return %7
  }
  %h = func.func @mem(%c: !i1, %v: !i32) -> !i32 {
    %a = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %b = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    ptr.store %v, %a
    ptr.store %v, %b
    cfg.cond_br %c, ^t, ^e
  ^t:
    ptr.store %v, %a
    cfg.br ^j
  ^e:
    ptr.store %v, %b
    cfg.br ^j
  ^j:
    %x = ptr.load %a : !i32
    %y = ptr.load %b : !i32
    %s = addi %x, %y : !i32
    func.return %s
  }
  module_end
}"#,
    )
    .unwrap();
    let funcs: Vec<_> = module
        .body()
        .op_ids()
        .into_iter()
        .filter_map(|id| context.get_op(id).as_op::<tir::func::FuncOp>())
        .collect();
    let [irr, gis, mem] = funcs.as_slice() else {
        panic!("expected three functions");
    };
    let irr = cfg::scope(&context, irr.id()).unwrap();
    let gis = cfg::scope(&context, gis.id()).unwrap();
    let mem = cfg::scope(&context, mem.id()).unwrap();
    let engine = context.egraph();
    let db = engine.database();

    // One two-block irreducible SCC with one exit and no tail-loop witness.
    assert_eq!(irr.irred_loop(db).unwrap().count(), 1);
    assert_eq!(irr.irred_scc(db).unwrap().count(), 2);
    assert_eq!(irr.loop_exit(db).unwrap().count(), 1);
    assert_eq!(irr.tail_loop(db).unwrap().count(), 0);

    // The goto into the switch forms an unstructured topological hammock.
    let hammocks = gis
        .top_hammock(db)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let [(_, _, top_h)] = hammocks.as_slice() else {
        panic!("expected one top-level hammock");
    };
    let members = gis
        .hammock_member(db)
        .unwrap()
        .filter(|row| row.as_ref().unwrap().2 == *top_h)
        .count();
    assert_eq!(members, 3);

    // The SESE branch demands both independent private allocation chains.
    assert_eq!(mem.region_chain(db).unwrap().count(), 2);
    assert_eq!(mem.sese_branch(db).unwrap().count(), 1);
    assert_eq!(mem.branch_output(db).unwrap().count(), 2);
}
