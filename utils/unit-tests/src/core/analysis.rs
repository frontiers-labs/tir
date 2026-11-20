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
        for (slot, value) in [(0_i64, -2_i64), (1, 9), (2, 9)] {
            let row = [db.base(0_i64), db.base(slot), db.base(value)];
            db.insert(input, &row).unwrap();
        }
    });
    AnalysisManager::install(&context, r#"
        pub let Values: Int -> Prop;
        pub let Members: (Int, std::table::IntSet) -> Prop;
        pub let Count: (Int, Int) -> Prop;
        pub let Conflict: (Int, Int) -> Prop;
        pub mod tables {
            pub let Members: std::Table<Int, std::table::IntSet, std::Merge::SetUnion>;
            pub let Conflict: std::Table<Int, Int, std::Merge::Unique>;
        }
        pub mod source {
            pub let input<root, slot, v>:
                (ir::AnalysisRoot(root), ir::Operand(0, slot, v), ir::equal_id(slot, slot) == true) -> Values(v);
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
