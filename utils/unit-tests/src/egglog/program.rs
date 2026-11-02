//! A compiled program saved to bytes and loaded back runs the same, and a
//! load checks the bytes and the database it binds to.

use tir_egglog::prelude::*;
use tir_egglog::{
    Database, LoadError, Merge, Mode, Program, Rules, Schedule, Signature, SortKind, Value,
};

fn primitives(db: &mut Database) {
    let int = db.sort("i64").unwrap();
    let signature = || Signature {
        inputs: vec![int, int],
        output: int,
    };
    db.add_primitive("+", signature(), |ctx, args| {
        let (a, b) = (
            ctx.base_value::<i64>(args[0]),
            ctx.base_value::<i64>(args[1]),
        );
        a.checked_add(b).map(|v| ctx.base(v))
    });
    db.add_primitive("*", signature(), |ctx, args| {
        let (a, b) = (
            ctx.base_value::<i64>(args[0]),
            ctx.base_value::<i64>(args[1]),
        );
        a.checked_mul(b).map(|v| ctx.base(v))
    });
}

// Arithmetic with constant folding and a relation of the folded terms.
fn schema() -> Database {
    let mut db = Database::new();
    primitives(&mut db);
    let math = db.add_sort("Math", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    db.add_constructor("Num", &[int], math);
    db.add_constructor("Add", &[math, math], math);
    db.add_constructor("Mul", &[math, math], math);
    db.add_relation("folded", &[math]);
    db.add_relation("unfolded", &[int]);
    db
}

fn rules(db: &Database) -> Rules {
    let mut rules = Rules::new();
    let mut rw = |lhs: Expr, rhs: Expr| rules.rewrite(db, "math", lhs, rhs).unwrap();
    rw(expr!((Add a b)), expr!((Add b a)));
    rw(expr!((Mul a b)), expr!((Mul b a)));
    rw(expr!((Mul a (Add b c))), expr!((Add (Mul a b) (Mul a c))));
    rw(expr!((Add (Num 0) a)), expr!(a));
    rules
        .rule(
            db,
            "fold",
            facts![(= e (Add (Num a) (Num b)))],
            actions![(union e (Num (+ a b))) (folded e)],
        )
        .unwrap();
    rules
        .rule(
            db,
            "fold",
            facts![(= e (Mul (Num a) (Num b)))],
            actions![(union e (Num (* a b))) (folded e)],
        )
        .unwrap();
    rules
        .naive_rule(
            db,
            "sweep",
            facts![(= e (Num n)) (not (folded e))],
            actions![(unfolded n)],
        )
        .unwrap();
    rules.set_mode("math", Mode::Relaxed);
    rules
}

fn terms(db: &mut Database) {
    let table = |db: &Database, name| db.table(name).unwrap();
    let num = |db: &mut Database, n: i64| {
        let n = db.base(n);
        db.entry(table(db, "Num"), &[n]).unwrap()
    };
    let (one, two, three, x) = (num(db, 1), num(db, 2), num(db, 3), num(db, 40));
    let sum = db.entry(table(db, "Add"), &[two, three]).unwrap();
    let product = db.entry(table(db, "Mul"), &[x, sum]).unwrap();
    db.entry(table(db, "Add"), &[product, one]).unwrap();
}

fn rows(db: &Database) -> Vec<(String, Vec<Vec<Value>>)> {
    let names = ["Num", "Add", "Mul", "folded", "unfolded"];
    (names.iter())
        .map(|&name| {
            let table = db.table(name).unwrap();
            let mut rows: Vec<Vec<Value>> =
                db.rows(table).map(|row| row.values().collect()).collect();
            rows.sort();
            (name.to_owned(), rows)
        })
        .collect()
}

fn schedule() -> Schedule {
    Schedule::saturate(Schedule::seq([
        Schedule::repeat(3, "math"),
        Schedule::saturate("fold"),
        Schedule::run("sweep"),
    ]))
}

#[test]
fn a_loaded_program_runs_like_the_compiled_one() {
    let mut compiled_db = schema();
    let mut compiled = rules(&compiled_db).compile(&compiled_db).unwrap();
    let bytes = compiled.to_bytes();
    terms(&mut compiled_db);
    compiled.run(&mut compiled_db, &schedule(), &[]).unwrap();

    for alignment in [None, Some(0), Some(1)] {
        let mut loaded_db = schema();
        terms(&mut loaded_db);
        let mut loaded = load_program(&bytes, &mut loaded_db, alignment).unwrap();
        assert_eq!(loaded.to_bytes(), bytes);
        loaded.run(&mut loaded_db, &schedule(), &[]).unwrap();

        assert_eq!(rows(&loaded_db), rows(&compiled_db));
        let folded = loaded_db.table("folded").unwrap();
        assert!(loaded_db.rows(folded).count() > 0);
        let unfolded = loaded_db.table("unfolded").unwrap();
        let mut values: Vec<i64> = loaded_db
            .rows(unfolded)
            .map(|r| loaded_db.base_value(r[0]))
            .collect();
        values.sort_unstable();
        assert_eq!(values, [1, 2, 3, 40]);
    }
}

#[test]
fn a_load_declares_the_tables_the_database_lacks() {
    let bytes = {
        let db = schema();
        rules(&db).compile(&db).unwrap().to_bytes()
    };
    let mut db = Database::new();
    primitives(&mut db);
    Program::from_bytes(&bytes, &mut db).unwrap();
    let math = db.sort("Math").unwrap();
    let int = db.sort("i64").unwrap();
    let columns = |name| {
        db.schema()
            .table(db.table(name).unwrap())
            .columns()
            .to_vec()
    };
    assert_eq!(columns("Num"), [int, math]);
    assert_eq!(columns("Add"), [math, math, math]);
    assert_eq!(columns("Mul"), [math, math, math]);
    assert_eq!(columns("folded"), [math]);
}

fn load(bytes: &[u8], db: &mut Database) -> LoadError {
    Program::from_bytes(bytes, db).err().unwrap()
}

#[test]
fn a_load_rejects_bad_bytes_and_mismatched_databases() {
    let words: [u32; 23] = [
        0, 0, 0, 0, 0, 0, 2, 0, 10, 17, 0, 0, 0, 0, 0, 0, 0, 99, 0, 0, 0, 0, 0,
    ];
    let bytes: Vec<u8> = [u32::from_le_bytes(*b"TEGG"), 11, 0]
        .into_iter()
        .chain(words)
        .flat_map(u32::to_le_bytes)
        .collect();
    for alignment in [None, Some(0), Some(1)] {
        assert_eq!(
            load_program(&bytes, &mut Database::new(), alignment)
                .err()
                .unwrap(),
            LoadError::Malformed("step")
        );
    }

    let bytes = {
        let db = schema();
        rules(&db).compile(&db).unwrap().to_bytes()
    };

    let mut magic = bytes.clone();
    magic[0] = b'X';
    assert!(matches!(
        load(&magic, &mut schema()),
        LoadError::Malformed(_)
    ));

    for found in [4_u32, 6, 7, 8, 99] {
        let mut version = bytes.clone();
        version[4..8].copy_from_slice(&found.to_le_bytes());
        assert!(
            matches!(load(&version, &mut schema()), LoadError::Version { found: actual, .. } if actual == found)
        );
    }

    let truncated = &bytes[..bytes.len() - 4];
    assert!(matches!(
        load(truncated, &mut schema()),
        LoadError::Malformed(_)
    ));

    assert_eq!(
        load(&bytes, &mut Database::new()),
        LoadError::MissingExtern {
            name: "+".into(),
            signature: "(i64 i64) i64".into(),
        }
    );

    let mut db = Database::new();
    primitives(&mut db);
    let math = db.add_sort("Math", SortKind::Eq);
    db.add_constructor("Add", &[math], math);
    assert_eq!(
        load(&bytes, &mut db),
        LoadError::TableMismatch { name: "Add".into() }
    );

    let mut db = Database::new();
    primitives(&mut db);
    db.add_sort("Math", SortKind::base::<i64>());
    assert_eq!(
        load(&bytes, &mut db),
        LoadError::SortMismatch {
            name: "Math".into()
        }
    );
}

// A database with the BigInt sort and the egglog primitives, where `pad`
// strings and integers were interned before anything else.
fn egglog_database(pad: &[&str], ints: &[i64]) -> Database {
    let db = tir_egglog::EGraph::default().database().clone();
    for &s in pad {
        db.base(s.to_owned());
    }
    for &i in ints {
        db.base(tir_egglog::sort::BigInt::new(i));
    }
    db
}

fn big(digits: &str) -> tir_egglog::sort::BigInt {
    digits.parse().unwrap()
}

// A string-valued merge constant, string and BigInt defaults and a string
// literal of a rule, saved where those values were interned in one order
// and loaded where other values came first.
#[test]
fn constants_keep_their_meaning_across_databases() {
    let mut offline = egglog_database(&["a", "b"], &[]);
    let (int, string) = (
        offline.sort("i64").unwrap(),
        offline.sort("String").unwrap(),
    );
    let bigint = offline.sort("BigInt").unwrap();
    let merged = offline.base("merged".to_owned());
    let label = offline.add_function("label", &[int], string, Merge::Const(merged));
    let fallback = offline.base("fallback".to_owned());
    offline.set_default(label, fallback);
    let large = offline.base(big("123456789012345678901234567890"));
    let wide = offline.add_function("wide", &[int], bigint, Merge::AssertEq);
    offline.set_default(wide, large);
    offline.add_relation("item", &[int]);
    offline.add_relation("out", &[string]);
    offline.add_relation("outbig", &[bigint]);
    let mut rules = Rules::new();
    rules
        .rule(
            &offline,
            "read",
            facts![(item x)],
            actions![(out (label x)) (outbig (wide x))],
        )
        .unwrap();
    let lit = exprs::string("lit");
    rules
        .rule(
            &offline,
            "write",
            facts![(item x)],
            actions![(set (label x) (unquote lit))],
        )
        .unwrap();
    let bytes = rules.compile(&offline).unwrap().to_bytes();

    let mut db = egglog_database(&["zzz", "yyy", "lit"], &[5, 7]);
    let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
    let one = db.base(1_i64);
    db.insert(db.table("item").unwrap(), &[one]).unwrap();
    program.run(&mut db, &Schedule::run("read"), &[]).unwrap();
    let strings: Vec<String> = (db.rows(db.table("out").unwrap()))
        .map(|row| db.base_value(row[0]))
        .collect();
    assert_eq!(strings, ["fallback"], "the default of label");
    let bigs: Vec<tir_egglog::sort::BigInt> = (db.rows(db.table("outbig").unwrap()))
        .map(|row| db.base_value(row[0]))
        .collect();
    assert_eq!(
        bigs,
        [big("123456789012345678901234567890")],
        "the default of wide"
    );
    program.run(&mut db, &Schedule::run("write"), &[]).unwrap();
    let label = db.lookup(db.table("label").unwrap(), &[one]).unwrap();
    assert_eq!(
        db.base_value::<String>(label),
        "merged",
        "the merge constant"
    );
}

// A container default keeps its elements, and an e-class has no constant.
#[test]
fn container_constants_load_by_their_elements() {
    let vec_sort = || tir_egglog::ast::Command::Sort {
        span: span!(),
        name: "IVec".into(),
        presort_and_args: Some(("Vec".into(), vec![exprs::var("i64")])),
    };
    let mut offline = tir_egglog::EGraph::default();
    offline.run_program(vec![vec_sort()]).unwrap();
    let db = offline.database_mut();
    let (int, ivec) = (db.sort("i64").unwrap(), db.sort("IVec").unwrap());
    let data = [7_i64, 8].map(|i| db.base(i)).to_vec();
    let elems = tir_egglog::sort::VecContainer::new(data, [false, false]);
    let vector = db.container(elems);
    let f = db.add_function("f", &[int], ivec, Merge::AssertEq);
    db.set_default(f, vector);
    db.add_relation("out", &[ivec]);
    let mut rules = Rules::new();
    rules.rule(db, "", facts![], actions![(out (f 1))]).unwrap();
    let bytes = rules.compile(db).unwrap().to_bytes();

    let mut online = tir_egglog::EGraph::default();
    online.run_program(vec![vec_sort()]).unwrap();
    let db = online.database_mut();
    db.base(1000_i64 << 40);
    let mut program = Program::from_bytes(&bytes, db).unwrap();
    program.run(db, &Schedule::run(""), &[]).unwrap();
    let out: Vec<Vec<i64>> = (db.rows(db.table("out").unwrap()))
        .map(|row| {
            let vector = db
                .container_value::<tir_egglog::sort::VecContainer>(row[0])
                .unwrap();
            vector.data.iter().map(|&v| db.base_value(v)).collect()
        })
        .collect();
    assert_eq!(out, [vec![7, 8]]);

    let mut db = Database::new();
    let math = db.add_sort("Math", SortKind::Eq);
    let class = db.fresh(math);
    let g = db.add_function("g", &[], math, Merge::AssertEq);
    db.set_default(g, class);
    db.add_relation("seen", &[math]);
    let mut rules = Rules::new();
    rules.rule(&db, "", facts![], actions![(seen(g))]).unwrap();
    assert!(matches!(
        rules.compile(&db),
        Err(tir_egglog::Error::Unsupported(_))
    ));
}

// A rule over rows that did not change matches them again when a run
// argument it reads changed.
#[test]
fn a_changed_argument_matches_old_rows_again() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let item = db.add_relation("item", &[int]);
    let hit = db.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    let arg = exprs::arg(0);
    rules
        .rule(
            &db,
            "",
            facts![(item x) (= x (unquote arg))],
            actions![(hit x)],
        )
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    for i in [1_i64, 2] {
        let row = [db.base(i)];
        db.insert(item, &row).unwrap();
    }
    for i in [1_i64, 2] {
        let arg = db.base(i);
        program.run(&mut db, &Schedule::run(""), &[arg]).unwrap();
    }
    let mut hits: Vec<i64> = db.rows(hit).map(|row| db.base_value(row[0])).collect();
    hits.sort_unstable();
    assert_eq!(hits, [1, 2]);
}

