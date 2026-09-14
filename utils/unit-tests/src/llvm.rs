use tir::Context;
use tir_llvm::ast::{Block, Function, Module, Param, Type};

#[test]
fn import_rejects_unsupported_float_width() {
    let module = Module {
        named_types: Vec::new(),
        globals: Vec::new(),
        declarations: Vec::new(),
        functions: vec![Function {
            name: "unsupported_float".into(),
            ret: Type::Void,
            params: vec![Param {
                name: "%value".into(),
                ty: Type::Float(128),
            }],
            blocks: vec![Block {
                label: None,
                insts: Vec::new(),
            }],
        }],
    };
    let error = match tir_llvm::import(&Context::default(), &module) {
        Ok(_) => panic!("unsupported float width was accepted"),
        Err(error) => error,
    };

    assert_eq!(
        error.to_string(),
        "unsupported instruction: LLVM float width 128"
    );
}
