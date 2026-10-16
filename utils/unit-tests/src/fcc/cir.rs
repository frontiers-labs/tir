//! CIR dialect verifier behavior. The dialect is registered by fcc only, so
//! these modules are invisible to the generic `tir` tool and cannot be LIT
//! checks.

use tir::{builtin::ModuleOp, parse::ir::parse_ir, verify_op_tree, Context, Operation};

use super::support::{fcc_context, print_ir};

fn verify(module: &str) -> Result<(), tir::Error> {
    let context = fcc_context();
    let module = parse_ir::<ModuleOp>(&context, module).expect("parse module");
    verify_op_tree(&context, module.id())
}

fn roundtrip(module: &str) -> String {
    let context = fcc_context();
    let parsed = parse_ir::<ModuleOp>(&context, module).expect("parse module");
    verify_op_tree(&context, parsed.id()).expect("verify module");
    let printed = print_ir(&parsed);

    let context = fcc_context();
    let reparsed = parse_ir::<ModuleOp>(&context, &printed).expect("parse printed module");
    assert_eq!(printed, print_ir(&reparsed), "printing is not stable");
    printed
}

#[test]
fn variadic_call_checks_only_the_fixed_prefix() {
    for (case, params, args, verifies) in [
        (
            "an extra argument",
            "%0: !ptr.p, %1: !i32",
            "%0, %1 : !ptr.p, !i32",
            true,
        ),
        ("an empty tail", "%0: !ptr.p", "%0 : !ptr.p", true),
        (
            "a mismatched fixed prefix",
            "%0: !i64, %1: !i32",
            "%0, %1 : !i64, !i32",
            false,
        ),
    ] {
        let result = verify(&format!(
            r#"module {{
  %fn_printf = func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  %fn_caller = func.func @caller({params}) -> !i32 {{
    %r = func.call %fn_printf({args}) -> !i32
    func.return %r
  }}
  module_end
}}"#
        ));
        assert_eq!(result.is_ok(), verifies, "{case}: {result:?}");
    }
}

#[test]
fn loop_syntax_round_trips() {
    let printed = roundtrip(
        r#"module {
  %fn_loops = func.func @loops(%0: !i1) -> !i32 {
    %1 = ptr.alloca {size = 4, align = 4} : !ptr.p
    cir.for cond {
      %2 = ptr.load %1 : !i32
      %3 = constant {value = 3} : !i32
      %4 = cmpi %2, %3 {predicate = "slt"} : !i1
      cir.condition %4
    } step {
      %5 = ptr.load %1 : !i32
      %6 = constant {value = 1} : !i32
      %7 = addi %5, %6 : !i32
      ptr.store %7, %1
      cir.yield
    } body {
      cir.yield
    }
    cir.while cond {
      cir.condition %0
    } body {
      cir.break
    }
    cir.do body {
      cir.continue
    } cond {
      cir.condition %0
    }
    %8 = ptr.load %1 : !i32
    func.return %8
  }
  module_end
}"#,
    );
    assert!(printed.contains("cir.for cond {"), "{printed}");
    assert!(printed.contains(" step {"), "{printed}");
    assert!(printed.contains(" body {"), "{printed}");
    assert!(printed.contains("cir.while cond {"), "{printed}");
    assert!(printed.contains("cir.do body {"), "{printed}");
    assert!(printed.contains(" cond {"), "{printed}");
}

#[test]
fn loop_regions_admit_multiple_blocks() {
    roundtrip(
        r#"module {
  %fn_spin = func.func @spin(%0: !i1) {
    cir.while cond {
      cfg.cond_br %0, ^bb1, ^bb2
      ^bb1:
      cfg.br ^bb2
      ^bb2:
      cir.condition %0
    } body {
      cir.yield
    }
    func.return
  }
  module_end
}"#,
    );
}

#[test]
fn loop_regions_reject_a_misplaced_terminator() {
    for (case, loop_op) in [
        (
            "a cir.while condition region ends in cir.condition",
            "cir.while cond { cir.yield } body { cir.yield }",
        ),
        (
            "a cir.while body region ends in cir.yield, cir.break or cir.continue",
            "cir.while cond { cir.condition %0 } body { cir.condition %0 }",
        ),
        (
            "a for step region only falls through",
            "cir.for cond { cir.condition %0 } step { cir.break } body { cir.yield }",
        ),
    ] {
        let result = verify(&format!(
            r#"module {{
  %fn_spin = func.func @spin(%0: !i1) {{
    {loop_op}
    func.return
  }}
  module_end
}}"#
        ));
        assert!(result.is_err(), "{case}");
    }
}

