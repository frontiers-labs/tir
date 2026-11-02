//! Table sizes of the oatlog thesis benchmarks (appendix D) against upstream
//! egglog. The expected rows per table are what egglog 4b5686be prints with
//! `(print-size)` after `(run N)` on the same theory and terms (math and
//! boolean_adder), or after `(run-schedule (saturate (run)))` (fuel).

use tir_egglog::{Database, Schedule};

// The theories and terms, shared with the benchmarks.
#[path = "../../../../libs/egglog/benches/support/theories.rs"]
mod theories;
use theories::{boolean_adder, math};

fn sizes(db: &Database, tables: &[&str]) -> Vec<usize> {
    let count = |name: &&str| db.rows(db.table(name).unwrap()).count();
    tables.iter().map(count).collect()
}

const MATH: &[&str] = &[
    "Add", "Const", "Cos", "Diff", "Div", "Integral", "Ln", "Mul", "Pow", "Sin", "Sqrt", "Sub",
    "Var",
];

// Rows per table of MATH after each step.
const MATH_SIZES: [[usize; 13]; 9] = [
    [7, 4, 1, 2, 3, 3, 1, 4, 2, 0, 1, 4, 3],
    [20, 5, 1, 5, 3, 6, 1, 17, 2, 0, 1, 5, 3],
    [36, 5, 1, 10, 3, 9, 1, 39, 2, 1, 1, 7, 3],
    [69, 5, 1, 20, 3, 15, 1, 77, 2, 1, 1, 10, 3],
    [150, 5, 1, 30, 3, 29, 1, 145, 2, 1, 1, 18, 3],
    [317, 5, 1, 41, 3, 60, 1, 314, 2, 1, 1, 35, 3],
    [646, 5, 1, 77, 3, 135, 1, 614, 2, 1, 1, 87, 3],
    [1165, 5, 1, 142, 3, 314, 1, 1330, 2, 1, 1, 192, 3],
    [2977, 5, 1, 338, 3, 782, 1, 3516, 2, 1, 1, 483, 3],
];

#[test]
fn math_matches_egglog_for_steps_0_to_8() {
    let (mut db, rules) = math(None);
    let mut program = rules.compile(&db).unwrap();
    for (step, expected) in MATH_SIZES.iter().enumerate() {
        if step > 0 {
            program.run(&mut db, &Schedule::run("")).unwrap();
        }
        assert_eq!(sizes(&db, MATH), expected, "math after {step} steps");
    }
}

const ADDER: &[&str] = &[
    "And",
    "False",
    "FullAddCarry",
    "FullAddSum",
    "HalfAddCarry",
    "HalfAddSum",
    "Not",
    "Or",
    "True",
    "Var",
    "Xor",
];

// Rows per table of ADDER after each step.
const ADDER_SIZES: [[usize; 11]; 9] = [
    [0, 0, 10, 10, 1, 1, 0, 0, 0, 22, 0],
    [21, 0, 10, 10, 1, 1, 0, 10, 0, 22, 31],
    [42, 0, 10, 10, 1, 1, 42, 20, 0, 22, 93],
    [78, 0, 10, 10, 1, 1, 133, 40, 0, 22, 216],
    [128, 0, 10, 10, 1, 1, 197, 110, 0, 22, 248],
    [208, 0, 10, 10, 1, 1, 212, 194, 0, 22, 248],
    [386, 0, 10, 10, 1, 1, 316, 338, 0, 22, 248],
    [842, 0, 10, 10, 1, 1, 474, 766, 0, 22, 248],
    [2020, 0, 10, 10, 1, 1, 884, 2050, 0, 22, 248],
];

#[test]
fn boolean_adder_matches_egglog_for_steps_0_to_8() {
    let (mut db, rules) = boolean_adder();
    let mut program = rules.compile(&db).unwrap();
    for (step, expected) in ADDER_SIZES.iter().enumerate() {
        if step > 0 {
            program.run(&mut db, &Schedule::run("")).unwrap();
        }
        assert_eq!(
            sizes(&db, ADDER),
            expected,
            "boolean_adder after {step} steps"
        );
    }
}

const FUEL: &[&str] = &[
    "Add", "Const", "Cos", "Diff", "Div", "Fuel", "Integral", "Ln", "Mul", "Pow", "Sin", "Sqrt",
    "Sub", "Var", "ZeroFuel",
];

#[test]
fn fuel_math_saturates_to_egglog_sizes() {
    let expected: [(usize, [usize; 15]); 2] = [
        (1, [613, 5, 1, 18, 3, 1, 7, 1, 310, 2, 1, 1, 6, 3, 1]),
        (2, [743, 5, 1, 24, 3, 2, 22, 1, 693, 2, 1, 1, 14, 3, 1]),
    ];
    for (fuel, sizes_at_fixpoint) in expected {
        let (mut db, rules) = math(Some(fuel));
        let mut program = rules.compile(&db).unwrap();
        program.run(&mut db, &Schedule::saturate("")).unwrap();
        assert_eq!(sizes(&db, FUEL), sizes_at_fixpoint, "fuel{fuel}_math");
    }
}
