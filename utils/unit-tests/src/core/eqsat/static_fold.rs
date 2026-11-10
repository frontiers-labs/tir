use super::*;

const LAYOUT_QUERIES: &str =
    include_str!("../../../../../core/checks/Inputs/eqsat/layout_queries_fold.tir");

fn layout_fixture(source: &str) -> (Context, tir::OpHandle, tir::OpId, tir::OpId) {
    let context = Context::with_default_dialects();
    let module = parse_operation(&context, source, &MODULE).unwrap();
    let function = context
        .get_region(context.get_op(module.id()).regions()[0])
        .op_ids()
        .into_iter()
        .find(|&op| context.get_op(op).is_operation(&tir::func::FUNC))
        .unwrap();
    let query = context
        .get_region(context.get_op(function).regions()[0])
        .op_ids()[0];
    (context, module, function, query)
}

#[test]
fn static_layout_queries_fold_in_place_through_the_public_pass() {
    for (query, expected) in [
        ("layout.sizeof {ty = !Node}", 128),
        ("layout.alignof {ty = !Node}", 64),
        ("layout.offsetof {ty = !Node, field = \"value\"}", 64),
    ] {
        let source = LAYOUT_QUERIES
            .replace("layout.sizeof {ty = !Node}", query)
            .replace("value = 128", &format!("value = {expected}"));
        let (context, module, function, operation) = layout_fixture(&source);
        tir::verify_op_tree(&context, module.id()).unwrap();
        let body = context.get_region(context.get_op(function).regions()[0]);
        let ops = body.op_ids();
        let results = context.get_op(operation).results();
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&context, module.id()).unwrap();
        let output = printed_ir(&context, &module);
        filecheck::verify(
            &filecheck::Source::new("layout_queries_fold.tir", &source),
            &filecheck::Source::new("optimized", &output),
            &filecheck::Config::default(),
        )
        .unwrap_or_else(|error| panic!("{query}: {error}\n{output}"));
        assert_eq!(body.op_ids(), ops, "{query}");
        assert_eq!(context.get_op(operation).results(), results, "{query}");
        let constant = context.get_op(operation);
        assert!(constant.is_operation(&tir::builtin::CONSTANT), "{query}");
        assert_eq!(
            constant.attr("value").unwrap().int_at(64).unwrap(),
            APInt::new(64, expected),
            "{query}"
        );
    }
}

#[test]
fn static_layout_queries_keep_unavailable_results_and_metadata() {
    let cases = [
        LAYOUT_QUERIES.replace(", !ptr.p = {size = 64, abi = 64}", ""),
        LAYOUT_QUERIES.replace("{ty = !Node}", "{ty = !Node, note = \"retain\"}"),
        LAYOUT_QUERIES.replace(
            "layout.sizeof {ty = !Node}",
            "layout.offsetof {ty = !Node, field = \"absent\"}",
        ),
        LAYOUT_QUERIES.replace("{ty = !Node}", "{ty = !Node, field = \"value\"}"),
    ];
    for source in cases {
        let (context, module, function, query) = layout_fixture(&source);
        tir::verify_op_tree(&context, module.id()).unwrap();
        let before = printed_ir(&context, &module);
        let attributes = context.get_op(query).attributes();
        let results = context.get_op(query).results();
        EqSatPass::new()
            .run(function, &context, &AnalysisManager::new())
            .unwrap();
        tir::verify_op_tree(&context, module.id()).unwrap();
        assert_eq!(printed_ir(&context, &module), before);
        assert_eq!(context.get_op(query).attributes(), attributes);
        assert_eq!(context.get_op(query).results(), results);
    }
}

#[test]
fn static_layout_queries_preserve_invalid_layout_diagnostics() {
    let source = LAYOUT_QUERIES.replace(
        "module {layout.spec = #layout_spec}",
        "module {layout.spec = \"unknown\"}",
    );
    let (context, module, function, _) = layout_fixture(&source);
    let before = printed_ir(&context, &module);
    let error = tir::verify_op_tree(&context, module.id()).unwrap_err();
    EqSatPass::new()
        .run(function, &context, &AnalysisManager::new())
        .unwrap();
    assert_eq!(printed_ir(&context, &module), before);
    assert_eq!(
        tir::verify_op_tree(&context, module.id())
            .unwrap_err()
            .to_string(),
        error.to_string()
    );
}
