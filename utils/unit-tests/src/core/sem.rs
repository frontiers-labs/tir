//! SemNode e-graph identity and the serialized sem-op blob format.

use tir::graph::{Dag, MutDag};
use tir::sem::{
    decode_sem_ops, int_payload, ExtendSemBytes, IrOp, Kind, Prov, SemBlobBuilder, SemGraph,
    SemNode, SemOp, SemPayloadDesc, SymKind,
};
use tir::{attributes::NamedAttribute, Context, TypeId};
use tir_adt::APInt;
use tir_symbolic::egraph::{EGraph, Id, Pattern, Var};

fn konst(width: u32, value: u64) -> SemNode {
    SemNode::constant(APInt::new(width, value), Prov::None)
}

fn ty(n: u32) -> TypeId {
    TypeId::from_number(n)
}

fn op(name: &'static str, ty: TypeId, attrs: Vec<NamedAttribute>, args: Vec<Id>) -> SemNode {
    SemNode {
        kind: Kind::Ir(IrOp {
            dialect: "builtin",
            name,
            attrs,
            commutative: false,
            cost: 1,
        }),
        payload: None,
        ty: Some(ty),
        children: args,
        prov: Prov::Introduced(0),
    }
}

fn op_pattern(name: &'static str, args: Vec<Id>) -> SemNode {
    op(name, ty(0), Vec::new(), Vec::new())
        .op_template(args)
        .expect("an op node has an op template")
}

// Exercise the `ENode` hash-cons paths lit can't reach (`cmpi` and constant
// width/bits identity); op result-type identity is covered by type_in_key.tir.

#[test]
fn equal_constants_share_a_class_distinct_widths_do_not() {
    let mut g: EGraph<SemNode> = EGraph::new();
    let a = g.add(konst(32, 0));
    let b = g.add(konst(32, 0));
    let c = g.add(konst(64, 0));
    assert_eq!(g.find(a), g.find(b));
    assert_ne!(g.find(a), g.find(c));
}

// Ops differing only in a value attribute must not hash-cons; identical ones must.
#[test]
fn ops_differing_in_attributes_stay_distinct() {
    let mut g: EGraph<SemNode> = EGraph::new();
    let x = g.add(konst(32, 0));
    let context = Context::with_default_dialects();
    let cmpi = |pred: &str, args: Vec<Id>| {
        let attrs = vec![context.named_attribute(
            "predicate",
            tir::attributes::AttributeValue::Str(pred.to_string().into()),
        )];
        op("cmpi", ty(1), attrs, args)
    };
    let slt = g.add(cmpi("slt", vec![x]));
    let sgt = g.add(cmpi("sgt", vec![x]));
    let slt2 = g.add(cmpi("slt", vec![x]));
    assert_ne!(g.find(slt), g.find(sgt));
    assert_eq!(g.find(slt), g.find(slt2));
}

// `addi i32`/`addi i64` share a search bucket (a wildcard visits both) yet must
// stay in distinct classes — merging would miscompile.
#[test]
fn wildcard_search_groups_result_types_without_merging_them() {
    let mut g: EGraph<SemNode> = EGraph::new();
    let x = g.add(konst(32, 0));
    let addi = |t: TypeId, args: Vec<Id>| op("addi", t, vec![], args);
    let a32 = g.add(addi(ty(32), vec![x, x]));
    let a64 = g.add(addi(ty(64), vec![x, x]));

    assert_ne!(g.find(a32), g.find(a64));

    let mut p: Pattern<SemNode, u32> = Pattern::new();
    let v0 = p.var(Var::Symbol(0));
    let v1 = p.var(Var::Symbol(1));
    p.add(op_pattern("addi", vec![v0, v1]));
    let roots: std::collections::HashSet<Id> =
        p.search(&g).iter().map(|m| g.find(m.root)).collect();
    assert_eq!(roots.len(), 2);
    assert!(roots.contains(&g.find(a32)) && roots.contains(&g.find(a64)));

    // Searching is read-only: classes remain distinct.
    assert_ne!(g.find(a32), g.find(a64));
}