const LABELED_LOOPS: &str = r#"module {
  %fn_main = func.func @main() {
    cir.while {label = "outer"} cond {
      %0 = constant {value = 1} : !i1
      cir.condition %0
    } body {
      cir.for cond {
        %1 = constant {value = 1} : !i1
        cir.condition %1
      } step {
        cir.yield
      } body {
        cir.break
      }
      cir.continue {label = "outer"}
    }
    func.return
  }
  module_end
}"#;

#[test]
fn a_loop_label_round_trips() {
    let printed = roundtrip(LABELED_LOOPS);
    assert!(
        printed.contains("cir.while {label = \"outer\"} cond {"),
        "{printed}"
    );
    assert!(
        printed.contains("cir.continue {label = \"outer\"}"),
        "{printed}"
    );
}

fn subtree(context: &Context, op: tir::OpId) -> Vec<tir::OpId> {
    let mut found = vec![op];
    let mut index = 0;
    while index < found.len() {
        for region in context.get_op(found[index]).regions() {
            found.extend(context.get_region(region).op_ids());
        }
        index += 1;
    }
    found
}

fn find<T: Operation>(context: &Context, module: &ModuleOp) -> tir::OpId {
    subtree(context, module.id())
        .into_iter()
        .find(|&op| context.get_op(op).is::<T>())
        .expect("the module holds the op")
}

#[test]
fn a_break_leaves_the_innermost_loop_and_a_labeled_continue_its_label() {
    let context = fcc_context();
    let module = parse_ir::<ModuleOp>(&context, LABELED_LOOPS).expect("parse module");
    let resolve = |exit| tir::analysis::exits::resolve_exit_target(&context, exit);

    assert_eq!(
        resolve(find::<fcc::cir::BreakOp>(&context, &module)).ok(),
        Some(find::<fcc::cir::ForOp>(&context, &module))
    );
    assert_eq!(
        resolve(find::<fcc::cir::ContinueOp>(&context, &module)).ok(),
        Some(find::<fcc::cir::WhileOp>(&context, &module))
    );
}

#[test]
fn an_exit_with_no_loop_to_leave_is_an_error() {
    let context = fcc_context();
    let module = parse_ir::<ModuleOp>(
        &context,
        r#"module {
  %fn_main = func.func @main() {
    cir.break {label = "missing"}
  }
  module_end
}"#,
    )
    .expect("parse module");

    let error = tir::analysis::exits::resolve_exit_target(
        &context,
        find::<fcc::cir::BreakOp>(&context, &module),
    )
    .expect_err("nothing carries the label");
    assert!(
        error.to_string().contains("scope labeled missing"),
        "{error}"
    );
}

/// `n` counts 1, 2, 3 through the outer loop; the inner loop leaves the outer
/// one when `n` is 2, so `i` is bumped once. An exit resolved to the inner loop
/// instead would bump it three times.
const LABELED_BREAK_OUT: &str = r#"module {
  %fn_main = func.func @main() -> !i32 {
    %n = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %i = ptr.alloca {size = 4, align = 4} : !ptr.p<!i32>
    %zero = constant {value = 0} : !i32
    %one = constant {value = 1} : !i32
    %two = constant {value = 2} : !i32
    %three = constant {value = 3} : !i32
    %ten = constant {value = 10} : !i32
    ptr.store %zero, %n
    ptr.store %zero, %i
    cir.while {label = "outer"} cond {
      %0 = ptr.load %n : !i32
      %1 = cmpi %0, %three {predicate = "slt"} : !i1
      cir.condition %1
    } body {
      %2 = ptr.load %n : !i32
      %3 = addi %2, %one : !i32
      ptr.store %3, %n
      cir.while cond {
        %t = constant {value = 1} : !i1
        cir.condition %t
      } body {
        %4 = ptr.load %n : !i32
        %5 = cmpi %4, %two {predicate = "eq"} : !i1
        cfg.cond_br %5, ^out, ^in
      ^out:
        cir.break {label = "outer"}
      ^in:
        cir.break
      }
      %6 = ptr.load %i : !i32
      %7 = addi %6, %ten : !i32
      ptr.store %7, %i
      cir.yield
    }
    %8 = ptr.load %i : !i32
    func.return %8
  }
  module_end
}"#;

#[test]
fn flattening_resolves_a_labeled_break_to_the_loop_it_names() {
    let context = fcc_context();
    let module = parse_ir::<ModuleOp>(&context, LABELED_BREAK_OUT).expect("parse module");
    let mut pm = tir::PassManager::new();
    pm.nest::<tir::func::FuncOp>()
        .add_pass(fcc::passes::RaiseLoopsPass::new());
    pm.run(&context, context.get_op(module.id()))
        .expect("loops flatten");
    verify_op_tree(&context, module.id()).expect("flattened module verifies");

    let main = find::<tir::func::FuncOp>(&context, &module);
    let result = tir::interp::run_function(&context, main, vec![]).expect("runs");
    assert_eq!(result[0].to_i64(), Some(10));
}
