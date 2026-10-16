//! SemNode e-graph identity and the serialized sem-op blob format.

use tir::sem::node::{field, int_carrier};
use tir::sem::{
    decode_sem_ops, int_payload, template_node, ExtendSemBytes, IrOp, Kind, Prov, SemBlobBuilder,
    SemGraph, SemNode, SemOp, SemPayload, SemPayloadDesc, SymKind, ANY_FORM, ANY_STATE,
};
use tir::{attributes::NamedAttribute, Context, TypeId};
use tir_adt::APInt;
use tir_egraph::{Atom, ClassId, Guard, HeadOp, LabelFill, NoExterns, Plan, Query, Rule, Source};
use tir_egraph::{ClassId as Id, Engine, Ref, Vocabulary};
use tir_relational::{Expr, Words};

/// A one-level query over `template`, binding its two operands.
fn addi_plan(template: SemNode) -> Plan<SemNode> {
    Plan::compile(Query::tree(
        3,
        0,
        vec![Atom::Node {
            template,
            args: smallvec::smallvec![1, 2],
            class: 0,
            row: None,
        }],
    ))
}

fn konst(width: u32, value: u64) -> SemNode {
    SemNode::constant(APInt::new(u64::from(width), value), Prov::None)
}

// A graph whose labels are ordered through a context's names, and the
// context its types are interned in.
fn graph() -> (Context, Engine<SemNode>) {
    let context = Context::with_default_dialects();
    let graph = Engine::over(Vocabulary::new(context.names()));
    (context, graph)
}

fn ty(context: &Context, bits: u32) -> TypeId {
    tir::builtin::IntegerType::new(context, u64::from(bits))
}

fn op(name: &'static str, ty: TypeId, attrs: Vec<NamedAttribute>, args: Vec<Id>) -> SemNode {
    SemNode {
        kind: Kind::Ir(IrOp {
            cost: 1,
            ..IrOp::template("builtin", name, attrs)
        }),
        payload: None,
        ty: Some(ty),
        children: args,
        prov: Prov::Introduced,
        carrier: None,
    }
}

/// `node` over the values `args`, its children standing in for them.
fn over(g: &mut Engine<SemNode>, mut node: SemNode, args: &[Ref]) -> Ref {
    node.children = args.iter().map(|arg| arg.class).collect();
    g.insert(node, args)
}

fn op_pattern(name: &'static str, args: Vec<Id>) -> SemNode {
    op(name, TypeId::STATE, Vec::new(), Vec::new())
        .op_template(args)
        .expect("an op node has an op template")
}

// Exercise the `ENode` hash-cons paths lit can't reach (`cmpi` and constant
// width/bits identity); op result-type identity is covered by type_in_key.tir.

#[test]
fn equal_constants_share_a_class_distinct_widths_do_not() {
    let (_, mut g) = graph();
    let a = g.add(konst(32, 0));
    let b = g.add(konst(32, 0));
    let c = g.add(konst(64, 0));
    assert_eq!(g.find(a), g.find(b));
    assert_ne!(g.find(a), g.find(c));
}

// Ops differing only in a value attribute must not hash-cons; identical ones must.
#[test]
fn ops_differing_in_attributes_stay_distinct() {
    let (context, mut g) = graph();
    let x = g.add(konst(32, 0));
    let cmpi = |pred: tir::attributes::Predicate, args: Vec<Id>| {
        let attrs = vec![context.named_attribute(
            "predicate",
            tir::attributes::AttributeValue::Predicate(pred),
        )];
        op("cmpi", ty(&context, 1), attrs, args)
    };
    use tir::attributes::Predicate;
    let slt = over(&mut g, cmpi(Predicate::Slt, Vec::new()), &[x]);
    let sgt = over(&mut g, cmpi(Predicate::Sgt, Vec::new()), &[x]);
    let slt2 = over(&mut g, cmpi(Predicate::Slt, Vec::new()), &[x]);
    assert_ne!(g.find(slt), g.find(sgt));
    assert_eq!(g.find(slt), g.find(slt2));
}

