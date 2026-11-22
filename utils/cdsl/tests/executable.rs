use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
use tir_cdsl::{
    OutputKind::{OpDef, Program as RustProgram},
    Schema, compile_file, compile_source,
};
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

#[test]
fn rust_emission_materializes_both_fp_refinement_paths() {
    let sources = Sources::new();
    sources.write(
        "defs/fp.cdsl",
        include_str!("../../../core/defs/dialects/fp.cdsl"),
    );
    sources.write("defs/lib.cdsl", "pub mod fp;");
    let built = sources.emit(&[("fixture::fp::model::*", "fp.rs")]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let probe = sources.write(
        "main.rs",
        r#"
#[allow(dead_code, unused_imports)]
mod fp { include!("out/fp.rs"); }
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
fn ordinary_enum_functions_do_not_export_rule_collections() {
    let checked = compile_source(
        r#"
use std::option::Option;
pub let ordinary_option = || -> Option<Prop> { Option::None<Prop> };
"#,
    )
    .unwrap()
    .lower()
    .unwrap();
    let generated = checked.render_program(&[]).unwrap();
    let rust = syn::parse_file(&generated).unwrap();
    assert!(!rust.items.iter().any(|item| {
        matches!(item, syn::Item::Fn(function) if function.sig.ident == "ordinary_option")
    }));
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

    let tuple = compile_source(
        "let Out: Int -> Prop; pub let collection = || -> (Prop, Prop) { let seed: Out(0); (seed, seed) };",
    )
    .unwrap()
    .lower()
    .unwrap();
    assert!(tuple.to_binary().is_err());

    let source = sources.write(
        "bad_merge.cdsl",
        "use std::Table; use std::Merge; pub let Bad: Table<Int, Int, Merge::Union>;",
    );
    let checked = compile_file(&source).unwrap().lower().unwrap();
    assert!(Schema::new(&checked).is_err());
}

#[test]
fn integer_folding_preserves_local_semantics_and_rejects_unsupported_terms() {
    if env::var_os("CDSL_TEST_BUILD_FOLD").is_some() {
        tir_cdsl::compile([("fixture::fold::*", RustProgram("fold.rs"))]).unwrap();
        return;
    }
    let compile = |exec: &str, extra: &str| {
        let source = r#"
use std::bigint::BigInt;
use std::schedule::Schedule;
let integers = std::schedule::integer_rules(dialects::builtin, dialects::builtin::ConstantLike<1>, {
    binary: EqSatBinaryKind,
    flag_field: EqSatFlagField,
    result: EqSatIntegerResult,
    operand: EqSatIntegerOperand,
    folded: EqSatFolded,
    constant_kind: EqSatConstantImplementation,
    flag_value: EqSatFlag,
    constant: EqSatConstant,
    attribute: Attribute,
    attr_int: attr_int,
    attr_case: attr_case,
    pick_case: pick_case,
    bool_bit: bool_bit,
    shift_amount: shift_amount,
});
pub let evaluate = || { Schedule::Rules(integers.fold) };
pub let discover = || { Schedule::Rules(integers.flags) };
pub mod input {
    pub let constant: EqSatConstantImplementation(dialects::builtin::zero.kind);
    pub let results: (
        EqSatIntegerResult(std::ir::operation_key(1), dialects::builtin::r#divide.kind, std::ir::value_key(1), 8),
        EqSatIntegerResult(std::ir::operation_key(2), dialects::builtin::r#divide.kind, std::ir::value_key(2), 8),
        EqSatIntegerResult(std::ir::operation_key(3), dialects::builtin::r#divide.kind, std::ir::value_key(3), 8),
        EqSatIntegerResult(std::ir::operation_key(4), dialects::builtin::r#wide_sum.kind, std::ir::value_key(4), 16),
        EqSatIntegerResult(std::ir::operation_key(5), dialects::builtin::r#zero.kind, std::ir::value_key(5), 16),
        EqSatIntegerResult(std::ir::operation_key(6), dialects::builtin::r#unary_zero.kind, std::ir::value_key(6), 16),
        EqSatIntegerResult(std::ir::operation_key(7), dialects::builtin::r#nullary_zero.kind, std::ir::value_key(7), 16),
    );
}
pub let seed = || { Schedule::Rules((input::constant, input::results)) };

let Attribute: (String, Int, String, Int) -> Prop;
let attr_int: Int -> BigInt;
let attr_case: Int -> String;
let pick_case: (String, String, Bool, Bool) -> Bool;
let bool_bit: Bool -> BigInt;
let shift_amount: BigInt -> Int;
pub let EqSatIntegerResult: (std::ir::OperationId, std::ir::OperationKind, std::ir::ValueId, Int) -> Prop;
pub let EqSatIntegerOperand: (std::ir::OperationId, Int, std::ir::ValueId, Int) -> Prop;
pub let EqSatFolded: (std::ir::ValueId, BigInt) -> Prop;
pub let EqSatConstant: (std::ir::OperationId, std::ir::ValueId, BigInt) -> Prop;
pub let EqSatFlagField: (String, Bool) -> Prop;
pub let EqSatFlag: (std::ir::OperationId, String, Bool, Bool) -> Prop;
pub let EqSatBinaryKind: std::ir::BinaryIntegerOperation -> Prop;
pub let EqSatConstantImplementation: std::ir::OperationKind -> Prop;
pub mod analyze { pub let marker: EqSatFlagField("unused", false); }
pub mod branch_local { pub let marker: EqSatFlagField("unused", false); }
pub mod dialects {
    pub mod builtin {
        use std::bits;
        use std::option;
        use std::ir::Bits;
        use std::ir::El;
        let Guards: Type = { reject_equal: option::Option<Bool> };
        pub interface ConstantLike<w: Nat> { constant_value: () -> El<Bits<w>> }
        pub let divide<w: Nat> = op {
            signature: (lhs: El<Bits<w>>, rhs: El<Bits<w>>) -> El<Bits<w>>,
            attributes: Guards,
            exec: |rhs: El<Bits<w>>, lhs: El<Bits<w>>| -> El<Bits<w>> { EXEC },
            requires: |numerator: El<Bits<w>>, divisor: El<Bits<w>>| -> Bool {
                let zero = bits::zero<w>();
                let nonzero = bits::ne(divisor, zero);
                nonzero
            },
            poison: |attrs: Guards, numerator: El<Bits<w>>, divisor: El<Bits<w>>| -> Bool {
                let guard = attrs;
                let same = bits::eq(numerator, divisor);
                option::get_or(guard.reject_equal, false) && same
            },
        };
        pub let wide_sum = op {
            signature: (x: Bits<8>, y: Bits<16>) -> Bits<16>,
            exec: |x: El<Bits<8>>, y: El<Bits<16>>| -> El<Bits<16>> {
                let total = bits::add_wrapping(y, y);
                total
            },
        };
        pub let binary_without_fold<w: Nat> = op {
            signature: (x: Bits<w>, y: Bits<w>) -> Bits<w>,
        };
        pub let compare<w: Nat> = op {
            signature: (x: Bits<w>, y: Bits<w>) -> Bits<1>,
        };
        pub let fixed_width = op {
            signature: (x: Bits<8>, y: Bits<8>) -> Bits<8>,
        };
        pub let ternary<w: Nat> = op {
            signature: (x: Bits<w>, y: Bits<w>, z: Bits<w>) -> Bits<w>,
        };
        pub let zero = op {
            signature: () -> Bits<16>,
            interfaces: { ConstantLike: { constant_value: || { bits::zero<16>() } } },
        };
        pub let unary_zero<w: Nat> = op {
            signature: (input: Bits<w>) -> Bits<w>,
            exec: |input: El<Bits<w>>| -> El<Bits<w>> {
                let z = bits::zero();
                bits::add_wrapping(z, z)
            },
        };
        pub let nullary_zero = op {
            signature: () -> Bits<16>,
            exec: || -> El<Bits<16>> { bits::add_wrapping(bits::zero(), bits::zero()) },
        };
        EXTRA
    }
}
"#
        .replace("EXEC", exec)
        .replace("EXTRA", extra);
        let (rules, dialects) = source.split_once("pub mod dialects").unwrap();
        let sources = Sources::new();
        sources.write(
            "defs/lib.cdsl",
            &format!("pub mod fold; pub mod dialects{dialects}"),
        );
        sources.write("defs/fold.cdsl", rules);
        let built = sources
            .build("integer_folding_preserves_local_semantics_and_rejects_unsupported_terms")
            .env("CDSL_TEST_BUILD_FOLD", "1")
            .output()
            .unwrap();
        if !built.status.success() {
            return Err(String::from_utf8_lossy(&built.stderr).into_owned());
        }
        Ok(sources)
    };
    let sources = compile(
        "let rhs = bits::udiv(rhs, lhs);
         let twice = { let lhs = bits::add_wrapping(rhs, rhs); lhs };
         bits::sub_wrapping(twice, rhs)",
        "",
    )
    .unwrap();
    let engine = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../libs/egglog");
    sources.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"fold-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[dependencies]\ntir-egglog = {{ path = {engine:?} }}\n"
        ),
    );
    sources.write(
        "src/main.rs",
        r#"
#![allow(dead_code)]
mod generated { include!("../out/fold.rs"); }
use tir_egglog::{EGraph, sort::BigInt};
use generated::{OperationId, ValueId};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = EGraph::default();
    let db = engine.database_mut();
    generated::install_schema(db)?;
    let binary_kinds = generated::table_eq_sat_binary_kind(db)?;
    let text = db.base("builtin.divide".to_owned());
    assert!(matches!(
        db.insert(binary_kinds, &[text]),
        Err(tir_egglog::Error::Sort { column: 0, .. })
    ));
    let mut program = generated::Program::load(engine.database_mut())?;
    program.seed()?;
    program.insert_eq_sat_integer_operand((OperationId::from_raw(6), 0, ValueId::from_raw(60), 16))?;
    program.insert_eq_sat_folded((ValueId::from_raw(60), BigInt::new(7)))?;
    for (node, lhs, rhs, rhs_width) in [(1, 84, 20, 8), (2, 84, 0, 8), (3, 20, 20, 8), (4, 7, 160, 16)] {
        program.insert_eq_sat_flag((OperationId::from_raw(node), "reject_equal".into(), false, true))?;
        for (slot, bits, width) in [(0, lhs, 8), (1, rhs, rhs_width)] {
            let input = node * 10 + slot;
            program.insert_eq_sat_integer_operand((OperationId::from_raw(node), i64::from(slot), ValueId::from_raw(input), width))?;
            program.insert_eq_sat_folded((ValueId::from_raw(input), BigInt::new(bits)))?;
        }
    }
    program.rebuild()?;
    program.evaluate()?;
    let mut rows = program.eq_sat_constant()
        .map(|row| row.map(|(node, _, bits)| (node, bits)))
        .collect::<Result<Vec<_>, _>>()?;
    rows.sort_by_key(|(node, _)| *node);
    assert_eq!(rows, [(1, 4), (4, 320), (6, 0), (7, 0)].map(|(id, value)| (OperationId::from_raw(id), BigInt::new(value))));
    assert!(program.eq_sat_folded().any(|row| row.is_ok_and(|(id, bits)| id == ValueId::from_raw(5) && bits == BigInt::new(0))));

    program.discover()?;
    let mut kinds = program.eq_sat_binary_kind()
        .map(|row| row.map(|(kind,)| kind.as_str().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    kinds.sort();
    assert_eq!(kinds, ["builtin.binary_without_fold", "builtin.divide"]);
    drop(program);
    let db = engine.database_mut();
    let results = generated::table_eq_sat_integer_result(db)?;
    let wrong = db.rows(results)
        .find(|row| db.base_value::<i64>(row[0]) == 5)
        .ok_or("missing nullary operation declaration")?[1];
    db.clear(binary_kinds);
    db.insert(binary_kinds, &[wrong])?;
    let program = generated::Program::load(db)?;
    assert!(program.eq_sat_binary_kind().next().ok_or("missing raw row")?.is_err());
    Ok(())
}
"#,
    );
    let probe = Command::new(env!("CARGO"))
        .args(["run", "--offline", "--quiet", "--manifest-path"])
        .arg(sources.0.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", sources.0.join("target"))
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );

    let error = compile("if bits::eq(lhs, rhs) { lhs } else { rhs }", "").unwrap_err();
    assert!(error.contains("builtin.divide.exec"), "{error}");

    for (field, operation) in [
        (
            "exec",
            r#"
pub let refused = op {
    signature: () -> Bits<16>,
    exec: || -> El<Bits<16>> { if true { bits::zero() } else { bits::zero() } },
};
"#,
        ),
        (
            "requires",
            r#"
pub let refused = op {
    signature: () -> Bits<16>,
    exec: || -> El<Bits<16>> { bits::zero() },
    requires: || -> Bool {
        let z: El<Bits<8>> = bits::zero();
        bits::slt(z, z)
    },
};
"#,
        ),
    ] {
        let error = compile("bits::udiv(rhs, lhs)", operation).unwrap_err();
        assert!(
            error.contains(&format!("builtin.refused.{field}")),
            "{error}"
        );
    }

    let error = compile(
        "bits::udiv(rhs, lhs)",
        r#"
pub let wrong_width = op {
    signature: () -> Bits<16>,
    interfaces: {
        ConstantLike: { constant_value: || -> El<Bits<8>> { bits::zero<8>() } },
    },
};
"#,
    )
    .unwrap_err();
    assert!(
        error.contains("builtin.wrong_width.ConstantLike.constant_value"),
        "{error}"
    );
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
fn cargo_dependencies_supply_crate_names_and_keep_their_own_module_roots() {
    if env::var_os("CDSL_TEST_DEPENDENCY_BUILD").is_some() {
        tir_cdsl::compile([("consumer::*", RustProgram("consumer.rs"))]).unwrap();
        let consumer = tir_cdsl::resolve_crate("consumer")
            .unwrap()
            .lower()
            .unwrap();
        let schema = Schema::new(&consumer).unwrap();
        let lowered = schema.lower(&consumer).unwrap();
        let mut db = schema.database().clone();
        let mut executable = lowered.compile(&db).unwrap();
        let execute = &lowered.schedules()[&vec!["execute".into()]];
        executable.run(&mut db, execute, &[]).unwrap();
        let outcome = db.rows(db.table("Sink").unwrap()).next().unwrap();
        assert_eq!(db.base_value::<i64>(outcome[0]), 5);

        // The consumer's serialized operation import must link against a schema
        // built by the declaring package on its own.
        let producer = tir_cdsl::resolve_crate("tir").unwrap().lower().unwrap();
        let mut producer_db = Schema::new(&producer).unwrap().database().clone();
        Program::from_bytes(&consumer.to_binary().unwrap(), &mut producer_db).unwrap();
        return;
    }

    let sources = Sources::new();
    sources.write(
        "Cargo.toml",
        r#"[workspace]
resolver = "3"
members = ["core", "bridge", "consumer"]
[workspace.dependencies]
tir = { path = "core" }
"#,
    );
    for package in ["core", "bridge", "consumer"] {
        sources.write(&format!("{package}/src/lib.rs"), "");
    }
    sources.write(
        "core/Cargo.toml",
        "[package]\nname = \"tir\"\nversion = \"0.1.0\"\n",
    );
    sources.write(
        "core/defs/lib.cdsl",
        r#"
pub mod ir;
pub let Word: Type = std::ir::El<std::ir::Bits<8>>;
pub mod dialects {
    pub mod example {
        pub let copy = op { signature: (value: super::super::Word) -> crate::Word };
    }
}
pub let Kind: std::ir::OperationKind -> Prop;
pub let identify = || { let seed: Kind(dialects::example::copy.kind); std::schedule::saturate(seed) };
"#,
    );
    sources.write(
        "core/defs/ir/mod.cdsl",
        "pub enum Mark { Read, } pub let Edge: (Int, Int) -> Prop; let Private: Int -> Prop;",
    );
    sources.write(
        "bridge/Cargo.toml",
        r#"[package]
name = "bridge"
version = "0.1.0"
[dependencies]
renamed = { package = "tir", path = "../core" }
"#,
    );
    sources.write("bridge/defs/lib.cdsl", "pub use renamed::ir::Edge as Link;");
    sources.write(
        "consumer/Cargo.toml",
        r#"[package]
name = "consumer"
version = "0.1.0"
[dependencies]
tir.workspace = true
bridge = { path = "../bridge" }
"#,
    );
    let consumer = sources.write(
        "consumer/defs/lib.cdsl",
        r#"
use tir::ir::*;
use bridge::Link;
pub let Sink: Int -> Prop;
let Private: (Int, Int) -> Prop;
pub let Seen: std::ir::OperationKind -> Prop;
pub let Marker: tir::ir::Mark -> Prop;
pub let execute = || {
    let seed: (Edge(2, 5), Marker(tir::ir::Mark::Read));
    let copy<x, y>: Link(x, y) -> self::Sink(y);
    std::schedule::saturate((seed, copy))
};
pub let identify = || {
    let seed: Seen(tir::dialects::example::copy.kind);
    std::schedule::saturate(seed)
};
"#,
    );

    let build = sources
        .build("cargo_dependencies_supply_crate_names_and_keep_their_own_module_roots")
        .env("CDSL_TEST_DEPENDENCY_BUILD", "1")
        .current_dir(&sources.0)
        .env("CARGO_MANIFEST_DIR", sources.0.join("consumer"))
        .env("CARGO_PKG_NAME", "consumer")
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stdout)
    );
    assert!(sources.0.join("out/consumer.rs").is_file());

    let package_error = || {
        let result = Command::new(env!("CARGO_BIN_EXE_cdslc"))
            .args([
                "--crate", "consumer", "::*", "--emit", "ast", "--output", "-",
            ])
            .current_dir(&sources.0)
            .output()
            .unwrap();
        assert!(!result.status.success());
        String::from_utf8(result.stderr).unwrap()
    };
    assert!(compile_file(&consumer).is_err());

    sources.write("consumer/defs/lib.cdsl", "use tir::ir::Private;");
    assert!(package_error().contains("tir::ir::Private"));
    sources.write(
        "consumer/Cargo.toml",
        "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\n[dependencies]\nbridge = { path = \"../bridge\" }\n",
    );
    sources.write(
        "consumer/defs/lib.cdsl",
        "use bridge::Link; use tir::ir::Edge;",
    );
    assert!(package_error().contains("tir::ir::Edge"));
}