#[test]
fn loaded_negation_observes_grouped_indexes_and_color_visibility() {
    let mut db = Database::new();
    primitives(&mut db);
    let int = db.sort("i64").unwrap();
    let item = db.add_relation("item", &[int]);
    let blocked = db.add_relation("blocked", &[int]);
    let hit = db.add_relation("hit", &[int]);
    db.add_kept_index(blocked, &[0], &[]);
    let mut rules = Rules::new();
    let arg = exprs::arg(0);
    rules
        .naive_rule(
            &db,
            "",
            facts![(item x) (= n (+ (unquote arg) 0)) (not (blocked x))],
            actions![(hit n)],
        )
        .unwrap();
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
    let one = db.base(1_i64);
    let two = db.base(2_i64);
    db.insert(item, &[one]).unwrap();
    db.insert(blocked, &[one]).unwrap();
    let hidden = db.color(&[]);
    let own = db.color(&[]);
    db.remove_in(hidden, blocked, &[one]);
    db.insert_in(own, item, &[two]).unwrap();
    db.insert_in(own, blocked, &[two]).unwrap();
    program.run(&mut db, &Schedule::run(""), &[one]).unwrap();
    assert_eq!(db.rows(hit).count(), 0);
    assert_eq!(
        db.rows_in(hidden, hit).map(|r| r[0]).collect::<Vec<_>>(),
        [one]
    );
    assert_eq!(db.rows_in(own, hit).count(), 0);
    program.run(&mut db, &Schedule::run(""), &[two]).unwrap();
    let mut hits: Vec<_> = db.rows_in(hidden, hit).map(|r| r[0]).collect();
    hits.sort();
    assert_eq!(hits, [one, two]);
    db.remove(blocked, &[one]);
    program.run(&mut db, &Schedule::run(""), &[two]).unwrap();
    assert_eq!(db.rows(hit).map(|r| r[0]).collect::<Vec<_>>(), [two]);
}

