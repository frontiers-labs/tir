//! Candidate selection over one context: each candidate is built in a fork,
//! scored, and only the cheapest is kept.

use tir::builtin::{ops, IntegerType};
use tir::{variants, Context, Operation, PassError};

use super::fixtures;

fn function() -> (Context, tir::BlockHandle, usize) {
    let (context, _, func, _) = fixtures::parse_function(
        r#"module {
func.func @demo() -> !i32 {
  %0 = constant {value = 1} : !i32
  func.return %0
}
module_end
}"#,
    );
    let body = func.body();
    let ops_live = context.census().ops_live;
    (context, body, ops_live)
}

fn build(context: &Context, count: &usize, body: &tir::BlockHandle) -> Result<bool, PassError> {
    let i32_ty = IntegerType::new(context, 32);
    let body = context.get_block(body.id());
    for value in 0..*count {
        body.insert(0, ops::constant(context, value as i64, i32_ty).build().id());
    }
    Ok(true)
}

fn constants(context: &Context, body: &tir::BlockHandle) -> usize {
    context.get_block(body.id()).op_ids().len() - 2
}

#[test]
fn a_cheaper_later_candidate_wins_and_the_losers_leave_nothing() {
    let (context, body, ops_before) = function();
    let chosen = variants::choose(
        &context,
        &[1usize, 3, 2],
        |fork, count| build(fork, count, &body),
        |fork| -(constants(fork, &body) as i64),
    )
    .expect("every candidate builds");
    assert_eq!(chosen, Some(1));
    assert_eq!(constants(&context, &body), 3);
    assert_eq!(context.census().ops_live, ops_before + 3);
}

#[test]
fn a_tie_goes_to_the_lower_index() {
    let (context, body, _) = function();
    let chosen = variants::choose(
        &context,
        &[2usize, 2, 1],
        |fork, count| build(fork, count, &body),
        |fork| if constants(fork, &body) > 0 { 5 } else { 9 },
    )
    .expect("every candidate builds");
    assert_eq!(chosen, Some(0));
    assert_eq!(constants(&context, &body), 2);
}

#[test]
fn nothing_is_adopted_when_no_candidate_beats_the_overlay_as_it_stands() {
    let (context, body, ops_before) = function();
    let chosen = variants::choose(
        &context,
        &[1usize, 2],
        |fork, count| build(fork, count, &body),
        |fork| constants(fork, &body) as i64,
    )
    .expect("every candidate builds");
    assert_eq!(chosen, None);
    assert_eq!(constants(&context, &body), 0);
    assert_eq!(context.census().ops_live, ops_before);
}

#[test]
fn a_candidate_that_does_not_apply_is_skipped() {
    let (context, body, _) = function();
    let chosen = variants::choose(
        &context,
        &[0usize, 1],
        |fork, count| {
            if *count == 0 {
                return Ok(false);
            }
            build(fork, count, &body)
        },
        |fork| -(constants(fork, &body) as i64),
    )
    .expect("every candidate builds");
    assert_eq!(chosen, Some(1));
}
