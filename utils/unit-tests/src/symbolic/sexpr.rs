use tir_adt::{APInt, RawBits};
use tir_graph::{Dag, GenericDag, MutDag, NodeId};
use tir_symbolic::lang::{
    build, execute, op_kind, op_name, parse, BuildError, SemBuilderHooks, SemExpr, SymKind,
    SymPayload, Value,
};

type Graph = GenericDag<SymKind, SymPayload<()>>;

/// Hooks whose `$get_vlen` splices a constant lane count; mirrors the vector dialect.
struct TestHooks {
    vlen: u64,
    width: Option<u64>,
}

impl SemBuilderHooks<Graph> for TestHooks {
    fn splice(&self, name: &str, g: &mut Graph) -> Option<NodeId> {
        match name {
            "get_vlen" => {
                let n = g.add_node(SymKind::Constant);
                g.set_leaf_data(n, SymPayload::Int(APInt::new(32, self.vlen)));
                Some(n)
            }
            _ => None,
        }
    }
    fn result_width(&self) -> Option<u64> {
        self.width
    }
}

fn no_hooks() -> TestHooks {
    TestHooks {
        vlen: 0,
        width: None,
    }
}

#[test]
fn parses_nested_list() {
    let e = parse("(set r (add lhs rhs))").unwrap();
    assert_eq!(
        e,
        SemExpr::List(vec![
            SemExpr::Atom("set".into()),
            SemExpr::Atom("r".into()),
            SemExpr::List(vec![
                SemExpr::Atom("add".into()),
                SemExpr::Atom("lhs".into()),
                SemExpr::Atom("rhs".into()),
            ]),
        ])
    );
}

#[test]
fn collects_splice_names_uniquely() {
    let e = parse("(set r (add (split a $get_vlen) (split b $get_vlen)))").unwrap();
    assert_eq!(e.splice_names(), vec!["get_vlen".to_string()]);
}

#[test]
fn builds_and_executes_binary_op() {
    let mut g = Graph::new();
    let root = build(
        &mut g,
        "(set result (add lhs rhs))",
        &[("lhs", 0), ("rhs", 1)],
        &no_hooks(),
    )
    .unwrap();
    assert_eq!(*g.get_kind(root), SymKind::Add);
    let out = execute(
        &g,
        &[
            Value::Int(APInt::new_signed(32, 3)),
            Value::Int(APInt::new_signed(32, 4)),
        ],
    );
    match out {
        Value::Int(v) => assert_eq!(v.to_i64(), 7),
        _ => panic!(),
    }
}

#[test]
fn builds_sext_with_result_width() {
    let mut g = Graph::new();
    let root = build(
        &mut g,
        "(set result (sext input))",
        &[("input", 0)],
        &TestHooks {
            vlen: 0,
            width: Some(64),
        },
    )
    .unwrap();
    assert_eq!(*g.get_kind(root), SymKind::SExt);
    let out = execute(&g, &[Value::Int(APInt::new_signed(8, -5))]);
    match out {
        Value::Int(v) => assert_eq!(v.to_i64(), -5),
        _ => panic!(),
    }
}

#[test]
fn builds_trunc_as_extract() {
    let mut g = Graph::new();
    let root = build(
        &mut g,
        "(set result (trunc input))",
        &[("input", 0)],
        &TestHooks {
            vlen: 0,
            width: Some(8),
        },
    )
    .unwrap();
    assert_eq!(*g.get_kind(root), SymKind::Extract);
    let out = execute(&g, &[Value::Int(APInt::new(32, 0x1234))]);
    match out {
        Value::Int(v) => assert_eq!(v.to_u64(), 0x34),
        _ => panic!(),
    }
}

#[test]
fn builds_vector_elementwise_via_splice() {
    // The vector dialect's shape: concat(map(zip(split a, split b), |x,y| x+y)).
    let mut g = Graph::new();
    build(
        &mut g,
        "(set result (concat (map (zip (split lhs $get_vlen) (split rhs $get_vlen)) (lambda (a b) (add a b)))))",
        &[("lhs", 0), ("rhs", 1)],
        &TestHooks { vlen: 2, width: None },
    )
    .unwrap();
    let a = Value::RawBits(RawBits::from_bytes(vec![0x01, 0x02]));
    let b = Value::RawBits(RawBits::from_bytes(vec![0x03, 0x04]));
    match execute(&g, &[a, b]) {
        Value::RawBits(bits) => assert_eq!(bits.bytes(), &[0x04, 0x06]),
        other => panic!("expected raw bits, got {other:?}"),
    }
}

#[test]
fn theta_is_a_binary_operator() {
    assert_eq!(op_kind("theta"), Some(SymKind::Theta));
    assert_eq!(op_name(SymKind::Theta), Some("theta"));
    assert_eq!(SymKind::Theta.arity(), 2);
}

#[test]
fn builds_comparison_and_if() {
    let mut g = Graph::new();
    build(
        &mut g,
        "(set r (if (ult a b) a b))",
        &[("a", 0), ("b", 1)],
        &no_hooks(),
    )
    .unwrap();
    let out = execute(
        &g,
        &[Value::Int(APInt::new(32, 7)), Value::Int(APInt::new(32, 3))],
    );
    match out {
        Value::Int(v) => assert_eq!(v.to_u64(), 3),
        _ => panic!(),
    }
}

#[test]
fn builds_explicit_width_extension() {
    // The binary form takes the target width from its operand, no hooks.
    let mut g = Graph::new();
    let root = build(&mut g, "(set r (zext x 16))", &[("x", 0)], &no_hooks()).unwrap();
    assert_eq!(*g.get_kind(root), SymKind::ZExt);
    match execute(&g, &[Value::Int(APInt::new(8, 0xff))]) {
        Value::Int(v) => {
            assert_eq!(v.width(), 16);
            assert_eq!(v.to_u64(), 0xff);
        }
        _ => panic!(),
    }
}

#[test]
fn wrong_arity_is_malformed() {
    let mut g = Graph::new();
    let err = build(&mut g, "(set r (add x))", &[("x", 0)], &no_hooks()).unwrap_err();
    assert_eq!(err, BuildError::BadForm("add".into()));
}

#[test]
fn missing_splice_errors() {
    let mut g = Graph::new();
    let err = build(&mut g, "(set r $nope)", &[], &no_hooks()).unwrap_err();
    assert_eq!(err, BuildError::MissingSplice("nope".into()));
}