#[test]
fn absence_only_rules_recheck_after_a_table_changes() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let blocked = db.add_relation("blocked", &[int]);
    let hit = db.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    rules
        .rule(&db, "", facts![(not (blocked 1))], actions![(hit 1)])
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    let one = db.base(1_i64);
    db.insert(blocked, &[one]).unwrap();
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    assert_eq!(db.rows(hit).count(), 0);
    db.remove(blocked, &[one]);
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    assert_eq!(db.rows(hit).map(|r| r[0]).collect::<Vec<_>>(), [one]);
}

#[test]
fn loaded_delta_rules_start_only_from_the_named_retirement_log() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let source = db.add_relation("source", &[int]);
    let retired = db.add_relation("retired", &[int]);
    let item = db.add_relation("item", &[int]);
    let hit = db.add_relation("hit", &[int]);
    db.log_retired(source, 0, retired);
    let mut rules = Rules::new();
    rules
        .delta_rule(
            &db,
            "",
            &[retired],
            facts![(retired x) (item x) (not (source x))],
            actions![(hit x)],
        )
        .unwrap();
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
    let one = db.base(1_i64);
    db.insert(source, &[one]).unwrap();
    db.remove(source, &[one]);
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    assert_eq!(db.rows(retired).map(|r| r[0]).collect::<Vec<_>>(), [one]);
    db.insert(item, &[one]).unwrap();
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    assert_eq!(db.rows(hit).count(), 0);
    db.insert(source, &[one]).unwrap();
    db.remove(retired, &[one]);
    db.rebuild().unwrap();
    db.remove(source, &[one]);
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    assert_eq!(db.rows(hit).map(|r| r[0]).collect::<Vec<_>>(), [one]);
}

#[test]
fn negation_requires_a_completed_stratum() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let item = db.add_relation("item", &[int]);
    let live = db.add_relation("live", &[int]);
    let dead = db.add_relation("dead", &[int]);
    let mut rules = Rules::new();
    rules
        .rule(&db, "mark", facts![(item 1)], actions![(live 1)])
        .unwrap();
    rules
        .rule(
            &db,
            "sweep",
            facts![(item x) (not (live x))],
            actions![(dead x)],
        )
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    for n in [1_i64, 2] {
        let n = db.base(n);
        db.insert(item, &[n]).unwrap();
    }
    program
        .run(
            &mut db,
            &Schedule::seq([Schedule::saturate("mark"), Schedule::run("sweep")]),
            &[],
        )
        .unwrap();
    assert_eq!(
        db.rows(live)
            .map(|r| db.base_value::<i64>(r[0]))
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(
        db.rows(dead)
            .map(|r| db.base_value::<i64>(r[0]))
            .collect::<Vec<_>>(),
        [2]
    );
    rules
        .add_combined_ruleset("both", &["mark".into(), "sweep".into()])
        .unwrap();
    assert!(matches!(
        rules.compile(&db),
        Err(tir_egglog::Error::Unsupported(_))
    ));
    rules
        .rule(&db, "sweep", facts![(item x)], actions![(live x)])
        .unwrap();
    assert!(matches!(
        rules.compile(&db),
        Err(tir_egglog::Error::Unsupported(_))
    ));
}

#[test]
fn a_negative_premise_prevents_an_unconditional_table_symmetry() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let pair = db.add_relation("pair", &[int, int]);
    let blocked = db.add_relation("blocked", &[int]);
    let mut rules = Rules::new();
    rules
        .rule(
            &db,
            "",
            facts![(pair a b) (not (blocked a))],
            actions![(pair b a)],
        )
        .unwrap();
    rules.set_mode("", Mode::Relaxed);
    let mut program = rules.compile(&db).unwrap();
    let [one, two, three, four] = [1_i64, 2, 3, 4].map(|n| db.base(n));
    db.insert(blocked, &[one]).unwrap();
    db.insert(pair, &[one, two]).unwrap();
    db.insert(pair, &[three, four]).unwrap();
    program.run(&mut db, &Schedule::saturate(""), &[]).unwrap();
    let mut pairs: Vec<_> = db
        .rows(pair)
        .map(|r| [db.base_value::<i64>(r[0]), db.base_value::<i64>(r[1])])
        .collect();
    pairs.sort_unstable();
    assert_eq!(pairs, [[1, 2], [3, 4], [4, 3]]);
}

