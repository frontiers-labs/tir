//! Object and escape facts over the pointers of one function.

use tir::analysis::objects::{accessed_only, object_base};
use tir::{
    analysis::{Base, Escape, EscapeFacts},
    AnalysisManager, Context, MemoryWrite, OpId, Operation, ValueId,
};

use super::fixtures::{self, module_ops};

fn stores(source: &str) -> (Context, OpId, Vec<ValueId>) {
    let (context, module) = fixtures::parse(source);
    let func = *module_ops(&context, module.id())
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
    (context, func, locations)
}

#[test]
fn escape_through_call_argument_and_store_to_memory() {
    let (context, func, locations) = stores(
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
    let (x, y, z) = (locations[1], locations[2], locations[3]);
    assert_eq!(escapes.escape(x), Escape::Escapes);
    assert_eq!(escapes.escape(y), Escape::Captured);
    assert_eq!(escapes.escape(z), Escape::Local);
}

#[test]
fn object_base_reads_ordered_parameters_and_allocations() {
    let (context, _, locations) = stores(
        r#"module {
  %g = global @g size 4 align 4
  %fn_f = func.func @f(%p: !ptr.p, %q: !ptr.p, %a: !i32) noalias [0] {
    %x = ptr.alloca {size = 4, align = 4} : !ptr.p
    %0 = constant {value = 4} : !i64
    %x4 = ptr.ptradd %x, %0 : !ptr.p
    ptr.store %a, %p
    ptr.store %a, %q
    ptr.store %a, %g
    ptr.store %a, %x4
    func.return
  }
  module_end
}"#,
    );
    let base = |index: usize| object_base(&context, locations[index]);
    assert_eq!(
        base(0),
        Some(Base::Param {
            pointer: locations[0],
            noalias: true
        })
    );
    assert_eq!(
        base(1),
        Some(Base::Param {
            pointer: locations[1],
            noalias: false
        })
    );
    assert_eq!(base(2), Some(Base::Global(locations[2])));
    assert!(matches!(base(3), Some(Base::Alloca(_))));
}

#[test]
fn accessed_only_sees_the_address_leave() {
    let (context, _, locations) = stores(
        r#"module {
  %fn_keep = func.declare @keep(!ptr.p) -> !unit
  %fn_f = func.func @f(%a: !i32) {
    %x = ptr.alloca {size = 4, align = 4} : !ptr.p
    %y = ptr.alloca {size = 4, align = 4} : !ptr.p
    ptr.store %a, %x
    ptr.store %a, %y
    func.call %fn_keep(%y : !ptr.p)
    func.return
  }
  module_end
}"#,
    );
    assert!(accessed_only(&context, locations[0]));
    assert!(!accessed_only(&context, locations[1]));
}
