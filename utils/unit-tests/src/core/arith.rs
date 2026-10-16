//! Builtin integer arithmetic: sem-derived folding, costs and sem-expr shapes.

use tir::{
    builtin::{ops, IntegerType},
    sem::{AsSemExpr, SemGraph, SymKind, SymPayload},
    Context, Operation,
};

#[test]
fn constant_fold_derived_from_sem() {
    use tir::sem::Value;
    use tir::ConstantFold;
    use tir_adt::APInt;

    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let a = context.create_value(i32_ty, None);
    let b = context.create_value(i32_ty, None);
    let op = ops::addi(&context, a.id(), b.id(), i32_ty).build();

    let fold = context
        .get_op(op.id())
        .as_interface::<dyn ConstantFold>()
        .expect("addi derives ConstantFold from its sem");
    let folded = fold
        .fold(&[
            Value::Int(APInt::new_signed(32, 2)),
            Value::Int(APInt::new_signed(32, 3)),
        ])
        .expect("folds two constants");
    match folded {
        Value::Int(v) => assert_eq!(v.to_i64(), 5),
        other => panic!("expected an integer, got {other:?}"),
    }
}

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

fn check_binary_sem(g: &SemGraph, root: tir::NodeId, expected_kind: SymKind) {
    assert_eq!(g.len(), 3, "expected 3 nodes: lhs symbol, rhs symbol, op");
    assert_eq!(g.get_node(root), &expected_kind);
    let children: Vec<_> = g.children(root).collect();
    assert_eq!(children.len(), 2);
    assert_eq!(g.get_node(children[0]), &SymKind::Symbol);
    assert_eq!(g.get_node(children[1]), &SymKind::Symbol);
    assert!(
        matches!(g.get_leaf_data(children[0]), Some(SymPayload::SymbolId(0))),
        "lhs should be symbol 0"
    );
    assert!(
        matches!(g.get_leaf_data(children[1]), Some(SymPayload::SymbolId(1))),
        "rhs should be symbol 1"
    );
}

#[test]
fn binary_ops_convert_to_their_sem_kind() {
    type Convert = fn(
        &Context,
        tir::ValueId,
        tir::ValueId,
        tir::TypeId,
        &mut SemGraph,
    ) -> (tir::NodeId, tir::OpId);

    macro_rules! converter {
        ($name:ident) => {
            (
                (|context: &Context, lhs, rhs, ty, g: &mut SemGraph| {
                    let op = ops::$name(context, lhs, rhs, ty).build();
                    (op.convert(g), op.id())
                }) as Convert,
                stringify!($name),
            )
        };
    }

    let cases: &[((Convert, &str), SymKind)] = &[
        (converter!(addi), SymKind::Add),
        (converter!(subi), SymKind::Sub),
        (converter!(muli), SymKind::Mul),
        (converter!(andi), SymKind::And),
        (converter!(ori), SymKind::Or),
        (converter!(xori), SymKind::Xor),
        (converter!(shli), SymKind::ShiftLeft),
        (converter!(shrui), SymKind::ShiftRightLogic),
        (converter!(shrsi), SymKind::ShiftRightArithmetic),
    ];

    for ((convert, name), kind) in cases {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let lhs = context.create_value(i32_ty, None);
        let rhs = context.create_value(i32_ty, None);
        let mut g = SemGraph::new();
        let (root, op_id) = convert(&context, lhs.id(), rhs.id(), i32_ty, &mut g);
        check_binary_sem(&g, root, *kind);
        assert_eq!(
            g.get_annotation(root).and_then(|m| m.original_op),
            Some(op_id),
            "{name}"
        );
        assert_eq!(
            g.get_annotation(root).and_then(|m| m.actual_type),
            Some(i32_ty),
            "{name}"
        );
    }
}

// The cast's sem root and each child's kind with its constant value, if any.
fn cast_sem<O: AsSemExpr>(
    from: u64,
    cast: impl FnOnce(&Context, tir::ValueId) -> O,
) -> (SymKind, Vec<(SymKind, Option<u64>)>) {
    let context = Context::with_default_dialects();
    let input = context.create_value(IntegerType::new(&context, from), None);
    let mut g = SemGraph::new();
    let root = cast(&context, input.id()).convert(&mut g);
    let children = g
        .children(root)
        .map(|child| match g.get_leaf_data(child) {
            Some(SymPayload::Int(v)) => (*g.get_node(child), Some(v.to_u64())),
            _ => (*g.get_node(child), None),
        })
        .collect();
    (*g.get_node(root), children)
}

#[test]
fn extsi_sem_expr_uses_result_width() {
    assert_eq!(
        cast_sem(16, |c, input| ops::extsi(c, input, IntegerType::new(c, 64))
            .build()),
        (
            SymKind::SExt,
            vec![(SymKind::Symbol, None), (SymKind::Constant, Some(64))]
        )
    );
}

#[test]
fn extui_sem_expr_uses_result_width() {
    assert_eq!(
        cast_sem(8, |c, input| ops::extui(c, input, IntegerType::new(c, 32))
            .build()),
        (
            SymKind::ZExt,
            vec![(SymKind::Symbol, None), (SymKind::Constant, Some(32))]
        )
    );
}

#[test]
fn trunci_sem_expr_is_low_bit_extract() {
    // Extract(x, high = W - 1, low = 0).
    assert_eq!(
        cast_sem(64, |c, input| ops::trunci(
            c,
            input,
            IntegerType::new(c, 16)
        )
        .build()),
        (
            SymKind::Extract,
            vec![
                (SymKind::Symbol, None),
                (SymKind::Constant, Some(15)),
                (SymKind::Constant, Some(0))
            ]
        )
    );
}

/// A compare's predicate is a typed attribute: the text parser hands the
/// attribute list a string, and the op's schema decides what it means.
mod predicate {
    use tir::{
        attributes::{AttributeValue, Predicate},
        builtin::ModuleOp,
        parse::ir::parse_ir,
        verify_op_tree, Context, Operation,
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
        let module = parse(&context, body)?;
        verify_op_tree(&context, module.id()).map_err(|error| error.to_string())
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
        assert!(error.contains("cmpi"), "{error}");
        assert!(error.contains("olt"), "{error}");
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
