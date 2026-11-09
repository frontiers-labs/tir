//! Codegen properties that are not textual matches: round-trips through the IR
//! parser, verifier behavior on hand-built IR, and the pre-lowering CIR shapes
//! that no `--stage` exposes (the driver lowers `cir` struct operations before
//! printing IR).

use super::support::{compile_ir, fcc_context, print_ir};
use tir::Operation;

fn reprint(ir: &str) -> String {
    let context = fcc_context();
    let module = tir::parse::ir::parse_operation(&context, ir, &tir::builtin::MODULE)
        .expect("emitted IR should parse back");
    print_ir(&module)
}

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
    let module = tir::builtin::ops::module(
        &context,
        None,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    {
        let op = tir::builtin::ops::global_zero(
            &context,
            "x",
            4,
            4,
            Default::default(),
            tir::InsertionPoint::Root,
        );
        (tir::builtin::module_body(&module))
            .context
            .insert(tir::builtin::module_body(&module).end(), op.id());
        op
    };
    let region = context.create_region();
    region.add_block(context.create_block(vec![]).id());
    let func = tir::func::ops::lambda(
        &context,
        "x",
        tir::builtin::UnitType::new(&context),
        &region,
        Default::default(),
        tir::InsertionPoint::Root,
    );
    tir::func::ops::r#return(
        &context,
        tir::Operand::none(),
        vec![],
        Default::default(),
        tir::func::func_body(&func).end(),
    );
    {
        let op = func;
        (tir::builtin::module_body(&module))
            .context
            .insert(tir::builtin::module_body(&module).end(), op.id());
        op
    };
    tir::builtin::ops::module_end(
        &context,
        Default::default(),
        tir::builtin::module_body(&module).end(),
    );

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
