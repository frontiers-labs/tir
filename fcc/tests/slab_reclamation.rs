//! Erasing IR must give its storage back: after every pass the context's slabs
//! may hold at most twice the entities the live IR actually contains.

use std::collections::HashSet;

use fcc::diagnostics::{Span, intern_file};
use fcc::lexer::Token;
use logos::Logos;
use tir::{Context, OpId, Operation, PassManager, RegionId, builtin::ModuleOp};

/// Entities reachable from the module root, which is what "live IR" means.
#[derive(Default, Debug)]
struct LiveIr {
    ops: usize,
    values: usize,
    blocks: usize,
    regions: usize,
}

fn count_live(context: &Context, root: OpId) -> LiveIr {
    let mut live = LiveIr::default();
    let mut values = HashSet::new();
    let mut pending: Vec<OpId> = vec![root];
    while let Some(op_id) = pending.pop() {
        let op = context.get_op(op_id);
        live.ops += 1;
        values.extend(op.results().iter().copied());
        for region_id in op.regions() {
            live.regions += 1;
            count_region(context, region_id, &mut live, &mut values, &mut pending);
        }
    }
    live.values = values.len();
    live
}

fn count_region(
    context: &Context,
    region: RegionId,
    live: &mut LiveIr,
    values: &mut HashSet<tir::ValueId>,
    pending: &mut Vec<OpId>,
) {
    for block in context.get_region(region).iter(context.clone()) {
        live.blocks += 1;
        values.extend(block.arguments().iter().map(|argument| argument.id()));
        pending.extend(block.op_ids());
    }
}

fn lower(source: &str) -> (Context, ModuleOp) {
    let file = intern_file("goto_shapes.c", source);
    let tokens: Vec<_> = Token::lexer(source)
        .spanned()
        .map(|(token, span)| (token.unwrap(), Span::new(file, span.start)))
        .collect();
    let options = Default::default();
    let unit = fcc::parser::parse(&tokens, options).expect("parse");
    let unit = fcc::sema::analyze(unit, options).expect("sema");
    let context = tir::Context::with_default_dialects();
    context.register_dialect::<fcc::cir::CirDialect>();
    let module = fcc::codegen::codegen(&context, &unit).expect("codegen");
    (context, module)
}

fn assert_within_budget(context: &Context, root: OpId, after: &str) {
    let live = count_live(context, root);
    let census = context.slab_census();
    let mut over = Vec::new();
    let mut check = |kind: &str, slab: usize, counted: usize| {
        if slab > 2 * counted {
            over.push(format!("{kind} {slab} held for {counted} live"));
        }
    };
    check("op", census.ops_live, live.ops);
    check("value", census.values_live, live.values);
    check("block", census.blocks_live, live.blocks);
    check("region", census.regions_live, live.regions);
    assert!(over.is_empty(), "after {after}: {}", over.join(", "));
}

#[test]
fn passes_do_not_leak_entities() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scalar/goto_shapes.c"),
    )
    .expect("test input");
    let (context, module) = lower(&source);
    let root = Operation::id(&module);

    assert_within_budget(&context, root, "lowering");

    let run = |name: &str, mut pm: PassManager| {
        pm.run(&context, context.get_op(root)).expect("pass runs");
        assert_within_budget(&context, root, name);
    };

    let mut pm = PassManager::new();
    pm.add_pass(fcc::passes::LowerCirStructsPass::new());
    run("lower-cir-structs", pm);

    for (name, pass) in [
        (
            "lower-cir-control-flow",
            Box::new(fcc::passes::LowerCirControlFlowPass::new()) as Box<dyn tir::Pass>,
        ),
        (
            "thread-state",
            Box::new(tir::passes::ThreadStatePass::new()),
        ),
        ("instcombine", Box::new(tir::passes::InstCombinePass::new())),
        ("erase-state", Box::new(tir::passes::EraseStatePass::new())),
    ] {
        let mut pm = PassManager::new();
        pm.nest::<tir::func::FuncOp>().add_boxed_pass(pass);
        run(name, pm);
    }
}