#[test]
fn loaded_failed_constant_premises_drop_only_their_own_matches() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let blocked = db.add_relation("blocked", &[int]);
    let hit = db.add_relation("hit", &[int]);
    db.add_primitive(
        "fail",
        Signature {
            inputs: vec![],
            output: int,
        },
        |_, _| None,
    );
    for (name, n) in [("one", 1_i64), ("two", 2)] {
        db.add_primitive(
            name,
            Signature {
                inputs: vec![],
                output: int,
            },
            move |ctx, _| Some(ctx.base(n)),
        );
    }
    let mut rules = Rules::new();
    for (premise, action) in [
        (facts![(= n (fail))], actions![(hit 1)]),
        (facts![(= n (fail)) (not (blocked n))], actions![(hit 2)]),
        (facts![], actions![(hit 3)]),
        (facts![(= n (one)) (= n (two))], actions![(hit 4)]),
        (facts![(blocked n) (= n (one))], actions![(hit 5)]),
    ] {
        rules.rule(&db, "", premise, action).unwrap();
    }
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    let mut hits: Vec<i64> = db.rows(hit).map(|row| db.base_value(row[0])).collect();
    hits.sort_unstable();
    assert_eq!(hits, [3]);
    assert_eq!(db.rows(blocked).count(), 0);
}

#[test]
fn loaded_primitives_preserve_full_width_scalar_cells() {
    let mut db = Database::new();
    let scalar = db.add_sort("u32", SortKind::base::<u32>());
    let item = db.add_relation("item", &[scalar]);
    let hit = db.add_relation("hit", &[scalar]);
    let chain = db.add_relation("chain", &[scalar]);
    db.add_primitive(
        "max",
        Signature {
            inputs: vec![],
            output: scalar,
        },
        |ctx, _| Some(ctx.base(u32::MAX)),
    );
    db.add_primitive(
        "id",
        Signature {
            inputs: vec![scalar],
            output: scalar,
        },
        |_, args| Some(args[0]),
    );
    db.add_primitive(
        "inc",
        Signature {
            inputs: vec![scalar],
            output: scalar,
        },
        |ctx, args| Some(ctx.base(ctx.base_value::<u32>(args[0]).wrapping_add(1))),
    );
    let mut rules = Rules::new();
    rules
        .rule(&db, "", facts![(item x) (= y (id x))], actions![(hit y)])
        .unwrap();
    rules
        .rule(
            &db,
            "",
            facts![(= x (max)) (= y (id x)) (= z (inc y))],
            actions![(chain z)],
        )
        .unwrap();
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
    let max = db.base(u32::MAX);
    db.insert(item, &[max]).unwrap();
    program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    let bound: Vec<u32> = db.rows(hit).map(|row| db.base_value(row[0])).collect();
    let computed: Vec<u32> = db.rows(chain).map(|row| db.base_value(row[0])).collect();
    assert_eq!((bound, computed), (vec![u32::MAX], vec![0]));
}

#[test]
fn loaded_read_primitives_recheck_rows_and_require_purity_in_colors() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let source = db.add_function("source", &[], int, Merge::New);
    let hit = db.add_relation("hit", &[int]);
    db.add_primitive_in(
        "read",
        Signature {
            inputs: vec![],
            output: int,
        },
        &[tir_egglog::Context::Read],
        move |ctx, _| ctx.lookup(source, &[]),
    );
    let mut rules = Rules::new();
    rules
        .naive_rule(&db, "", facts![(= n (read))], actions![(hit n)])
        .unwrap();
    rules
        .naive_rule(&db, "colored", facts![(= n (read))], actions![(hit n)])
        .unwrap();
    rules.set_colored("colored");
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
    for n in [1_i64, 2] {
        let value = db.base(n);
        db.insert(source, &[value]).unwrap();
        db.rebuild().unwrap();
        program.run(&mut db, &Schedule::run(""), &[]).unwrap();
    }
    let mut hits: Vec<i64> = db.rows(hit).map(|row| db.base_value(row[0])).collect();
    hits.sort_unstable();
    db.color(&[]);
    let impure = matches!(
        program.run(&mut db, &Schedule::run("colored"), &[]),
        Err(tir_egglog::Error::Unsupported(_))
    );
    assert_eq!((hits, impure), (vec![1, 2], true));
}

#[test]
fn a_load_rejects_invalid_absent_operands_and_computed_references() {
    let mut db = Database::new();
    primitives(&mut db);
    let int = db.sort("i64").unwrap();
    db.add_relation("blocked", &[int]);
    db.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    rules
        .naive_rule(&db, "", facts![(not (blocked 17))], actions![(hit 23)])
        .unwrap();
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_le_bytes)
        .collect();
    // Opcode 6 has an index, an unused word, then its key's length and registers.
    let absent = words.windows(4).rposition(|w| w == [6, 0, 0, 1]).unwrap();
    let mut rejected = Vec::new();
    let mut expected = Vec::new();
    for (offset, value, error) in [
        (1, u32::MAX, "index"),
        (3, 0, "check"),
        (4, u32::MAX, "register"),
    ] {
        let mut bad = bytes.clone();
        let at = (absent + offset) * 4;
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        if offset == 3 {
            bad.drain((absent + 4) * 4..(absent + 5) * 4);
        }
        rejected.push(load(&bad, &mut db));
        expected.push(LoadError::Malformed(error));
        let bad = Box::leak(bad.into_boxed_slice());
        rejected.push(Program::from_static(bad, &mut db).err().unwrap());
        expected.push(LoadError::Malformed(error));
    }

    db.add_relation("input", &[int]);
    db.add_primitive(
        "identity",
        Signature {
            inputs: vec![int],
            output: int,
        },
        |_, args| Some(args[0]),
    );
    let mut rules = Rules::new();
    rules
        .rule(&db, "", facts![(= n (+ 17 23))], actions![(hit n)])
        .unwrap();
    rules
        .rule(
            &db,
            "",
            facts![(input x) (= y (identity x))],
            actions![(hit y)],
        )
        .unwrap();
    let bytes = rules.compile(&db).unwrap().to_bytes();
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_le_bytes)
        .collect();
    // One computed call of primitive zero with two literal registers.
    let call = words.windows(5).position(|w| w == [1, 0, 2, 0, 1]).unwrap();
    for (offset, value) in [(1, u32::MAX), (3, u32::MAX), (1, 1)] {
        let mut bad = bytes.clone();
        let at = (call + offset) * 4;
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        rejected.push(load(&bad, &mut db));
        expected.push(LoadError::Malformed("call"));
        let bad = Box::leak(bad.into_boxed_slice());
        rejected.push(Program::from_static(bad, &mut db).err().unwrap());
        expected.push(LoadError::Malformed("call"));
    }
    assert_eq!(rejected, expected);
}

