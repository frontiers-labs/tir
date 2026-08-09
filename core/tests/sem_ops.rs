use tir::graph::{Dag, MutDag};
use tir::sem::{ExtendSemOps, SemGraph, SemOp, SemPayloadDesc, SymKind, int_payload};

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

#[test]
fn extend_sem_ops_builds_the_same_graph_as_imperative_construction() {
    let mut expected = SemGraph::new();
    let a = expected.add_node(SymKind::Symbol);
    expected.set_leaf_data(a, tir::sem::SymPayload::SymbolId(0));
    let b = expected.add_node(SymKind::Constant);
    expected.set_leaf_data(b, int_payload(32, 7, false));
    let add = expected.add_node(SymKind::Add);
    expected.add_edge(add, a);
    expected.add_edge(add, b);

    let mut decoded = SemGraph::new();
    let root = decoded.extend_sem_ops(&[
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
    ]);

    assert_eq!(root, add);
    assert_same_graph(&decoded, &expected, root);
}
