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
    let names = ["Num", "Add", "Mul", "folded"];
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
    ]))
}

#[test]
fn a_loaded_program_runs_like_the_compiled_one() {
    let mut compiled_db = schema();
    let mut compiled = rules(&compiled_db).compile(&compiled_db).unwrap();
    let bytes = compiled.to_bytes();
    terms(&mut compiled_db);
    compiled.run(&mut compiled_db, &schedule()).unwrap();

    let mut loaded_db = schema();
    terms(&mut loaded_db);
    let mut loaded = Program::from_bytes(&bytes, &mut loaded_db).unwrap();
    assert_eq!(loaded.to_bytes(), bytes);
    loaded.run(&mut loaded_db, &schedule()).unwrap();

    assert_eq!(rows(&loaded_db), rows(&compiled_db));
    let folded = loaded_db.table("folded").unwrap();
    assert!(loaded_db.rows(folded).count() > 0);
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

    let mut version = bytes.clone();
    version[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert!(matches!(
        load(&version, &mut schema()),
        LoadError::Version { found: 99, .. }
    ));

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
    program.run(&mut db, &Schedule::run("read")).unwrap();
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
    program.run(&mut db, &Schedule::run("write")).unwrap();
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
    program.run(db, &Schedule::run("")).unwrap();
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
