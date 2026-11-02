//! tir-egglog on the oatlog thesis benchmarks and a seeded synthetic theory.
//! `step` and `saturate` time a run from the initial terms, as upstream
//! egglog's `(run N)` and `(run-schedule (saturate (run)))` would.

use std::cell::OnceCell;
use std::hint::black_box;

use tir_bench::{BatchSize, Criterion, TMAMetrics, criterion_group, criterion_main};
use tir_egglog::{Database, Program, Rules, Schedule};

#[path = "support/synth.rs"]
mod synth;
#[path = "support/theories.rs"]
mod theories;

type Theory = (&'static str, fn() -> (Database, Rules));

const THEORIES: [Theory; 6] = [
    ("math", || theories::math(None)),
    ("boolean_adder", theories::boolean_adder),
    ("fuel1_math", || theories::math(Some(1))),
    ("fuel2_math", || theories::math(Some(2))),
    ("fuel3_math", || theories::math(Some(3))),
    ("synth/small", || synth::synth(1_000)),
];

const MEDIUM: Theory = ("synth/medium", || synth::synth(10_000));
const SPARSE: Theory = ("sparse", || synth::sparse(150, 1_000_000, 8));

// The fuel theories share their rules, so one stands for them in compile and
// load.
const COMPILED: [Theory; 5] = [THEORIES[0], THEORIES[1], THEORIES[4], THEORIES[5], MEDIUM];

// Upstream egglog's rows over all tables after `(run N)`, N = 1..=8, and at
// the fixed point of the fuel theories, so a case measures the same work.
const MATH_ROWS: [usize; 8] = [69, 118, 208, 389, 784, 1576, 3160, 8113];
const ADDER_ROWS: [usize; 8] = [106, 241, 511, 727, 906, 1332, 2374, 5246];
const FUEL_ROWS: [usize; 3] = [973, 1516, 50021];

// Steps of a synth run: enough for the seminaive path to dominate the first
// naive iteration.
const SYNTH_STEPS: usize = 3;

fn rows(db: &Database) -> usize {
    let tables = db.schema().tables().map(|(table, _)| table);
    tables.map(|table| db.rows(table).count()).sum()
}

// The initial database and its compiled program. Criterion calls a routine
// many times, so the case builds them on the first call.
fn initial(theory: Theory, cell: &OnceCell<(Database, Program)>) -> &(Database, Program) {
    cell.get_or_init(|| {
        let (db, rules) = (theory.1)();
        let program = rules.compile(&db).unwrap();
        (db, program)
    })
}

// Times `schedule` on a fresh copy of the initial database, after checking
// once that it ends with `expected` rows.
fn run(
    c: &mut criterion::BenchmarkGroup<'_, TMAMetrics>,
    id: String,
    theory: Theory,
    schedule: Schedule,
    expected: Option<usize>,
) {
    let cell = OnceCell::new();
    c.bench_function(id, |b| {
        let (db, program) = initial(theory, &cell);
        let fresh = || (db.clone(), program.clone());
        if let Some(expected) = expected {
            let (mut db, mut program) = fresh();
            program.run(&mut db, &schedule).unwrap();
            assert_eq!(rows(&db), expected, "rows after {schedule:?}");
        }
        b.iter_batched(
            fresh,
            |(mut db, mut program)| {
                program.run(&mut db, &schedule).unwrap();
                db
            },
            BatchSize::LargeInput,
        );
    });
}

fn compile(c: &mut Criterion, _tma: &TMAMetrics) {
    let mut group = c.benchmark_group("egglog/compile");
    for theory in COMPILED {
        let cell = OnceCell::new();
        group.bench_function(theory.0, |b| {
            let (db, rules) = cell.get_or_init(theory.1);
            b.iter(|| black_box(rules.compile(db).unwrap()));
        });
    }
    group.finish();
}

fn load(c: &mut Criterion, _tma: &TMAMetrics) {
    let mut group = c.benchmark_group("egglog/load");
    for theory in COMPILED {
        let cell = OnceCell::new();
        group.bench_function(theory.0, |b| {
            let (db, program) = initial(theory, &cell);
            let bytes = program.to_bytes();
            let mut db = db.clone();
            b.iter(|| black_box(Program::from_bytes(black_box(&bytes), &mut db).unwrap()));
        });
    }
    group.finish();
}

fn step(c: &mut Criterion, _tma: &TMAMetrics) {
    for (theory, expected) in [(THEORIES[0], MATH_ROWS), (THEORIES[1], ADDER_ROWS)] {
        let mut group = c.benchmark_group(format!("egglog/step/{}", theory.0));
        for (n, rows) in (1..=8).zip(expected) {
            let schedule = Schedule::repeat(n, Schedule::run(""));
            run(&mut group, n.to_string(), theory, schedule, Some(rows));
        }
        group.finish();
    }
}

fn saturate(c: &mut Criterion, _tma: &TMAMetrics) {
    let mut group = c.benchmark_group("egglog/saturate");
    for (theory, rows) in THEORIES[2..5].iter().zip(FUEL_ROWS) {
        let schedule = Schedule::saturate(Schedule::run(""));
        run(
            &mut group,
            theory.0.to_owned(),
            *theory,
            schedule,
            Some(rows),
        );
    }
    group.finish();
}

fn synth(c: &mut Criterion, _tma: &TMAMetrics) {
    let mut group = c.benchmark_group("egglog/synth");
    for theory in [THEORIES[5], MEDIUM, SPARSE] {
        let size = theory.0.trim_start_matches("synth/");
        let schedule = Schedule::repeat(SYNTH_STEPS, Schedule::run(""));
        run(&mut group, size.to_owned(), theory, schedule, None);
    }
    group.finish();
}

// A clone rebuilds every key map, as egglog's push does.
fn clone(c: &mut Criterion, _tma: &TMAMetrics) {
    let mut group = c.benchmark_group("egglog/clone");
    for theory in [MEDIUM, SPARSE] {
        let cell = OnceCell::new();
        group.bench_function(theory.0, |b| {
            let (db, _) = cell.get_or_init(theory.1);
            b.iter(|| black_box(db.clone()));
        });
    }
    group.finish();
}

criterion_group!(benches, compile, load, step, saturate, synth, clone);
criterion_main!(benches);
