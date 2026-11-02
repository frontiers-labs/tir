//! Find-or-make over groups of rows, and the congruence of keyed groups.

use tir_egglog::prelude::*;
use tir_egglog::{Color, Database, Merge, Program, Rules, Schedule, SortKind, TableId, Value};

const ADD: i64 = 1;
const NEG: i64 = 2;
const LEAF: i64 = 3;

// An op is a group of rows: `(Operation o) = kind` and `(Operands o i) =
// value`. `Result` makes the value of an op and is no part of it.
struct Ir {
    db: Database,
    operation: TableId,
    operands: TableId,
    result: TableId,
    x: Value,
    y: Value,
}

impl Ir {
    fn new() -> Self {
        let mut db = Database::new();
        let op = db.add_sort("Op", SortKind::Eq);
        let val = db.add_sort("Val", SortKind::Eq);
        let int = db.sort("i64").unwrap();
        let operation = db.add_function("Operation", &[op], int, Merge::AssertEq);
        let operands = db.add_function("Operands", &[op, int], val, Merge::Union);
        let keyed = db.add_relation("Keyed", &[op]);
        let result = db.add_constructor("Result", &[op, int], val);
        db.add_group(op, &[operation, operands], keyed).unwrap();
        let (x, y) = (db.fresh(val), db.fresh(val));
        Self {
            db,
            operation,
            operands,
            result,
            x,
            y,
        }
    }

    fn op(&mut self, kind: i64, args: &[Value]) -> Value {
        self.op_in(Color::BASE, kind, args)
    }

    fn op_in(&mut self, color: Color, kind: i64, args: &[Value]) -> Value {
        let kind = [self.db.base(kind)];
        let slots: Vec<[Value; 2]> = (args.iter().enumerate())
            .map(|(i, &arg)| [self.db.base(i as i64), arg])
            .collect();
        let mut rows = vec![(self.operation, &kind[..])];
        rows.extend(slots.iter().map(|slot| (self.operands, &slot[..])));
        self.db.find_or_make_in(color, &rows).unwrap()
    }

    // The value an op makes.
    fn result(&mut self, op: Value) -> Value {
        let slot = self.db.base(0i64);
        self.db.entry(self.result, &[op, slot]).unwrap()
    }

    fn ops(&self) -> usize {
        self.db.rows(self.operation).count()
    }
}

#[test]
fn find_or_make_finds_the_op_with_equal_rows_and_makes_the_others() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let add = ir.op(ADD, &[x, y]);
    assert_eq!(ir.op(ADD, &[x, y]), add);
    assert_ne!(ir.op(ADD, &[y, x]), add);
    assert_ne!(ir.op(NEG, &[x, y]), add);
    assert_eq!(ir.ops(), 3);
}

#[test]
fn ops_with_fewer_or_more_operands_differ() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let ops = [&[][..], &[x], &[x, y, x]].map(|args| ir.op(LEAF, args));
    assert!(ops[0] != ops[1] && ops[1] != ops[2] && ops[0] != ops[2]);
    assert_ne!(ir.op(LEAF, &[x, y]), ops[2]);
    assert_eq!(
        [&[][..], &[x], &[x, y, x]].map(|args| ir.op(LEAF, args)),
        ops
    );
    assert_eq!(ir.ops(), 4);
}

#[test]
fn a_rebuild_unites_keyed_ops_whose_operands_become_equal() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let (a, b) = (ir.op(ADD, &[x, x]), ir.op(ADD, &[y, x]));
    let (ra, rb) = (ir.result(a), ir.result(b));
    let (na, nb) = (ir.op(NEG, &[ra]), ir.op(NEG, &[rb]));
    assert_ne!(ir.db.find(na), ir.db.find(nb));
    ir.db.union(x, y);
    ir.db.rebuild().unwrap();
    // The adds unite, so their results do, so the negations do.
    assert_eq!(ir.db.find(a), ir.db.find(b));
    assert_eq!(ir.db.find(ra), ir.db.find(rb));
    assert_eq!(ir.db.find(na), ir.db.find(nb));
    assert_eq!(ir.ops(), 2);
}

