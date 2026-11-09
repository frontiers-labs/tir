use tir_cdsl::{SourceMap, compile, relations};

const REACHABILITY: &str = r#"
mod relational;
use relational::*;
extern type Node = Type;
pub let edge = relation<(Node, Node)>("analysis.edge");
pub let reach = relation<(Node, Node)>("analysis.reach");
pub let reach_seed = forall(fn(x: Expr<Node>, y: Expr<Node>) {
    { when: edge(x, y), then: reach(x, y) }
});
pub let reach_step = forall(fn(x: Expr<Node>, y: Expr<Node>, z: Expr<Node>) {
    { when: reach(x, y) & edge(y, z), then: reach(x, z) }
});
pub let reach_program = program({
    tables: [edge, reach],
    stages: [saturate([reach_seed, reach_step])],
    result: query_result(reach),
    completion: fixed_point,
    publication: analysis_only,
});
"#;

#[test]
fn approved_relations_quantify_typed_closures_and_emit_clauses() {
    let mut sources = SourceMap::new("reach.cdsl", REACHABILITY);
    relations::install(&mut sources);
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(
        relations::emit(&checked, "reach_program").unwrap(),
        "(relation analysis.edge (Node Node))\n(relation analysis.reach (Node Node))\n(ruleset stage0)\n(rule ((analysis.edge v0 v1)) ((analysis.reach v0 v1)) :ruleset stage0)\n(rule ((analysis.reach v0 v1) (analysis.edge v1 v2)) ((analysis.reach v0 v2)) :ruleset stage0)\n"
    );
    let mut sources = SourceMap::new(
        "mismatch.cdsl",
        REACHABILITY.replace("y: Expr<Node>", "y: Expr<Nat>"),
    );
    relations::install(&mut sources);
    let errors = match compile(&sources) {
        Ok(_) => panic!("accepted a relation sort mismatch"),
        Err(errors) => errors,
    };
    assert!(
        errors
            .iter()
            .any(|error| error.phase == tir_cdsl::diagnostic::Phase::Type
                && error.message.contains("Node")
                && error.message.contains("Nat")),
        "{errors:?}"
    );
}

#[test]
fn quantified_clauses_reject_unbound_and_escaped_variables() {
    for (source, diagnostic) in [
        (
            "let clause=forall(fn(x:Expr<Node>,y:Expr<Node>){ {when:edge(x,x),then:edge(x,y)} });",
            "unbound variable",
        ),
        (
            "let clause=forall(fn(x:Expr<Node>){ let inner=forall(fn(y:Expr<Node>){ {when:edge(x,y),then:edge(y,x)} }); {when:edge(x,x),then:edge(x,x)} });",
            "escaped its forall scope",
        ),
    ] {
        let mut sources = SourceMap::new(
            "scope.cdsl",
            format!(
                "mod relational;use relational::*;extern type Node=Type;let edge=relation<(Node,Node)>(\"edge\");{source}"
            ),
        );
        relations::install(&mut sources);
        let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
        let error = checked.evaluate("clause").unwrap_err();
        assert!(error.message.contains(diagnostic), "{error}");
    }
}

#[test]
fn enum_lifting_and_unique_table_rows_lower_through_checked_library_calls() {
    let mut sources = SourceMap::new(
        "rows.cdsl",
        r#"
        mod relational;use relational::*;
        enum Tree{Leaf,List(Forest)};enum Forest{Nil,Cons(Tree,Forest)};
        let cons=constructor(Forest::Cons);
        let item=relation<(Forest,Integer,Tree)>("Item");
        let slot=table<(Forest,Integer),Tree>("Slot",unique);
        let clause=forall(fn(xs:Expr<Forest>,x:Expr<Tree>,tail:Expr<Forest>){
            {when:xs == cons(x,tail),then:slot.has(xs,0,x) & item(xs,0,x)}
        });
        let step=forall(fn(xs:Expr<Forest>,n:Expr<Integer>,x:Expr<Tree>){
            {when:slot.has(xs,n,x),then:item(xs,n+1,x)}
        });
        let main=program({tables:[slot,item],stages:[saturate([clause,step])],result:query_result(item),completion:fixed_point,publication:analysis_only});
    "#,
    );
    relations::install(&mut sources);
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(
        relations::emit_schema(
            &checked,
            &[
                relations::SchemaEntry::Datatypes(&["Tree", "Forest"]),
                relations::SchemaEntry::Declaration("slot"),
                relations::SchemaEntry::Declaration("item"),
                relations::SchemaEntry::Ruleset("rows"),
                relations::SchemaEntry::Rule("clause", "rows"),
                relations::SchemaEntry::Rule("step", "rows"),
            ]
        )
        .unwrap(),
        "(datatype* (Tree (Leaf) (List Forest)) (Forest (Nil) (Cons Tree Forest)))\n(function Slot (Forest i64) Tree :no-merge)\n(relation Item (Forest i64 Tree))\n(ruleset rows)\n(rule ((= v0 (Cons v1 v2))) ((set (Slot v0 0) v1) (Item v0 0 v1)) :ruleset rows)\n(rule ((= (Slot v0 v1) v2)) ((Item v0 (+ v1 1) v2)) :ruleset rows)\n"
    );
    checked.evaluate("main").unwrap();
    assert!(
        compile(&SourceMap::new(
            "negative.cdsl",
            "enum Left{Make((x:Right)->Integer)};enum Right{MakeRight(Left)};"
        ))
        .is_err()
    );
    let mut sources = SourceMap::new(
        "bad-row.cdsl",
        r#"mod relational;use relational::*;let slot=table<Integer,Bool>("Slot",unique);let bad=forall(fn(x:Expr<Integer>){ {when:truth,then:slot.has(x,"wrong")} });"#,
    );
    relations::install(&mut sources);
    assert!(compile(&sources).is_err());
}

