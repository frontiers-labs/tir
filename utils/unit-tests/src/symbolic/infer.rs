use std::collections::HashSet;

use tir_adt::APInt;
use tir_graph::{Dag, GenericDag, MutDag, NodeId};
use tir_symbolic::lang::{
    canonicalize_for_selection, infer_types, FloatFormat, SemType, SymKind, SymPayload,
    TypeUnifier, Width,
};

type Graph = GenericDag<SymKind, SymPayload<()>>;

fn binary(kind: SymKind) -> (Graph, NodeId, NodeId, NodeId) {
    let mut graph = Graph::new();
    let lhs = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(lhs, SymPayload::SymbolId(0));
    let rhs = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(rhs, SymPayload::SymbolId(1));
    let root = graph.add_node(kind);
    graph.add_edge(root, lhs);
    graph.add_edge(root, rhs);
    (graph, lhs, rhs, root)
}

#[test]
fn integer_binary_operation_is_width_polymorphic() {
    let (graph, lhs, rhs, root) = binary(SymKind::Add);
    let types = infer_types(&graph, |node| {
        (node == lhs || node == rhs).then(|| SemType::bits(32))
    })
    .unwrap();

    assert_eq!(types[root.index()], SemType::bits(32));
}

#[test]
fn integer_binary_operation_rejects_mixed_widths() {
    let (graph, lhs, rhs, _) = binary(SymKind::Add);
    let error = infer_types(&graph, |node| {
        if node == lhs {
            Some(SemType::bits(32))
        } else if node == rhs {
            Some(SemType::bits(64))
        } else {
            None
        }
    })
    .unwrap_err();

    assert!(error.to_string().contains("width mismatch"));
}

#[test]
fn float_operation_preserves_the_operand_format() {
    let (graph, lhs, rhs, root) = binary(SymKind::FAdd);
    let f32 = SemType::Float(FloatFormat::new(8, 23));
    let types = infer_types(&graph, |node| {
        (node == lhs || node == rhs).then(|| f32.clone())
    })
    .unwrap();

    assert_eq!(types[root.index()], f32);
}

#[test]
fn bitcast_accepts_a_float_and_preserves_its_bit_width() {
    let mut graph = Graph::new();
    let input = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(input, SymPayload::SymbolId(0));
    let root = graph.add_node(SymKind::Bitcast);
    graph.add_edge(root, input);

    let types = infer_types(&graph, |node| {
        (node == input).then(|| SemType::Float(FloatFormat::new(8, 23)))
    })
    .unwrap();

    assert_eq!(types[root.index()], SemType::raw_bits(32));
}

#[test]
fn raw_memory_bits_admit_a_float_interpretation() {
    let mut graph = Graph::new();
    let address = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(address, SymPayload::SymbolId(0));
    let bytes = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(bytes, SymPayload::Int(APInt::new(8, 4)));
    let metadata = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(metadata, SymPayload::Int(APInt::new(1, 0)));
    let load = graph.add_node(SymKind::LoadMemory);
    graph.add_edge(load, address);
    graph.add_edge(load, bytes);
    graph.add_edge(load, metadata);

    let types = infer_types(&graph, |_| None).unwrap();
    assert_eq!(types[load.index()], SemType::RawBits(Width::Const(32)));

    let mut unifier = TypeUnifier::default();
    unifier
        .unify(
            &types[load.index()],
            &SemType::Float(FloatFormat::new(8, 23)),
        )
        .unwrap();
}