#[test]
fn unkeyed_ops_are_neither_found_nor_united() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let op = ir.db.schema().sort_named("Op").unwrap();
    let [kind, zero, one] = [ADD, 0, 1].map(|n| ir.db.base(n));
    let unkeyed = [(); 2].map(|()| ir.db.fresh(op));
    for (u, first) in unkeyed.into_iter().zip([x, y]) {
        ir.db.insert(ir.operation, &[u, kind]).unwrap();
        ir.db.insert(ir.operands, &[u, zero, first]).unwrap();
        ir.db.insert(ir.operands, &[u, one, x]).unwrap();
    }
    let keyed = ir.op(ADD, &[x, x]);
    assert!(!unkeyed.contains(&keyed));
    let other = ir.op(ADD, &[y, x]);
    ir.db.union(x, y);
    ir.db.rebuild().unwrap();
    assert_eq!(ir.db.find(keyed), ir.db.find(other));
    assert_ne!(ir.db.find(unkeyed[0]), ir.db.find(unkeyed[1]));
    assert_ne!(ir.db.find(unkeyed[0]), ir.db.find(keyed));
}

#[test]
fn a_rule_finds_the_op_it_made_so_saturation_stops() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let add = ir.op(ADD, &[x, y]);
    let neg = ir.op(NEG, &[x]);
    let mut rules = Rules::new();
    let commute = actions![
        (let s (find-or-make (Operation 1) (Operands 0 b) (Operands 1 a)))
        (union (Result s 0) (Result o 0))
    ];
    let facts = facts![(= (Operation o) 1) (= (Operands o 0) a) (= (Operands o 1) b)];
    rules.rule(&ir.db, "", facts, commute).unwrap();
    // Both ops have `x` first, and get one leaf of `x` between them.
    let leaf = actions![(find-or-make (Operation 3) (Operands 0 a))];
    rules
        .rule(&ir.db, "", facts![(= (Operands o 0) a)], leaf)
        .unwrap();
    let mut program = rules.compile(&ir.db).unwrap();
    program
        .run(&mut ir.db, &Schedule::saturate(""), &[])
        .unwrap();

    assert_eq!(ir.ops(), 5);
    let swapped = ir.op(ADD, &[y, x]);
    assert_eq!(ir.result(swapped), ir.result(add));
    assert_eq!(ir.ops(), 5);
    let leaves = [x, y].map(|value| ir.op(LEAF, &[value]));
    assert_eq!(ir.ops(), 5);
    assert!(!leaves.contains(&neg));
}

#[test]
fn find_or_make_in_a_color_sees_base_rows_and_makes_rows_in_the_color() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let add = ir.op(ADD, &[x, y]);
    let color = ir.db.color(&[]);
    assert_eq!(ir.op_in(color, ADD, &[x, y]), add);
    let swapped = ir.op_in(color, ADD, &[y, x]);
    assert_ne!(swapped, add);
    assert_eq!(ir.op_in(color, ADD, &[y, x]), swapped);
    assert!(ir.db.lookup(ir.operation, &[swapped]).is_none());
    assert!(ir.db.lookup_in(color, ir.operation, &[swapped]).is_some());

    // The base makes the leaf of `x`, and the color the leaf of `y`.
    let mut rules = Rules::new();
    let leaf = actions![(find-or-make (Operation 3) (Operands 0 a))];
    rules
        .rule(&ir.db, "", facts![(= (Operands o 0) a)], leaf)
        .unwrap();
    let mut program = rules.compile(&ir.db).unwrap();
    program
        .run(&mut ir.db, &Schedule::saturate(""), &[])
        .unwrap();
    program
        .run(&mut ir.db, &Schedule::saturate(""), &[])
        .unwrap();
    assert_eq!(ir.ops(), 2);
    assert_eq!(ir.db.own_rows_in(color, ir.operation).count(), 2);
    let base_leaf = ir.op(LEAF, &[x]);
    assert_eq!(ir.op_in(color, LEAF, &[x]), base_leaf);
    let color_leaf = ir.op_in(color, LEAF, &[y]);
    assert!(ir.db.lookup(ir.operation, &[color_leaf]).is_none());
    assert_eq!(ir.ops(), 2);
    assert_eq!(ir.db.own_rows_in(color, ir.operation).count(), 2);
}

