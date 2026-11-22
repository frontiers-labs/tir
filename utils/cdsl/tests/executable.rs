use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use tir_cdsl::{OutputKind::OpDef, Schema, compile_file, compile_source};
use tir_egglog::{EGraph, Program, Schedule};

#[test]
fn rust_filters_compile_their_module_exports() {
    if let Ok(batch) = env::var("CDSL_TEST_BUILD_BATCH") {
        tir_cdsl::compile(batch.lines().map(|request| {
            let (filter, path) = request.split_once('\t').unwrap();
            (filter, OpDef(path))
        }))
        .unwrap();
        return;
    }

    let sources = Sources::new();
    sources.write(
        "defs/lib.cdsl",
        r#"
mod hidden {
    pub let enabled: Bool = true;
    let Flag: Type = Bool;
    pub let Visible: Type = { flag: Flag };
}
pub mod first {
    pub use hidden::enabled as exported;
    pub use hidden::Visible as Exported;
    let internal: Bool = false;
}
pub mod second {
    pub let label: String = "second";
    pub enum Color { Red, Green, }
    pub use Color as Shade;
}
pub use second as external;
"#,
    );
    let alias = format!("{}::external::*", sources.0.file_name().unwrap().display());
    let second = sources.0.join("absolute/second.rs");
    let built = sources.emit(&[
        ("fixture::first::*", "nested/first.rs"),
        (&alias, second.to_str().unwrap()),
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let probe = sources.write(
        "main.rs",
        r#"
mod first { include!("out/nested/first.rs"); }
mod second { include!("absolute/second.rs"); }
const _: () = {
    assert!(first::exported);
    assert!(first::Exported { flag: true }.flag);
    assert!(second::label.as_bytes().len() == 6);
};
const _: second::Shade = second::Color::Green;
"#,
    );
    let compiler = Command::new("rustc")
        .args(["--crate-type=lib", "--emit=metadata"])
        .arg("--out-dir")
        .arg(&sources.0)
        .arg(&probe)
        .output()
        .unwrap();
    assert!(
        compiler.status.success(),
        "{}",
        String::from_utf8_lossy(&compiler.stderr)
    );
    std::fs::write(
        &probe,
        "mod first { include!(\"out/nested/first.rs\"); } const _: bool = first::internal;",
    )
    .unwrap();
    let compiler = Command::new("rustc")
        .args(["--crate-type=lib", "--emit=metadata"])
        .arg("--out-dir")
        .arg(&sources.0)
        .arg(probe)
        .output()
        .unwrap();
    assert!(
        !compiler.status.success()
            && String::from_utf8_lossy(&compiler.stderr).contains("cannot find value `internal`")
    );

    let existing = sources.write("out/nested/first.rs", "sentinel");
    for (filter, path, diagnostic) in [
        (
            alias.as_str(),
            "nested/../nested/first.rs",
            "duplicate Rust output",
        ),
        ("fixture::missing::*", "missing.rs", "unknown module"),
    ] {
        let built = sources.emit(&[("fixture::first::*", "nested/first.rs"), (filter, path)]);
        let stderr = String::from_utf8_lossy(&built.stderr);
        assert!(
            !built.status.success() && stderr.contains(diagnostic),
            "{stderr}"
        );
        assert_eq!(fs::read_to_string(&existing).unwrap(), "sentinel");
    }
}

#[derive(Debug)]
struct Sources(PathBuf);
impl Sources {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "cdsl-executable-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        path
    }

    fn emit(&self, outputs: &[(&str, &str)]) -> Output {
        let batch = outputs
            .iter()
            .map(|(filter, path)| format!("{filter}\t{path}\n"))
            .collect::<String>();
        self.build("rust_filters_compile_their_module_exports")
            .env("CDSL_TEST_BUILD_BATCH", batch)
            .output()
            .unwrap()
    }

    fn build(&self, test: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env("CARGO_MANIFEST_DIR", &self.0)
            .env("CARGO_PKG_NAME", "fixture")
            .env("OUT_DIR", self.0.join("out"));
        command
    }
}
impl Drop for Sources {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_checked_operation_keeps_its_interface_specialization_and_identity() {
    let source = compile_source(
        r#"
pub mod traits { pub interface Read<T: Type> { read: () -> T } }
use traits::Read as ReadFlag;
let truth = true;
let guarded = {
    op {
        signature: () -> Bool,
        attributes: { flag: Bool },
        interfaces: { ReadFlag: { read: || -> Bool { self.flag } } },
    }
};
"#,
    )
    .unwrap();
    let checked = source.lower().unwrap();
    let [operation] = checked.operation_interfaces() else {
        panic!("expected one checked operation");
    };
    assert_eq!(operation.operation().path(), ["guarded"]);
    let (interface, methods) = operation.implementations().next().unwrap();
    assert_eq!(interface.path(), ["traits", "Read"]);
    assert_eq!(
        methods.arguments(),
        [checked.types()[&vec!["truth".into()]].ty.clone()]
    );

    let second = source.lower().unwrap();
    assert!(second.operation_interfaces()[0].get(interface).is_none());
}

#[test]
fn inferring_an_interface_operation_checks_exec_against_its_signature() {
    let error = compile_source(
        r#"
interface Tag {}
let make = || {
    op { signature: () -> Bool, interfaces: { Tag: {} }, exec: || -> Int { 1 } }
};
"#,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("operation exec disagrees with signature")
    );
}

#[test]
fn rewrite_matches_use_declared_operand_and_result_types() {
    let source = r#"
pub mod dialects {
    pub mod downstream {
        pub let compare = op { signature: (lhs: std::Bits<8>, rhs: std::Bits<8>) -> std::Bits<1> };
        pub let widen = op { signature: (input: std::Bits<1>) -> std::Bits<16> };
    }
}
pub let Replaced: (std::ir::OperationId, std::ir::ValueId) -> Prop;
pub let rewrite<first, last, x, y, flag, result>:
    (dialects::downstream::compare.matches(first, (x, y), (flag,)),
     dialects::downstream::widen.matches(last, (flag,), (result,)))
    -> Replaced(first, result);
"#;
    compile_source(source).unwrap();
    for (original, invalid, diagnostic) in [
        ("compare.matches", "missing.matches", "unresolved name"),
        ("(x, y)", "(x,)", "rewrite: HM:"),
        ("(flag,), (result,)", "(x,), (result,)", "rewrite: HM:"),
    ] {
        let error = compile_source(&source.replace(original, invalid)).unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{error}");
    }
}

#[test]
fn a_shared_named_relation_preserves_its_column_order() {
    let first = compile_source("pub let Edge: { from: Int, to: Int } -> Prop;")
        .unwrap()
        .lower()
        .unwrap();
    let reversed = compile_source("pub let Edge: { to: Int, from: Int } -> Prop;")
        .unwrap()
        .lower()
        .unwrap();
    let mut schema = Schema::new(&first).unwrap();
    let error = schema.extend(&reversed).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("incompatible named columns for Edge"),
        "{error}"
    );
}

#[test]
fn separately_compiled_programs_share_populated_tables_and_keep_incremental_state() {
    let sources = Sources::new();
    sources.write("schema.cdsl", "pub let Input: Int -> Prop; pub let Middle: Int -> Prop; pub let Sentinel: Int -> Prop; pub mod unrelated { pub let poison<x>: Input(x) -> Sentinel(x); }");
    let root = sources.write("lib.cdsl", "pub mod schema; pub mod first { pub mod rules { use schema::*; pub let step<x>: Input(x) -> Middle(x + 1); } pub let execute = || { std::schedule::saturate(rules::step) }; } pub mod second { pub mod rules { use schema::*; pub let step<x>: Middle(x) -> Input(x + 1); } pub let execute = || { std::schedule::seq(std::schedule::Schedule::Rules(rules::step), std::schedule::saturate(rules::step)) }; }");
    let first = compile_file(&root)
        .unwrap()
        .select(&["first".into()])
        .unwrap()
        .lower()
        .unwrap();
    let second = compile_file(&root)
        .unwrap()
        .select(&["second".into()])
        .unwrap()
        .lower()
        .unwrap();
    let mut schema = Schema::new(&first).unwrap();
    schema.extend(&second).unwrap();
    let first_lowered = schema.lower(&first).unwrap();
    let second_lowered = schema.lower(&second).unwrap();
    let first_schedule = &first_lowered.schedules()[&vec!["first".into(), "execute".into()]];
    let second_schedule = &second_lowered.schedules()[&vec!["second".into(), "execute".into()]];
    let first_bytes = first_lowered.compile(schema.database()).unwrap().to_bytes();
    let second_bytes = second_lowered
        .compile(schema.database())
        .unwrap()
        .to_bytes();
    let mut engine = EGraph::default();
    let db = engine.database_mut();
    Program::from_bytes(&schema.to_bytes().unwrap(), db).unwrap();
    let input = db.table("Input").unwrap();
    let middle = db.table("Middle").unwrap();
    db.insert(input, &[db.base(0_i64)]).unwrap();
    let mut first = Program::from_bytes(&first_bytes, db).unwrap();
    let mut second = Program::from_bytes(&second_bytes, db).unwrap();
    assert_eq!(db.table("Input").unwrap(), input);
    assert_eq!(db.table("Middle").unwrap(), middle);
    first.run(db, first_schedule, &[]).unwrap();
    second.run(db, second_schedule, &[]).unwrap();
    first.run(db, first_schedule, &[]).unwrap();
    let values = |db: &tir_egglog::Database, table| {
        let mut values = db
            .rows(table)
            .map(|row| db.base_value::<i64>(row.values().next().unwrap()))
            .collect::<Vec<_>>();
        values.sort_unstable();
        values
    };
    assert_eq!(values(db, middle), [1, 3]);
    assert_eq!(values(db, input), [0, 2]);
    second.run(db, second_schedule, &[]).unwrap();
    assert_eq!(values(db, input), [0, 2, 4]);
    assert_eq!(db.table_count(), 3);
    assert_eq!(db.rows(db.table("Sentinel").unwrap()).count(), 0);
}

#[test]
fn cli_emits_the_executable_library_program_to_file_and_stdout() {
    let sources = Sources::new();
    let source = sources.write("cli.cdsl", "pub let Input: Int -> Prop; pub let Output: Int -> Prop; pub mod copying { pub let seeds: (Output(99), (Output(100), Output(101))); pub let rule<x>: Input(x) -> (Output(x + 7), (Output(x + 8), Output(x + 9))); } pub let copy = || { std::schedule::Schedule::Rules((copying::seeds, copying::rule)) };");
    let checked = compile_file(&source).unwrap().lower().unwrap();
    let schema = Schema::new(&checked).unwrap();
    let expected = schema
        .lower(&checked)
        .unwrap()
        .compile(schema.database())
        .unwrap()
        .to_bytes();
    let file = sources.0.join("rules.bin");
    for (emit, expected) in [
        ("ast", compile_file(&source).unwrap().render()),
        ("typed", checked.render().unwrap()),
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_cdslc"))
            .args([
                "--input",
                source.to_str().unwrap(),
                "--emit",
                emit,
                "--output",
                "-",
            ])
            .output()
            .unwrap();
        assert!(result.status.success());
        let text = String::from_utf8(result.stdout).unwrap();
        if emit == "ast" {
            assert_eq!(text, expected);
        } else {
            assert!(text.contains("CheckedProgram"));
        }
    }
    for output in [file.to_str().unwrap(), "-"] {
        let result = Command::new(env!("CARGO_BIN_EXE_cdslc"))
            .args([
                "--input",
                source.to_str().unwrap(),
                "--emit",
                "bin",
                "--output",
                output,
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let bytes = if output == "-" {
            result.stdout
        } else {
            fs::read(&file).unwrap()
        };
        assert_eq!(bytes, expected);
        let mut engine = EGraph::default();
        let db = engine.database_mut();
        Program::from_bytes(&schema.to_bytes().unwrap(), db).unwrap();
        let mut program = Program::from_bytes(&bytes, db).unwrap();
        let input = db.table("Input").unwrap();
        db.insert(input, &[db.base(5_i64)]).unwrap();
        program.run(db, &Schedule::run("copy"), &[]).unwrap();
        let mut outputs = db
            .rows(db.table("Output").unwrap())
            .map(|row| db.base_value::<i64>(row.values().next().unwrap()))
            .collect::<Vec<_>>();
        outputs.sort_unstable();
        assert_eq!(outputs, [12, 13, 14, 99, 100, 101]);
    }
}

#[test]
fn declaration_names_do_not_capture_local_rule_variables() {
    let sources = Sources::new();
    let source = sources.write(
        "locals.cdsl",
        "pub let x: Int -> Prop; pub let Input: (Int, Int) -> Prop; pub let Output: (Int, Int) -> Prop; pub mod rules { pub let copy<x, y>: Input(x, y) -> Output(x, y); } pub let execute = || { std::schedule::Schedule::Rules(rules::copy) };",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    let schema = Schema::new(&checked).unwrap();
    let bytes = schema
        .lower(&checked)
        .unwrap()
        .compile(schema.database())
        .unwrap()
        .to_bytes();
    let mut engine = EGraph::default();
    let db = engine.database_mut();
    Program::from_bytes(&schema.to_bytes().unwrap(), db).unwrap();
    let mut program = Program::from_bytes(&bytes, db).unwrap();
    let one = db.base(1i64);
    let two = db.base(2i64);
    db.insert(db.table("Input").unwrap(), &[one, two]).unwrap();
    program.run(db, &Schedule::run("execute"), &[]).unwrap();
    let rows = db
        .rows(db.table("Output").unwrap())
        .map(|row| row.values().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(rows, vec![vec![one, two]]);
}

#[test]
fn unsupported_executable_computations_and_invalid_merges_fail() {
    let sources = Sources::new();
    let source = sources.write(
        "function.cdsl",
        "pub let identity = |x: Int| -> Int { x }; ",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    assert!(
        checked
            .to_binary()
            .unwrap_err()
            .to_string()
            .contains("unsupported executable")
    );

    let source = sources.write(
        "bad_merge.cdsl",
        "use std::Table; use std::Merge; pub let Bad: Table<Int, Int, Merge::Union>;",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    assert!(Schema::new(&checked).is_err());
}

#[test]
fn local_rules_run_only_the_selected_members_with_each_calls_arguments() {
    let checked = compile_source(
        r#"
pub let Edge: (Int, Int) -> Prop;
pub let Reach: (Int, Int) -> Prop;
pub let unselected<x, y>: Edge(x, y) -> Reach(y, x);

pub let selected: std::List<Prop> = {
    let direct<x, y>: Edge(x, y) -> Reach(x, y);
    let indirect<x, y, z>: (Reach(x, y), Edge(y, z)) -> Reach(x, z);
    std::List::Cons(direct, std::List::Cons(indirect, std::List::Nil))
};
pub let data = std::schedule::saturate(selected);
pub let closure = || { data };

pub let reach_from = |start: Int| {
    let direct<y>: Edge(start, y) -> Reach(start, y);
    let indirect<y, z>: (Reach(start, y), Edge(y, z)) -> Reach(start, z);
    std::schedule::saturate((direct, indirect))
};

pub let reach_from_both = |first: Int, second: Int| {
    std::schedule::seq(reach_from(first), reach_from(second))
};
"#,
    )
    .unwrap()
    .lower()
    .unwrap();
    let schema = Schema::new(&checked).unwrap();
    let lowered = schema.lower(&checked).unwrap();
    assert_eq!(lowered.schedules().len(), 3);
    let mut db = schema.database().clone();
    let mut program = lowered.compile(&db).unwrap();
    let edge = db.table("Edge").unwrap();
    let reach = db.table("Reach").unwrap();
    for (from, to) in [(10_i64, 11_i64), (11, 12), (20, 21), (30, 31)] {
        let row = [db.base(from), db.base(to)];
        db.insert(edge, &row).unwrap();
    }
    let rows = |db: &tir_egglog::Database| {
        let mut rows = db
            .rows(reach)
            .map(|row| (db.base_value::<i64>(row[0]), db.base_value::<i64>(row[1])))
            .collect::<Vec<_>>();
        rows.sort_unstable();
        rows
    };
    let schedule = |name: &str| &lowered.schedules()[&vec![name.into()]];
    let first = db.base(10_i64);
    program
        .run(&mut db, schedule("reach_from"), &[first])
        .unwrap();
    assert_eq!(rows(&db), [(10, 11), (10, 12)]);
    let both = [db.base(20_i64), db.base(10_i64)];
    program
        .run(&mut db, schedule("reach_from_both"), &both)
        .unwrap();
    assert_eq!(rows(&db), [(10, 11), (10, 12), (20, 21)]);
    program.run(&mut db, schedule("closure"), &[]).unwrap();
    assert_eq!(
        rows(&db),
        [(10, 11), (10, 12), (11, 12), (20, 21), (30, 31)]
    );
}

#[test]
fn local_tables_feed_checked_rules_and_are_released_on_return_and_failure() {
    let checked = compile_source(
        r#"
pub let Output: Int -> Prop;
pub let ordinary = || { let write: Output(9); std::schedule::saturate(write) };

let seed = |value: Int, scratch: Int -> Prop| {
    let insert: scratch(value);
    insert
};

pub let execute = || {
    let Scratch: Int -> Prop;
    std::schedule::saturate(seed(7, Scratch))?;
    let publish<value>: Scratch(value) -> Output(value);
    std::schedule::saturate(publish)?;
    ()
};

pub let fail = || {
    let Scratch: std::Table<Int, Int, std::Merge::Unique>;
    let first: Scratch(0, 1);
    let second: Scratch(0, 2);
    std::schedule::saturate((first, second))?;
    ()
};
"#,
    )
    .unwrap()
    .lower()
    .unwrap();
    let schema = Schema::new(&checked).unwrap();
    let mut db = schema.database().clone();
    let count = db.table_count();
    let result = schema
        .run_local_schedule(&checked, &["execute".into()], &mut db)
        .unwrap();
    assert!(result.unwrap().updated);
    let output = db.table("Output").unwrap();
    let rows = db.rows(output).map(|row| db.base_value::<i64>(row[0]));
    assert_eq!(rows.collect::<Vec<_>>(), [7]);
    assert_eq!(db.table_count(), count);
    assert!(
        schema
            .run_local_schedule(&checked, &["fail".into()], &mut db)
            .is_err()
    );
    assert_eq!(db.table_count(), count);

    let lowered = schema.lower(&checked).unwrap();
    let ordinary = &lowered.schedules()[&vec!["ordinary".into()]];
    let mut program = lowered.compile(&db).unwrap();
    program.run(&mut db, ordinary, &[]).unwrap();
    assert!(db.lookup(output, &[db.base(9_i64)]).is_some());
}
