use tir_symbolic::lang::{SymKind, SymPayload};
use tir_symbolic::smtlib::ast::*;
use tir_symbolic::smtlib::convert::{lift_script, lower_script, ConvertError, Lowered, SymbolInfo};
use tir_symbolic::smtlib::parser::{parse_script, parse_term};

use tir_graph::{Dag, GenericDag, NodeId};

// ── Parser ─────────────────────────────────────────────────────────────────

#[test]
fn parses_bitvec_constant_term() {
    let t = parse_term("(_ bv13 8)").unwrap();
    assert_eq!(
        t,
        Term::Ident(QualIdentifier::Plain(Identifier {
            symbol: Symbol("bv13".into()),
            indices: vec![Index::Numeral(8)],
        }))
    );
}

#[test]
fn parses_application_and_literals() {
    let t = parse_term("(bvadd #x0f #b1010)").unwrap();
    match t {
        Term::App(QualIdentifier::Plain(id), args) => {
            assert_eq!(id.symbol, Symbol("bvadd".into()));
            assert_eq!(args.len(), 2);
            assert_eq!(
                args[0],
                Term::Constant(SpecConstant::Hexadecimal("0f".into()))
            );
            assert_eq!(args[1], Term::Constant(SpecConstant::Binary("1010".into())));
        }
        other => panic!("expected app, got {other:?}"),
    }
}

#[test]
fn parses_let_and_extract() {
    let t = parse_term("(let ((x #x0f)) ((_ extract 3 0) x))").unwrap();
    match t {
        Term::Let(binds, body) => {
            assert_eq!(binds.len(), 1);
            assert_eq!(binds[0].var, Symbol("x".into()));
            match *body {
                Term::App(QualIdentifier::Plain(id), _) => {
                    assert_eq!(id.symbol, Symbol("extract".into()));
                    assert_eq!(id.indices, vec![Index::Numeral(3), Index::Numeral(0)]);
                }
                other => panic!("expected extract app, got {other:?}"),
            }
        }
        other => panic!("expected let, got {other:?}"),
    }
}

#[test]
fn parses_forall_with_comment() {
    let t = parse_term("; a comment\n(forall ((x (_ BitVec 8))) (= x x))").unwrap();
    assert!(matches!(t, Term::Forall(_, _)));
}

#[test]
fn parses_script() {
    let src = "(set-logic QF_BV)\n\
               (declare-const x (_ BitVec 32))\n\
               (assert (= (bvadd x #x00000001) x))\n\
               (check-sat)\n\
               (exit)";
    let script = parse_script(src).unwrap();
    assert_eq!(script.0.len(), 5);
    assert_eq!(script.0[0], Command::SetLogic(Symbol("QF_BV".into())));
    assert!(matches!(script.0[1], Command::DeclareConst(_, _)));
    assert!(matches!(script.0[2], Command::Assert(_)));
    assert_eq!(script.0[3], Command::CheckSat);
    assert_eq!(script.0[4], Command::Exit);
}

#[test]
fn enforces_numeral_and_keyword_lexis() {
    assert_eq!(
        parse_term("0").unwrap(),
        Term::Constant(SpecConstant::Numeral(0))
    );
    // No leading zeros (other than a bare `0`).
    assert!(parse_term("0123").is_err());
    assert!(parse_script("(push 00)").is_err());
    // Keywords may not start with a digit.
    assert!(parse_script("(set-info :123 x)").is_err());
    assert!(parse_script("(set-info :status sat)").is_ok());
}

// ── Printer ────────────────────────────────────────────────────────────────

fn term_roundtrips(src: &str) {
    let a = parse_term(src).unwrap();
    let printed = a.to_string();
    let b = parse_term(&printed).unwrap();
    assert_eq!(a, b, "printed as `{printed}`");
}

#[test]
fn roundtrips_terms() {
    term_roundtrips("(_ bv13 8)");
    term_roundtrips("(bvadd #x0f #b1010)");
    term_roundtrips("(let ((x #x0f)) ((_ extract 3 0) x))");
    term_roundtrips("(forall ((x (_ BitVec 8))) (= x x))");
    term_roundtrips("(! (= x y) :named foo)");
    term_roundtrips("(as nil (List Int))");
}