#[test]
fn find_group_finds_without_making() {
    let mut ir = Ir::new();
    let add = ir.op(ADD, &[ir.x, ir.y]);
    let rows = |ir: &mut Ir, args: &[Value]| -> Vec<Vec<Value>> {
        let mut rows = vec![vec![ir.db.base(ADD)]];
        rows.extend((args.iter().enumerate()).map(|(i, &arg)| vec![ir.db.base(i as i64), arg]));
        rows
    };
    let (x, y) = (ir.x, ir.y);
    let same = rows(&mut ir, &[x, y]);
    let other = rows(&mut ir, &[y, x]);
    let spec = |ir: &Ir, rows: &[Vec<Value>]| -> Vec<(TableId, Vec<Value>)> {
        let table = |at: usize| if at == 0 { ir.operation } else { ir.operands };
        (rows.iter().enumerate())
            .map(|(at, row)| (table(at), row.clone()))
            .collect()
    };
    let found = |ir: &Ir, spec: &[(TableId, Vec<Value>)]| {
        let spec: Vec<(TableId, &[Value])> = spec.iter().map(|(t, r)| (*t, &r[..])).collect();
        ir.db.find_group(&spec).unwrap()
    };
    let before = ir.ops();
    assert_eq!(found(&ir, &spec(&ir, &same)), Some(add));
    assert_eq!(found(&ir, &spec(&ir, &other)), None);
    assert_eq!(ir.ops(), before);
}

#[test]
fn color_operand_union_unites_keyed_groups_only_in_that_color() {
    let mut ir = Ir::new();
    let (x, y) = (ir.x, ir.y);
    let (a, b) = (ir.op(ADD, &[x]), ir.op(ADD, &[y]));
    let c = ir.db.color(&[]);
    ir.db.union_in(c, x, y);
    ir.db.rebuild().unwrap();
    assert_eq!(ir.db.find_in(c, a), ir.db.find_in(c, b));
    assert_ne!(ir.db.find(a), ir.db.find(b));
}

#[test]
fn adopted_member_deletion_unites_keyed_groups() {
    let mut ir = Ir::new();
    let x = ir.x;
    let a = ir.op(ADD, &[x]);
    let b = ir.op(ADD, &[]);
    ir.db.rebuild().unwrap();
    let c = ir.db.color(&[]);
    let zero = ir.db.base(0i64);
    ir.db.remove_in(c, ir.operands, &[a, zero]);
    ir.db.rebuild().unwrap();
    ir.db.adopt(c);
    ir.db.rebuild().unwrap();
    assert!(ir.db.lookup(ir.operands, &[a, zero]).is_none());
    assert_eq!(ir.db.find(a), ir.db.find(b));
}