// `addi i32`/`addi i64` share a search bucket (a wildcard visits both) yet must
// stay in distinct classes — merging would miscompile.
#[test]
fn wildcard_search_groups_result_types_without_merging_them() {
    let (context, mut g) = graph();
    let x = g.add(konst(32, 0));
    let addi = |t: TypeId, args: Vec<Id>| op("addi", t, vec![], args);
    let a32 = over(&mut g, addi(ty(&context, 32), Vec::new()), &[x, x]);
    let a64 = over(&mut g, addi(ty(&context, 64), Vec::new()), &[x, x]);

    assert_ne!(g.find(a32), g.find(a64));

    let plan = addi_plan(op_pattern(
        "addi",
        vec![ClassId::from_raw(1), ClassId::from_raw(2)],
    ));
    let found = plan.search(&g, plan.roots(&g), &|_, _| true, false, &NoExterns);
    let roots: std::collections::HashSet<Id> = found.iter().map(|m| g.root(m.root)).collect();
    assert_eq!(roots.len(), 2);
    assert!(roots.contains(&a32.class) && roots.contains(&a64.class));

    // Searching is read-only: classes remain distinct.
    assert_ne!(g.find(a32), g.find(a64));
}

#[test]
fn graph_operation_controls_commutative_matching() {
    let (context, mut g) = graph();
    let zero = g.add(konst(32, 0));
    let x = g.add(konst(32, 1));
    let mut addi = op("addi", ty(&context, 32), vec![], Vec::new());
    let Kind::Ir(ir) = &mut addi.kind else {
        unreachable!()
    };
    ir.commutative = true;
    let root = over(&mut g, addi, &[zero, x]);

    // `addi(?0, 0)`: the literal operand is what the class is known to be, and
    // the commutative flag is what lets it match on either side.
    let plan = Plan::compile(Query::tree(
        3,
        0,
        vec![
            Atom::Node {
                template: op_pattern("addi", vec![ClassId::from_raw(1), ClassId::from_raw(2)]),
                args: smallvec::smallvec![1, 2],
                class: 0,
                row: None,
            },
            Atom::Literal {
                value: konst(32, 0),
                class: 2,
            },
        ],
    ));
    let matches = plan.search(&g, plan.roots(&g), &|_, _| true, false, &NoExterns);
    assert_eq!(matches.len(), 1);
    assert_eq!(g.root(matches[0].root), root.class);
    assert_eq!(matches[0].binding(1), Some(g.find(x)));
}

/// A number spelled typed and untyped is one value: both are its carrier's
/// zero at that offset, so a class proven it either way is proven one thing.
#[test]
fn two_spellings_of_one_constant_are_one_value() {
    let (context, mut g) = graph();
    let untyped = g.add(konst(32, 5));
    let typed = g.add(konst(32, 5).typed(ty(&context, 32)));
    assert_eq!(g.find(untyped), g.find(typed));
    assert_eq!(g.int_const(typed), Some(5));
    assert_eq!(
        g.const_of(typed).as_ref().and_then(SemNode::int),
        Some(&APInt::new(32, 5))
    );
}

fn selected(kind: Kind, arity: usize) -> SemNode {
    SemNode {
        kind,
        payload: None,
        ty: None,
        children: vec![Id::from_raw(0); arity],
        prov: Prov::None,
        carrier: None,
    }
}

// The seed chain: `st(REG)(c)` in `c`.
fn seed(g: &mut Engine<SemNode>, class: Ref) {
    let chain = g.insert(selected(Kind::State(REG), 1), &[class]);
    g.union(chain, class).unwrap();
}

const REG: u32 = 0;

#[test]
fn a_form_is_its_pattern_at_its_arity_and_type() {
    let (context, mut g) = graph();
    let x = g.add(konst(32, 1));
    let y = g.add(konst(64, 1));
    let form = |pattern| selected(Kind::Form(pattern), 2);
    let three = g.insert(form(3), &[x, y]);
    assert_eq!(g.insert(form(3), &[x, y]), three);
    let others = [
        g.insert(form(4), &[x, y]),
        g.insert(form(3).typed(ty(&context, 32)), &[x, y]),
        g.insert(selected(Kind::Form(3), 1), &[x]),
        g.insert(selected(Kind::State(3), 1), &[x]),
    ];
    g.rebuild();
    let classes: std::collections::HashSet<Ref> = others.into_iter().chain([three]).collect();
    assert_eq!(classes.len(), 5);

    // Any form of two operands, its pattern read off the row.
    let plan = Plan::compile(Query {
        vars: 3,
        scalars: 2,
        root: 0,
        atoms: vec![Atom::Node {
            template: form(ANY_FORM),
            args: smallvec::smallvec![1, 2],
            class: 0,
            row: Some(0),
        }],
        guards: vec![Guard::Read {
            term: Source::Row(0),
            field: field::FORM,
            out: 1,
        }],
        nots: Vec::new(),
    });
    let found = plan.search(&g, plan.roots(&g), &|_, _| true, false, &NoExterns);
    let mut patterns: Vec<u64> = found.iter().map(|m| m.scalars[1]).collect();
    patterns.sort_unstable();
    assert_eq!(patterns, vec![3, 3, 4]);
}

