// The oatlog thesis benchmarks (appendix D): math, boolean_adder, and math
// with fuel on `Integral`. utils/unit-tests/src/egglog/reference.rs checks
// their table sizes against upstream egglog.

use tir_egglog::prelude::*;
use tir_egglog::{Database, Rules, SortKind, Value};

// An Eq sort and one constructor per variant.
fn datatype(db: &mut Database, sort: &str, variants: &[(&str, &[&str])]) {
    let id = db.add_sort(sort, SortKind::Eq);
    for (name, args) in variants {
        let args: Vec<_> = args.iter().map(|a| db.sort(a).unwrap()).collect();
        db.add_constructor(name, &args, id);
    }
}

// Makes the term in the database and returns its e-class.
fn term(db: &mut Database, expr: &Expr) -> Value {
    match expr {
        Expr::Lit(_, Literal::Int(i)) => db.base(*i),
        Expr::Lit(_, Literal::String(s)) => db.base(s.clone()),
        Expr::Call(_, head, args) => {
            let args: Vec<Value> = args.iter().map(|arg| term(db, arg)).collect();
            let table = db.table(head).unwrap();
            db.entry(table, &args).unwrap()
        }
        other => panic!("not a ground term: {other}"),
    }
}

fn rewrites(db: &Database, rules: &mut Rules, pairs: Vec<(Expr, Expr)>) {
    for (lhs, rhs) in pairs {
        rules.rewrite(db, "", lhs, rhs).unwrap();
    }
}

// Thesis D.1. With `fuel` units, `Integral` takes a FuelUnit first, and every
// Integral rule but the Sin one needs and spends a unit.
pub fn math(fuel: Option<usize>) -> (Database, Rules) {
    let mut db = Database::new();
    if fuel.is_some() {
        let variants: &[(&str, &[&str])] = &[("Fuel", &["FuelUnit"]), ("ZeroFuel", &[])];
        datatype(&mut db, "FuelUnit", variants);
    }
    let fueled: &[&str] = if fuel.is_some() {
        &["FuelUnit", "Math", "Math"]
    } else {
        &["Math", "Math"]
    };
    let (one, two): (&[&str], &[&str]) = (&["Math"], &["Math", "Math"]);
    let variants: &[(&str, &[&str])] = &[
        ("Diff", two),
        ("Integral", fueled),
        ("Add", two),
        ("Sub", two),
        ("Mul", two),
        ("Div", two),
        ("Pow", two),
        ("Ln", one),
        ("Sqrt", one),
        ("Sin", one),
        ("Cos", one),
        ("Const", &["i64"]),
        ("Var", &["String"]),
    ];
    datatype(&mut db, "Math", variants);
    let m1 = exprs::int(-1);
    let mut rules = Rules::new();
    let pairs = vec![
        (
            expr!((Sub a b)),
            expr!((Add a (Mul (Const (unquote m1.clone())) b))),
        ),
        (
            expr!((Diff x (Cos x))),
            expr!((Mul (Const (unquote m1.clone())) (Sin x))),
        ),
        (expr!((Add a b)), expr!((Add b a))),
        (expr!((Mul a b)), expr!((Mul b a))),
        (expr!((Add a (Add b c))), expr!((Add (Add a b) c))),
        (expr!((Mul a (Mul b c))), expr!((Mul (Mul a b) c))),
        (expr!((Add a (Const 0))), expr!(a)),
        (expr!((Mul a (Const 0))), expr!((Const 0))),
        (expr!((Mul a (Const 1))), expr!(a)),
        (expr!((Mul a (Add b c))), expr!((Add (Mul a b) (Mul a c)))),
        (expr!((Add (Mul a b) (Mul a c))), expr!((Mul a (Add b c)))),
        (expr!((Mul (Pow a b) (Pow a c))), expr!((Pow a (Add b c)))),
        (expr!((Pow x (Const 1))), expr!(x)),
        (expr!((Pow x (Const 2))), expr!((Mul x x))),
        (
            expr!((Diff x (Add a b))),
            expr!((Add (Diff x a) (Diff x b))),
        ),
        (
            expr!((Diff x (Mul a b))),
            expr!((Add (Mul a (Diff x b)) (Mul b (Diff x a)))),
        ),
        (expr!((Diff x (Sin x))), expr!((Cos x))),
    ];
    rewrites(&db, &mut rules, pairs);
    // `(Integral f x)`, with a fuel argument first in the fuel variant.
    let int = |fuel: &str, f: Expr, x: Expr| {
        let fuel = match fuel {
            "spend" => Some(expr!((Fuel fuel))),
            _ => Some(exprs::var("fuel")),
        };
        let fuel = fuel.filter(|_| fueled.len() == 3);
        exprs::call("Integral", fuel.into_iter().chain([f, x]).collect())
    };
    let x = || exprs::var("x");
    let less = |f: &str| int("keep", exprs::var(f), x());
    let pairs = vec![
        (
            int("keep", expr!((Sin x)), x()),
            expr!((Mul (Const (unquote m1)) (Cos x))),
        ),
        (int("spend", expr!((Const 1)), x()), x()),
        (int("spend", expr!((Cos x)), x()), expr!((Sin x))),
        (
            int("spend", expr!((Add f g)), x()),
            exprs::call("Add", vec![less("f"), less("g")]),
        ),
        (
            int("spend", expr!((Sub f g)), x()),
            exprs::call("Sub", vec![less("f"), less("g")]),
        ),
        (
            int("spend", expr!((Mul a b)), x()),
            exprs::call(
                "Sub",
                vec![
                    exprs::call("Mul", vec![exprs::var("a"), less("b")]),
                    int(
                        "keep",
                        exprs::call("Mul", vec![expr!((Diff x a)), less("b")]),
                        x(),
                    ),
                ],
            ),
        ),
    ];
    rewrites(&db, &mut rules, pairs);
    let var = |name: &str| exprs::call("Var", vec![exprs::string(name)]);
    let units = fuel.map(|n| (0..n).fold(expr!((ZeroFuel)), |f, _| exprs::call("Fuel", vec![f])));
    for f in [
        exprs::call("Ln", vec![var("x")]),
        exprs::call("Add", vec![var("x"), exprs::call("Cos", vec![var("x")])]),
        exprs::call("Mul", vec![exprs::call("Cos", vec![var("x")]), var("x")]),
    ] {
        let args = units.clone().into_iter().chain([f, var("x")]).collect();
        term(&mut db, &exprs::call("Integral", args));
    }
    let (vx, vy, vz) = (var("x"), var("y"), var("z"));
    for t in [
        expr!((Diff (unquote vx.clone()) (Add (Const 1) (Mul (Const 2) (unquote vx.clone()))))),
        expr!((Diff (unquote vx.clone()) (Sub (Pow (unquote vx.clone()) (Const 3)) (Mul (Const 7) (Pow (unquote vx.clone()) (Const 2)))))),
        expr!((Add (Mul (unquote vy.clone()) (Add (unquote vx.clone()) (unquote vy.clone()))) (Sub (Add (unquote vx.clone()) (Const 2)) (Add (unquote vx.clone()) (unquote vx))))),
        expr!((Div (Const 1) (Sub (Div (Add (Const 1) (Sqrt (unquote vz.clone()))) (Const 2)) (Div (Sub (Const 1) (Sqrt (unquote vz))) (Const 2))))),
    ] {
        term(&mut db, &t);
    }
    (db, rules)
}

