use tir_adt::{APFloat, APInt};
use tir_graph::{GenericDag, MutDag};
use tir_symbolic::lang::{execute, SymKind, SymPayload, Value};

#[test]
fn rounded_add_and_flags() {
    for (rounding, expected) in [(0, 0x3f800000), (3, 0x3f800001)] {
        let mut graph = GenericDag::<SymKind, SymPayload<()>>::new();
        let a = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(a, SymPayload::SymbolId(0));
        let b = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(b, SymPayload::SymbolId(1));
        let rm = graph.add_node(SymKind::Constant);
        graph.set_leaf_data(rm, SymPayload::Int(APInt::new(3, rounding)));
        let add = graph.add_node(SymKind::FAddRound);
        for child in [a, b, rm] {
            graph.add_edge(add, child);
        }
        let values = [
            Value::Float(APFloat::from_bits(8, 23, false, 0x3f800000)),
            Value::Float(APFloat::from_bits(8, 23, false, 0x33800000)),
        ];
        let Value::Float(result) = execute(&graph, &values) else {
            panic!("expected float")
        };
        assert_eq!(result.to_bits(), expected);
        let flags = graph.add_node(SymKind::FPFlags);
        graph.add_edge(flags, add);
        assert_eq!(execute(&graph, &values), Value::Int(APInt::new(5, 1)));
    }
}

#[test]
fn selection_canonicalizes_matching_rounding_modes() {
    use std::collections::HashSet;
    use tir_graph::Dag;
    use tir_symbolic::lang::canonicalize_for_selection;

    for (rounded, generic, default_rounding) in [
        (SymKind::SIToFPRound, SymKind::SIToFP, 0),
        (SymKind::UIToFPRound, SymKind::UIToFP, 0),
    ] {
        for rounding in [0, 1] {
            let mut graph = GenericDag::<SymKind, SymPayload<()>>::new();
            let operands: Vec<_> = (0..generic.arity())
                .map(|i| {
                    let leaf = graph.add_node(SymKind::Symbol);
                    graph.set_leaf_data(leaf, SymPayload::SymbolId(i as u32));
                    leaf
                })
                .collect();
            let rm = graph.add_node(SymKind::Constant);
            graph.set_leaf_data(rm, SymPayload::Int(APInt::new(3, rounding)));
            let value = graph.add_node(rounded);
            for operand in operands.into_iter().chain([rm]) {
                graph.add_edge(value, operand);
            }
            let (canonical, root, _) = canonicalize_for_selection(&graph, value, &HashSet::new());
            assert_eq!(
                *canonical.get_kind(root),
                if rounding == default_rounding {
                    generic
                } else {
                    rounded
                }
            );
            assert_eq!(
                canonical.children(root).count(),
                if rounding == default_rounding {
                    generic.arity()
                } else {
                    rounded.arity()
                }
            );
            let flags = graph.add_node(SymKind::FPFlags);
            graph.add_edge(flags, value);
            let (canonical, root, _) = canonicalize_for_selection(&graph, flags, &HashSet::new());
            let operation = canonical.children(root).next().unwrap();
            assert_eq!(*canonical.get_kind(operation), rounded);
        }
    }
}

#[test]
fn integer_to_float_avoids_double_rounding() {
    let mut graph = GenericDag::<SymKind, SymPayload<()>>::new();
    let input = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(
        input,
        SymPayload::Int(APInt::new(64, 0x8000_0080_0000_0001)),
    );
    let exponent = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(exponent, SymPayload::Int(APInt::new(32, 8)));
    let mantissa = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(mantissa, SymPayload::Int(APInt::new(32, 23)));
    let conversion = graph.add_node(SymKind::UIToFP);
    for child in [input, exponent, mantissa] {
        graph.add_edge(conversion, child);
    }
    let Value::Float(result) = execute(&graph, &[]) else {
        panic!("expected float")
    };
    assert_eq!(result.to_bits(), 0x5f000001);
}

#[test]
fn rounded_rtz_conversion_refines_partial_conversion() {
    use tir_symbolic::sem::{EquivalenceOracle, SemGraph, SmtOracle};
    let graph = |kind: SymKind| {
        let mut graph = SemGraph::<()>::new();
        let input = graph.add_node(SymKind::Symbol);
        graph.set_leaf_data(input, SymPayload::SymbolId(0));
        let width = graph.add_node(SymKind::Constant);
        graph.set_leaf_data(width, SymPayload::Int(APInt::new(32, 32)));
        let rm = graph.add_node(SymKind::Constant);
        graph.set_leaf_data(rm, SymPayload::Int(APInt::new(3, 1)));
        let result = graph.add_node(kind);
        graph.add_edge(result, input);
        graph.add_edge(result, width);
        if kind.arity() == 3 {
            graph.add_edge(result, rm);
        }
        graph
    };
    for (generic, rounded) in [
        (SymKind::FPToSI, SymKind::FPToSIRound),
        (SymKind::FPToUI, SymKind::FPToUIRound),
    ] {
        assert!(SmtOracle.refines(&graph(generic), &graph(rounded), &[64]));
        assert!(SmtOracle.refines_typed(
            &graph(generic),
            &graph(rounded),
            &[tir_symbolic::lang::SemType::Float(
                tir_symbolic::lang::FloatFormat::new(11, 52)
            )]
        ));
    }
}

