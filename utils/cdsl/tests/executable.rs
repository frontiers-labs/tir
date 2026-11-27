use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use tir_cdsl::{
    OutputKind::{Lean, OpDef, Program as RustProgram},
    compile_source,
    format::{FormatOptions, format_source},
};
use tir_egglog::{EGraph, Program, Schedule};

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

    fn run_batch(&self, test: &str, batch: &str) -> Output {
        Command::new(env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("CARGO_MANIFEST_DIR", &self.0)
            .env("CARGO_PKG_NAME", "fixture")
            .env("OUT_DIR", self.0.join("out"))
            .env("CDSL_TEST_BUILD_BATCH", batch)
            .output()
            .unwrap()
    }
}

impl Drop for Sources {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn build_script_compile_filters_modules_and_rejects_duplicate_outputs() {
    if let Ok(batch) = env::var("CDSL_TEST_BUILD_BATCH") {
        tir_cdsl::compile(batch.lines().map(|line| {
            let mut parts = line.split('\t');
            let kind = parts.next().unwrap();
            let filter = parts.next().unwrap();
            let dest = parts.next().unwrap();
            let out = match kind {
                "opdef" => OpDef(dest),
                "program" => RustProgram(dest),
                "lean" => Lean(dest),
                other => panic!("unexpected kind {other}"),
            };
            (filter, out)
        }))
        .unwrap();
        return;
    }

    let sources = Sources::new();
    sources.write(
        "defs/lib.cdsl",
        r#"
use std::core::*;

pub mod first {
    use std::core::*;
    use std::ir::Bits;
    use std::bits;

    pub enum FirstTag { Alpha, Beta }

    pub let addi<w: Nat> = op {
        signature: (lhs: Bits<w>, rhs: Bits<w>) -> (res: Bits<w>),
        attributes: { nsw: Option<Bool> },
        exec: |lhs, rhs| bits::add_wrap(lhs, rhs),
        interfaces: (Pure, Commutative),
    };
}

pub mod second {
    use std::core::*;
    pub let Edge: Relation<(Int, Int)>;
    pub let Reach: Relation<(Int, Int)>;
    let r_base<x, y>: Rule(Edge(x, y) -> Reach(x, y));
    pub let run = || -> Schedule { Schedule::Saturate(List(r_base)) };
}
"#,
    );

    let batch = "opdef\tfixture::first::*\tnested/first.rs\nprogram\tfixture::second::*\tsecond.rs\nlean\tfixture::*\tproofs.lean\n";
    let built = sources.run_batch(
        "build_script_compile_filters_modules_and_rejects_duplicate_outputs",
        batch,
    );
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let first_rs = fs::read_to_string(sources.0.join("out/nested/first.rs")).unwrap();
    assert!(first_rs.contains("pub enum FirstTag"), "{first_rs}");
    assert!(first_rs.contains("pub struct AddiOp"), "{first_rs}");
    assert!(first_rs.contains("Pattern::Bound(0usize)"), "{first_rs}");
    assert!(!first_rs.contains("Edge"), "{first_rs}");

    let second_rs = fs::read_to_string(sources.0.join("out/second.rs")).unwrap();
    assert!(second_rs.contains("pub fn table_edge"), "{second_rs}");
    assert!(second_rs.contains("pub fn schedule_run"), "{second_rs}");
    assert!(!second_rs.contains("AddiOp"), "{second_rs}");

    let lean = fs::read_to_string(sources.0.join("out/proofs.lean")).unwrap();
    assert!(lean.contains("first"), "{lean}");

