use tir_cdsl::diagnostic::Phase;
use tir_cdsl::{EvaluationLimits, SourceMap, compile};

fn evaluate(source: &str) -> String {
    compile(&SourceMap::new("test.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"))
        .evaluate("main")
        .unwrap_or_else(|error| panic!("{error}"))
        .to_string()
}

#[test]
fn higher_order_generics_rows_and_lexical_closures_evaluate() {
    let cases = [
        (
            "let twice<T> = fn(f: (x:T)->T, x:T)->T {f(f(x))}; let increment=fn(x:Integer)->Integer{x+1}; let main=twice<Integer>(increment,40);",
            "42",
        ),
        (
            "let value<T>=fn(record)->T{record.value}; let alias<T>=value<T>; let main=alias<Integer>({other:false,value:42});",
            "42",
        ),
        (
            "let make=fn(x:Integer){fn(y:Integer){x+y}}; let main={let x=40;let add=make(x);let x=0;add(2)};",
            "42",
        ),
        (
            "let identity=fn<T>(x:T)->T{x}; let main=identity<Integer>(42);",
            "42",
        ),
        (
            "let schema={value:Integer}; type Item=Record<schema>; let main:Item={value:42};",
            "{value: 42}",
        ),
        (
            "type Access<O,T>=Record<{value:(self:O)->T}>; let get<T>:Access<Record<{value:T}>,T>={value:fn(self)->T{self.value}}; let main=get<Integer>.value({value:42});",
            "42",
        ),
        (
            "let describe<T>=fn(x:T){T}; let main=describe(42);",
            "type(Integer)",
        ),
        (
            "let f:(left:Integer,right:Integer)->Integer=fn(x:Integer,y:Integer){x-y}; let main=f(right:2,left:7);",
            "5",
        ),
        (
            "enum Width<N:Nat>{Token}; let main=Width::Token<{1+1}>();",
            "Width::Token()",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(evaluate(source), expected, "{source}");
    }
}

#[test]
fn structurally_recursive_inductive_computation_evaluates() {
    let source = "enum Chain<T>{Nil,Cons(T,Chain<T>)};let rec length<T>=fn(xs:Chain<T>)->Integer{match xs{Chain::Nil()=>0,Chain::Cons(head,tail)=>1+length<T>(tail)}};let main=length<Integer>(Chain::Cons(5,Chain::Cons(9,Chain::Cons(4,Chain::Nil()))));";
    assert_eq!(evaluate(source), "3");
}

#[test]
fn branching_evaluation_and_static_normalization_have_work_budgets() {
    let declarations = "enum Counter{Z,S(Counter)};let rec count=fn(n:Counter)->Integer{match n{Counter::Z()=>0,Counter::S(t)=>count(t)+count(t)}};";
    let small = compile(&SourceMap::new(
        "small.cdsl",
        format!("{declarations}let main=count(Counter::S(Counter::Z()));"),
    ))
    .unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(small.evaluate("main").unwrap().to_string(), "0");

    let mut argument = "Counter::Z()".to_owned();
    for _ in 0..20 {
        argument = format!("Counter::S({argument})");
    }
    let source = format!("{declarations}let main=count({argument});");
    let checked = compile(&SourceMap::new("branching.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    for limits in [
        EvaluationLimits::default(),
        EvaluationLimits {
            max_depth: 512,
            max_steps: 2_000,
        },
    ] {
        let error = checked.evaluate_with_limits("main", limits).unwrap_err();
        assert_eq!(error.phase, Phase::Evaluate);
        assert!(error.message.contains("step budget"), "{error}");
    }

    let source = format!(
        "{declarations}enum Marker<N:Integer>{{Token}};let main=Marker::Token<{{count({argument})}}>();"
    );
    let errors = match compile(&SourceMap::new("normalization.cdsl", source)) {
        Ok(_) => panic!("accepted normalization beyond its work budget"),
        Err(errors) => errors,
    };
    assert_eq!(errors[0].phase, Phase::Type);
    assert!(errors[0].message.contains("step budget"));
}

#[test]
fn checking_rejects_invalid_bodies_types_patterns_and_constraints() {
    let cases = [
        ("let bad=fn(x:Bool)->Integer{x};let main=42;", Phase::Type),
        ("let main=if true{42}else{false};", Phase::Type),
        (
            "extern type Left=Type;extern type Right=Type;let bad=fn(x:Left)->Right{x};",
            Phase::Type,
        ),
        (
            "let bad=fn(x:Record<{value:Integer}>){x.missing};",
            Phase::Type,
        ),
        ("let id=fn(x:Integer){x};let main=id(1,2);", Phase::Type),
        ("let id=fn(x:Integer){x};let main=id(false);", Phase::Type),
        (
            "let add<T>=fn(x:T,y:T)->T{x+y};let main=add<Bool>(true,false);",
            Phase::Type,
        ),
        (
            "let id<T>=fn(x:T){x};let main={let f=id;let x=f(1);f(false)};",
            Phase::Type,
        ),
        ("let bad=fn<N:Nat>(x:N){x};", Phase::Type),
        ("let id<T>=fn(x:T){x};let alias=id;", Phase::Type),
        ("enum Bad{Make((x:Bad)->Integer)};", Phase::Type),
        (
            "enum Choice{Left,Right};let bad=fn(x:Choice){match x{Choice::Left()=>1}};",
            Phase::Type,
        ),
        (
            "enum Unary{Z,S(Unary)};let bad=fn(x:Unary){match x{Unary::Z()=>0,Unary::S(Unary::Z())=>1}};",
            Phase::Type,
        ),
        (
            "let rec loop=fn(x:Integer)->Integer{loop(x)};",
            Phase::Termination,
        ),
        ("enum Option<T>{Absent,Present(T)};", Phase::Resolve),
        ("extern type Invalid=Bool;", Phase::Type),
        (
            "extern type Int<W:Nat>;extern let bad:Int<true>;",
            Phase::Type,
        ),
        ("extern type Int<W:Nat>;extern let bad:Int;", Phase::Type),
        (
            "extern type Int<W:Nat>;extern let bad:Int<32,64>;",
            Phase::Type,
        ),
        (
            "extern type Int<W:Nat>;extern fn add<W:Nat>(lhs:Int<W>,rhs:Int<W>)->Int<W>;extern let x:Int<32>;extern let y:Int<64>;let bad=add(x,y);",
            Phase::Type,
        ),
        (
            "extern type Int<W:Nat>;extern fn add<W:Nat>(lhs:Int<W>,rhs:Int<W>)->Int<W>;let bad=add<true>;",
            Phase::Type,
        ),
        (
            "let low<T:Type_0>=fn(x:T){x};let high<T:Type_1>=fn(x:T){low<T>(x)};",
            Phase::Type,
        ),
        (
            "let low<T:Type_0>=fn(x:T){x};let high<T:Type_1>=fn(x:T){low(x)};",
            Phase::Type,
        ),
        (
            "enum Higher{Wrap(Type_0)};extern let x:Higher;let low<T>=fn(x:T){x};let main=low(x);",
            Phase::Type,
        ),
        (
            "let id<T:Type_0>=fn(x:T){x};let main=id<Type_0>(Type_0);",
            Phase::Type,
        ),
    ];
    for (source, phase) in cases {
        let errors = match compile(&SourceMap::new("reject.cdsl", source)) {
            Ok(_) => panic!("accepted invalid source: {source}"),
            Err(errors) => errors,
        };
        assert_eq!(errors[0].phase, phase, "{source}: {errors:?}");
        assert_eq!(errors[0].source, "reject.cdsl");
    }
}

#[test]
fn nested_recursive_parameter_shadow_cannot_forge_a_smaller_argument() {
    let source = "enum Unary{Z,S(Unary)};let rec f=fn(n:Unary)->Integer{match n{Unary::Z()=>0,Unary::S(t)=>{let rec g=fn(t:Unary)->Integer{f(t)};g(n)}}};";
    let errors = match compile(&SourceMap::new("shadow.cdsl", source)) {
        Ok(_) => panic!("accepted structurally nondecreasing nested recursion"),
        Err(errors) => errors,
    };
    assert_eq!(errors[0].phase, Phase::Termination);
}

#[test]
fn supplied_modules_control_visibility_and_keep_qualified_identity() {
    let mut sources = SourceMap::new(
        "root.cdsl",
        "mod library;use library::answer as result;let main=result;",
    );
    sources.insert("library", "pub let answer=42;let hidden=false;");
    assert_eq!(
        compile(&sources)
            .unwrap_or_else(|e| panic!("{e:?}"))
            .evaluate("main")
            .unwrap()
            .to_string(),
        "42"
    );
    let mut private = SourceMap::new("root.cdsl", "mod library;use library::hidden;let main=42;");
    private.insert("library", "let hidden=false;");
    assert!(compile(&private).is_err());
    let source = "extern type Namespace=Type;extern type example=Namespace;pub let example::item<T>={value:T};let alias<T>=example::item<T>;let main=(example::item<Integer>.identity,alias<Integer>.identity);";
    assert_eq!(
        evaluate(source),
        "(\"main::example::item<T=Integer>\", \"main::example::item<T=Integer>\")"
    );
}

#[test]
fn inferred_constraints_and_signature_metadata_survive_checking() {
    let source = "extern type Namespace=Type;extern type example=Namespace;pub let example::add<T>={signature:(lhs:T,rhs:T)->T,exec:fn(lhs:T,rhs:T)->T{lhs+rhs}};";
    let checked =
        compile(&SourceMap::new("contracts.cdsl", source)).unwrap_or_else(|e| panic!("{e:?}"));
    let constraints = checked
        .constraints("example::add")
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(constraints.contains(&"Add<T, T, T>".into()));
    assert!(
        checked
            .type_of("example::add")
            .unwrap()
            .to_string()
            .contains("TypeRep<(lhs: T, rhs: T) -> T>")
    );
}

#[test]
fn lazy_conditions_opaque_foreign_values_and_total_indexing_are_distinct() {
    assert_eq!(
        evaluate("extern let unavailable:()->Bool;let main=false&&unavailable();"),
        "false"
    );
    assert_eq!(evaluate("let main=[1,2][9];"), "Option::None()");
    assert_eq!(evaluate("let main=[1,2][1];"), "Option::Some(2)");
    let checked = compile(&SourceMap::new(
        "foreign.cdsl",
        "extern let unavailable:()->Bool;let main=unavailable();",
    ))
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(checked.evaluate("main").unwrap_err().phase, Phase::Evaluate);
}

#[test]
fn numeric_limits_fail_without_panics_or_negative_naturals() {
    let cases = [
        "let main:Nat=0-1;",
        "let main=1/0;",
        "let main=170141183460469231731687303715884105727+1;",
        "let main=(-170141183460469231731687303715884105727-1)/(-1);",
    ];
    for source in cases {
        let checked =
            compile(&SourceMap::new("arithmetic.cdsl", source)).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            checked.evaluate("main").unwrap_err().phase,
            Phase::Evaluate,
            "{source}"
        );
    }
}

#[test]
fn cli_rejects_a_source_cycle_before_recursing() {
    let directory = std::env::temp_dir().join(format!("tir-cdsl-cycle-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let root = directory.join("root.cdsl");
    let recursive = directory.join("loop.cdsl");
    std::fs::write(&root, "mod loop;pub let main=42;").unwrap();
    std::fs::write(&recursive, "mod loop;").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cdslc"))
        .arg("check")
        .arg(&root)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&directory).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cyclic source module")
    );
}

#[test]
fn accepted_operation_records_use_source_imports_and_explicit_self() {
    let root = "mod core;use core::ops::*;extern type builtin=Dialect;pub let builtin::addi<T>={signature:(lhs:T,rhs:T)->T,exec:fn(lhs:T,rhs:T)->T{lhs+rhs}};pub let builtin::constant<T>={signature:()->T,attributes:{value:T},exec:fn(self)->T{self.value}};let main=(builtin::addi<Integer>.payload.exec(20,22),builtin::constant<Integer>.payload.exec({value:42}));";
    let mut sources = SourceMap::new("operations.cdsl", root);
    sources.insert("core", "pub mod ops{pub extern type Dialect=Type;};");
    let checked = compile(&sources).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(checked.evaluate("main").unwrap().to_string(), "(42, 42)");
}

#[test]
fn constructor_collisions_cycles_and_duplicate_product_labels_are_rejected() {
    let cases = [
        (
            "enum E{Make(Integer)};let Make=fn(x:Bool)->Integer{x};let main=42;",
            Phase::Type,
        ),
        (
            "let rec f=fn(x:Integer)->Integer{g(x)};let g=fn(x:Integer)->Integer{f(x)};let main=42;",
            Phase::Termination,
        ),
        (
            "let rec f=fn(x:Integer)->Integer{g()(x)};let g=fn(){f};let main=42;",
            Phase::Termination,
        ),
        (
            "let f=fn(x:(a:Bool,a:Integer))->Bool{x.a};let main=f({a:42});",
            Phase::Type,
        ),
        (
            "let f=fn(x:(a:Integer))->Integer{x.0};let main=f({a:42});",
            Phase::Type,
        ),
        ("let unused<T>=42;let main=unused;", Phase::Type),
    ];
    for (source, phase) in cases {
        let errors = match compile(&SourceMap::new("boundary.cdsl", source)) {
            Ok(_) => panic!("accepted invalid source: {source}"),
            Err(errors) => errors,
        };
        assert_eq!(errors[0].phase, phase, "{source}: {errors:?}");
    }
}

#[test]
fn public_reexports_survive_glob_imports_and_namespace_capabilities() {
    let source = "pub mod alpha{pub let answer=42;};pub mod beta{pub use alpha::answer as value;};use beta::*;let main=value;";
    assert_eq!(evaluate(source), "42");
    let source = "pub mod library{pub extern type Space=Type;};use library::Space as owned;pub let owned::item=42;let main=(library::Space::item.identity,owned::item.payload);";
    let mut sources = SourceMap::new("capability.cdsl", source);
    assert!(compile(&sources).is_err());
    sources
        .namespace_capabilities
        .insert("library::Space".into());
    let checked = compile(&sources).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(
        checked.evaluate("main").unwrap().to_string(),
        "(\"main::library::Space::item\", 42)"
    );
}

#[test]
fn generic_host_models_remain_typed_references_in_evaluated_records() {
    use std::collections::BTreeMap;
    use tir_cdsl::{Value, types::Type};

    let mut sources = SourceMap::new(
        "host.cdsl",
        "mod host;extern let input:host::Int<32>;let main={signature:(lhs:host::Int<32>,rhs:host::Int<32>)->host::Int<32>,exec:host::add<32>,value:input};",
    );
    sources.package = "demo".into();
    sources.insert(
        "host",
        "pub extern type Int<W:Nat>;pub extern fn add<W:Nat>(lhs:Int<W>,rhs:Int<W>)->Int<W>;",
    );
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    let Value::Record(fields) = checked.evaluate("main").unwrap() else {
        panic!("expected evaluated specification record")
    };
    let Value::Foreign {
        identity,
        ty,
        arguments,
    } = &fields[1].1
    else {
        panic!("expected retained opaque function")
    };
    assert_eq!(identity, "demo::host::add");
    assert_eq!(
        arguments,
        &BTreeMap::from([("W".into(), Type::IntegerIndex(32))])
    );
    assert_eq!(
        ty.to_string(),
        "(lhs: host::Int<32>, rhs: host::Int<32>) -> host::Int<32>"
    );
    let Value::Foreign { identity, ty, .. } = &fields[2].1 else {
        panic!("expected retained opaque nominal value")
    };
    assert_eq!(identity, "demo::input");
    assert_eq!(ty.to_string(), "host::Int<32>");
}

#[test]
fn checked_export_preserves_quantification_and_typed_host_calls() {
    use tir_cdsl::{core::TermKind, types::Type};

    let source = "extern type Int<W:Nat>;extern fn add<W:Nat>(lhs:Int<W>,rhs:Int<W>)->Int<W>;let model<W:Nat>=fn(lhs:Int<W>,rhs:Int<W>)->Int<W>{add(lhs,rhs)};";
    let checked = compile(&SourceMap::new("export.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let definition = checked.definition("model").unwrap();
    assert_eq!(definition.scheme.parameters, vec![("W".into(), Type::Nat)]);
    assert_eq!(definition.term.ty, definition.scheme.ty);
    let TermKind::Function(_, body) = &definition.term.kind else {
        panic!("expected function")
    };
    assert_eq!(body.ty.to_string(), "Int<W>");
    let TermKind::Call(function, arguments) = &body.kind else {
        panic!("expected checked host call")
    };
    assert_eq!(arguments[0].ty, body.ty);
    assert_eq!(arguments[1].ty, body.ty);
    let TermKind::Global(name, substitution, _) = &function.kind else {
        panic!("expected checked global reference")
    };
    assert_eq!(name, "add");
    assert_eq!(
        substitution["W"],
        Type::Index("W".into(), Box::new(Type::Nat))
    );
    let foreign = checked.definition(name).unwrap();
    assert!(matches!(&foreign.term.kind, TermKind::Foreign(identity) if identity == "main::add"));
}

#[test]
fn evaluated_function_exports_preserve_checked_code_and_captures() {
    use tir_cdsl::{Value, types::Type};

    let source = "extern let input<T>:T;let factory<T>=fn(captured:T){{callback:fn(value:T)->T{captured}}};let metadata<T>=factory<T>(input<T>);";
    let checked = compile(&SourceMap::new("evaluated-code.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let Value::Record(fields) = checked.evaluate_template("metadata").unwrap() else {
        panic!("expected computed metadata record")
    };
    let Value::Function(function) = &fields[0].1 else {
        panic!("expected evaluated callback")
    };
    assert_eq!(function.parameters(), &["value"]);
    assert_eq!(function.code().ty, Type::Parameter("T".into()));
    assert_eq!(function.type_arguments()["T"], Type::Parameter("T".into()));
    let Value::Foreign { identity, ty, .. } = &function.captures()["captured"] else {
        panic!("expected captured typed host value")
    };
    assert_eq!(identity, "main::input");
    assert_eq!(ty, &Type::Parameter("T".into()));
}

#[test]
fn output_completion_closes_direct_and_aliased_callback_rows() {
    use std::collections::BTreeMap;
    use tir_cdsl::{core::TermKind, types::Type};

    let source = "let callback<T>=fn(self)->T{self.value};let alias<T>=callback<T>;let metadata<T>={exec:alias<T>};let direct<T>={exec:fn(self)->T{self.value}};let main=metadata<Integer>.exec({value:42,flag:true});";
    let checked = compile(&SourceMap::new("completion.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let receiver = Type::Record(
        BTreeMap::from([
            ("value".into(), Type::Parameter("T".into())),
            ("flag".into(), Type::Bool),
        ]),
        None,
    );
    let expected_callback = Type::Function(
        vec![("self".into(), receiver.clone())],
        Box::new(Type::Parameter("T".into())),
    );
    let expected = Type::Record(
        BTreeMap::from([("exec".into(), expected_callback.clone())]),
        None,
    );
    let checked = checked
        .complete(&[
            ("metadata".into(), expected.clone()),
            ("direct".into(), expected.clone()),
        ])
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    for name in ["metadata", "direct"] {
        assert_eq!(checked.type_of(name), Some(&expected));
        let TermKind::Record(fields) = &checked.definition(name).unwrap().term.kind else {
            panic!("expected checked metadata record")
        };
        assert_eq!(fields[0].1.ty, expected_callback);
        if let TermKind::Function(_, body) = &fields[0].1.kind {
            let TermKind::Project(value, _) = &body.kind else {
                panic!("expected checked receiver projection")
            };
            assert_eq!(value.ty, receiver);
        }
    }
    let TermKind::Record(fields) = &checked.definition("metadata").unwrap().term.kind else {
        panic!("expected checked metadata record")
    };
    let TermKind::Global(name, _, rows) = &fields[0].1.kind else {
        panic!("expected checked alias reference")
    };
    assert_eq!(name, "alias");
    assert_eq!(
        checked.type_of(name).unwrap().substitute_variables(rows),
        expected_callback
    );
    let alias = checked.definition(name).unwrap();
    assert_eq!(alias.term.ty.substitute_variables(rows), expected_callback);
    let TermKind::Global(name, _, alias_rows) = &alias.term.kind else {
        panic!("expected checked callback reference")
    };
    let callback = checked.definition(name).unwrap();
    let TermKind::Function(_, body) = &callback.term.kind else {
        panic!("expected checked callback function")
    };
    let TermKind::Project(value, _) = &body.kind else {
        panic!("expected checked receiver projection")
    };
    assert_eq!(
        value
            .ty
            .substitute_variables(alias_rows)
            .substitute_variables(rows),
        receiver
    );
    assert_eq!(
        rows.values().collect::<Vec<_>>(),
        vec![&Type::Record(
            BTreeMap::from([("flag".into(), Type::Bool)]),
            None
        )]
    );
    assert!(
        checked
            .type_of("callback")
            .unwrap()
            .to_string()
            .contains("..")
    );
    assert_eq!(checked.evaluate("main").unwrap().to_string(), "42");
}

#[test]
fn output_completion_rejects_incompatible_callback_types() {
    use std::collections::BTreeMap;
    use tir_cdsl::types::Type;

    for (source, field_type, message) in [
        (
            "let metadata={exec:fn(self)->Integer{self.value}};",
            Type::Bool,
            "type mismatch",
        ),
        (
            "let callback=fn(self)->Integer{self.value+self.other};let alias=callback;let metadata={exec:alias};",
            Type::Integer,
            "record fields do not match",
        ),
    ] {
        let checked = compile(&SourceMap::new("completion-error.cdsl", source))
            .unwrap_or_else(|errors| panic!("{errors:?}"));
        let expected = Type::Record(
            BTreeMap::from([(
                "exec".into(),
                Type::Function(
                    vec![(
                        "self".into(),
                        Type::Record(BTreeMap::from([("value".into(), field_type)]), None),
                    )],
                    Box::new(Type::Integer),
                ),
            )]),
            None,
        );
        let errors = match checked.complete(&[("metadata".into(), expected)]) {
            Ok(_) => panic!("completed an incompatible receiver for {source}"),
            Err(errors) => errors,
        };
        assert_eq!(errors[0].phase, Phase::Type);
        assert!(errors[0].message.contains(message), "{errors:?}");
    }
}

#[test]
fn output_requirements_complete_an_unused_inferred_receiver_before_evaluation() {
    use std::collections::BTreeMap;
    use tir_cdsl::{compile_with_requirements, types::Type};

    let source = "extern type Marker;let metadata={schema:{flag:Bool},apply:fn(receiver)->Integer{0}};let main=metadata.apply({flag:true});";
    let receiver = Type::Record(BTreeMap::from([("flag".into(), Type::Bool)]), None);
    let expected = Type::Record(
        BTreeMap::from([
            (
                "schema".into(),
                Type::Record(
                    BTreeMap::from([("flag".into(), Type::Description(Box::new(Type::Bool)))]),
                    None,
                ),
            ),
            (
                "apply".into(),
                Type::Function(vec![("receiver".into(), receiver)], Box::new(Type::Integer)),
            ),
        ]),
        None,
    );
    let checked = compile_with_requirements(
        &SourceMap::new("inferred-receiver.cdsl", source),
        |definitions, nominals| {
            assert!(definitions.contains_key("metadata"));
            assert_eq!(nominals["Marker"].category, Type::Universe(0));
            Ok(vec![("metadata".into(), expected.clone())])
        },
    )
    .unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(checked.type_of("metadata"), Some(&expected));
    assert_eq!(checked.evaluate("main").unwrap().to_string(), "0");
}

#[test]
fn output_requirements_reject_mismatched_receivers_and_unsolved_definitions() {
    use std::collections::BTreeMap;
    use tir_cdsl::{compile_with_requirements, types::Type};

    let cases = [
        (
            "let callback=fn(receiver:Bool)->Integer{0};",
            vec![(
                "callback".into(),
                Type::Function(
                    vec![(
                        "receiver".into(),
                        Type::Record(BTreeMap::from([("flag".into(), Type::Bool)]), None),
                    )],
                    Box::new(Type::Integer),
                ),
            )],
            "type mismatch",
        ),
        (
            "let unrelated=fn(unused)->Integer{0};",
            Vec::new(),
            "unsolved type",
        ),
        (
            "let callback=fn(value){!value};",
            vec![(
                "callback".into(),
                Type::Function(
                    vec![("value".into(), Type::Integer)],
                    Box::new(Type::Integer),
                ),
            )],
            "no coherent dictionary",
        ),
    ];
    for (source, requirements, message) in cases {
        let errors = match compile_with_requirements(
            &SourceMap::new("requirement-error.cdsl", source),
            |_, _| Ok(requirements),
        ) {
            Ok(_) => panic!("accepted an invalid output requirement for {source}"),
            Err(errors) => errors,
        };
        assert_eq!(errors[0].phase, Phase::Type);
        assert!(errors[0].message.contains(message), "{errors:?}");
    }
    let errors = match compile(&SourceMap::new(
        "strict.cdsl",
        "let unrelated=fn(unused)->Integer{0};",
    )) {
        Ok(_) => panic!("ordinary compilation accepted an ambiguous function"),
        Err(errors) => errors,
    };
    assert!(errors[0].message.contains("unsolved type"), "{errors:?}");
}

#[test]
fn completion_rejects_untrusted_type_requirements_before_solving() {
    use std::collections::BTreeMap;
    use tir_cdsl::types::Type;

    let source = "extern type Box<T>;extern type Int<W:Nat>;let callback<N:Nat>=fn(self)->Integer{self.value};";
    for (extra, message) in [
        (Type::Variable(999), "inference variable"),
        (
            Type::Nominal("Missing".into(), vec![]),
            "unknown nominal type",
        ),
        (
            Type::Nominal("Box".into(), vec![]),
            "expects 1 generic arguments",
        ),
        (
            Type::Nominal("Int".into(), vec![Type::Bool]),
            "does not inhabit kind Nat",
        ),
        (
            Type::Nominal("Int".into(), vec![Type::IntegerIndex(-1)]),
            "does not inhabit kind Nat",
        ),
        (
            Type::Nominal("Box".into(), vec![Type::Universe(0)]),
            "does not inhabit kind Type_0",
        ),
        (
            Type::Nominal(
                "Int".into(),
                vec![Type::Index("unknown".into(), Box::new(Type::Nat))],
            ),
            "unknown or mismatched static parameter",
        ),
        (Type::Parameter("unknown".into()), "unknown type parameter"),
        (Type::IntegerIndex(1), "cannot be used as a type"),
        (
            Type::Record(BTreeMap::new(), Some(Box::new(Type::Bool))),
            "record tail must be an inferred row",
        ),
        (
            Type::Function(
                vec![("x".into(), Type::Integer), ("x".into(), Type::Integer)],
                Box::new(Type::Integer),
            ),
            "duplicate parameter",
        ),
    ] {
        let checked = compile(&SourceMap::new("untrusted-types.cdsl", source))
            .unwrap_or_else(|errors| panic!("{errors:?}"));
        let expected = Type::Function(
            vec![(
                "self".into(),
                Type::Record(
                    BTreeMap::from([("value".into(), Type::Integer), ("extra".into(), extra)]),
                    None,
                ),
            )],
            Box::new(Type::Integer),
        );
        let errors = match checked.complete(&[("callback".into(), expected)]) {
            Ok(_) => panic!("completed an invalid host type requirement"),
            Err(errors) => errors,
        };
        assert_eq!(errors[0].phase, Phase::Type);
        assert!(errors[0].message.contains(message), "{errors:?}");
    }
    let checked = compile(&SourceMap::new("checked-types.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let expected = Type::Function(
        vec![(
            "self".into(),
            Type::Record(
                BTreeMap::from([
                    ("value".into(), Type::Integer),
                    (
                        "typed".into(),
                        Type::Nominal("Box".into(), vec![Type::Bool]),
                    ),
                    (
                        "indexed".into(),
                        Type::Nominal(
                            "Int".into(),
                            vec![Type::Index("N".into(), Box::new(Type::Nat))],
                        ),
                    ),
                ]),
                None,
            ),
        )],
        Box::new(Type::Integer),
    );
    let checked = checked
        .complete(&[("callback".into(), expected.clone())])
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(checked.type_of("callback"), Some(&expected));
}

#[test]
fn checked_namespaces_preserve_nominal_categories_and_named_alias_identity() {
    use tir_cdsl::types::Type;

    let source = "pub mod library{pub extern type Dialect;pub extern type Category;pub extern type Family<T>=Category;pub extern type Space=Dialect;};use library::Space as owned;let owned::item=42;let alias=owned::item;let main=alias.identity;";
    let mut sources = SourceMap::new("categories.cdsl", source);
    sources
        .namespace_capabilities
        .insert("library::Space".into());
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    let family = checked.nominal_definition("library::Family").unwrap();
    assert_eq!(family.parameters, vec![("T".into(), Type::Universe(0))]);
    assert_eq!(
        family.category,
        Type::Nominal("library::Category".into(), vec![])
    );
    assert_eq!(
        checked
            .nominal_definition("library::Space")
            .unwrap()
            .category,
        Type::Nominal("library::Dialect".into(), vec![])
    );
    assert_eq!(
        checked
            .definition("library::Space::item")
            .unwrap()
            .namespace
            .as_deref(),
        Some("library::Space")
    );
    assert_eq!(checked.definition("alias").unwrap().namespace, None);
    assert!(matches!(checked.type_of("alias"), Some(Type::Named(_))));
    assert_eq!(
        checked.evaluate("main").unwrap().to_string(),
        "\"main::library::Space::item\""
    );
}

#[test]
fn generic_templates_export_rigid_types_and_reject_unknown_static_values() {
    use tir_cdsl::{Value, types::Type};

    let source = "extern type Int<W:Nat>;extern fn add<W:Nat>(lhs:Int<W>,rhs:Int<W>)->Int<W>;extern let input<T>:T;let template<W:Nat,T>={signature:(lhs:Int<W>,rhs:Int<W>)->Int<W>,exec:add<W>,element:T,closure:fn(value:T)->T{value}};let invoke<W:Nat,T>=template<W,T>.closure(input<T>);let static_value<W:Nat>=W;";
    let checked = compile(&SourceMap::new("template.cdsl", source))
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let Value::Record(fields) = checked.evaluate_template("template").unwrap() else {
        panic!("expected generic metadata record")
    };
    assert!(matches!(&fields[0].1, Value::Description(ty)
        if ty.to_string() == "(lhs: Int<W>, rhs: Int<W>) -> Int<W>"));
    let Value::Foreign {
        identity,
        ty,
        arguments,
    } = &fields[1].1
    else {
        panic!("expected generic host reference")
    };
    assert_eq!(identity, "main::add");
    assert_eq!(ty.to_string(), "(lhs: Int<W>, rhs: Int<W>) -> Int<W>");
    assert_eq!(arguments["W"], Type::Index("W".into(), Box::new(Type::Nat)));
    assert!(matches!(&fields[2].1, Value::Description(Type::Parameter(name)) if name == "T"));
    let Value::Foreign { identity, ty, .. } = checked.evaluate_template("invoke").unwrap() else {
        panic!("expected closure to retain generic type substitution")
    };
    assert_eq!(identity, "main::input");
    assert_eq!(ty, Type::Parameter("T".into()));
    let error = checked.evaluate_template("static_value").unwrap_err();
    assert_eq!(error.phase, Phase::Evaluate);
    assert!(error.message.contains("requires specialization"), "{error}");
    assert_eq!(
        checked.evaluate("template").unwrap_err().phase,
        Phase::Evaluate
    );
    let error = checked
        .evaluate_template_with_limits(
            "template",
            EvaluationLimits {
                max_depth: 512,
                max_steps: 0,
            },
        )
        .unwrap_err();
    assert_eq!(error.phase, Phase::Evaluate);
    assert!(error.message.contains("step budget"), "{error}");
}

#[cfg(unix)]
#[test]
fn file_loader_tracks_canonical_transitive_sources_and_rejects_symlink_cycles() {
    use std::collections::BTreeSet;
    use std::os::unix::fs::symlink;

    let directory = std::env::temp_dir().join(format!("tir-cdsl-loader-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let root = directory.join("root.cdsl");
    let shared = directory.join("shared.cdsl");
    let nested = directory.join("nested.cdsl");
    std::fs::write(
        &root,
        "mod one;mod two;pub let main=one::nested::answer+two::nested::answer;",
    )
    .unwrap();
    std::fs::write(&shared, "pub mod nested;").unwrap();
    std::fs::write(&nested, "pub let answer=21;").unwrap();
    symlink(&shared, directory.join("one.cdsl")).unwrap();
    symlink(&shared, directory.join("two.cdsl")).unwrap();
    let (sources, tracked) = SourceMap::from_file(&root).unwrap();
    let expected: BTreeSet<_> = [&root, &shared, &nested]
        .into_iter()
        .map(|path| path.canonicalize().unwrap())
        .collect();
    assert_eq!(tracked, expected.into_iter().collect::<Vec<_>>());
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(checked.evaluate("main").unwrap().to_string(), "42");

    std::fs::write(&shared, "mod again;").unwrap();
    symlink(&root, directory.join("again.cdsl")).unwrap();
    let errors = SourceMap::from_file(&root).unwrap_err();
    assert_eq!(errors[0].phase, Phase::Resolve);
    assert!(errors[0].message.contains("cyclic source module"));
    std::fs::remove_dir_all(&directory).unwrap();
}