fn load_program(
    bytes: &[u8],
    db: &mut Database,
    alignment: Option<usize>,
) -> Result<Program, LoadError> {
    let Some(misalignment) = alignment else {
        return Program::from_bytes(bytes, db);
    };
    let mut storage = vec![0; bytes.len() + 4];
    let offset = (4 - storage.as_ptr().addr() % 4) % 4 + misalignment;
    storage[offset..offset + bytes.len()].copy_from_slice(bytes);
    let storage = Box::leak(storage.into_boxed_slice());
    Program::from_static(&storage[offset..offset + bytes.len()], db)
}

#[test]
fn computed_calls_require_purity_when_loaded_and_after_callback_replacement() {
    let mut offline = Database::new();
    let int = offline.sort("i64").unwrap();
    let signature = || Signature {
        inputs: vec![],
        output: int,
    };
    offline.add_primitive_in(
        "read",
        signature(),
        &[tir_egglog::Context::Pure, tir_egglog::Context::Read],
        |ctx, _| Some(ctx.base(1_i64)),
    );
    offline.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    rules
        .naive_rule(&offline, "", facts![(= n (read))], actions![(hit n)])
        .unwrap();
    let bytes = rules.compile(&offline).unwrap().to_bytes();
    let mut rejected_loads = Vec::new();
    let mut rejected_runs = Vec::new();
    for alignment in [None, Some(0), Some(1)] {
        let mut db = Database::new();
        let source = db.add_function("source", &[], int, Merge::New);
        db.add_primitive_in(
            "read",
            signature(),
            &[tir_egglog::Context::Read],
            move |ctx, _| ctx.lookup(source, &[]),
        );
        rejected_loads.push(matches!(load_program(&bytes, &mut db, alignment), Err(LoadError::ExternContext { name, context: tir_egglog::Context::Pure }) if name == "read"));
        for prepared in [false, true] {
            let mut db = Database::new();
            let source = db.add_function("source", &[], int, Merge::New);
            db.add_primitive_in(
                "read",
                signature(),
                &[tir_egglog::Context::Pure, tir_egglog::Context::Read],
                |ctx, _| Some(ctx.base(1_i64)),
            );
            let mut program = load_program(&bytes, &mut db, alignment).unwrap();
            if prepared {
                program.run(&mut db, &Schedule::run(""), &[]).unwrap();
            }
            db.add_primitive_in(
                "read",
                signature(),
                &[tir_egglog::Context::Read],
                move |ctx, _| ctx.lookup(source, &[]),
            );
            rejected_runs.push(matches!(
                program.run(&mut db, &Schedule::run(""), &[]),
                Err(tir_egglog::Error::Unsupported(reason)) if reason.contains("primitive `read`") && reason.contains("Pure")
            ));
        }
    }
    assert_eq!(
        (rejected_loads, rejected_runs),
        (vec![true; 3], vec![true; 6])
    );
}

#[test]
fn computed_value_consumers_require_a_dominating_success_guard() {
    let mut rejected = Vec::new();
    let mut errors = Vec::new();
    for absent in [false, true] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        db.add_primitive(
            "fail",
            Signature {
                inputs: vec![],
                output: int,
            },
            |_, _| None,
        );
        db.add_relation("blocked", &[int]);
        let hit = db.add_relation("hit", &[int]);
        let mut rules = Rules::new();
        let query = if absent {
            facts![(= n (fail)) (not (blocked n))]
        } else {
            facts![(= n (fail))]
        };
        rules.rule(&db, "", query, actions![(hit n)]).unwrap();
        let bytes = rules.compile(&db).unwrap().to_bytes();
        let mut program = Program::from_bytes(&bytes, &mut db).unwrap();
        program.run(&mut db, &Schedule::run(""), &[]).unwrap();
        assert_eq!(db.rows(hit).count(), 0);
        let (words, _) = bytes.as_chunks::<4>();
        let words: Vec<_> = words.iter().map(|word| u32::from_le_bytes(*word)).collect();
        let guard = words
            .windows(3)
            .rposition(|words| words == [4, 1, 1])
            .unwrap();
        let mut bad = bytes.clone();
        bad[(guard + 1) * 4..(guard + 3) * 4].fill(0);
        for alignment in [None, Some(0), Some(1)] {
            let error = load_program(&bad, &mut db, alignment).err();
            rejected.push(matches!(
                &error,
                Some(LoadError::Malformed("computed register"))
            ));
            errors.push(format!("{error:?}"));
        }
    }
    assert_eq!(rejected, vec![true; 6], "{errors:?}");
}

#[test]
fn egraph_steps_reject_impure_replacements_before_and_after_preparation() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let mut offline = Database::new();
    let int = offline.sort("i64").unwrap();
    let signature = || Signature {
        inputs: vec![],
        output: int,
    };
    offline.add_primitive("read", signature(), |ctx, _| Some(ctx.base(1_i64)));
    offline.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    rules
        .naive_rule(&offline, "", facts![(= n (read))], actions![(hit n)])
        .unwrap();
    let bytes = rules.compile(&offline).unwrap().to_bytes();
    let mut rejected = Vec::new();
    for prepared in [false, true] {
        let mut graph = tir_egglog::EGraph::default();
        graph
            .database_mut()
            .add_primitive("read", signature(), |ctx, _| Some(ctx.base(1_i64)));
        graph.load_program(&bytes).unwrap();
        if prepared {
            graph.step_rules("").unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        graph.database_mut().add_primitive_in(
            "read",
            signature(),
            &[tir_egglog::Context::Read],
            move |ctx, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Some(ctx.base(2_i64))
            },
        );
        let rejected_for_purity = matches!(graph.step_rules(""), Err(tir_egglog::Error::Unsupported(reason)) if reason.contains("primitive `read`") && reason.contains("Pure"));
        rejected.push((rejected_for_purity, calls.load(Ordering::SeqCst)));
    }
    assert_eq!(rejected, vec![(true, 0); 2]);
}

