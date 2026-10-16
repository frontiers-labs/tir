//! Codegen properties that are not textual matches: round-trips through the IR
//! parser, verifier behavior on hand-built IR, and the pre-lowering CIR shapes
//! that no `--stage` exposes (the driver lowers `cir` struct operations before
//! printing IR).

use super::support::{compile_ir, fcc_context, print_ir};

fn reprint(ir: &str) -> String {
    let context = fcc_context();
    let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, ir)
        .expect("emitted IR should parse back");
    print_ir(&module)
}

/// The emitted IR parses back, and printing what parsed is a fixpoint. Value
/// numbers are not compared against the emitted text: codegen and the parser
/// allocate a function's λ value at different points in its body.
#[test]
fn ir_roundtrips_through_parser() {
    for source in [
        "int sum(int a, int b) { return a + b; }",
        r#"int printf(const char *restrict format, ...);
int main(void) { printf("hello"); return 0; }"#,
        "struct Pair { char tag; int value; }; int main(void) { struct Pair source; struct Pair destination; source.value = 1; destination = source; return destination.value; }",
        "int f(void) { int i = 0; while (i < 3) { i = i + 1; } return i; }",
    ] {
        let printed = reprint(&compile_ir(source));
        assert_eq!(printed, reprint(&printed), "{source}");
    }
}

#[test]
fn global_and_function_cannot_share_a_name() {
    // A data symbol owns its name outright, so no overload may hide behind
    // it. C rejects this, so the conflict is built directly.
    let context = fcc_context();
    let module = tir::builtin::ops::module(&context, None).build();
    module
        .body()
        .append_op(tir::builtin::ops::global_zero(&context, "x", 4, 4).build());
    let region = context.create_region();
    region.add_block(context.create_block(vec![]).id());
    let func = tir::func::ops::lambda(
        &context,
        "x",
        tir::builtin::UnitType::new(&context),
        &region,
    )
    .build();
    func.body()
        .append_op(tir::func::ops::r#return(&context, tir::Operand::none()).build());
    module.body().append_op(func);
    module
        .body()
        .append_op(tir::builtin::ops::module_end(&context).build());

    tir::verify_op_tree(&context, tir::Operation::id(&module))
        .expect_err("a data symbol and a function cannot share a name");
}

#[test]
fn emits_struct_definition_and_type() {
    // Every record is a type definition, and a field of record type names it.
    let ir = compile_ir(
        "struct Pair { char tag; int value; }; struct Boxed { struct Pair pair; }; int main(void) { struct Boxed boxed; return 0; }",
    );

    assert!(
        ir.contains("type.def !Pair = !struct<{tag: !i8, value: !i32}>"),
        "{ir}"
    );
    assert!(
        ir.contains("type.def !Boxed = !struct<{pair: !Pair}>"),
        "{ir}"
    );
}

#[test]
fn emits_member_address_and_scalar_load() {
    let ir = compile_ir(
        "struct Pair { char tag; int value; }; int read(void) { struct Pair pair; return pair.value; }",
    );

    assert!(ir.contains("ptr.field"), "{ir}");
    assert!(ir.contains("ptr.load"), "{ir}");
}

#[test]
fn emits_scalar_member_store() {
    let ir = compile_ir(
        "struct Pair { int value; }; int write(void) { struct Pair pair; pair.value = 7; return pair.value; }",
    );

    assert!(ir.matches("ptr.field").count() >= 2, "{ir}");
    assert!(ir.contains("ptr.store"), "{ir}");
}

#[test]
fn emits_whole_struct_copy() {
    let ir = compile_ir(
        "struct Pair { int value; }; int copy(void) { struct Pair source; struct Pair destination; destination = source; return 0; }",
    );

    assert!(ir.contains("ptr.copy"), "{ir}");
}