#[test]
fn roundtrips_script() {
    let src = "(set-logic QF_BV)\n\
               (declare-const x (_ BitVec 32))\n\
               (assert (= (bvadd x #x00000001) x))\n\
               (check-sat)\n\
               (exit)";
    let a = parse_script(src).unwrap();
    let b = parse_script(&a.to_string()).unwrap();
    assert_eq!(a, b);
}

// ── SMT <-> graph conversion ───────────────────────────────────────────────

type Graph = GenericDag<SymKind, SymPayload<()>>;

fn lower(src: &str) -> Lowered<()> {
    lower_script::<()>(&parse_script(src).unwrap()).unwrap()
}

/// Structural isomorphism ignoring sharing and `SymbolId` numbering (symbols
/// compared by name).
fn iso(
    g1: &Graph,
    n1: NodeId,
    s1: &[SymbolInfo],
    g2: &Graph,
    n2: NodeId,
    s2: &[SymbolInfo],
) -> bool {
    let k1 = *g1.get_kind(n1);
    if k1 != *g2.get_kind(n2) {
        return false;
    }
    match k1 {
        SymKind::Symbol => {
            let name = |g: &Graph, n: NodeId, s: &[SymbolInfo]| match g.get_leaf_data(n) {
                Some(SymPayload::SymbolId(id)) => s[*id as usize].name.clone(),
                _ => unreachable!(),
            };
            name(g1, n1, s1) == name(g2, n2, s2)
        }
        SymKind::Constant => g1.get_leaf_data(n1) == g2.get_leaf_data(n2),
        _ => {
            let c1: Vec<_> = g1.children(n1).collect();
            let c2: Vec<_> = g2.children(n2).collect();
            c1.len() == c2.len() && c1.iter().zip(&c2).all(|(&a, &b)| iso(g1, a, s1, g2, b, s2))
        }
    }
}

/// SMT -> graph -> SMT -> graph must produce isomorphic graphs.
fn roundtrips(src: &str) {
    let a = lower(src);
    let script = lift_script(&a.graph, a.root, &a.symbols).unwrap();
    let b = lower_script::<()>(&parse_script(&script.to_string()).unwrap()).unwrap();
    assert!(
        iso(&a.graph, a.root, &a.symbols, &b.graph, b.root, &b.symbols),
        "round-trip diverged for `{src}`\nlifted to:\n{script}"
    );
}

#[test]
fn lowers_structure_and_sharing() {
    let lo = lower(
        "(declare-const x (_ BitVec 8))\
         (declare-const y (_ BitVec 8))\
         (assert (= (bvadd x y) x))",
    );
    let g = &lo.graph;
    assert_eq!(*g.get_kind(lo.root), SymKind::Eq);
    let rc: Vec<_> = g.children(lo.root).collect();
    assert_eq!(*g.get_kind(rc[0]), SymKind::Add);
    assert_eq!(*g.get_kind(rc[1]), SymKind::Symbol);
    let add: Vec<_> = g.children(rc[0]).collect();
    // Both occurrences of `x` share one node.
    assert_eq!(add[0], rc[1]);
    assert_eq!(lo.widths[lo.root.index()], Some(1));
    assert_eq!(lo.widths[rc[0].index()], Some(8));
}

#[test]
fn lowers_extract_and_literal() {
    let lo = lower(
        "(declare-const x (_ BitVec 8))\
         (assert (= ((_ extract 3 0) x) #x5))",
    );
    let g = &lo.graph;
    let rc: Vec<_> = g.children(lo.root).collect();
    assert_eq!(*g.get_kind(rc[0]), SymKind::Extract);
    assert_eq!(lo.widths[rc[0].index()], Some(4));
    // `#x5` is a 4-bit constant.
    match g.get_leaf_data(rc[1]) {
        Some(SymPayload::Int(v)) => {
            assert_eq!(v.width(), 4);
            assert_eq!(v.to_u64(), 5);
        }
        other => panic!("expected constant, got {other:?}"),
    }
}