#[test]
fn pure_callback_changes_invalidate_prepared_clones_and_recompiled_history() {
    let mut offline = Database::new();
    let int = offline.sort("i64").unwrap();
    let signature = || Signature {
        inputs: vec![],
        output: int,
    };
    offline.add_primitive("value", signature(), |ctx, _| Some(ctx.base(1_i64)));
    let hit = offline.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    rules
        .rule(&offline, "", facts![(= n (value))], actions![(hit n)])
        .unwrap();
    let bytes = rules.compile(&offline).unwrap().to_bytes();
    enum Reuse {
        Prepared,
        Clone,
        Recompile,
    }
    let mut observed = Vec::new();
    for alignment in [None, Some(0), Some(1)] {
        for reuse in [Reuse::Prepared, Reuse::Clone, Reuse::Recompile] {
            let mut db = offline.clone();
            let mut program = load_program(&bytes, &mut db, alignment).unwrap();
            program.run(&mut db, &Schedule::run(""), &[]).unwrap();
            if matches!(reuse, Reuse::Clone) {
                program = program.clone();
            }
            db.add_primitive("value", signature(), |ctx, _| Some(ctx.base(2_i64)));
            if matches!(reuse, Reuse::Recompile) {
                program = rules.recompile(&db, &program).unwrap();
            }
            program.run(&mut db, &Schedule::run(""), &[]).unwrap();
            let mut values: Vec<i64> = db.rows(hit).map(|row| db.base_value(row[0])).collect();
            values.sort_unstable();
            observed.push(values);
        }
    }
    assert_eq!(observed, vec![vec![1, 2]; 9]);
    let mut db = offline.clone();
    let program = rules.compile(&db).unwrap();
    db.add_primitive_in(
        "value",
        signature(),
        &[tir_egglog::Context::Read],
        |ctx, _| Some(ctx.base(2_i64)),
    );
    let rejected: Vec<_> = [rules.recompile(&db, &program), rules.compile(&db)]
        .into_iter().map(|result| matches!(result, Err(tir_egglog::Error::Unsupported(reason)) if reason.contains("`value`") && reason.contains("Pure"))).collect();
    assert_eq!(rejected, vec![true; 2]);
}

#[test]
fn loaded_index_metadata_covers_every_index_the_rules_read() {
    let mut offline = Database::new();
    let int = offline.sort("i64").unwrap();
    let blocked = offline.add_relation("blocked", &[int]);
    offline.add_relation("other", &[int]);
    let hit = offline.add_relation("hit", &[int]);
    let mut rules = Rules::new();
    rules
        .naive_rule(
            &offline,
            "absence",
            facts![(not (blocked 17))],
            actions![(hit 17)],
        )
        .unwrap();
    rules
        .naive_rule(
            &offline,
            "other",
            facts![(not (other 17))],
            actions![(hit 23)],
        )
        .unwrap();
    let bytes = rules.compile(&offline).unwrap().to_bytes();
    let mut cursor = 12;
    let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let mut name = None;
    for id in 0..count {
        let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        if &bytes[cursor..cursor + len] == b"absence" {
            name = Some(id);
        }
        cursor = (cursor + len).next_multiple_of(4);
    }
    let (words, _) = bytes[cursor..].as_chunks::<4>();
    let words: Vec<_> = words.iter().map(|word| u32::from_le_bytes(*word)).collect();
    // Name, one root, no first roots, one used index, one rule name.
    let positions: Vec<_> = words
        .windows(7)
        .enumerate()
        .filter_map(|(at, words)| {
            (words[0] == name.unwrap() && words[1] == 1 && words[3..] == [0, 1, 0, 1]).then_some(at)
        })
        .collect();
    assert_eq!(positions.len(), 1);
    let at = cursor + (positions[0] + 5) * 4;
    let mut bad = bytes.clone();
    bad[at..at + 4].copy_from_slice(&1_u32.to_le_bytes());
    let mut observed = Vec::new();
    for alignment in [None, Some(0), Some(1)] {
        let mut db = offline.clone();
        let value = db.base(17_i64);
        db.insert(blocked, &[value]).unwrap();
        let mut valid = load_program(&bytes, &mut db, alignment).unwrap();
        valid.run(&mut db, &Schedule::run("absence"), &[]).unwrap();
        assert_eq!(db.rows(hit).count(), 0);
        let mut fresh = offline.clone();
        let value = fresh.base(17_i64);
        fresh.insert(blocked, &[value]).unwrap();
        match load_program(&bad, &mut fresh, alignment) {
            Err(LoadError::Malformed("used index")) => observed.push((true, None)),
            Ok(mut program) => {
                program
                    .run(&mut fresh, &Schedule::run("absence"), &[])
                    .unwrap();
                observed.push((false, Some(fresh.rows(hit).count())));
            }
            Err(other) => panic!("wrong rejection: {other}"),
        }
    }
    assert_eq!(observed, vec![(true, None); 3]);
}

