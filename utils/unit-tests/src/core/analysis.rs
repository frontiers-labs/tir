//! AnalysisManager caching, def-use analysis and the monotone fact solver.

use tir::{
    analysis::DefUse,
    builtin::{ops, IntegerType, UnitType},
    func::ops as func_ops,
    AnalysisManager, Context, Operation,
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
fn catalog_producers_publish_complete_current_facts() {
    let context = Context::with_default_dialects();
    let root =
        tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, "module { module_end }")
            .unwrap()
            .id();
    let edit = |f: fn(&mut tir_egglog::Database, tir_egglog::TableId)| {
        let db = &mut *context.egraph();
        let input = db.database().table("Operand").unwrap();
        f(db.database_mut(), input);
    };
    edit(|db, input| {
        for (owner, slot) in [(0_i64, 2_i64), (0, 9), (1, 9)] {
            let row = [db.base(owner), db.base(slot), db.base(1_i64)];
            db.insert(input, &row).unwrap();
        }
    });
    AnalysisManager::install(&context, r#"
        pub let Values: Int -> Prop;
        pub let Members: std::Table<Int, std::table::IntSet, std::Merge::SetUnion>;
        pub let Count: (Int, Int) -> Prop;
        pub let Conflict: std::Table<Int, Int, std::Merge::Unique>;

        pub mod source {
            pub let input<root, owner, slot, value>:
                (ir::AnalysisRoot(root), ir::Operand(owner, slot, value), std::int::equal(slot, slot) == true) -> Values(slot);
        }
        pub mod bounds {
            pub let collect<v>: Values(v) -> Members(0, std::table::singleton(v));
        }
        pub mod answers {
            pub let count<vs>: Members(0, vs) -> Count(0, std::table::cardinality(vs));
            pub let empty: !Values(9) -> Count(0, 0);
        }
        pub mod fail {
            pub let conflict<v>: Values(v) -> Conflict(0, v);
        }
    "#).unwrap();
    let require = |table| AnalysisManager::require(&context, root, table);
    let read = |table| {
        let view = require(table).unwrap();
        let row = view.rows().next().unwrap();
        (view.base_value::<i64>(row[1]), row.timestamp())
    };
    let count = read("Count");
    assert_eq!(count.0, 2);
    assert_eq!(read("Count"), count);
    assert!(require("Conflict").is_err());
    edit(|db, input| {
        assert_eq!(db.row_count(db.table("Conflict").unwrap()), 0);
        db.clear(input);
    });
    assert_eq!(read("Count").0, 0);
    assert_eq!(require("Conflict").unwrap().rows().count(), 0);
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
    tir::analysis::cfg::ensure_installed(&context).unwrap();
    let funcs: Vec<_> = module
        .body()
        .op_ids()
        .into_iter()
        .filter_map(|id| context.get_op(id).as_op::<tir::func::FuncOp>())
        .collect();
    let [irr, gis, mem] = funcs.as_slice() else {
        panic!("expected three functions");
    };
    let count_for = |root: tir::OpId, table: &str| -> usize {
        let facts = AnalysisManager::require(&context, root, table).unwrap();
        facts.rows().count()
    };

    // Irreducible loop: one 2-block irreducible SCC with 1 exit and no tail-loop witness.
    assert_eq!(count_for(irr.id(), "IrredLoop"), 1);
    assert_eq!(count_for(irr.id(), "IrredScc"), 2);
    assert_eq!(count_for(irr.id(), "LoopExit"), 1);
    assert_eq!(count_for(irr.id(), "TailLoop"), 0);

    // Goto-into-switch: top-level branch is classified as an unstructured topological hammock.
    let top_h = {
        let facts = AnalysisManager::require(&context, gis.id(), "TopHammock").unwrap();
        let rows: Vec<i64> = facts
            .rows()
            .map(|row| facts.base_value::<i64>(row[2]))
            .collect();
        assert_eq!(rows.len(), 1);
        rows[0]
    };
    {
        let facts = AnalysisManager::require(&context, gis.id(), "HammockMember").unwrap();
        let members = facts
            .rows()
            .filter(|row| facts.base_value::<i64>(row[2]) == top_h)
            .count();
        assert_eq!(members, 3);
    }

    // Memory chains: two disjoint private allocas, both demanded across the SESE branch.
    assert_eq!(count_for(mem.id(), "RegionChain"), 2);
    assert_eq!(count_for(mem.id(), "SeseBranch"), 1);
    assert_eq!(count_for(mem.id(), "BranchOutput"), 2);
}