// Thesis D.2, with a ten-bit ripple-carry adder.
pub fn boolean_adder() -> (Database, Rules) {
    let mut db = Database::new();
    let (one, two): (&[&str], &[&str]) = (&["Math"], &["Math", "Math"]);
    let three: &[&str] = &["Math", "Math", "Math"];
    let variants: &[(&str, &[&str])] = &[
        ("And", two),
        ("Or", two),
        ("Xor", two),
        ("Not", one),
        ("HalfAddSum", two),
        ("HalfAddCarry", two),
        ("FullAddSum", three),
        ("FullAddCarry", three),
        ("Var", &["String"]),
        ("True", &[]),
        ("False", &[]),
    ];
    datatype(&mut db, "Math", variants);
    let mut rules = Rules::new();
    let mut pairs = vec![
        (expr!((And x (True))), expr!(x)),
        (expr!((And x (False))), expr!((False))),
        (expr!((Or x (False))), expr!(x)),
        (expr!((Or x (True))), expr!((True))),
        (expr!((Xor x (False))), expr!(x)),
        (expr!((Xor x (True))), expr!((Not x))),
        (expr!((And x x)), expr!(x)),
        (expr!((Or x x)), expr!(x)),
        (expr!((And x (Not x))), expr!((False))),
        (expr!((Or x (Not x))), expr!((True))),
        (expr!((Xor x (Not x))), expr!((True))),
        (expr!((And x y)), expr!((And y x))),
        (expr!((Or x y)), expr!((Or y x))),
        (expr!((Xor x y)), expr!((Xor y x))),
        (expr!((And x (Or x y))), expr!(x)),
        (expr!((Or x (And x y))), expr!(x)),
        (expr!((Not (Not x))), expr!(x)),
        (expr!((FullAddSum x y z)), expr!((Xor x (Xor y z)))),
        (
            expr!((FullAddCarry a b c)),
            expr!((Or (And c (Xor a b)) (And a b))),
        ),
        (expr!((HalfAddSum x y)), expr!((Xor x y))),
        (expr!((HalfAddCarry x y)), expr!((And x y))),
    ];
    let leaf = |name: &str| exprs::call(name, vec![]);
    for (op, table) in [
        ("And", [1, 0, 0, 0]),
        ("Or", [1, 1, 1, 0]),
        ("Xor", [0, 1, 1, 0]),
    ] {
        let inputs = [
            ("True", "True"),
            ("False", "True"),
            ("True", "False"),
            ("False", "False"),
        ];
        for ((a, b), out) in inputs.into_iter().zip(table) {
            let out = if out == 1 { "True" } else { "False" };
            pairs.push((exprs::call(op, vec![leaf(a), leaf(b)]), leaf(out)));
        }
    }
    rewrites(&db, &mut rules, pairs);
    for (lhs, rhs) in [
        (expr!((And x (Or y z))), expr!((Or (And x y) (And x z)))),
        (expr!((Not (And x y))), expr!((Or (Not x) (Not y)))),
        (expr!((Not (Or x y))), expr!((And (Not x) (Not y)))),
        (expr!((Not (Xor x y))), expr!((Xor x (Not y)))),
        (expr!((Xor x y)), expr!((Xor (Not x) (Not y)))),
    ] {
        rules.birewrite(&db, "", lhs, rhs).unwrap();
    }
    let var = |name: String| exprs::call("Var", vec![exprs::string(&name)]);
    let (x0, y0) = (var("x0".into()), var("y0".into()));
    term(
        &mut db,
        &exprs::call("HalfAddSum", vec![x0.clone(), y0.clone()]),
    );
    let mut carry = exprs::call("HalfAddCarry", vec![x0, y0]);
    term(&mut db, &carry);
    for bit in 1..=10 {
        let args = vec![carry, var(format!("x{bit}")), var(format!("y{bit}"))];
        term(&mut db, &exprs::call("FullAddSum", args.clone()));
        carry = exprs::call("FullAddCarry", args);
        term(&mut db, &carry);
    }
    (db, rules)
}