#[test]
fn cached_callbacks_receive_bound_arguments_before_first_run_and_refresh() {
    use std::sync::{Arc, Mutex};
    for alignment in [None, Some(0), Some(1)] {
        let mut db = Database::new();
        let scalar = db.add_sort("u32", SortKind::base::<u32>());
        let hit = db.add_relation("hit", &[scalar]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let signature = || Signature {
            inputs: vec![scalar],
            output: scalar,
        };
        db.add_primitive("quotient", signature(), move |ctx, args| {
            let n = ctx.base_value::<u32>(args[0]);
            recorded.lock().unwrap().push(n);
            Some(ctx.base(100_u32 / n))
        });
        let mut rules = Rules::new();
        let arg = exprs::arg(0);
        rules
            .rule(
                &db,
                "",
                facts![(= n (quotient (unquote arg)))],
                actions![(hit n)],
            )
            .unwrap();
        let bytes = rules.compile(&db).unwrap().to_bytes();
        let mut program = load_program(&bytes, &mut db, alignment).unwrap();
        let two = db.base(2_u32);
        program.run(&mut db, &Schedule::run(""), &[two]).unwrap();
        let recorded = seen.clone();
        db.add_primitive("quotient", signature(), move |ctx, args| {
            let n = ctx.base_value::<u32>(args[0]);
            recorded.lock().unwrap().push(n);
            Some(ctx.base(200_u32 / (n - 2)))
        });
        let four = db.base(4_u32);
        program.run(&mut db, &Schedule::run(""), &[four]).unwrap();
        let mut values: Vec<u32> = db.rows(hit).map(|row| db.base_value(row[0])).collect();
        values.sort_unstable();
        assert_eq!(values, vec![50, 100]);
        let mut fresh = load_program(&bytes, &mut db, alignment).unwrap();
        assert!(
            matches!(fresh.run(&mut db, &Schedule::run(""), &[]), Err(tir_egglog::Error::Unsupported(reason)) if reason.contains("argument 0"))
        );
        assert_eq!(*seen.lock().unwrap(), vec![2, 4]);
    }
}

#[test]
fn noncached_query_and_action_bindings_retain_their_required_contexts() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    #[derive(Clone, Copy)]
    enum Site {
        Query,
        Action,
    }
    let mut observed = Vec::new();
    for site in [Site::Query, Site::Action] {
        for naive in [false, true] {
            let mut offline = Database::new();
            let int = offline.sort("i64").unwrap();
            let unit = offline.sort("Unit").unwrap();
            let item = offline.add_relation("item", &[int]);
            let hit = offline.add_relation("hit", &[int]);
            let source = offline.add_function("source", &[], int, Merge::New);
            let (name, output, replacement_context, required) = match site {
                Site::Query => ("read", int, tir_egglog::Context::Read, "Pure"),
                Site::Action => ("record", unit, tir_egglog::Context::Full, "Write"),
            };
            let signature = || Signature {
                inputs: vec![int],
                output,
            };
            match site {
                Site::Query => {
                    offline.add_primitive(name, signature(), |_, args| Some(args[0]));
                }
                Site::Action => {
                    offline.add_primitive(name, signature(), |ctx, _| Some(ctx.base(())));
                }
            }
            let (query, actions) = match site {
                Site::Query => (facts![(item x) (= y (read x))], actions![(hit y)]),
                Site::Action => (facts![(item x)], actions![(record x) (hit x)]),
            };
            let mut rules = Rules::new();
            if naive {
                rules.naive_rule(&offline, "", query, actions).unwrap();
            } else {
                rules.rule(&offline, "", query, actions).unwrap();
            }
            let bytes = rules.compile(&offline).unwrap().to_bytes();
            for alignment in [None, Some(0), Some(1)] {
                for prepared in [None, Some(false), Some(true)] {
                    let mut db = offline.clone();
                    let one = db.base(1_i64);
                    let two = db.base(2_i64);
                    db.insert(item, &[one]).unwrap();
                    db.insert(source, &[two]).unwrap();
                    let mut program = prepared.map(|run| {
                        let mut program = load_program(&bytes, &mut db, alignment).unwrap();
                        if run {
                            program.run(&mut db, &Schedule::run(""), &[]).unwrap();
                        }
                        program
                    });
                    let calls = Arc::new(AtomicUsize::new(0));
                    let counted = calls.clone();
                    let contexts: &'static [tir_egglog::Context] = match replacement_context {
                        tir_egglog::Context::Read => &[tir_egglog::Context::Read],
                        _ => &[tir_egglog::Context::Full],
                    };
                    db.add_primitive_in(name, signature(), contexts, move |ctx, _| {
                        counted.fetch_add(1, Ordering::SeqCst);
                        match site {
                            Site::Query => ctx.lookup(source, &[]),
                            Site::Action => Some(ctx.base(())),
                        }
                    });
                    let error = if program.is_none() {
                        match load_program(&bytes, &mut db, alignment) {
                            Ok(loaded) => {
                                program = Some(loaded);
                                None
                            }
                            Err(error) => Some(error.to_string()),
                        }
                    } else {
                        None
                    };
                    let error = error.or_else(|| {
                        program
                            .as_mut()
                            .unwrap()
                            .run(&mut db, &Schedule::run(""), &[])
                            .err()
                            .map(|error| error.to_string())
                    });
                    if naive {
                        assert_eq!(error, None);
                        assert_eq!(calls.load(Ordering::SeqCst), 1);
                        if matches!(site, Site::Query) {
                            let mut values: Vec<i64> =
                                db.rows(hit).map(|row| db.base_value(row[0])).collect();
                            values.sort_unstable();
                            assert_eq!(
                                values,
                                if prepared == Some(true) {
                                    vec![1, 2]
                                } else {
                                    vec![2]
                                }
                            );
                        }
                    } else {
                        observed.push(
                            error.is_some_and(|reason| {
                                reason.contains(name) && reason.contains(required)
                            }) && calls.load(Ordering::SeqCst) == 0,
                        );
                    }
                }
            }
        }
    }
    assert_eq!(observed, vec![true; 18]);
}

#[test]
fn zero_argument_fact_is_written_by_a_rule() {
    let mut db = Database::new();
    let ready = db.add_relation("ready", &[]);
    let mut rules = Rules::new();
    rules.rule(&db, "", facts![], actions![(ready)]).unwrap();
    let mut program = rules.compile(&db).unwrap();

    program.run(&mut db, &Schedule::run(""), &[]).unwrap();

    assert_eq!(db.rows(ready).count(), 1);
}

#[test]
fn loaded_constant_container_calls_follow_color_unions() {
    use tir_egglog::sort::VecContainer;
    for alignment in [None, Some(0), Some(1)] {
        let mut eg = tir_egglog::EGraph::default();
        eg.run_program(vec![
            tir_egglog::ast::Command::Sort {
                span: span!(),
                name: "Math".into(),
                presort_and_args: None,
            },
            tir_egglog::ast::Command::Sort {
                span: span!(),
                name: "MathVec".into(),
                presort_and_args: Some(("Vec".into(), vec![exprs::var("Math")])),
            },
        ])
        .unwrap();
        let db = eg.database_mut();
        let math = db.sort("Math").unwrap();
        let vector_sort = db.sort("MathVec").unwrap();
        let int = db.sort("i64").unwrap();
        let seed = db.add_relation("seed", &[int]);
        let hit = db.add_relation("hit", &[int]);
        db.add_primitive(
            "same",
            Signature {
                inputs: vec![vector_sort],
                output: int,
            },
            |ctx, args| {
                let values = &ctx.container_value::<VecContainer>(args[0])?.data;
                Some(ctx.base(i64::from(values[0] == values[1])))
            },
        );
        let mut rules = Rules::new();
        let arg = exprs::arg(0);
        rules
            .rule(
                db,
                "",
                facts![(seed x) (= n (same (unquote arg)))],
                actions![(hit n)],
            )
            .unwrap();
        rules.set_colored("");
        let bytes = rules.compile(db).unwrap().to_bytes();
        let mut program = load_program(&bytes, db, alignment).unwrap();
        let [a, b] = [db.fresh(math), db.fresh(math)];
        let vector = db.container(VecContainer::new(vec![a, b], [true, true]));
        let color = db.color(&[]);
        let [zero, one] = [db.base(0_i64), db.base(1_i64)];
        db.insert_in(color, seed, &[zero]).unwrap();
        program.run(db, &Schedule::run(""), &[vector]).unwrap();
        db.union_in(color, a, b);
        db.insert_in(color, seed, &[one]).unwrap();
        program.run(db, &Schedule::run(""), &[vector]).unwrap();
        let mut hits: Vec<i64> = db
            .rows_in(color, hit)
            .map(|row| db.base_value(row[0]))
            .collect();
        hits.sort_unstable();
        assert_eq!(hits, [0, 1]);
    }
}

mod review_regressions {
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tir_egglog::prelude::*;
use tir_egglog::{Database, Error, Program, Rules, Schedule, Signature};

#[test]
fn grouped_index_removes_a_tied_row() {
    for ordered in [false, true] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let t = db.add_relation("t", &[int, int]);
        let index = db.add_kept_index(t, &[0], if ordered { &[1] } else { &[] });
        let one = db.base(1_i64);
        let ten = db.base(10_i64);
        let twenty = db.base(20_i64);
        db.insert(t, &[one, ten]).unwrap();
        db.insert(t, &[one, twenty]).unwrap();
        db.remove(t, &[one, twenty]);
        let found: Vec<_> = db.listed(index, &[one]).map(|row| row[1]).collect();
        assert_eq!(found, [ten], "ordered={ordered}");
        assert_eq!(db.rows(t).count(), 1);
        let mut rules = Rules::new();
        let arg = exprs::arg(0);
        let out = db.add_relation("out", &[int]);
        rules
            .rule(&db, "", facts![(t (unquote arg) y)], actions![(out y)])
            .unwrap();
        let mut program = rules.compile(&db).unwrap();
        program.run(&mut db, &Schedule::run(""), &[one]).unwrap();
        assert_eq!(db.rows(out).map(|row| row[0]).collect::<Vec<_>>(), [ten]);
        db.remove(t, &[one, ten]);
        assert_eq!(db.listed(index, &[one]).count(), 0);
        db.insert(t, &[one, twenty]).unwrap();
        assert_eq!(
            db.listed(index, &[one])
                .map(|row| row[1])
                .collect::<Vec<_>>(),
            [twenty]
        );
    }
}