fn selectable(node: &SemNode) -> bool {
    !matches!(node.kind, Kind::State(_) | Kind::Form(_))
}

// The one rule a unary row is selected by, whatever its label:
//   op(c) in C, c == b + d, st(T)(v) in b, step(L, T) = P
//   => f := form(P)(v + d), C == st(P)(f)
fn unary_step(step: u32) -> Rule<SemNode> {
    let query = Query {
        vars: 4,
        scalars: 6,
        root: 0,
        atoms: vec![
            Atom::Any {
                accept: selectable,
                args: smallvec::smallvec![1],
                class: 0,
                row: None,
                label: Some(1),
            },
            Atom::Offset {
                key: 1,
                base: 2,
                offset: 2,
            },
            Atom::Node {
                template: selected(Kind::State(ANY_STATE), 1),
                args: smallvec::smallvec![3],
                class: 2,
                row: Some(3),
            },
            Atom::Table {
                table: step,
                terms: vec![Expr::Scalar(1), Expr::Scalar(4), Expr::Scalar(5)],
            },
        ],
        guards: vec![Guard::Read {
            term: Source::Row(3),
            field: field::STATE,
            out: 4,
        }],
        nots: Vec::new(),
    };
    let filled = |kind, field| LabelFill {
        template: selected(kind, 1),
        fills: smallvec::smallvec![(field, 5)],
    };
    Rule {
        name: "step-1".into(),
        plan: Plan::compile(query),
        head: vec![
            HeadOp::Offset {
                base: 3,
                offset: Expr::Scalar(2),
                into: 4,
            },
            HeadOp::Insert {
                label: filled(Kind::Form(ANY_FORM), field::FORM),
                args: smallvec::smallvec![4],
                into: 5,
            },
            HeadOp::Insert {
                label: filled(Kind::State(ANY_STATE), field::STATE),
                args: smallvec::smallvec![5],
                into: 6,
            },
            HeadOp::Union(0, 6),
        ],
        head_vars: 3,
        post_saturation: false,
    }
}

// `not(neg(x + 5))` and `neg(7)` under a step table that covers `neg` and
// `not` of a register, folds a `neg` into the `not` that reads it, and has a
// second, dearer pattern for `neg` of a register.
#[test]
fn one_rule_over_any_label_steps_seeds_through_a_client_table() {
    const NEG: u64 = 1;
    const NOT_OF_NEG: u64 = 2;
    const NOT: u64 = 3;
    const WIDE_NEG: u64 = 4;
    let (context, mut g) = graph();
    let word = int_carrier(32);
    let unary = |name| op(name, ty(&context, 32), Vec::new(), Vec::new()).carried(word);
    let x = g.add(SemNode {
        payload: Some(SemPayload::Opaque(0)),
        ..template_node(SymKind::Symbol, None, Some(ty(&context, 32))).carried(word)
    });
    let seven = g.add(konst(32, 7));
    let neg = over(&mut g, unary("neg"), &[Ref::new(x.class, 5)]);
    let not = over(&mut g, unary("not"), &[neg]);
    let neg_seven = over(&mut g, unary("neg"), &[seven]);
    for class in [x, neg, Ref::from(seven.class)] {
        seed(&mut g, class);
    }
    g.rebuild();

    let label = |g: &Engine<SemNode>, of: Ref| g.label(g.rows(of.class).next().unwrap()).0;
    let mut step = Words::new(3);
    step.push(&[label(&g, neg), REG, NEG as u32]);
    step.push(&[label(&g, not), REG, NOT as u32]);
    step.push(&[label(&g, not), NEG as u32, NOT_OF_NEG as u32]);
    step.push(&[label(&g, neg), REG, WIDE_NEG as u32]);
    let rule = unary_step(g.add_words(std::sync::Arc::new(step)));
    g.saturate_rules(&[rule], &NoExterns, 8, usize::MAX);

    let chain = |g: &Engine<SemNode>, pattern: u64, operand: Ref| {
        let form = g.lookup_at(&selected(Kind::Form(pattern as u32), 1), &[operand])?;
        g.lookup_at(&selected(Kind::State(pattern as u32), 1), &[form])
    };
    let neg_form = g
        .lookup_at(
            &selected(Kind::Form(NEG as u32), 1),
            &[Ref::new(x.class, 5)],
        )
        .expect("neg over the register at its offset");
    assert_eq!(chain(&g, NEG, Ref::new(x.class, 5)), Some(g.find(neg)));
    assert_eq!(chain(&g, NEG, seven), Some(g.find(neg_seven)));
    assert_eq!(chain(&g, WIDE_NEG, Ref::new(x.class, 5)), Some(g.find(neg)));
    assert_eq!(chain(&g, WIDE_NEG, seven), Some(g.find(neg_seven)));
    assert_eq!(chain(&g, NOT, neg), Some(g.find(not)));
    assert_eq!(chain(&g, NOT_OF_NEG, neg_form), Some(g.find(not)));
    assert_eq!(g.find(x), x);
    assert!(!g.connected(neg, neg_seven));

    // Accepted chains cost their tree, everything else selected nothing.
    let extraction = g.extract_best(|_, node, _, _| match node.kind {
        Kind::State(REG) | Kind::Ir(_) => u64::MAX,
        Kind::State(pattern) if u64::from(pattern) == WIDE_NEG => 20,
        Kind::State(_) => 10,
        _ => 0,
    });
    let chosen = |of: Ref| {
        extraction
            .node(g.root(of.class))
            .map(|node| node.kind.clone())
    };
    assert_eq!(chosen(not), Some(Kind::State(NOT_OF_NEG as u32)));
    assert_eq!(chosen(neg), Some(Kind::State(NEG as u32)));
    assert_eq!(chosen(x), Some(Kind::Sym(SymKind::Symbol)));
}