#[test]
fn selection_fallback_preserves_conversion_width_wrapper() {
    use tir_graph::Dag;
    use tir_symbolic::lang::selection_fallback;
    use tir_symbolic::sem::SemGraph;
    let mut graph = SemGraph::<()>::new();
    let input = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(input, SymPayload::SymbolId(0));
    let width = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(width, SymPayload::Int(APInt::new(32, 32)));
    let rm = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(rm, SymPayload::Int(APInt::new(3, 1)));
    let value = graph.add_node(SymKind::FPToSIRound);
    for child in [input, width, rm] {
        graph.add_edge(value, child);
    }
    let guard = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(guard, SymPayload::SymbolId(1));
    let select = graph.add_node(SymKind::If);
    for child in [guard, width, value] {
        graph.add_edge(select, child);
    }
    let extend = graph.add_node(SymKind::SExt);
    graph.add_edge(extend, select);
    graph.add_edge(extend, width);
    let fallback = selection_fallback(&graph, extend).unwrap();
    let root = fallback.root().unwrap();
    assert_eq!(*fallback.get_kind(root), SymKind::SExt);
    assert_eq!(
        *fallback.get_kind(fallback.children(root).next().unwrap()),
        SymKind::FPToSI
    );
}

#[test]
fn selection_fallback_proposes_arithmetic_inside_nan_wrapper() {
    use tir_graph::Dag;
    use tir_symbolic::lang::selection_fallback;
    use tir_symbolic::sem::SemGraph;
    let mut graph = SemGraph::<()>::new();
    let input = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(input, SymPayload::SymbolId(0));
    let rm = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(rm, SymPayload::Int(APInt::new(3, 0)));
    let add = graph.add_node(SymKind::FAddRound);
    for child in [input, input, rm] {
        graph.add_edge(add, child);
    }
    let ordered = graph.add_node(SymKind::Ge);
    graph.add_edge(ordered, add);
    graph.add_edge(ordered, add);
    let nan_bits = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(
        nan_bits,
        SymPayload::Int(APInt::new(64, 0x7ff8000000000000)),
    );
    let nan = graph.add_node(SymKind::AsFloat);
    graph.add_edge(nan, nan_bits);
    let full = graph.add_node(SymKind::If);
    for child in [ordered, add, nan] {
        graph.add_edge(full, child);
    }
    let candidate = selection_fallback(&graph, full).unwrap();
    assert_eq!(
        *candidate.get_kind(candidate.root().unwrap()),
        SymKind::FAdd
    );
}

#[test]
fn typed_refinement_observes_float_result_bits() {
    use tir_symbolic::sem::{SemGraph, SmtOracle};
    let graph = |bits| {
        let mut graph = SemGraph::<()>::new();
        let bits_node = graph.add_node(SymKind::Constant);
        graph.set_leaf_data(bits_node, SymPayload::Int(APInt::new(64, bits)));
        let value = graph.add_node(SymKind::AsFloat);
        graph.add_edge(value, bits_node);
        graph
    };
    assert!(!SmtOracle.refines_typed(&graph(0), &graph(0x8000000000000000), &[]));
    assert!(SmtOracle.refines_typed(&graph(0x7ff8123456789abc), &graph(0x7ff8123456789abc), &[]));
}

#[test]
fn float_bit_extraction_preserves_nan_payload() {
    let mut graph = GenericDag::<SymKind, SymPayload<()>>::new();
    let value = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(value, SymPayload::SymbolId(0));
    let high = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(high, SymPayload::Int(APInt::new(32, 31)));
    let low = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(low, SymPayload::Int(APInt::new(32, 0)));
    let extract = graph.add_node(SymKind::Extract);
    for child in [value, high, low] {
        graph.add_edge(extract, child);
    }
    let types = tir_symbolic::lang::infer_types(&graph, |node| {
        (node == value).then_some(tir_symbolic::lang::SemType::Float(
            tir_symbolic::lang::FloatFormat::new(8, 23),
        ))
    })
    .unwrap();
    assert_eq!(
        types[extract.index()],
        tir_symbolic::lang::SemType::bits(32)
    );
    assert_eq!(
        execute(
            &graph,
            &[Value::Float(APFloat::from_bits(8, 23, false, 0xffa01234))]
        ),
        Value::Int(APInt::new(32, 0xffa01234)),
    );
}

#[test]
fn selection_fallback_preserves_flag_projection() {
    use tir_symbolic::lang::selection_fallback;
    use tir_symbolic::sem::SemGraph;
    let mut graph = SemGraph::<()>::new();
    let input = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(input, SymPayload::SymbolId(0));
    let rm = graph.add_node(SymKind::Constant);
    graph.set_leaf_data(rm, SymPayload::Int(APInt::new(3, 0)));
    let add = graph.add_node(SymKind::FAddRound);
    for child in [input, input, rm] {
        graph.add_edge(add, child);
    }
    let flags = graph.add_node(SymKind::FPFlags);
    graph.add_edge(flags, add);
    assert!(selection_fallback(&graph, flags).is_none());
}

#[test]
fn selection_fallback_preserves_existing_unrounded_guards() {
    use tir_symbolic::lang::selection_fallback;
    use tir_symbolic::sem::SemGraph;
    let mut graph = SemGraph::<()>::new();
    let input = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(input, SymPayload::SymbolId(0));
    let guard = graph.add_node(SymKind::Symbol);
    graph.set_leaf_data(guard, SymPayload::SymbolId(1));
    let product = graph.add_node(SymKind::FMul);
    graph.add_edge(product, input);
    graph.add_edge(product, input);
    let value = graph.add_node(SymKind::If);
    for child in [guard, input, product] {
        graph.add_edge(value, child);
    }
    assert!(selection_fallback(&graph, value).is_none());
}
