use tir_cdsl::diagnostic::{Diagnostic, Phase};
use tir_cdsl::types::Type;
use tir_cdsl::{CheckedModule, SourceMap, Value, compile};

fn checked(source: &str) -> CheckedModule {
    compile(&SourceMap::new("frontend.cdsl", source))
        .unwrap_or_else(|errors| panic!("source failed checking: {errors:?}"))
}

fn rejected(source: &str) -> Vec<Diagnostic> {
    match compile(&SourceMap::new("frontend.cdsl", source)) {
        Err(errors) => errors,
        Ok(_) => panic!("source unexpectedly passed checking"),
    }
}

#[test]
fn lexical_errors_preserve_byte_spans_after_unicode() {
    for token in ["@", "1.5", "1e+2", "1.", "'x'", "'name", "0b102", r#""\q""#] {
        let source = format!("let λ = 0;\nlet invalid = {token};");
        let start = source.find(token).expect("fixture includes rejected token");
        let errors = rejected(&source);
        assert!(
            matches!(errors.as_slice(), [error]
                if error.phase == Phase::Lex
                    && error.source == "frontend.cdsl"
                    && error.span == (start..start + token.len())),
            "{token}: {errors:?}",
        );
    }
    let source = "let λ = 0;\n/* outer /* nested */";
    let start = source.find("/*").expect("fixture includes comment");
    let errors = rejected(source);
    assert!(
        matches!(errors.as_slice(), [error]
            if error.phase == Phase::Lex && error.span == (start..source.len())),
        "{errors:?}",
    );
}

#[test]
fn unicode_raw_tokens_and_nested_comments_evaluate() {
    let module = checked(
        r####"
/* outer /* nested */ comment */
let r#λ = 0x2_a + 0b1010 + 0o10 + 1_0;
let r#type = 1;
pub let main = (r#λ + r#type, r###"a "## λ\n"###, "\u{3bb}\x21");
"####,
    );
    let value = module.evaluate("main").expect("checked value evaluates");
    assert!(
        matches!(value, Value::Tuple(ref fields)
            if matches!(fields.as_slice(), [Value::Integer(71), Value::String(raw), Value::String(escaped)]
                if raw == "a \"## λ\\n" && escaped == "λ!")),
        "{value:?}",
    );
}

#[test]
fn generic_suffix_wins_and_comparisons_remain_nonassociative() {
    let module = checked(
        "
let a<T> = fn(x: T) -> T { x };
type b = Integer;
let c = 42;
pub let main = (a<b>(c), (1 < 2) == true);
",
    );
    let value = module.evaluate("main").expect("checked value evaluates");
    assert!(
        matches!(value, Value::Tuple(ref fields)
            if matches!(fields.as_slice(), [Value::Integer(42), Value::Bool(true)])),
        "{value:?}",
    );
    let errors = rejected("let a = 1; let b = 2; let c = 3; pub let main = a<b>c;");
    assert!(
        !errors.is_empty() && errors.iter().all(|error| error.phase == Phase::Parse),
        "{errors:?}"
    );
}

#[test]
fn nested_generic_closers_do_not_consume_shift_operators() {
    let module = checked(
        "
let id<T> = fn(x: T) -> T { x };
pub let main: List<List<Integer>> = id<List<List<Integer>>>([[8 >> 1, 1 << 3]]);
",
    );
    let value = module.evaluate("main").expect("checked value evaluates");
    assert!(
        matches!(value, Value::List(ref rows)
            if matches!(rows.as_slice(), [Value::List(values)]
                if matches!(values.as_slice(), [Value::Integer(4), Value::Integer(8)]))),
        "{value:?}",
    );
}

#[test]
fn description_results_preserve_grouping_and_singleton_product_labels() {
    let module = checked(
        "
pub let grouped = () -> (Integer);
pub let labelled = () -> (only: Integer);
pub let trailing = () -> (Integer,);
pub let value_group = (7);
pub let value_tuple = (7,);
",
    );
    for (binding, result) in [
        ("grouped", Type::Integer),
        (
            "labelled",
            Type::Tuple(vec![(Some("only".into()), Type::Integer)]),
        ),
        ("trailing", Type::Tuple(vec![(None, Type::Integer)])),
    ] {
        match module
            .evaluate(binding)
            .expect("checked description evaluates")
        {
            Value::Description(ty) => assert_eq!(ty, Type::Function(Vec::new(), Box::new(result))),
            value => panic!("{binding} is not a description: {value:?}"),
        }
    }
    assert!(matches!(
        module.evaluate("value_group"),
        Ok(Value::Integer(7))
    ));
    let tuple = module
        .evaluate("value_tuple")
        .expect("checked tuple evaluates");
    assert!(
        matches!(tuple, Value::Tuple(ref fields) if matches!(fields.as_slice(), [Value::Integer(7)]))
    );
}

#[test]
fn labelled_calls_reorder_values_and_reject_later_positional_arguments() {
    let module = checked(
        "
let pair = fn(x: Integer, y: Integer) -> Integer { x * 10 + y };
pub let main = pair(y: 2, x: 4);
",
    );
    assert!(matches!(module.evaluate("main"), Ok(Value::Integer(42))));
    let source = "pub let main = fn(x: Integer, y: Integer) { x }(x: 1, 2);";
    let start = source.rfind('2').expect("fixture has positional argument");
    let errors = rejected(source);
    assert!(
        errors
            .iter()
            .any(|error| error.phase == Phase::Parse && error.span == (start..start + 1)),
        "{errors:?}",
    );
}

#[test]
fn compact_generic_bindings_split_a_close_angle_from_equal() {
    let module =
        checked("let id<T>=fn(x:T)->T{x};pub let main=(id<Integer>(42), 5 >= 3, 3 >= 5, 3 >= 3);");
    let value = module.evaluate("main").expect("checked value evaluates");
    assert!(
        matches!(value, Value::Tuple(ref fields)
            if matches!(fields.as_slice(), [Value::Integer(42), Value::Bool(true), Value::Bool(false), Value::Bool(true)])),
        "{value:?}",
    );
}
