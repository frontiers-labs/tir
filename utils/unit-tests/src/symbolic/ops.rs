use tir_adt::APInt;
use tir_symbolic::lang::{scalar_op_named, SmtTemplate, SymKind, WidthRule, SCALAR_OPS};

#[test]
fn scalar_table_has_unique_kinds_and_names() {
    for (index, op) in SCALAR_OPS.iter().enumerate() {
        assert!(!SCALAR_OPS[..index].iter().any(|prev| prev.kind == op.kind));
        assert!(!SCALAR_OPS[..index].iter().any(|prev| prev.name == op.name));
        assert!(!op.rust.is_empty());
    }
}

#[test]
fn xnor_row_drives_every_scalar_consumer_field() {
    let op = scalar_op_named("xnor").unwrap();
    assert_eq!(op.kind, SymKind::Xnor);
    assert_eq!(op.arity, 2);
    assert_eq!(op.width, WidthRule::First);
    assert_eq!(op.smt, SmtTemplate::Binary("bvxnor"));
    assert_eq!(op.rust, "Xnor");
    assert_eq!(
        op.eval_int(&[APInt::new(4, 0b1010), APInt::new(4, 0b1100)]),
        APInt::new(4, 0b1001)
    );
}
