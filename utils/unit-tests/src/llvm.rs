//! Unit tests for the `tir-llvm` importer's public parser.

use tir_llvm::ast::{BinOp, Inst, Operand, Type};
use tir_llvm::parse_module;

#[test]
fn parses_a_simple_function() {
    let src = "define i32 @add(i32 %a, i32 %b) {\n  %s = add i32 %a, %b\n  ret i32 %s\n}\n";
    let module = parse_module(src).unwrap();
    assert_eq!(module.functions.len(), 1);
    let f = &module.functions[0];
    assert_eq!(f.name, "add");
    assert_eq!(f.ret, Type::Int(32));
    assert_eq!(f.params.len(), 2);
    assert_eq!(f.blocks.len(), 1);
    assert_eq!(
        f.blocks[0].insts[0],
        Inst::Binary {
            result: "s".into(),
            op: BinOp::Add,
            ty: Type::Int(32),
            lhs: Operand::Ref("a".into()),
            rhs: Operand::Ref("b".into()),
        }
    );
}

#[test]
fn splits_labelled_blocks() {
    let src = "define void @f(i1 %c) {\nentry:\n  br i1 %c, label %t, label %e\nt:\n  ret void\ne:\n  ret void\n}\n";
    let f = &parse_module(src).unwrap().functions[0];
    assert_eq!(f.blocks.len(), 3);
    assert_eq!(f.blocks[0].label.as_deref(), Some("entry"));
    assert!(matches!(f.blocks[0].insts[0], Inst::CondBr { .. }));
}

#[test]
fn unknown_opcode_becomes_unsupported() {
    let src = "define i32 @f(i32 %x) {\n  %y = freeze i32 %x\n  ret i32 %y\n}\n";
    let f = &parse_module(src).unwrap().functions[0];
    assert_eq!(f.blocks[0].insts[0], Inst::Unsupported("freeze".into()));
}

#[test]
fn skips_declarations_metadata_and_trailing_attrs() {
    let src = "target datalayout = \"e\"\n\
               declare i32 @ext(i32)\n\
               @g = global i32 0\n\
               define i32 @f() {\n  %p = alloca i32, align 4\n  ret i32 0\n}\n\
               !0 = !{}\n";
    let module = parse_module(src).unwrap();
    assert_eq!(module.functions.len(), 1);
    assert!(matches!(
        module.functions[0].blocks[0].insts[0],
        Inst::Alloca { .. }
    ));
}
