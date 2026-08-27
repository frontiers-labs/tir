//! The affine view's arithmetic: forms, the wrap check, and the distance test.

use tir::analysis::affine::{distances, AffineForm, AffineView, Component, Sign};
use tir::{builtin, Context, Operation};

#[test]
fn forms_add_terms_of_the_same_variable() {
    let form = AffineForm::counter(0)
        .scale(4)
        .add(&AffineForm::counter(0))
        .sub(&AffineForm::constant(3));
    assert_eq!(form.counter_coefficient(0), 5);
    assert_eq!(form.constant_term(), -3);
    assert!(!form.is_constant());
    assert!(!form.is_uniform());
}

#[test]
fn forms_over_symbols_alone_are_uniform() {
    let form = AffineForm::symbol(1).scale(2).add(&AffineForm::constant(7));
    assert!(form.is_uniform());
    assert!(!form.is_constant());
    assert_eq!(form.symbol_coefficient(1), 2);
    assert_eq!(AffineForm::constant(7).as_constant(), Some(7));
}

#[test]
fn a_form_ranges_over_what_its_variables_do() {
    // `4·d0 - d1 + 1` with `d0 ∈ [0, 3]` and `d1 ∈ [-2, 2]`.
    let form = AffineForm::counter(0)
        .scale(4)
        .sub(&AffineForm::counter(1))
        .add(&AffineForm::constant(1));
    assert_eq!(form.range(&[(0, 3), (-2, 2)], &[]), (-1, 15));
}

/// Every value an `i8` form can take, against what the width actually holds.
#[test]
fn the_wrap_check_is_exact_at_width_eight() {
    for low in -200i128..200 {
        for high in low..200 {
            let inside = (-128..=127).contains(&low) && (-128..=127).contains(&high);
            assert_eq!(AffineForm::fits(8, low, high), inside, "[{low}, {high}]");
        }
    }
}

#[test]
fn the_gcd_test_refuses_what_no_multiple_reaches() {
    // `2·δ = 1` has no integer solution, whatever the space.
    assert!(distances(&[2], &[Some(7)], &[(1, 1)]).is_none());
    // `2·δ = 4` has one, and the box pins it.
    assert_eq!(
        distances(&[2], &[Some(7)], &[(4, 4)]),
        Some(vec![Component::Distance(2)])
    );
}

/// `8·δ ∈ [37, 43]` holds for `δ = 5` alone: the ends round inward, whichever
/// way the coefficient points.
#[test]
fn the_distance_interval_rounds_inward() {
    assert_eq!(
        distances(&[8], &[Some(7)], &[(37, 43), (-43, -37)]),
        Some(vec![Component::Distance(5)])
    );
    assert_eq!(
        distances(&[-4], &[Some(7)], &[(-7, -1), (1, 7)]),
        Some(vec![Component::Distance(1)])
    );
}

#[test]
fn a_product_past_the_form_is_refused() {
    let wide = AffineForm::counter(0).scale(i128::MAX / 2);
    assert!(wide.checked_scale(4).is_none());
    assert!(!AffineForm::fits(
        64,
        wide.range(&[(0, 1)], &[]).0,
        wide.scale(4).range(&[(0, 1)], &[]).1
    ));
}

#[test]
fn the_box_refuses_what_the_space_cannot_reach() {
    // `4·δ = 40` needs `δ = 10`, and the loop runs eight iterations.
    assert!(distances(&[4], &[Some(7)], &[(40, 40)]).is_none());
    // Without a trip count there is no box, so the same equation is admitted.
    assert_eq!(
        distances(&[4], &[None], &[(40, 40)]),
        Some(vec![Component::Distance(10)])
    );
}

#[test]
fn a_free_depth_keeps_only_its_direction() {
    // `C[i][j]` against itself in a three-deep nest: `32·δi + 4·δj = 0` pins the
    // first two, and `k` is not in the equation at all.
    let components = distances(&[32, 4, 0], &[Some(7), Some(7), Some(7)], &[(0, 0)])
        .expect("the pair depends on itself");
    assert_eq!(
        components,
        vec![
            Component::Distance(0),
            Component::Distance(0),
            Component::Direction(Sign::Positive)
        ]
    );
}

#[test]
fn extents_widen_the_target_to_the_bytes_an_access_covers() {
    // Four-byte accesses one byte apart still meet, and the distance is zero.
    assert_eq!(
        distances(&[4], &[Some(7)], &[(-3, 3)]),
        None,
        "no positive distance reaches a target the zero distance covers"
    );
    // `a[i]` against `a[i-1]`: `4·δ ∈ [1, 7]` admits one distance.
    assert_eq!(
        distances(&[4], &[Some(7)], &[(1, 7)]),
        Some(vec![Component::Distance(1)])
    );
}

/// §7.1: a red view is read off the IR and allocates nothing into it.
#[test]
fn building_a_view_allocates_nothing() {
    let context = Context::with_default_dialects();
    let module: builtin::ModuleOp = tir::parse::ir::parse_ir(
        &context,
        r#"module {data_layout = {types = {i32 = {abi = 32, size = 32}, i64 = {abi = 64, size = 64}, p = {abi = 64, size = 64}}}} {
  %0 = func.func @f() {
    %1 = ptr.alloca {size = 1024, align = 4} : !ptr.p
    %2 = constant {value = 0} : !i32
    %3 = constant {value = 64} : !i32
    %4 = constant {value = 1} : !i32
    %5 = constant {value = 4} : !i64
    %6 = scf.for %2, %3, %4 iter_args(%7 = %2) -> !i32 {
      %8 = extsi %7 : !i64
      %9 = muli %8, %5 : !i64
      %10 = ptr.ptradd %1, %9 : !ptr.p
      ptr.store %4, %10
      %11 = addi %7, %4 : !i32
      scf.yield %11
    }
    func.return
  }
  module_end
}"#,
    )
    .expect("the fixture parses");

    let before = context.slab_census();
    let views = tir::analysis::affine::nests_under(&context, module.id());
    let after = context.slab_census();

    assert_eq!(views.len(), 1);
    assert_eq!(views[0].depth(), 1);
    assert_eq!(before.ops_live, after.ops_live);
    assert_eq!(before.values_live, after.values_live);
    assert_eq!(before.blocks_live, after.blocks_live);
    assert_eq!(before.regions_live, after.regions_live);
}

#[test]
fn a_loop_that_does_not_count_has_no_view() {
    let context = Context::with_default_dialects();
    let module: builtin::ModuleOp = tir::parse::ir::parse_ir(
        &context,
        r#"module {
  %0 = func.func @f(%1: !i1) {
    scf.while {
      scf.condition %1
    } do {
      scf.yield
    }
    func.return
  }
  module_end
}"#,
    )
    .expect("the fixture parses");
    assert!(tir::analysis::affine::nests_under(&context, module.id()).is_empty());
    assert!(AffineView::build(&context, module.id()).is_none());
}
