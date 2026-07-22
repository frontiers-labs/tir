use tir::{
    Context, DialectName, Operation, OperationName,
    builtin::{BuiltinDialect, ModuleOp, ModuleOpBuilder},
};

#[test]
fn identifies_operations_by_type() {
    let context = Context::new();
    let module = ModuleOpBuilder::new(&context).build();
    let instance = context.get_op(module.id());

    assert!(instance.is::<ModuleOp>());
    assert!(!instance.is::<tir::builtin::FuncOp>());
    assert_eq!(instance.name(), OperationName::of::<ModuleOp>());
    assert_eq!(instance.dialect(), DialectName::of::<BuiltinDialect>());
}