#[test]
fn graph_operation_controls_commutative_matching() {
    let mut g: EGraph<SemNode> = EGraph::new();
    let zero = g.add(konst(32, 0));
    let x = g.add(konst(32, 1));
    let mut addi = op("addi", ty(32), vec![], vec![zero, x]);
    let Kind::Ir(ir) = &mut addi.kind else {
        unreachable!()
    };
    ir.commutative = true;
    let root = g.add(addi);

    let mut pattern: Pattern<SemNode, u32> = Pattern::new();
    let variable = pattern.var(Var::Symbol(0));
    let literal = pattern.var(Var::Int(APInt::new(32, 0)));
    pattern.add(op_pattern("addi", vec![variable, literal]));

    let matches = pattern.search(&g);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.find(matches[0].root), g.find(root));
    assert_eq!(matches[0].subst.get(&Var::Symbol(0)), Some(g.find(x)));
}

fn assert_same_graph(actual: &SemGraph, expected: &SemGraph, root: tir::graph::NodeId) {
    for node in expected.postorder(root) {
        assert_eq!(actual.get_node(node), expected.get_node(node));
        assert_eq!(actual.get_leaf_data(node), expected.get_leaf_data(node));
        assert_eq!(
            actual.children(node).collect::<Vec<_>>(),
            expected.children(node).collect::<Vec<_>>()
        );
    }
}

fn add_program() -> Vec<SemOp> {
    vec![
        SemOp::Node(SymKind::Symbol),
        SemOp::Payload(SemPayloadDesc::SymbolId(0)),
        SemOp::Node(SymKind::Constant),
        SemOp::Payload(SemPayloadDesc::Int {
            width: 32,
            value: 7,
            signed: false,
        }),
        SemOp::Node(SymKind::Add),
        SemOp::Edge(2, 0),
        SemOp::Edge(2, 1),
    ]
}

#[test]
fn extend_sem_bytes_builds_the_same_graph_as_imperative_construction() {
    let mut expected = SemGraph::new();
    let a = expected.add_node(SymKind::Symbol);
    expected.set_leaf_data(a, tir::sem::SymPayload::SymbolId(0));
    let b = expected.add_node(SymKind::Constant);
    expected.set_leaf_data(b, int_payload(32, 7, false));
    let add = expected.add_node(SymKind::Add);
    expected.add_edge(add, a);
    expected.add_edge(add, b);

    let mut builder = SemBlobBuilder::new();
    let offset = builder.intern(&add_program());
    let (blob, kinds) = builder.finish();

    let mut decoded = SemGraph::new();
    let root = decoded.extend_sem_bytes(&kinds, &blob, offset);

    assert_eq!(root, add);
    assert_same_graph(&decoded, &expected, root);
}

#[test]
fn every_op_round_trips_through_the_blob() {
    let ops = vec![
        SemOp::Node(SymKind::Constant),
        SemOp::Payload(SemPayloadDesc::Int {
            width: 12,
            value: (-3i64) as u64,
            signed: true,
        }),
        SemOp::Node(SymKind::Constant),
        SemOp::Payload(SemPayloadDesc::Float(-0.5)),
        SemOp::Node(SymKind::Symbol),
        SemOp::Payload(SemPayloadDesc::Value(9)),
        SemOp::Node(SymKind::FAdd),
        SemOp::Typed(64),
        SemOp::Edge(3, 1),
        SemOp::Edge(3, 2),
    ];

    let mut builder = SemBlobBuilder::new();
    let offset = builder.intern(&ops);
    let (blob, kinds) = builder.finish();

    assert_eq!(
        format!("{:?}", decode_sem_ops(&blob, offset, &kinds)),
        format!("{ops:?}")
    );
}

#[test]
fn identical_programs_share_one_record() {
    let mut builder = SemBlobBuilder::new();
    let first = builder.intern(&add_program());
    let second = builder.intern(&add_program());
    let third = builder.intern(&[SemOp::Node(SymKind::Symbol)]);

    assert_eq!(first, second);
    assert_ne!(first, third);
}