#[test]
fn empty_assertions_lower_to_true() {
    let lo = lower("(declare-const x (_ BitVec 8))");
    assert_eq!(*lo.graph.get_kind(lo.root), SymKind::Constant);
}

#[test]
fn inlines_define_fun() {
    let lo = lower(
        "(declare-const x (_ BitVec 8))\
         (define-fun dbl ((a (_ BitVec 8))) (_ BitVec 8) (bvadd a a))\
         (assert (= (dbl x) x))",
    );
    let g = &lo.graph;
    let rc: Vec<_> = g.children(lo.root).collect();
    assert_eq!(*g.get_kind(rc[0]), SymKind::Add);
    let add: Vec<_> = g.children(rc[0]).collect();
    assert_eq!(add[0], add[1]); // both `a` bind to the same `x` node
}

#[test]
fn roundtrips_bitvec_terms() {
    roundtrips(
        "(declare-const x (_ BitVec 8))\
         (declare-const y (_ BitVec 8))\
         (assert (= (bvadd x y) x))",
    );
    roundtrips(
        "(declare-const x (_ BitVec 16))\
         (assert (bvule ((_ extract 7 0) x) #x0f))",
    );
    roundtrips(
        "(declare-const x (_ BitVec 8))\
         (assert (= ((_ zero_extend 4) x) ((_ sign_extend 4) x)))",
    );
    roundtrips(
        "(declare-const a (_ BitVec 4))\
         (declare-const b (_ BitVec 4))\
         (assert (= (concat a b) (concat b a)))",
    );
    // `let` sharing collapses to identical subtrees on re-lowering.
    roundtrips(
        "(declare-const x (_ BitVec 8))\
         (declare-const y (_ BitVec 8))\
         (assert (let ((z (bvand x y))) (= z z)))",
    );
    // Boolean structure: and/or/not over comparisons stays boolean.
    roundtrips(
        "(declare-const x (_ BitVec 8))\
         (assert (and (bvult x #x0a) (not (= x #x00))))",
    );
    // A boolean constant in boolean position survives as true/false.
    roundtrips(
        "(declare-const x (_ BitVec 8))\
         (assert (and true (= x #x00)))",
    );
    // Bool-sorted symbols re-declare as Bool and stay boolean operands.
    roundtrips(
        "(declare-const b Bool)\
         (declare-const x (_ BitVec 8))\
         (assert (and b (bvult x #x05)))",
    );
}

#[test]
fn rejects_oversized_or_zero_widths_without_panicking() {
    // 17 hex digits = 68 bits, > the 64-bit APInt backing.
    let cases = [
        "(assert (= #x00000000000000000 #x00000000000000000))",
        "(declare-const x (_ BitVec 100)) (assert (= x x))",
        "(assert (= (_ bv0 0) (_ bv0 0)))",
    ];
    for src in cases {
        let script = parse_script(src).unwrap();
        assert!(
            lower_script::<()>(&script).is_err(),
            "expected error (not panic) for `{src}`"
        );
    }
}

#[test]
fn rejects_quantifiers() {
    let script = parse_script(
        "(declare-const x (_ BitVec 8))\
         (assert (forall ((y (_ BitVec 8))) (= x y)))",
    )
    .unwrap();
    assert!(matches!(
        lower_script::<()>(&script),
        Err(ConvertError::Quantifier)
    ));
}

#[test]
fn rejects_unknown_symbol() {
    let script = parse_script("(assert (= x x))").unwrap();
    match lower_script::<()>(&script) {
        Err(ConvertError::UnknownSymbol(s)) => assert_eq!(s, "x"),
        _ => panic!("expected unknown-symbol error"),
    }
}

#[test]
fn rejects_bvsmod() {
    let script = parse_script(
        "(declare-const x (_ BitVec 8))\
         (declare-const y (_ BitVec 8))\
         (assert (= (bvsmod x y) x))",
    )
    .unwrap();
    assert!(matches!(
        lower_script::<()>(&script),
        Err(ConvertError::Unsupported(_))
    ));
}