    // Duplicate destination must fail without clobbering existing outputs.
    let sentinel = sources.write("out/nested/first.rs", "sentinel");
    let dup = sources.run_batch(
        "build_script_compile_filters_modules_and_rejects_duplicate_outputs",
        "opdef\tfixture::first::*\tnested/first.rs\nprogram\tfixture::second::*\tnested/../nested/first.rs\n",
    );
    let stderr = String::from_utf8_lossy(&dup.stderr);
    assert!(
        !dup.status.success() && stderr.contains("duplicate Rust output"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "sentinel");
}

#[test]
fn tegg_bytecode_executes_joins_seminaive_negation_merges_and_action_prims() {
    let source = r#"
use std::core::*;

let prog: EgglogProgram = EgglogProgram::ProgramFull(
    List(
        SortDecl::BaseSort("i64"),
        SortDecl::EqSort("Tag"),
    ),
    List(),
    List(
        TableDecl::Relation("Edge", List("i64", "i64")),
        TableDecl::Relation("Blocked", List("i64")),
        TableDecl::Relation("Reach", List("i64", "i64")),
        TableDecl::Constructor("Tag::Mark", List("i64", "Tag")),
        TableDecl::Relation("Marked", List("Tag", "i64")),
        TableDecl::Function("Best", List("i64", "i64"), "min"),
    ),
    List(
        Ruleset::Ruleset(
            "reach",
            List(
                Rule::Clause(
                    "base",
                    List(
                        Fact::Row("Edge", List(Atom::Var("a"), Atom::Var("b"))),
                        Fact::NotRow("Blocked", List(Atom::Var("b"))),
                    ),
                    List(
                        Action::Insert("Reach", List(Atom::Var("a"), Atom::Var("b"))),
                        Action::Insert(
                            "Marked",
                            List(
                                Atom::Call("Tag::Mark", List(Atom::Var("a"))),
                                Atom::Call("+", List(Atom::Var("b"), Atom::IntLit(10))),
                            ),
                        ),
                        Action::Insert("Best", List(Atom::Var("a"), Atom::Var("b"))),
                    ),
                ),
                Rule::Clause(
                    "step",
                    List(
                        Fact::Row("Reach", List(Atom::Var("a"), Atom::Var("b"))),
                        Fact::Row("Edge", List(Atom::Var("b"), Atom::Var("c"))),
                        Fact::NotRow("Blocked", List(Atom::Var("c"))),
                    ),
                    List(
                        Action::Insert("Reach", List(Atom::Var("a"), Atom::Var("c"))),
                        Action::Insert("Best", List(Atom::Var("a"), Atom::Var("c"))),
                    ),
                ),
            ),
            false,
            false,
        ),
    ),
    Schedule::RunNamed("reach", 0),
);
"#;

    let mut checked = compile_source(source).unwrap();
    let bytes = checked.emit_tegg().unwrap();

    let mut engine = EGraph::default();
    let db = engine.database_mut();
    let mut program = Program::from_bytes(&bytes, db).unwrap();
    let edge = db.table("Edge").unwrap();
    let blocked = db.table("Blocked").unwrap();
    db.insert(edge, &[db.base(1_i64), db.base(3_i64)]).unwrap();
    db.insert(edge, &[db.base(1_i64), db.base(2_i64)]).unwrap();
    db.insert(edge, &[db.base(2_i64), db.base(4_i64)]).unwrap();
    db.insert(edge, &[db.base(4_i64), db.base(5_i64)]).unwrap();
    db.insert(blocked, &[db.base(5_i64)]).unwrap();

    program
        .run(db, &Schedule::saturate(Schedule::run("reach")), &[])
        .unwrap();

    let mut reach: Vec<(i64, i64)> = db
        .rows(db.table("Reach").unwrap())
        .map(|row| (db.base_value::<i64>(row[0]), db.base_value::<i64>(row[1])))
        .collect();
    reach.sort_unstable();
    assert_eq!(reach, vec![(1, 2), (1, 3), (1, 4), (2, 4)]);

    // `Best` merges with `min`: from node 1, reachable {2, 3, 4} -> min is 2.
    let best_tbl = db.table("Best").unwrap();
    let best_1 = db.lookup(best_tbl, &[db.base(1_i64)]).unwrap();
    assert_eq!(db.base_value::<i64>(best_1), 2);

    // `Marked` combined constructor `Tag::Mark(a)` and action primitive `b + 10`.
    let mut marked: Vec<i64> = db
        .rows(db.table("Marked").unwrap())
        .map(|row| db.base_value::<i64>(row[1]))
        .collect();
    marked.sort_unstable();
    assert_eq!(marked, vec![12, 13, 14]);
}

#[test]
fn cdslc_cli_emits_typed_egg_tegg_rust_and_lean() {
    let sources = Sources::new();
    let input = sources.write(
        "prog.cdsl",
        r#"
use std::core::*;

pub let prog: EgglogProgram = EgglogProgram::ProgramFull(
    List(SortDecl::BaseSort("i64")),
    List(),
    List(
        TableDecl::Relation("In", List("i64")),
        TableDecl::Relation("Out", List("i64")),
    ),
    List(
        Ruleset::Ruleset(
            "copy",
            List(
                Rule::Clause(
                    "shift",
                    List(Fact::Row("In", List(Atom::Var("x")))),
                    List(Action::Insert("Out", List(Atom::Call("+", List(Atom::Var("x"), Atom::IntLit(7)))))),
                ),
            ),
            false,
            false,
        ),
    ),
    Schedule::RunNamed("copy", 1),
);
"#,
    );

    for (emit, needle) in [
        ("typed", "Typechecked"),
        ("egg", "(rule"),
        ("rust", "pub const"),
        ("lean", "def prog"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_cdslc"))
            .args([
                "--input",
                input.to_str().unwrap(),
                "--emit",
                emit,
                "--output",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "emit={emit}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.contains(needle),
            "emit={emit} missing {needle:?}: {text}"
        );
    }

    let tegg_file = sources.0.join("prog.tegg");
    let out = Command::new(env!("CARGO_BIN_EXE_cdslc"))
        .args([
            "--input",
            input.to_str().unwrap(),
            "--emit",
            "tegg",
            "--output",
            tegg_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = fs::read(&tegg_file).unwrap();
    let mut engine = EGraph::default();
    let db = engine.database_mut();
    let mut program = Program::from_bytes(&bytes, db).unwrap();
    db.insert(db.table("In").unwrap(), &[db.base(5_i64)])
        .unwrap();
    program.run(db, &Schedule::run("copy"), &[]).unwrap();
    let vals: Vec<i64> = db
        .rows(db.table("Out").unwrap())
        .map(|row| db.base_value::<i64>(row[0]))
        .collect();
    assert_eq!(vals, vec![12]);
}

#[test]
fn prop_elimination_restriction_and_erasure() {
    // Eliminating a 2-ctor Prop into Type must be rejected by the kernel.
    let err = compile_source(
        r#"
enum ChoiceP: Prop { Left, Right }
pub let leak = |p: ChoiceP| -> Int {
    match p {
        ChoiceP::Left => 0,
        ChoiceP::Right => 1,
    }
};
"#,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("cannot eliminate Prop"),
        "unexpected error: {err}"
    );

    // Eliminating Prop into Prop is valid, and Prop witnesses erase from Rust
    // while surviving in Lean 4 output.
    let mut ok = compile_source(
        r#"
enum OrP: Prop { Inl, Inr }
pub let swap_or = |p: OrP| -> OrP {
    match p {
        OrP::Inl => OrP::Inr,
        OrP::Inr => OrP::Inl,
    }
};
pub let keep_val = |x: Int, _p: OrP| -> Int { x + 1 };
"#,
    )
    .unwrap();
    let lean = ok.emit_lean();
    assert!(lean.contains("inductive OrP : Prop"));
    let rust = ok.emit_rust().unwrap();
    assert!(!rust.contains("enum OrP"));
    assert!(rust.contains("pub fn keep_val"));
}

#[test]
fn formatter_is_idempotent_and_preserves_typechecking() {
    let raw = r#"
enum Color{Red,Green,Blue}
pub let to_int=|c:Color|->Int{match c{Color::Red=>1,Color::Green=>2,Color::Blue=>3,}};
"#;
    let first = format_source(raw, FormatOptions::default()).unwrap();
    let second = format_source(&first, FormatOptions::default()).unwrap();
    assert_eq!(first, second);
    compile_source(&second).unwrap();
}

#[test]
fn table_merge_with_lowers_primitives_lambdas_and_externs() {
    use tir_egglog::sort::BigInt;

    let source = r#"
use std::core::*;
use std::bigint::bigint;

let bigint_or: (BigInt, BigInt) -> BigInt;
extract egglog bigint_or => "bit_or";

let host_merge: (Int, Int) -> Int;

pub let KnownZeros: Table<Int, BigInt, Merge::With(bigint_or)>;
pub let Sum: Table<Int, Int, Merge::With(|a, b| a + b)>;
pub let Custom: Table<Int, Int, Merge::With(host_merge)>;

let seed_bits: Rule(() -> (KnownZeros(1, bigint(5)), KnownZeros(1, bigint(3))));
let seed_sum: Rule(() -> (Sum(1, 10), Sum(1, 32)));
let seed_custom: Rule(() -> (Custom(1, 4), Custom(1, 9)));

pub let run: Schedule = Schedule::Once(List(seed_bits, seed_sum, seed_custom));
"#;

    let mut checked = compile_source(source).unwrap();
    let ml = checked
        .erase_for_mode(None, tir_cdsl::erase::EraseMode::Program)
        .unwrap();
    let bundle = ml
        .decls
        .iter()
        .find_map(|d| match d {
            tir_cdsl::erase::MlDecl::ProgramBundle(b) => Some(b),
            _ => None,
        })
        .expect("program bundle");
    // Built-in egglog primitive must not generate a host binder; private custom extern must be kept.
    assert!(
        bundle.externs.iter().all(|e| e.engine_name != "bigint_or"),
        "unexpected host extern for built-in egglog primitive: {:?}",
        bundle.externs
    );
    assert!(
        bundle.externs.iter().any(|e| e.engine_name == "host_merge"),
        "missing private Merge::With extern in bundle: {:?}",
        bundle.externs
    );

    let mut engine = EGraph::default();
    let db = engine.database_mut();
    let i64_sort = db.sort("i64").unwrap();
    db.add_primitive(
        "host_merge",
        tir_egglog::Signature {
            inputs: vec![i64_sort, i64_sort],
            output: i64_sort,
        },
        |ctx, args| {
            let a = ctx.base_value::<i64>(args[0]);
            let b = ctx.base_value::<i64>(args[1]);
            Some(ctx.base(a * 100 + b))
        },
    );

    let _ = Program::from_bytes(&bundle.catalog_bytes, db).unwrap();
    let mut program = Program::from_bytes(bundle.code_bytes.as_deref().unwrap(), db).unwrap();
    let sched = match &bundle.schedules[0].schedule {
        tir_cdsl::tegg::IrSchedule::Run(rs, 1) => Schedule::run(rs),
        other => panic!("unexpected schedule: {other:?}"),
    };
    program.run(db, &sched, &[]).unwrap();

    let zeros = db
        .lookup(db.table("KnownZeros").unwrap(), &[db.base(1_i64)])
        .unwrap();
    assert_eq!(db.base_value::<BigInt>(zeros), BigInt::new(7));

    let sum = db
        .lookup(db.table("Sum").unwrap(), &[db.base(1_i64)])
        .unwrap();
    assert_eq!(db.base_value::<i64>(sum), 42);

    let custom = db
        .lookup(db.table("Custom").unwrap(), &[db.base(1_i64)])
        .unwrap();
    assert_eq!(db.base_value::<i64>(custom), 409);

    // Unsupported multi-op Merge::With body must fail cleanly instead of emitting `"custom"`.
    let mut bad = compile_source(
        r#"
use std::core::*;
pub let Bad: Table<Int, Int, Merge::With(|a, b| (a + b) + 1)>;
let seed: Rule(() -> Bad(1, 2));
pub let run: Schedule = Schedule::Once(List(seed));
"#,
    )
    .unwrap();
    let err = bad
        .erase_for_mode(None, tir_cdsl::erase::EraseMode::Program)
        .unwrap_err();
    assert!(
        err.to_string().contains("Merge::With"),
        "unexpected error: {err}"
    );
}