fn assert_same_graph(actual: &SemGraph, expected: &SemGraph, root: tir::NodeId) {
    for node in expected.postorder(root) {
        assert_eq!(actual.get_node(node), expected.get_node(node));
        assert_eq!(actual.get_leaf_data(node), expected.get_leaf_data(node));
        assert_eq!(
            actual.children(node).collect::<Vec<_>>(),
            expected.children(node).collect::<Vec<_>>()
        );
    }
}

fn add_program() -> Vec<SemOp> {
    vec![
        SemOp::Node(SymKind::Symbol),
        SemOp::Payload(SemPayloadDesc::SymbolId(0)),
        SemOp::Node(SymKind::Constant),
        SemOp::Payload(SemPayloadDesc::Int {
            width: 32,
            value: 7,
            signed: false,
        }),
        SemOp::Node(SymKind::Add),
        SemOp::Edge(2, 0),
        SemOp::Edge(2, 1),
    ]
}

#[test]
fn extend_sem_bytes_builds_the_same_graph_as_imperative_construction() {
    let mut expected = SemGraph::new();
    let a = expected.add_node(SymKind::Symbol);
    expected.set_leaf_data(a, tir::sem::SymPayload::SymbolId(0));
    let b = expected.add_node(SymKind::Constant);
    expected.set_leaf_data(b, int_payload(32, 7, false));
    let add = expected.add_node(SymKind::Add);
    expected.add_edge(add, a);
    expected.add_edge(add, b);

    let mut builder = SemBlobBuilder::new();
    let offset = builder.intern(&add_program());
    let (blob, kinds) = builder.finish();

    let mut decoded = SemGraph::new();
    let root = decoded.extend_sem_bytes(&kinds, &blob, offset);

    assert_eq!(root, add);
    assert_same_graph(&decoded, &expected, root);
}

#[test]
fn every_op_round_trips_through_the_blob() {
    let ops = vec![
        SemOp::Node(SymKind::Constant),
        SemOp::Payload(SemPayloadDesc::Int {
            width: 12,
            value: (-3i64) as u64,
            signed: true,
        }),
        SemOp::Node(SymKind::Constant),
        SemOp::Payload(SemPayloadDesc::Float(-0.5)),
        SemOp::Node(SymKind::Symbol),
        SemOp::Payload(SemPayloadDesc::Value(9)),
        SemOp::Node(SymKind::FAdd),
        SemOp::Typed(64),
        SemOp::Edge(3, 1),
        SemOp::Edge(3, 2),
    ];

    let mut builder = SemBlobBuilder::new();
    let offset = builder.intern(&ops);
    let (blob, kinds) = builder.finish();

    assert_eq!(
        format!("{:?}", decode_sem_ops(&blob, offset, &kinds)),
        format!("{ops:?}")
    );
}

#[test]
fn identical_programs_share_one_record() {
    let mut builder = SemBlobBuilder::new();
    let first = builder.intern(&add_program());
    let second = builder.intern(&add_program());
    let third = builder.intern(&[SemOp::Node(SymKind::Symbol)]);

    assert_eq!(first, second);
    assert_ne!(first, third);
}
