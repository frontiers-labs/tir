//! Builtin integer arithmetic: sem-derived folding, costs and sem-expr shapes.

use tir::{
    builtin::{ops, IntegerType},
    graph::{Dag, MetaDag},
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

fn check_binary_sem(g: &SemGraph, root: tir::graph::NodeId, expected_kind: SymKind) {
    assert_eq!(g.len(), 3, "expected 3 nodes: lhs symbol, rhs symbol, op");
    assert_eq!(g.get_kind(root), &expected_kind);
    let children: Vec<_> = g.children(root).collect();
    assert_eq!(children.len(), 2);
    assert_eq!(g.get_kind(children[0]), &SymKind::Symbol);
    assert_eq!(g.get_kind(children[1]), &SymKind::Symbol);
    assert!(
        matches!(g.get_leaf_data(children[0]), Some(SymPayload::SymbolId(0))),
        "lhs should be symbol 0"
    );
    assert!(
        matches!(g.get_leaf_data(children[1]), Some(SymPayload::SymbolId(1))),
        "rhs should be symbol 1"
    );
}

/// Every binary integer op converts to a two-symbol sem expression rooted at
/// its own kind, carrying the originating op and its result type.
#[test]
fn binary_ops_convert_to_their_sem_kind() {
    type Convert = fn(
        &Context,
        tir::ValueId,
        tir::ValueId,
        tir::TypeId,
        &mut SemGraph,
    ) -> (tir::graph::NodeId, tir::OpId);

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
        assert_eq!(g.get_original_op(root), Some(op_id), "{name}");
        assert_eq!(g.get_actual_type(root), Some(i32_ty), "{name}");
    }
}

/// The width-changing ops take their width from the result type via the unary
/// sem-DSL forms: `extsi -> SExt(x, W)`, `extui -> ZExt(x, W)`,
/// `trunci -> Extract(x, W-1, 0)`.
fn const_value(g: &SemGraph, node: tir::graph::NodeId) -> u64 {
    match g.get_leaf_data(node) {
        Some(SymPayload::Int(v)) => v.to_u64(),
        other => panic!("expected an integer constant, got {other:?}"),
    }
}

#[test]
fn extsi_sem_expr_uses_result_width() {
    let context = Context::with_default_dialects();
    let input = context.create_value(IntegerType::new(&context, 16), None);
    let op = ops::extsi(&context, input.id(), IntegerType::new(&context, 64)).build();
    let mut g = SemGraph::new();
    let root = op.convert(&mut g);
    assert_eq!(g.get_kind(root), &SymKind::SExt);
    let children: Vec<_> = g.children(root).collect();
    assert_eq!(g.get_kind(children[0]), &SymKind::Symbol);
    assert_eq!(const_value(&g, children[1]), 64);
}

#[test]
fn extui_sem_expr_uses_result_width() {
    let context = Context::with_default_dialects();
    let input = context.create_value(IntegerType::new(&context, 8), None);
    let op = ops::extui(&context, input.id(), IntegerType::new(&context, 32)).build();
    let mut g = SemGraph::new();
    let root = op.convert(&mut g);
    assert_eq!(g.get_kind(root), &SymKind::ZExt);
    assert_eq!(const_value(&g, g.children(root).nth(1).unwrap()), 32);
}

#[test]
fn trunci_sem_expr_is_low_bit_extract() {
    let context = Context::with_default_dialects();
    let input = context.create_value(IntegerType::new(&context, 64), None);
    let op = ops::trunci(&context, input.id(), IntegerType::new(&context, 16)).build();
    let mut g = SemGraph::new();
    let root = op.convert(&mut g);
    assert_eq!(g.get_kind(root), &SymKind::Extract);
    let children: Vec<_> = g.children(root).collect();
    assert_eq!(g.get_kind(children[0]), &SymKind::Symbol);
    assert_eq!(const_value(&g, children[1]), 15); // high = W - 1
    assert_eq!(const_value(&g, children[2]), 0); // low
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
            "#data_layout = {{types = {{p = {{abi = 64, size = 64}}}}}}\n\
             module {{data_layout = #data_layout}} {{\n\
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

    /// The first operation of the function the fixture defines.
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
            r#"%3 = cmpf %1, %1 {predicate = "bogus"} : !i1"#,
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
