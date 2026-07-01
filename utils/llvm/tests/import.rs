//! End-to-end tests: LLVM textual IR in, printed TIR out, verified.

use tir::{Context, IRFormatter, OpId, Operation};

fn import_and_print(src: &str) -> String {
    let context = Context::with_default_dialects();
    let module = tir_llvm::import_str(&context, src).expect("import should succeed");
    verify_recursive(&context, module.id()).expect("imported module must verify");
    let mut out = String::new();
    module.print(&mut IRFormatter::new(&mut out)).unwrap();
    out
}

fn verify_recursive(context: &Context, op_id: OpId) -> Result<(), String> {
    let instance = context.get_op(op_id);
    instance
        .clone()
        .as_dyn_op()
        .verify(context)
        .map_err(|e| format!("{e:?}"))?;
    for region_id in instance.regions.clone() {
        let region = context.get_region(region_id);
        for block in region.iter(context.clone()) {
            for child in block.op_ids() {
                verify_recursive(context, child)?;
            }
        }
    }
    Ok(())
}

#[test]
fn arithmetic_function() {
    let out = import_and_print(
        "define i32 @add(i32 %a, i32 %b) {\n  %s = add i32 %a, %b\n  ret i32 %s\n}\n",
    );
    assert!(out.contains("func @add"), "{out}");
    assert!(out.contains("addi"), "{out}");
    assert!(out.contains("return"), "{out}");
}

#[test]
fn inline_constant_is_materialised() {
    // `add i32 %a, 5` has no inline-constant form in TIR: a `constant` op must
    // appear before the `addi`.
    let out =
        import_and_print("define i32 @inc(i32 %a) {\n  %s = add i32 %a, 5\n  ret i32 %s\n}\n");
    assert!(out.contains("constant"), "{out}");
    assert!(out.contains("addi"), "{out}");
}

#[test]
fn memory_and_control_flow() {
    let src = "define i32 @select(i1 %c, i32 %x, i32 %y) {\n\
               entry:\n\
               \x20 %p = alloca i32\n\
               \x20 br i1 %c, label %t, label %f\n\
               t:\n\
               \x20 store i32 %x, ptr %p\n\
               \x20 br label %done\n\
               f:\n\
               \x20 store i32 %y, ptr %p\n\
               \x20 br label %done\n\
               done:\n\
               \x20 %r = load i32, ptr %p\n\
               \x20 ret i32 %r\n\
               }\n";
    let out = import_and_print(src);
    for needle in ["alloca", "cond_br", "store", "br ^", "load", "return"] {
        assert!(out.contains(needle), "missing {needle} in:\n{out}");
    }
}

#[test]
fn icmp_cast_and_call() {
    let src = "define i32 @f(i32 %a, i32 %b) {\n\
               \x20 %c = icmp slt i32 %a, %b\n\
               \x20 %w = zext i1 %c to i32\n\
               \x20 %r = call i32 @g(i32 %w)\n\
               \x20 ret i32 %r\n\
               }\n";
    let out = import_and_print(src);
    assert!(out.contains("cmpi"), "{out}");
    assert!(out.contains("extui"), "{out}");
    assert!(out.contains("call @g"), "{out}");
}

#[test]
fn unsupported_instruction_errors() {
    let context = Context::with_default_dialects();
    let src =
        "define i32 @f(i32 %x) {\n  %y = mul i32 %x, %x\n  %z = sdiv i32 %y, %x\n  ret i32 %z\n}\n";
    match tir_llvm::import_str(&context, src) {
        Err(tir_llvm::Error::Unsupported(op)) => assert_eq!(op, "sdiv"),
        Err(other) => panic!("wrong error: {other:?}"),
        Ok(_) => panic!("expected an unsupported-instruction error"),
    }
}