#[test]
fn selection_drops_extension_of_narrow_division_result() {
    let mut graph = Graph::new();
    let lhs = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(lhs, SymPayload::SymbolId(0));
    let rhs = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(rhs, SymPayload::SymbolId(1));
    let hi = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(hi, SymPayload::Int(APInt::new(32, 31)));
    let lo = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(lo, SymPayload::Int(APInt::new(32, 0)));
    let lhs_word = graph.add_node(SymKind::Extract);
    graph.add_edge(lhs_word, lhs);
    graph.add_edge(lhs_word, hi);
    graph.add_edge(lhs_word, lo);
    let rhs_word = graph.add_node(SymKind::Extract);
    graph.add_edge(rhs_word, rhs);
    graph.add_edge(rhs_word, hi);
    graph.add_edge(rhs_word, lo);
    let div = graph.add_node(SymKind::Div);
    graph.add_edge(div, lhs_word);
    graph.add_edge(div, rhs_word);
    let width = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(width, SymPayload::Int(APInt::new(32, 64)));
    let root = graph.add_node(SymKind::SExt);
    graph.add_edge(root, div);
    graph.add_edge(root, width);

    let (canonical, root, forced_widths) =
        canonicalize_for_selection(&graph, root, &HashSet::new());

    assert_eq!(*canonical.get_node(root), SymKind::Div);
    assert_eq!(forced_widths[root.index()], Some(32));
}

#[test]
fn selection_drops_addition_of_zero_extended_zero() {
    let mut graph = Graph::new();
    let zero = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(zero, SymPayload::Int(APInt::new(1, 0)));
    let width = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(width, SymPayload::SymbolId(0));
    let extended_zero = graph.add_node(SymKind::ZExt);
    graph.add_edge(extended_zero, zero);
    graph.add_edge(extended_zero, width);
    let value = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(value, SymPayload::SymbolId(1));
    let root = graph.add_node(SymKind::Add);
    graph.add_edge(root, extended_zero);
    graph.add_edge(root, value);

    let (canonical, root, _) = canonicalize_for_selection(&graph, root, &HashSet::new());

    assert_eq!(*canonical.get_node(root), SymKind::Symbol);
    assert_eq!(
        canonical.get_leaf_data(root),
        Some(&SymPayload::SymbolId(1))
    );
}

// riscv `remw` = sext(x_w32 - (x_w32 / y_w32) * y_w32, 64): the extension wraps a
// compound Euclidean remainder, not a bare division, so the collapse must fire on
// the inferred width of the whole sub-tree and type the interior `Div` as narrow.
#[test]
fn selection_types_the_interior_division_of_a_narrow_remainder() {
    let mut graph = Graph::new();
    let lhs = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(lhs, SymPayload::SymbolId(0));
    let rhs = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(rhs, SymPayload::SymbolId(1));
    let hi = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(hi, SymPayload::Int(APInt::new(32, 31)));
    let lo = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(lo, SymPayload::Int(APInt::new(32, 0)));
    let word = |graph: &mut Graph, src| {
        let e = graph.add_node(SymKind::Extract);
        graph.add_edge(e, src);
        graph.add_edge(e, hi);
        graph.add_edge(e, lo);
        e
    };
    let div_lhs = word(&mut graph, lhs);
    let div_rhs = word(&mut graph, rhs);
    let div = graph.add_node(SymKind::Div);
    graph.add_edge(div, div_lhs);
    graph.add_edge(div, div_rhs);
    let mul_rhs = word(&mut graph, rhs);
    let mul = graph.add_node(SymKind::Mul);
    graph.add_edge(mul, div);
    graph.add_edge(mul, mul_rhs);
    let sub_lhs = word(&mut graph, lhs);
    let sub = graph.add_node(SymKind::Sub);
    graph.add_edge(sub, sub_lhs);
    graph.add_edge(sub, mul);
    let width = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(width, SymPayload::Int(APInt::new(32, 64)));
    let root = graph.add_node(SymKind::SExt);
    graph.add_edge(root, sub);
    graph.add_edge(root, width);

    let (canonical, root, forced_widths) =
        canonicalize_for_selection(&graph, root, &HashSet::new());

    assert_eq!(*canonical.get_node(root), SymKind::Sub);
    assert_eq!(forced_widths[root.index()], Some(32));
    let interior_division = (0..canonical.len())
        .map(NodeId::from_index)
        .find(|&node| *canonical.get_node(node) == SymKind::Div)
        .expect("the remainder keeps an interior division");
    assert_eq!(forced_widths[interior_division.index()], Some(32));
}
