//! Builtin integer arithmetic: costs and predicates.

use tir::{
    builtin::{ops, IntegerType},
    Context, Operation,
};

#[test]
fn op_cost_read_through_interface() {
    use tir::OpCost;

    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let a = context.create_value(i32_ty, None);
    let b = context.create_value(i32_ty, None);

    let mul = ops::muli(&context, a.id(), b.id(), i32_ty).build();
    let cost = context
        .get_op(mul.id())
        .as_interface::<dyn OpCost>()
        .expect("muli opts into OpCost");
    assert_eq!(cost.cost(), 4);

    // An op that does not opt in has no OpCost interface; callers default to 1.
    let add = ops::addi(&context, a.id(), b.id(), i32_ty).build();
    assert!(context
        .get_op(add.id())
        .as_interface::<dyn OpCost>()
        .is_none());
}

/// A compare's predicate is a typed attribute: the text parser hands the
/// attribute list a string, and the op's schema decides what it means.
mod predicate {
    use tir::{
        attributes::{AttributeValue, Predicate},
        builtin::ModuleOp,
        check_types,
        parse::ir::parse_ir,
        Context, Operation,
    };

    fn module(body: &str) -> String {
        format!(
            "#layout_spec = #layout.spec<!ptr.p = {{size = 64, abi = 64}}>\n\
             module {{layout.spec = #layout_spec}} {{\n\
             %fn = func.func @f(%0: !i32, %1: !f32, %2: !ptr.p) -> !i1 {{\n\
             {body}\n\
             func.return %3\n}}\nmodule_end\n}}"
        )
    }

    fn parse(context: &Context, body: &str) -> Result<ModuleOp, String> {
        parse_ir::<ModuleOp>(context, &module(body)).map_err(|(_, error)| error.to_string())
    }

    fn verify(body: &str) -> Result<(), String> {
        let context = Context::with_default_dialects();
        parse(&context, body)?;
        check_types(&context).map_err(|error| error.to_string())
    }

    fn compare(context: &Context, module: &ModuleOp) -> tir::OpHandle {
        let body = |op| {
            context
                .get_region(context.get_op(op).regions()[0])
                .iter(context.clone())
                .next()
                .expect("a single-block body")
        };
        let func = body(module.id()).op_ids()[0];
        context.get_op(body(func).op_ids()[0])
    }

    #[test]
    fn a_parsed_predicate_is_typed() {
        let context = Context::with_default_dialects();
        let module =
            parse(&context, r#"%3 = cmpi %0, %0 {predicate = "slt"} : !i1"#).expect("it parses");
        assert_eq!(
            compare(&context, &module).attr("predicate"),
            Some(AttributeValue::Predicate(Predicate::Slt))
        );
    }

    #[test]
    fn a_predicate_outside_the_vocabulary_fails_verification() {
        let error = verify(r#"%3 = cmpi %0, %0 {predicate = "olt"} : !i1"#)
            .expect_err("olt is a float comparison");
        assert!(error.contains("predicate"), "{error}");
    }

    #[test]
    fn an_unknown_predicate_fails_to_parse() {
        for op in [
            r#"%3 = cmpi %0, %0 {predicate = "bogus"} : !i1"#,
            r#"%3 = fp.cmp %1, %1 {predicate = "bogus", semantics = {kind = "comparison", behavior = "quiet", exceptions = "ignore", subnormals = "gradual"}} : !i1"#,
            r#"%3 = ptr.cmp %2, %2 {predicate = "bogus"} : !i1"#,
        ] {
            let context = Context::with_default_dialects();
            let Err(error) = parse(&context, op) else {
                panic!("bogus is no comparison");
            };
            assert!(error.contains("bogus"), "{error}");
            assert!(error.contains("cmp"), "{error}");
        }
    }
}

#[test]
fn later_port_resolves_a_width_equation() {
    use tir::schema::{Argument, BinderKind, Pattern, Port, Signature};
    use tir::TypeArg;

    const W: Pattern = Pattern::Bound(0);
    const V: Pattern = Pattern::Bound(1);
    let signature = Signature {
        binders: &[BinderKind::Nat, BinderKind::Nat],
        inputs: &[
            Port::Fixed(Pattern::Constructor(
                "builtin",
                "i",
                &[Pattern::NatSum(&[W, V])],
            )),
            Port::Optional(Pattern::Constructor("builtin", "i", &[V])),
        ],
        outputs: &[],
    };
    let ctx = Context::with_default_dialects();
    let terms = ctx.terms();
    let resolve = |later| {
        signature.substitution(
            terms,
            &[Argument::Fixed(terms.int(13)), Argument::Optional(later)],
            &[],
        )
    };

    assert_eq!(
        resolve(Some(terms.int(8))),
        Some(vec![Some(TypeArg::Nat(5)), Some(TypeArg::Nat(8))])
    );
    assert_eq!(resolve(None), None);
    assert_eq!(resolve(Some(terms.int(14))), None);
}
