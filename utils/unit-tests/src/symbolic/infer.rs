use std::collections::HashSet;

use tir_adt::{APInt, NodeId};
use tir_symbolic::lang::{
    canonicalize_for_selection, execute, infer_types, FloatFormat, SemType, StateAccessKind,
    StateFieldKind, StateResourceKind, SymKind, SymPayload, TypeUnifier, Value, Width,
};

use super::support::{con, op, sym, Graph};

fn binary(kind: SymKind) -> (Graph, NodeId, NodeId, NodeId) {
    let mut graph = Graph::new();
    let lhs = sym(&mut graph, 0);
    let rhs = sym(&mut graph, 1);
    let root = op(&mut graph, kind, &[lhs, rhs]);
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
    let input = sym(&mut graph, 0);
    let root = op(&mut graph, SymKind::Bitcast, &[input]);

    let types = infer_types(&graph, |node| {
        (node == input).then(|| SemType::Float(FloatFormat::new(8, 23)))
    })
    .unwrap();

    assert_eq!(types[root.index()], SemType::raw_bits(32));
}

#[test]
fn raw_memory_bits_admit_a_float_interpretation() {
    let mut graph = Graph::new();
    let address = sym(&mut graph, 0);
    let bytes = con(&mut graph, 8, 4);
    let metadata = con(&mut graph, 1, 0);
    let load = op(&mut graph, SymKind::LoadMemory, &[address, bytes, metadata]);

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
fn fp_state_field_reads_have_intrinsic_types() {
    let mut graph = Graph::new();
    let state = sym(&mut graph, 0);
    let resource = con(&mut graph, 2, StateResourceKind::FpEnvironment as u64);
    let rounding_field = con(&mut graph, 2, StateFieldKind::FpRounding as u64);
    let flags_field = con(&mut graph, 2, StateFieldKind::FpFlags as u64);
    let rounding = op(
        &mut graph,
        SymKind::StateRead,
        &[state, resource, rounding_field],
    );
    let flags = op(
        &mut graph,
        SymKind::StateRead,
        &[state, resource, flags_field],
    );

    let types = infer_types(&graph, |node| (node == state).then_some(SemType::State)).unwrap();

    assert_eq!(types[rounding.index()], SemType::bits(3));
    assert_eq!(types[flags.index()], SemType::bits(5));
}

#[test]
fn fp_state_assignment_rejects_the_wrong_field_type() {
    let mut graph = Graph::new();
    let state = sym(&mut graph, 0);
    let resource = con(&mut graph, 2, StateResourceKind::FpEnvironment as u64);
    let field = con(&mut graph, 2, StateFieldKind::FpRounding as u64);
    let access = con(&mut graph, 1, StateAccessKind::Change as u64);
    let flags = sym(&mut graph, 1);
    op(
        &mut graph,
        SymKind::StateAssign,
        &[state, resource, field, access, flags],
    );

    assert!(infer_types(&graph, |node| {
        if node == state {
            Some(SemType::State)
        } else if node == flags {
            Some(SemType::bits(5))
        } else {
            None
        }
    })
    .is_err());
}

#[test]
fn selection_drops_extension_of_narrow_division_result() {
    let mut graph = Graph::new();
    let lhs = sym(&mut graph, 0);
    let rhs = sym(&mut graph, 1);
    let hi = con(&mut graph, 32, 31);
    let lo = con(&mut graph, 32, 0);
    let lhs_word = op(&mut graph, SymKind::Extract, &[lhs, hi, lo]);
    let rhs_word = op(&mut graph, SymKind::Extract, &[rhs, hi, lo]);
    let div = op(&mut graph, SymKind::Div, &[lhs_word, rhs_word]);
    let width = con(&mut graph, 32, 64);
    let root = op(&mut graph, SymKind::SExt, &[div, width]);

    let (canonical, root, forced_widths) =
        canonicalize_for_selection(&graph, root, &HashSet::new());

    assert_eq!(*canonical.get_node(root), SymKind::Div);
    assert_eq!(forced_widths[root.index()], Some(32));
}

#[test]
fn selection_drops_addition_of_zero_extended_zero() {
    let mut graph = Graph::new();
    let zero = con(&mut graph, 1, 0);
    let width = sym(&mut graph, 0);
    let extended_zero = op(&mut graph, SymKind::ZExt, &[zero, width]);
    let value = sym(&mut graph, 1);
    let root = op(&mut graph, SymKind::Add, &[extended_zero, value]);

    let (canonical, root, _) = canonicalize_for_selection(&graph, root, &HashSet::new());

    assert_eq!(*canonical.get_node(root), SymKind::Symbol);
    assert_eq!(
        canonical.get_leaf_data(root),
        Some(&SymPayload::SymbolId(1))
    );
}

#[test]
fn selection_observes_wide_boolean_result_as_its_low_bit() {
    for width in [8, 32, 64] {
        for bit in [0, 1] {
            let mut graph = Graph::new();
            let input = sym(&mut graph, 0);
            let bit_one = con(&mut graph, 1, 1);
            let condition = op(&mut graph, SymKind::Eq, &[input, bit_one]);
            let one = con(&mut graph, width, 1);
            let zero = con(&mut graph, width, 0);
            let root = op(&mut graph, SymKind::If, &[condition, one, zero]);
            let input = Value::Int(APInt::new(1, bit));
            let Value::Int(original) = execute(&graph, std::slice::from_ref(&input)) else {
                panic!("conditional must return an integer");
            };

            let (mut canonical, root, _) =
                canonicalize_for_selection(&graph, root, &HashSet::new());
            let true_value = con(&mut canonical, 1, 1);
            op(&mut canonical, SymKind::If, &[true_value, root, root]);
            let Value::Int(observed) = execute(&canonical, &[input]) else {
                panic!("canonical observation must return an integer");
            };

            assert_eq!(observed.width(), 1);
            assert_eq!(observed.to_u64(), original.to_u64() & 1);
        }
    }
}

// riscv `remw` = sext(x_w32 - (x_w32 / y_w32) * y_w32, 64): the extension wraps a
// compound Euclidean remainder, not a bare division, so the collapse must fire on
// the inferred width of the whole sub-tree and type the interior `Div` as narrow.
#[test]
fn selection_types_the_interior_division_of_a_narrow_remainder() {
    let mut graph = Graph::new();
    let lhs = sym(&mut graph, 0);
    let rhs = sym(&mut graph, 1);
    let hi = con(&mut graph, 32, 31);
    let lo = con(&mut graph, 32, 0);
    let word = |graph: &mut Graph, src| op(graph, SymKind::Extract, &[src, hi, lo]);
    let div_lhs = word(&mut graph, lhs);
    let div_rhs = word(&mut graph, rhs);
    let div = op(&mut graph, SymKind::Div, &[div_lhs, div_rhs]);
    let mul_rhs = word(&mut graph, rhs);
    let mul = op(&mut graph, SymKind::Mul, &[div, mul_rhs]);
    let sub_lhs = word(&mut graph, lhs);
    let sub = op(&mut graph, SymKind::Sub, &[sub_lhs, mul]);
    let width = con(&mut graph, 32, 64);
    let root = op(&mut graph, SymKind::SExt, &[sub, width]);

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
