//! CIR dialect verifier behavior. The dialect is registered by fcc only, so
//! these modules are invisible to the generic `tir` tool and cannot be LIT
//! checks.

use tir::{builtin::ModuleOp, parse::ir::parse_ir, verify_op_tree, Context, Operation};

fn cir_context() -> Context {
    let context = Context::with_default_dialects();
    context.register_dialect::<fcc::cir::CirDialect>();
    context
}

fn verify(module: &str) -> Result<(), tir::Error> {
    let context = cir_context();
    let module = parse_ir::<ModuleOp>(&context, module).expect("parse module");
    verify_op_tree(&context, module.id())
}

fn print(module: &ModuleOp) -> String {
    let mut printed = String::new();
    let mut fmt = tir::IRFormatter::new(&mut printed);
    module.print(&mut fmt).expect("print module");
    printed
}

/// Parse `module`, print it, and parse the printed form again: the two printings
/// agree exactly when the op's syntax carries everything its structure holds.
fn roundtrip(module: &str) -> String {
    let context = cir_context();
    let parsed = parse_ir::<ModuleOp>(&context, module).expect("parse module");
    verify_op_tree(&context, parsed.id()).expect("verify module");
    let printed = print(&parsed);

    let context = cir_context();
    let reparsed = parse_ir::<ModuleOp>(&context, &printed).expect("parse printed module");
    assert_eq!(printed, print(&reparsed), "printing is not stable");
    printed
}

#[test]
fn variadic_call_accepts_arguments_beyond_the_fixed_prefix() {
    verify(
        r#"module {
  %fn_printf = func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  %fn_caller = func.func @caller(%0: !ptr.p, %1: !i32) -> !i32 {
    %2 = func.call %fn_printf(%0, %1 : !ptr.p, !i32) -> !i32
    func.return %2
  }
  module_end
}"#,
    )
    .expect("a variadic call verifies");
}

#[test]
fn variadic_call_accepts_an_empty_tail() {
    verify(
        r#"module {
  %fn_printf = func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  %fn_caller = func.func @caller(%0: !ptr.p) -> !i32 {
    %1 = func.call %fn_printf(%0 : !ptr.p) -> !i32
    func.return %1
  }
  module_end
}"#,
    )
    .expect("a variadic call with no variadic argument verifies");
}

#[test]
fn variadic_call_rejects_a_mismatched_fixed_prefix() {
    verify(
        r#"module {
  %fn_printf = func.declare @printf(!ptr.p, !cir.varargs) -> !i32
  %fn_caller = func.func @caller(%0: !i64, %1: !i32) -> !i32 {
    %2 = func.call %fn_printf(%0, %1 : !i64, !i32) -> !i32
    func.return %2
  }
  module_end
}"#,
    )
    .expect_err("the fixed prefix must still match");
}

#[test]
fn for_loop_round_trips() {
    let printed = roundtrip(
        r#"module {
  %fn_count = func.func @count() -> !i32 {
    %0 = ptr.alloca {size = 4, align = 4} : !ptr.p
    cir.for cond {
      %1 = ptr.load %0 : !i32
      %2 = constant {value = 3} : !i32
      %3 = cmpi %1, %2 {predicate = "slt"} : !i1
      cir.condition %3
    } step {
      %4 = ptr.load %0 : !i32
      %5 = constant {value = 1} : !i32
      %6 = addi %4, %5 : !i32
      ptr.store %6, %0
      cir.yield
    } body {
      cir.yield
    }
    %7 = ptr.load %0 : !i32
    func.return %7
  }
  module_end
}"#,
    );
    assert!(printed.contains("cir.for cond {"), "{printed}");
    assert!(printed.contains(" step {"), "{printed}");
    assert!(printed.contains(" body {"), "{printed}");
}

#[test]
fn while_loop_round_trips() {
    let printed = roundtrip(
        r#"module {
  %fn_spin = func.func @spin(%0: !i1) {
    cir.while cond {
      cir.condition %0
    } body {
      cir.break
    }
    func.return
  }
  module_end
}"#,
    );
    assert!(printed.contains("cir.while cond {"), "{printed}");
    assert!(printed.contains(" body {"), "{printed}");
}

#[test]
fn do_loop_round_trips() {
    let printed = roundtrip(
        r#"module {
  %fn_spin = func.func @spin(%0: !i1) {
    cir.do body {
      cir.continue
    } cond {
      cir.condition %0
    }
    func.return
  }
  module_end
}"#,
    );
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
fn condition_region_must_end_in_a_condition() {
    verify(
        r#"module {
  %fn_spin = func.func @spin(%0: !i1) {
    cir.while cond {
      cir.yield
    } body {
      cir.yield
    }
    func.return
  }
  module_end
}"#,
    )
    .expect_err("a cir.while condition region ends in cir.condition");
}

#[test]
fn body_region_must_not_end_in_a_condition() {
    verify(
        r#"module {
  %fn_spin = func.func @spin(%0: !i1) {
    cir.while cond {
      cir.condition %0
    } body {
      cir.condition %0
    }
    func.return
  }
  module_end
}"#,
    )
    .expect_err("a cir.while body region ends in cir.yield, cir.break or cir.continue");
}

#[test]
fn step_region_admits_no_break() {
    verify(
        r#"module {
  %fn_spin = func.func @spin(%0: !i1) {
    cir.for cond {
      cir.condition %0
    } step {
      cir.break
    } body {
      cir.yield
    }
    func.return
  }
  module_end
}"#,
    )
    .expect_err("a for step region only falls through");
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
        %2 = constant {value = 1} : !i1
        scf.if %2 {
          cir.break
        } else {
          cir.continue {label = "outer"}
        }
        cir.yield
      }
      cir.yield
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

/// Every op under `op`, outermost first.
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
    let context = cir_context();
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
    let context = cir_context();
    let module = parse_ir::<ModuleOp>(
        &context,
        r#"module {
  %fn_main = func.func @main() {
    %0 = constant {value = 1} : !i1
    scf.if %0 {
      cir.break {label = "missing"}
    } else {
      scf.yield
    }
    func.return
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
    let context = cir_context();
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