#[test]
fn hoisted_panic_is_an_error_on_each_attempt_and_recovers_after_rebinding() {
    for loaded in [false, true] {
        for result_after_panic in [false, true] {
            for primed in [false, true] {
                let mut db = Database::new();
                let int = db.sort("i64").unwrap();
                let out = db.add_relation("out", &[int]);
                let signature = || Signature {
                    inputs: vec![int],
                    output: int,
                };
                let attempts = Arc::new(AtomicUsize::new(0));
                let recorded = attempts.clone();
                db.add_primitive("scalar", signature(), move |ctx, args| {
                    recorded.fetch_add(1, Ordering::SeqCst);
                    if ctx.base_value::<i64>(args[0]) == 2 {
                        ctx.panic();
                        return result_after_panic.then_some(args[0]);
                    }
                    Some(args[0])
                });
                let arg = exprs::arg(0);
                let mut rules = Rules::new();
                rules
                    .rule(
                        &db,
                        "",
                        facts![(= n (scalar (unquote arg)))],
                        actions![(out n)],
                    )
                    .unwrap();
                let mut program = rules.compile(&db).unwrap();
                if loaded {
                    program = Program::from_bytes(&program.to_bytes(), &mut db).unwrap();
                }
                let one = db.base(1_i64);
                let two = db.base(2_i64);
                if primed {
                    program.run(&mut db, &Schedule::run(""), &[one]).unwrap();
                }
                let expected = if primed { vec![one] } else { vec![] };
                for _ in 0..2 {
                    let result = program.run(&mut db, &Schedule::run(""), &[two]);
                    assert!(
                        matches!(result, Err(Error::Panic(_))),
                        "{result:?}, loaded={loaded}"
                    );
                    assert_eq!(db.rows(out).map(|row| row[0]).collect::<Vec<_>>(), expected);
                }
                assert_eq!(attempts.load(Ordering::SeqCst), 2 + usize::from(primed));
                db.add_primitive("scalar", signature(), |_, args| Some(args[0]));
                program.run(&mut db, &Schedule::run(""), &[two]).unwrap();
                let mut rows: Vec<i64> = db.rows(out).map(|row| db.base_value(row[0])).collect();
                rows.sort_unstable();
                assert_eq!(rows, if primed { vec![1, 2] } else { vec![2] });
            }
        }
    }
}

#[test]
fn ordinary_hoisted_failure_is_cached_and_new_arguments_refresh_it() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let out = db.add_relation("out", &[int]);
    let attempts = Arc::new(AtomicUsize::new(0));
    let recorded = attempts.clone();
    db.add_primitive(
        "scalar",
        Signature {
            inputs: vec![int],
            output: int,
        },
        move |ctx, args| {
            recorded.fetch_add(1, Ordering::SeqCst);
            (ctx.base_value::<i64>(args[0]) != 2).then_some(args[0])
        },
    );
    let mut rules = Rules::new();
    let arg = exprs::arg(0);
    rules
        .rule(
            &db,
            "",
            facts![(= n (scalar (unquote arg)))],
            actions![(out n)],
        )
        .unwrap();
    let mut program = rules.compile(&db).unwrap();
    let two = db.base(2_i64);
    for _ in 0..2 {
        program.run(&mut db, &Schedule::run(""), &[two]).unwrap();
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(db.rows(out).count(), 0);
    let three = db.base(3_i64);
    program.run(&mut db, &Schedule::run(""), &[three]).unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(db.rows(out).map(|row| row[0]).collect::<Vec<_>>(), [three]);
}

#[test]
fn negative_only_variables_cannot_be_used_by_actions() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    db.add_relation("blocked", &[int]);
    db.add_relation("out", &[int]);
    let mut rules = Rules::new();
    let result = rules.naive_rule(&db, "", facts![(not (blocked x))], actions![(out x)]);
    assert!(
        matches!(result, Err(Error::Type(_, message)) if message.contains("unbound variable `x`"))
    );
}

#[test]
fn negative_variables_correlate_with_positive_atoms_in_either_order() {
    for negative_first in [false, true] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let item = db.add_relation("item", &[int]);
        let blocked = db.add_relation("blocked", &[int]);
        let out = db.add_relation("out", &[int]);
        let mut rules = Rules::new();
        let facts = if negative_first {
            facts![(not (blocked x)) (item x)]
        } else {
            facts![(item x) (not (blocked x))]
        };
        rules.naive_rule(&db, "", facts, actions![(out x)]).unwrap();
        let mut program = rules.compile(&db).unwrap();
        let one = db.base(1_i64);
        let two = db.base(2_i64);
        db.insert(item, &[one]).unwrap();
        db.insert(item, &[two]).unwrap();
        db.insert(blocked, &[two]).unwrap();
        program.run(&mut db, &Schedule::run(""), &[]).unwrap();
        assert_eq!(db.rows(out).map(|row| row[0]).collect::<Vec<_>>(), [one]);
    }
}

#[test]
fn negative_locals_remain_existential_and_equalities_keep_correlations() {
    for body in [
        facts![(not (blocked local)) (= x 1)],
        facts![(not (blocked alias)) (= alias x) (item x)],
    ] {
        let mut db = Database::new();
        let int = db.sort("i64").unwrap();
        let item = db.add_relation("item", &[int]);
        let blocked = db.add_relation("blocked", &[int]);
        let out = db.add_relation("out", &[int]);
        let mut rules = Rules::new();
        rules.naive_rule(&db, "", body, actions![(out x)]).unwrap();
        let mut program = rules.compile(&db).unwrap();
        let one = db.base(1_i64);
        db.insert(item, &[one]).unwrap();
        program.run(&mut db, &Schedule::run(""), &[]).unwrap();
        assert_eq!(db.rows(out).map(|row| row[0]).collect::<Vec<_>>(), [one]);
        db.remove(out, &[one]);
        db.insert(blocked, &[one]).unwrap();
        program.run(&mut db, &Schedule::run(""), &[]).unwrap();
        assert_eq!(db.rows(out).count(), 0);
    }
}

}
