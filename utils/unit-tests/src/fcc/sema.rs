//! TypedAst introspection: layouts, annotations and conversions that are
//! properties of the analysis result, not of any printable output.

use fcc::lang_options::LangOptions;
use tir::graph::Dag;

use super::support::{lex, typed_for};

#[test]
fn computes_named_struct_layout() {
    let typed = typed_for(
        "struct Pair { char tag; int value; }; int main(void) { return 0; }",
        "riscv64",
    );
    let pair = typed
        .records()
        .find(|record| record.name == "Pair")
        .unwrap();

    assert_eq!(pair.size, 8);
    assert_eq!(pair.align, 4);
    assert_eq!(pair.fields[0].offset, 0);
    assert_eq!(pair.fields[1].offset, 4);
}

#[test]
fn computes_named_union_layout() {
    let typed = typed_for(
        "union Value { int integer; long wide; }; int main(void) { return 0; }",
        "riscv64",
    );
    let value = typed
        .records()
        .find(|record| record.name == "Value")
        .unwrap();

    assert_eq!(value.size, 8);
    assert_eq!(value.align, 8);
    assert_eq!(value.fields[0].offset, 0);
    assert_eq!(value.fields[1].offset, 0);
}

#[test]
fn gives_anonymous_structs_distinct_compiler_names() {
    let typed = typed_for(
        "typedef struct { int value; } First; typedef struct { int value; } Second;",
        "riscv64",
    );
    let names = typed
        .records()
        .map(|record| record.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(names.len(), 2);
    assert_ne!(names[0], names[1]);
    assert!(names
        .iter()
        .all(|name| name.starts_with("__fcc_anon_struct.")));
}

#[test]
fn resolves_struct_member_type() {
    let typed = typed_for(
        "struct Pair { int value; }; int read(void) { struct Pair pair; return pair.value; }",
        "riscv64",
    );
    let ast = typed.ast();
    let member = ast
        .postorder(ast.root().unwrap())
        .find(|node| ast.get_node(*node).kind == fcc::ast::AstKind::Member)
        .unwrap();
    let semantics = ast.get_annotation(member).unwrap();

    assert!(matches!(
        typed.types().kind(semantics.ty.unwrap()),
        fcc::sema::TypeKind::Integer(fcc::sema::IntegerKind::Int)
    ));
    assert_eq!(semantics.member_index, Some(0));
}

#[test]
fn resolves_pointer_member_against_the_tag_identity() {
    let typed = typed_for(
        "struct Other { char byte; }; struct Pair { int value; }; int read(struct Pair *pair) { return pair->value; }",
        "riscv64",
    );
    let ast = typed.ast();
    let member = ast
        .postorder(ast.root().unwrap())
        .find(|node| ast.get_node(*node).kind == fcc::ast::AstKind::Member)
        .unwrap();
    let semantics = ast.get_annotation(member).unwrap();

    assert!(matches!(
        typed.types().kind(semantics.ty.unwrap()),
        fcc::sema::TypeKind::Integer(fcc::sema::IntegerKind::Int)
    ));
}

#[test]
fn target_profile_controls_long_width() {
    let source = "long identity(long value) { return value; }";
    let ilp32 = typed_for(source, "riscv32");
    let lp64 = typed_for(source, "riscv64");
    let find_parameter_width = |typed: &fcc::sema::TypedAst| {
        let root = typed.ast().root().unwrap();
        typed
            .ast()
            .postorder(root)
            .find(|&node| typed.ast().get_node(node).kind == fcc::ast::AstKind::Param)
            .and_then(|node| typed.ast().get_annotation(node)?.ty)
            .map(|ty| typed.integer_width(ty).unwrap())
            .unwrap()
    };

    assert_eq!(find_parameter_width(&ilp32), 32);
    assert_eq!(find_parameter_width(&lp64), 64);
}

#[test]
fn identifier_uses_are_bound_to_their_declarations() {
    let typed = typed_for(
        "int choose(int value) { { int value = 2; value = 3; } return value; }",
        "riscv64",
    );
    let root = typed.ast().root().unwrap();
    let nodes = typed.ast().postorder(root).collect::<Vec<_>>();
    let parameter = nodes
        .iter()
        .copied()
        .find(|&node| typed.ast().get_node(node).kind == fcc::ast::AstKind::Param)
        .unwrap();
    let local = nodes
        .iter()
        .copied()
        .find(|&node| typed.ast().get_node(node).kind == fcc::ast::AstKind::Decl)
        .unwrap();
    let uses = nodes
        .iter()
        .copied()
        .filter(|&node| {
            matches!(
                typed.ast().get_node(node).kind,
                fcc::ast::AstKind::Assign | fcc::ast::AstKind::Var
            )
        })
        .collect::<Vec<_>>();

    let parameter_entity = typed.ast().get_annotation(parameter).unwrap().entity;
    let local_entity = typed.ast().get_annotation(local).unwrap().entity;
    assert_ne!(parameter_entity, local_entity);
    assert_eq!(
        typed.ast().get_annotation(uses[0]).unwrap().entity,
        local_entity
    );
    assert_eq!(
        typed.ast().get_annotation(uses[1]).unwrap().entity,
        parameter_entity
    );
}

#[test]
fn usual_arithmetic_conversions_are_recorded() {
    let typed = typed_for(
        "long add(long left, int right) { return left + right; }",
        "riscv64",
    );
    let root = typed.ast().root().unwrap();
    let add = typed
        .ast()
        .postorder(root)
        .find(|&node| typed.ast().get_node(node).kind == fcc::ast::AstKind::Add)
        .unwrap();
    let result = typed.ast().get_annotation(add).unwrap().ty.unwrap();
    let operands = typed.ast().children(add).collect::<Vec<_>>();

    assert!(typed
        .ast()
        .get_annotation(operands[0])
        .unwrap()
        .conversions
        .is_empty());
    assert_eq!(
        typed.ast().get_annotation(operands[1]).unwrap().conversions,
        vec![result]
    );
}

#[test]
fn usual_arithmetic_conversions_follow_the_target_data_model() {
    let source = "long mix(long signed_value, unsigned int unsigned_value) { return signed_value + unsigned_value; }";
    let result_kind = |typed: &fcc::sema::TypedAst| {
        let root = typed.ast().root().unwrap();
        let add = typed
            .ast()
            .postorder(root)
            .find(|&node| typed.ast().get_node(node).kind == fcc::ast::AstKind::Add)
            .unwrap();
        let ty = typed.ast().get_annotation(add).unwrap().ty.unwrap();
        typed.types().kind(ty).clone()
    };

    assert_eq!(
        result_kind(&typed_for(source, "riscv32")),
        fcc::sema::TypeKind::Integer(fcc::sema::IntegerKind::UnsignedLong)
    );
    assert_eq!(
        result_kind(&typed_for(source, "riscv64")),
        fcc::sema::TypeKind::Integer(fcc::sema::IntegerKind::Long)
    );
}

#[test]
fn assignment_like_contexts_record_the_destination_conversion() {
    let typed = typed_for(
        "long widen(int value) { long copy = value; return copy; }",
        "riscv64",
    );
    let root = typed.ast().root().unwrap();
    let declaration = typed
        .ast()
        .postorder(root)
        .find(|&node| typed.ast().get_node(node).kind == fcc::ast::AstKind::Decl)
        .unwrap();
    let destination = typed.ast().get_annotation(declaration).unwrap().ty.unwrap();
    let initializer = typed.ast().children(declaration).next().unwrap();

    assert_eq!(
        typed.ast().get_annotation(initializer).unwrap().conversions,
        vec![destination]
    );
}

/// In C17 an empty parameter list is not a prototype, so extra arguments are
/// accepted. The rejecting C23 half lives in `fcc/checks/Sema`; the accepting
/// half cannot be a LIT check because `--stage ir` also runs codegen, which
/// does not lower calls through unprototyped functions yet.
#[test]
fn c17_accepts_a_call_through_an_empty_parameter_list() {
    let options: LangOptions = "c17".parse().unwrap();
    let source = "int legacy(); int main(void) { return legacy(1); }";
    let ast = fcc::parser::parse(&lex("<c17-test>", source), options).expect("parse");
    assert!(fcc::sema::analyze(ast, options).is_ok());
}