#[test]
fn relation_keys_reuse_declarations_and_reject_conflicting_schemas() {
    let mut sources = SourceMap::new(
        "keys.cdsl",
        r#"
        mod relational;use relational::*;
        let first=relation<(Integer,)>("same");let second=relation<(Bool,)>("same");
        let main=program({tables:[first,second],stages:[],result:query_result(first),completion:fixed_point,publication:analysis_only});
    "#,
    );
    relations::install(&mut sources);
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert!(
        checked
            .evaluate("main")
            .unwrap_err()
            .message
            .contains("incompatible declarations")
    );
    assert!(
        relations::emit_schema(
            &checked,
            &[
                relations::SchemaEntry::Declaration("first"),
                relations::SchemaEntry::Declaration("second")
            ]
        )
        .unwrap_err()
        .message
        .contains("incompatible declarations")
    );

    let mut sources = SourceMap::new(
        "reference.cdsl",
        r#"
        mod relational;use relational::*;
        let first=relation<(Integer,)>("same");let second=relation<(Bool,)>("same");
        let clause=forall(fn(x:Expr<Bool>){ {when:second(x),then:second(x)} });
        let main=program({tables:[first],stages:[saturate([clause])],result:query_result(first),completion:fixed_point,publication:analysis_only});
    "#,
    );
    relations::install(&mut sources);
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert!(
        checked
            .evaluate("main")
            .unwrap_err()
            .message
            .contains("incompatible declarations")
    );
    let mut sources = SourceMap::new(
        "reuse.cdsl",
        r#"
        mod relational;use relational::*;
        let first=relation<(Integer,)>("same");let alias=relation<(Integer,)>("same");
        let main=program({tables:[first,alias],stages:[],result:query_result(first),completion:fixed_point,publication:analysis_only});
    "#,
    );
    relations::install(&mut sources);
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(
        relations::emit(&checked, "main").unwrap(),
        "(relation same (i64))\n"
    );
}

#[test]
fn scalar_arguments_are_injected_before_ordinary_symbolic_function_calls() {
    for call in ["f(1)", "apply(f,1)"] {
        let mut sources = SourceMap::new(
            "injection.cdsl",
            format!(
                r#"
            mod relational;use relational::*;
            let output=relation<(Integer,)>("Output");
            let f=fn(x:Expr<Integer>){{x==1}};
            let apply=fn(f:(x:Expr<Integer>)->Goal,x:Expr<Integer>){{f(x)}};
            let clause=forall(fn(){{ {{when:{call},then:output(1)}} }});
        "#
            ),
        );
        relations::install(&mut sources);
        let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
        assert_eq!(
            relations::emit_schema(
                &checked,
                &[
                    relations::SchemaEntry::Declaration("output"),
                    relations::SchemaEntry::Ruleset("injection"),
                    relations::SchemaEntry::Rule("clause", "injection"),
                ]
            )
            .unwrap(),
            "(relation Output (i64))\n(ruleset injection)\n(rule ((= 1 1)) ((Output 1)) :ruleset injection)\n"
        );
    }
}

#[test]
fn trusted_library_bindings_require_the_canonical_source_module() {
    for source in [
        "mod relational{pub extern let truth:Integer;};let main=relational::truth;",
        "mod relational;let main=relational::truth;",
    ] {
        let mut sources = SourceMap::new("spoof.cdsl", source);
        sources.insert("relational", "pub extern let truth:Integer;");
        assert!(
            compile(&sources).is_err(),
            "accepted a spoofed standard library"
        );
    }
    let sources = SourceMap::new(
        "nested.cdsl",
        "mod foo{pub mod relational{pub extern let truth:Integer;};};let main=foo::relational::truth;",
    );
    let checked = compile(&sources).unwrap_or_else(|errors| panic!("{errors:?}"));
    assert!(matches!(
        checked.evaluate("main").unwrap(),
        tir_cdsl::Value::Foreign { .. }
    ));
}
