use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use tir_cdsl::{Schema, compile_file, compile_source};
use tir_egglog::{EGraph, Program, Schedule};

#[test]
fn rust_filters_compile_their_module_exports() {
    let sources = Sources::new();
    let package = sources.write(
        "lib.cdsl",
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
    let checked = tir_cdsl::CheckedProgram::compile_for_rust(&package).unwrap();
    for (module, alias) in [("first", "first"), ("second", "external")] {
        let rust = checked
            .emit_rust_filter("fixture", &format!("fixture::{alias}::*"))
            .unwrap();
        sources.write(&format!("{module}.rs"), &rust);
    }
    let probe = sources.write(
        "main.rs",
        r#"
mod first { include!("first.rs"); }
mod second { include!("second.rs"); }
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
        "mod first { include!(\"first.rs\"); } const _: bool = first::internal;",
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
}

#[test]
fn rust_emission_materializes_both_fp_refinement_paths() {
    let sources = Sources::new();
    sources.write(
        "fp.cdsl",
        include_str!("../../../core/defs/dialects/fp.cdsl"),
    );
    let package = sources.write("lib.cdsl", "pub mod fp;");
    let checked = tir_cdsl::CheckedProgram::compile_for_rust(&package).unwrap();
    let rust = checked
        .emit_rust_filter("fixture", "fixture::fp::model::*")
        .unwrap();
    sources.write("fp.rs", &rust);
    let probe = sources.write(
        "main.rs",
        r#"
#[allow(dead_code, unused_imports)]
mod fp { include!("fp.rs"); }
use fp::{Computation, Float, Refinement, Rounding, ValueAlgebra, implementation, materialize, reference};

fn bits(input: Float) -> f64 {
    let Float::Binary64(bits) = input else { panic!("expected binary64") };
    f64::from_bits(bits)
}

fn float(value: f64) -> Float {
    Float::Binary64(value.to_bits())
}

fn contract() -> Refinement {
    let delta = 2f64.powi(-27);
    Refinement::Contract(
        Computation::Capture(float(1.0 + delta)),
        Computation::Capture(float(1.0 - delta)),
        Computation::Capture(float(-1.0)),
    )
}

fn main() {
    let algebra = ValueAlgebra {
        add: |mode, x, y| {
            assert!(matches!(mode, Rounding::NearestEven));
            float(bits(x) + bits(y))
        },
        sub: |_, x, y| float(bits(x) - bits(y)),
        div: |_, x, y| float(bits(x) / bits(y)),
        mul: |mode, x, y| {
            assert!(matches!(mode, Rounding::NearestEven));
            float(bits(x) * bits(y))
        },
        fma: |mode, x, y, z| {
            assert!(matches!(mode, Rounding::TowardPositive));
            float(bits(x).mul_add(bits(y), bits(z)))
        },
    };
    let separate = materialize(Rounding::NearestEven, algebra, reference(contract()));
    // The fused result is exactly representable in either rounding mode.
    let fused = materialize(Rounding::TowardPositive, algebra, implementation(contract()));
    assert_eq!(bits(separate).to_bits(), 0);
    assert_eq!(bits(fused), -2f64.powi(-54));
}
"#,
    );
    let binary = sources.0.join("fp-probe");
    let compiled = Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&binary)
        .arg(probe)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = Command::new(binary).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

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
        fs::write(&path, source).unwrap();
        path
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
        interfaces: { ReadFlag: { read: fn () -> Bool { self.flag } } },
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
fn inferring_an_interface_operation_does_not_bypass_its_schema() {
    let error = compile_source(
        r#"
interface Tag {}
let make = fn () {
    op { signature: () -> Bool, interfaces: { Tag: {} }, exec: false }
};
"#,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("operation descriptor requires a source-defined contract annotation")
    );
}

#[test]
fn separately_compiled_programs_share_populated_tables_and_keep_incremental_state() {
    let sources = Sources::new();
    sources.write("schema.cdsl", "pub let Input: Int -> Prop; pub let Middle: Int -> Prop; pub let Sentinel: Int -> Prop; pub mod unrelated { pub let poison<x>: Input(x) -> Sentinel(x); }");
    let root = sources.write("lib.cdsl", "pub mod schema; pub mod first { pub mod rules { use schema::*; pub let step<x>: Input(x) -> Middle(x + 1); } pub let execute = std::schedule::saturate(rules); } pub mod second { pub mod rules { use schema::*; pub let step<x>: Middle(x) -> Input(x + 1); } pub let execute = std::schedule::seq(std::schedule::Schedule::Rules(rules), std::schedule::saturate(rules)); }");
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
    let input = db.table("schema::Input").unwrap();
    let middle = db.table("schema::Middle").unwrap();
    db.insert(input, &[db.base(0_i64)]).unwrap();
    let mut first = Program::from_bytes(&first_bytes, db).unwrap();
    let mut second = Program::from_bytes(&second_bytes, db).unwrap();
    assert_eq!(db.table("schema::Input").unwrap(), input);
    assert_eq!(db.table("schema::Middle").unwrap(), middle);
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
    assert_eq!(db.rows(db.table("schema::Sentinel").unwrap()).count(), 0);
}

#[test]
fn cli_emits_the_executable_library_program_to_file_and_stdout() {
    let sources = Sources::new();
    let source = sources.write("cli.cdsl", "pub let Input: Int -> Prop; pub let Output: Int -> Prop; pub mod copy { pub let seeds: (Output(99), (Output(100), Output(101))); pub let rule<x>: Input(x) -> (Output(x + 7), (Output(x + 8), Output(x + 9))); }");
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
fn host_names_do_not_capture_local_rule_variables() {
    let sources = Sources::new();
    let source = sources.write(
        "locals.cdsl",
        "pub let x: Int -> Prop; pub let Input: (Int, Int) -> Prop; pub let Output: (Int, Int) -> Prop; pub mod rules { pub let copy<x, y>: Input(x, y) -> Output(x, y); }",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    let names = std::collections::BTreeMap::from([(vec!["x".into()], "y".into())]);
    let schema = Schema::with_names(&checked, names).unwrap();
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
    program.run(db, &Schedule::run("rules"), &[]).unwrap();
    let rows = db
        .rows(db.table("Output").unwrap())
        .map(|row| row.values().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(rows, vec![vec![one, two]]);
}

#[test]
fn unsupported_computations_and_incompatible_host_bindings_fail_explicitly() {
    let sources = Sources::new();
    let source = sources.write(
        "function.cdsl",
        "pub let identity = fn (x: Int) -> Int { x }; ",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    assert!(
        checked
            .to_binary()
            .unwrap_err()
            .to_string()
            .contains("unsupported executable")
    );

    let source = sources.write("enum.cdsl", "pub enum Term { Leaf }");
    let checked = compile_file(&source).unwrap().lower().unwrap();
    let names = std::collections::BTreeMap::from([(vec!["Term".into()], "i64".into())]);
    assert!(Schema::with_names(&checked, names).is_err());

    let source = sources.write(
        "bad_merge.cdsl",
        "use std::Table; use std::Merge; pub let Bad: Table<Int, Int, Merge::Union>;",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    assert!(Schema::new(&checked).is_err());
}
