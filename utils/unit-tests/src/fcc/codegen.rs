//! Codegen properties that are not textual matches: round-trips through the IR
//! parser, verifier behavior on hand-built IR, and the pre-lowering CIR shapes
//! that no `--stage` exposes (the driver lowers `cir` struct operations before
//! printing IR).

use tir::attributes::AttributeValue;

use super::support::{compile_ir, fcc_context, print_ir};

fn assert_roundtrips(ir: &str) {
    let context = fcc_context();
    let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, ir)
        .expect("emitted IR should parse back");
    assert_eq!(ir, print_ir(&module));
}

#[test]
fn ir_roundtrips_through_parser() {
    assert_roundtrips(&compile_ir("int sum(int a, int b) { return a + b; }"));
}

#[test]
fn cir_variadic_ir_roundtrips_through_parser() {
    assert_roundtrips(&compile_ir(
        r#"int printf(const char *restrict format, ...);
int main(void) { printf("hello"); return 0; }"#,
    ));
}

#[test]
fn struct_ir_roundtrips_through_parser() {
    assert_roundtrips(&compile_ir(
        "struct Pair { char tag; int value; }; int main(void) { struct Pair source; struct Pair destination; source.value = 1; destination = source; return destination.value; }",
    ));
}

#[test]
fn loop_ir_roundtrips_through_parser() {
    assert_roundtrips(&compile_ir(
        "int f(void) { int i = 0; while (i < 3) { i = i + 1; } return i; }",
    ));
}

#[test]
fn global_and_function_cannot_share_a_name() {
    // A data symbol owns its name outright, so no overload may hide behind
    // it. C rejects this, so the conflict is built directly.
    let context = fcc_context();
    let module = tir::builtin::ops::module(&context, None).build();
    module.body().append_op(
        fcc::cir::ZeroGlobalOpBuilder::new(&context)
            .attr("sym_name", AttributeValue::Str("x".to_string().into()))
            .attr("size", AttributeValue::UInt(4))
            .attr("align", AttributeValue::UInt(4))
            .build(),
    );
    let region = context.create_region();
    region.add_block(context.create_block(vec![]).id());
    let func = tir::func::ops::func(
        &context,
        "x",
        tir::builtin::UnitType::new(&context),
        Some(region.id()),
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
    // A nested record is what still names a struct *type* in the IR: every
    // pointer is spelled opaquely, so only a field of struct type keeps the
    // `!cir.struct` spelling.
    let ir = compile_ir(
        "struct Pair { char tag; int value; }; struct Boxed { struct Pair pair; }; int main(void) { struct Boxed boxed; return 0; }",
    );

    assert!(ir.contains("cir.define_struct"), "{ir}");
    assert!(ir.contains("!cir.struct<\"Pair\">"), "{ir}");
}

#[test]
fn emits_member_address_and_scalar_load() {
    let ir = compile_ir(
        "struct Pair { char tag; int value; }; int read(void) { struct Pair pair; return pair.value; }",
    );

    assert!(ir.contains("cir.get_member"), "{ir}");
    assert!(ir.contains("ptr.load"), "{ir}");
}

#[test]
fn emits_scalar_member_store() {
    let ir = compile_ir(
        "struct Pair { int value; }; int write(void) { struct Pair pair; pair.value = 7; return pair.value; }",
    );

    assert!(ir.matches("cir.get_member").count() >= 2, "{ir}");
    assert!(ir.contains("ptr.store"), "{ir}");
}

#[test]
fn emits_whole_struct_copy() {
    let ir = compile_ir(
        "struct Pair { int value; }; int copy(void) { struct Pair source; struct Pair destination; destination = source; return 0; }",
    );

    assert!(ir.contains("cir.copy_struct"), "{ir}");
}