#[test]
fn repeated_relation_members_have_set_identity() {
    let mut db = Database::new();
    let sort = db.add_sort("Group", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    let member = db.add_relation("Member", &[sort, int]);
    let keyed = db.add_relation("Keyed", &[sort]);
    db.add_group(sort, &[member], keyed).unwrap();
    let row = [db.base(5i64)];
    let a = db.find_or_make(&[(member, &row)]).unwrap();
    let spec = [(member, &row[..]), (member, &row[..])];
    assert_eq!(db.find_or_make(&spec).unwrap(), a);
    assert_eq!(db.find_group(&spec).unwrap(), Some(a));
    let c = db.color(&[]);
    assert_eq!(db.find_or_make_in(c, &spec).unwrap(), a);
}

#[test]
fn empty_keyed_groups_merge_after_last_member_deletion() {
    let mut db = Database::new();
    let group = db.add_sort("G", tir_egglog::SortKind::Eq);
    let int = db.sort("i64").unwrap();
    let member = db.add_relation("Member", &[group, int]);
    let keyed = db.add_relation("Keyed", &[group]);
    db.add_group(group, &[member], keyed).unwrap();
    let (zero, one) = (db.base(0_i64), db.base(1_i64));
    let a = db.find_or_make(&[(member, &[zero])]).unwrap();
    let b = db.find_or_make(&[(member, &[one])]).unwrap();
    db.rebuild().unwrap();
    let color = db.color(&[]);
    db.remove_in(color, member, &[a, zero]);
    db.remove_in(color, member, &[b, one]);
    db.rebuild().unwrap();
    assert_eq!(db.rows_in(color, member).count(), 0);
    assert!(db.lookup_in(color, keyed, &[a]).is_some());
    assert!(db.lookup_in(color, keyed, &[b]).is_some());
    assert_eq!(db.find_in(color, a), db.find_in(color, b));
    db.adopt(color);
    db.rebuild().unwrap();
    assert_eq!(db.rows(member).count(), 0);
    assert!(db.lookup(keyed, &[a]).is_some());
    assert!(db.lookup(keyed, &[b]).is_some());
    assert_eq!(db.find(a), db.find(b));
}

#[test]
fn compiled_member_deletions_close_group_congruence() {
    for colored in [false, true] {
        let mut db = Database::new();
        let group = db.add_sort("G", SortKind::Eq);
        let int = db.sort("i64").unwrap();
        let member = db.add_relation("Member", &[group, int]);
        let keyed = db.add_relation("Keyed", &[group]);
        let trigger = db.add_relation("Trigger", &[group, int]);
        db.add_group(group, &[member], keyed).unwrap();
        let [zero, one] = [0_i64, 1].map(|n| db.base(n));
        let a = db.find_or_make(&[(member, &[zero])]).unwrap();
        let b = db.find_or_make(&[(member, &[one])]).unwrap();
        db.rebuild().unwrap();
        let color = if colored { db.color(&[]) } else { Color::BASE };
        let sibling = db.color(&[]);
        for row in [[a, zero], [b, one]] {
            db.insert_in(color, trigger, &row).unwrap();
        }
        let mut rules = Rules::new();
        rules
            .rule(
                &db,
                "",
                facts![(Trigger g x)],
                actions![(delete (Member g x))],
            )
            .unwrap();
        let mut program = rules.compile(&db).unwrap();
        program.run(&mut db, &Schedule::run(""), &[]).unwrap();
        assert_eq!(db.rows_in(color, member).count(), 0);
        assert!(db.lookup_in(color, keyed, &[a]).is_some());
        assert!(db.lookup_in(color, keyed, &[b]).is_some());
        assert_eq!(db.find_in(color, a), db.find_in(color, b));
        if colored {
            assert_eq!(db.rows(member).count(), 2);
            assert_eq!(db.rows_in(sibling, member).count(), 2);
            assert_ne!(db.find(a), db.find(b));
            db.adopt(color);
            db.rebuild().unwrap();
            assert_eq!(db.find(a), db.find(b));
        }
    }
}

fn group_program(duplicate: bool) -> (Database, Program) {
    let mut db = Database::new();
    let group = db.add_sort("Group", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    let member = db.add_relation("Member", &[group, int]);
    let other = db.add_relation("OtherMember", &[group, int]);
    let keyed = db.add_relation("Keyed", &[group]);
    db.add_group(group, &[member, other], keyed).unwrap();
    let mut rules = Rules::new();
    let action = if duplicate {
        actions![(find-or-make (Member value) (Member value))]
    } else {
        actions![(find-or-make (Member 5))]
    };
    let facts = if duplicate {
        facts![(Member g value)]
    } else {
        facts![]
    };
    rules.rule(&db, "", facts, action).unwrap();
    rules
        .rule(
            &db,
            "distinct",
            facts![],
            actions![(find-or-make (Member 5) (Member 6))],
        )
        .unwrap();
    let program = rules.compile(&db).unwrap();
    (db, program)
}

#[test]
fn compiled_duplicate_members_find_existing_group() {
    let (mut db, mut program) = group_program(true);
    let member = db.schema().table_named("Member").unwrap();
    let keyed = db.schema().table_named("Keyed").unwrap();
    let row = [db.base(5i64)];
    db.find_or_make(&[(member, &row)]).unwrap();
    let bounded = Schedule::Repeat(span!(), 3, Box::new(Schedule::run("")));
    let report = program.run(&mut db, &bounded, &[]).unwrap();
    assert!(
        !report.updated,
        "finding an existing set must not write a redundant group"
    );
    assert_eq!(
        db.rows(keyed).count(),
        1,
        "duplicate membership denotes the same set"
    );
    program
        .run(&mut db, &Schedule::run("distinct"), &[])
        .unwrap();
    assert_eq!(
        db.rows(keyed).count(),
        2,
        "distinct member rows form another group"
    );
}

#[test]
fn serialized_group_runs_in_fresh_database() {
    let (_, program) = group_program(false);
    let mut db = Database::new();
    let mut loaded = Program::from_bytes(&program.to_bytes(), &mut db).unwrap();
    loaded.run(&mut db, &Schedule::run(""), &[]).unwrap();
    let member = db.schema().table_named("Member").unwrap();
    let keyed = db.schema().table_named("Keyed").unwrap();
    let other = db.schema().table_named("OtherMember").unwrap();
    let row = [db.base(5i64)];
    let found = db.find_group(&[(member, &row)]).unwrap().unwrap();
    let remade = db.find_or_make(&[(member, &row)]).unwrap();
    assert_eq!(found, remade);
    assert_eq!(db.rows(keyed).count(), 1);
    let other_row = [db.base(6i64)];
    db.find_or_make(&[(other, &other_row)]).unwrap();
    assert_eq!(db.rows(keyed).count(), 2);
}

#[test]
fn program_rejects_pre_group_metadata_format() {
    let (_, program) = group_program(false);
    let mut bytes = program.to_bytes();
    bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
    assert!(matches!(
        Program::from_bytes(&bytes, &mut Database::new()),
        Err(tir_egglog::LoadError::Version { found: 3, .. })
    ));
}

#[test]
fn loading_rejects_conflicting_group_membership() {
    let (_, program) = group_program(false);
    let mut db = Database::new();
    let group = db.add_sort("Group", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    let member = db.add_relation("Member", &[group, int]);
    db.add_relation("OtherMember", &[group, int]);
    let keyed = db.add_relation("Keyed", &[group]);
    db.add_group(group, &[member], keyed).unwrap();
    assert!(matches!(
        Program::from_bytes(&program.to_bytes(), &mut db),
        Err(tir_egglog::LoadError::TableMismatch { .. })
    ));
}

#[test]
fn compiled_duplicate_members_have_set_semantics_for_merge_effects() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tir_egglog::{Merge, Signature};
    let mut db = Database::new();
    let group = db.add_sort("G", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let merge = db.add_primitive(
        "merge",
        Signature {
            inputs: vec![int, int],
            output: int,
        },
        move |ctx, args| {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(ctx.base(ctx.base_value::<i64>(args[0]) + ctx.base_value::<i64>(args[1])))
        },
    );
    let member = db.add_function(
        "Member",
        &[group],
        int,
        Merge::Prim(merge, vec![Merge::Old, Merge::New]),
    );
    let keyed = db.add_relation("Keyed", &[group]);
    db.add_group(group, &[member], keyed).unwrap();
    let six = db.base(6i64);
    db.find_or_make(&[(member, &[six]), (member, &[six])])
        .unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "the Rust group API inserts a duplicated set member once"
    );
    let mut rules = Rules::new();
    rules
        .rule(
            &db,
            "",
            facts![],
            actions![(find-or-make (Member 5) (Member 5))],
        )
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    assert_eq!(db.rows(keyed).count(), 2);
    let mut values: Vec<i64> = db
        .rows(member)
        .map(|row| db.base_value::<i64>(row[1]))
        .collect();
    values.sort();
    assert_eq!(
        (values, count.load(Ordering::SeqCst)),
        (vec![5, 6], 0),
        "a duplicated set member is inserted once"
    );
}
