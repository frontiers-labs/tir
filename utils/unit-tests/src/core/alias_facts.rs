//! Alias and escape facts over the pointers of one function.

use tir::{
    analysis::{AliasFacts, AliasResult, Escape, EscapeFacts},
    builtin, AnalysisManager, Context, MemoryWrite, OpId, Operation, ValueId,
};

/// The first function of `source` and the locations its stores write, in order.
fn stores(context: &Context, source: &str) -> (OpId, Vec<ValueId>) {
    let module: builtin::ModuleOp =
        tir::parse::ir::parse_ir(context, source).expect("the fixture parses");
    let body = context
        .get_region(context.get_op(module.id()).regions()[0])
        .iter(context.clone())
        .next()
        .expect("module body");
    let func = *body
        .op_ids()
        .iter()
        .find(|&&op| context.get_op(op).is::<tir::func::FuncOp>())
        .expect("a function");
    let locations = context
        .get_region(context.get_op(func).regions()[0])
        .iter(context.clone())
        .next()
        .expect("function body")
        .op_ids()
        .into_iter()
        .filter_map(|op| {
            context
                .get_op(op)
                .as_interface::<dyn MemoryWrite>()
                .map(|write| write.write_location())
        })
        .collect();
    (func, locations)
}

#[test]
fn same_base_same_offset_must_alias() {
    let context = Context::with_default_dialects();
    let (func, locations) = stores(
        &context,
        r#"module {
  %g = global @g size 16 align 4
  %fn_f = func.func @f(%a: !i32) {
    %0 = constant {value = 4} : !i64
    %p = ptr.ptradd %g, %0 : !ptr.p
    ptr.store %a, %p
    %q = ptr.ptradd %g, %0 : !ptr.p
    ptr.store %a, %q
    %1 = constant {value = 8} : !i64
    %r = ptr.ptradd %g, %1 : !ptr.p
    ptr.store %a, %r
    func.return
  }
  module_end
}"#,
    );
    let facts = AnalysisManager::new().get::<AliasFacts>(&context, func);
    let alias = |a: usize, b: usize| facts.alias(locations[a], Some(4), locations[b], Some(4));
    assert_eq!(alias(0, 1), AliasResult::MustAlias);
    assert_eq!(alias(0, 2), AliasResult::NoAlias);
    assert_eq!(
        facts.alias(locations[0], Some(8), locations[2], Some(4)),
        AliasResult::MayAlias
    );
}

#[test]
fn distinct_allocas_never_alias_but_parameters_may() {
    let context = Context::with_default_dialects();
    let (func, locations) = stores(
        &context,
        r#"module {
  %g = global @g size 4 align 4
  %fn_f = func.func @f(%p: !ptr.p, %q: !ptr.p, %a: !i32) {
    %x = ptr.alloca {size = 4, align = 4} : !ptr.p
    %y = ptr.alloca {size = 4, align = 4} : !ptr.p
    ptr.store %a, %x
    ptr.store %a, %y
    ptr.store %a, %p
    ptr.store %a, %q
    ptr.store %a, %g
    func.return
  }
  module_end
}"#,
    );
    let facts = AnalysisManager::new().get::<AliasFacts>(&context, func);
    let alias = |a: usize, b: usize| facts.alias(locations[a], Some(4), locations[b], Some(4));
    assert_eq!(alias(0, 1), AliasResult::NoAlias);
    assert_eq!(alias(0, 2), AliasResult::NoAlias);
    assert_eq!(alias(2, 3), AliasResult::MayAlias);
    assert_eq!(alias(2, 4), AliasResult::MayAlias);
    assert_eq!(alias(0, 4), AliasResult::NoAlias);
}

#[test]
fn escape_through_call_argument_and_store_to_memory() {
    let context = Context::with_default_dialects();
    let (func, locations) = stores(
        &context,
        r#"module {
  %fn_keep = func.declare @keep(!ptr.p) -> !unit
  %fn_f = func.func @f(%pp: !ptr.p, %a: !i32) {
    %x = ptr.alloca {size = 4, align = 4} : !ptr.p
    %y = ptr.alloca {size = 4, align = 4} : !ptr.p
    %z = ptr.alloca {size = 4, align = 4} : !ptr.p
    %0 = constant {value = 4} : !i64
    %x4 = ptr.ptradd %x, %0 : !ptr.p
    func.call %fn_keep(%x4 : !ptr.p)
    ptr.store %y, %pp
    ptr.store %a, %x
    ptr.store %a, %y
    ptr.store %a, %z
    %u = ptr.load %pp : !ptr.p
    ptr.store %a, %u
    func.return
  }
  module_end
}"#,
    );
    let analyses = AnalysisManager::new();
    let escapes = analyses.get::<EscapeFacts>(&context, func);
    let (x, y, z, unknown) = (locations[1], locations[2], locations[3], locations[4]);
    assert_eq!(escapes.escape(x), Escape::Escapes);
    assert_eq!(escapes.escape(y), Escape::Captured);
    assert_eq!(escapes.escape(z), Escape::Local);

    let facts = analyses.get::<AliasFacts>(&context, func);
    assert_eq!(
        facts.alias(unknown, Some(4), x, Some(4)),
        AliasResult::MayAlias
    );
    assert_eq!(
        facts.alias(unknown, Some(4), y, Some(4)),
        AliasResult::MayAlias
    );
    assert_eq!(
        facts.alias(unknown, Some(4), z, Some(4)),
        AliasResult::NoAlias
    );
}
