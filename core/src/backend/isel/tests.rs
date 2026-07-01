use tir::{
    Context, IRBuilder, IRFormatter, Operation, PassManager, TypeId,
    builtin::{FuncOp, IntegerType, ops},
    graph::{MetaMutDag, MutDag, OperandConstraint},
    sem::{SemGraph, SymKind, SymPayload},
};

use super::{
    EmitRequest, InstructionSelectPass, IselCostModel, Rule, RuleMatch, SemEGraph, SemNode,
    extension_rewrite, template_node,
};

fn symbol(g: &mut SemGraph, id: u32) -> tir::graph::NodeId {
    let node = g.add_node(SymKind::Symbol);
    g.set_leaf_data(node, SymPayload::SymbolId(id));
    node
}

fn binary(
    g: &mut SemGraph,
    kind: SymKind,
    lhs: tir::graph::NodeId,
    rhs: tir::graph::NodeId,
) -> tir::graph::NodeId {
    let node = g.add_node(kind);
    g.add_edge(node, lhs);
    g.add_edge(node, rhs);
    node
}

fn atomic_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let lhs = symbol(&mut g, 0);
    let rhs = symbol(&mut g, 1);
    binary(&mut g, kind, lhs, rhs);
    g
}

fn add_mul_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let x = symbol(&mut g, 0);
    let y = symbol(&mut g, 1);
    let mul = binary(&mut g, SymKind::Mul, x, y);
    let z = symbol(&mut g, 2);
    binary(&mut g, SymKind::Add, mul, z);
    g
}

fn emit_add(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let op = req.op.expect("backed by an op");
    let lhs = m
        .value_binding(0)
        .unwrap_or_else(|| op.op().operands.first().copied().unwrap());
    let rhs = m
        .value_binding(2)
        .or_else(|| m.value_binding(1))
        .unwrap_or_else(|| op.op().operands[1]);
    let result_ty = req.result_ty.expect("typed result");
    Ok(Box::new(ops::addi(context, lhs, rhs, result_ty).build()))
}

fn emit_mul(
    context: &Context,
    req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let op = req.op.expect("backed by an op");
    let result_ty = req.result_ty.expect("typed result");
    Ok(Box::new(
        ops::muli(context, op.op().operands[0], op.op().operands[1], result_ty).build(),
    ))
}

#[test]
fn pbqp_selector_consumes_internal_nodes_of_selected_pattern() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let x = context.create_value(i32_ty, None);
    let y = context.create_value(i32_ty, None);
    let z = context.create_value(i32_ty, None);
    let x_id = x.id();
    let y_id = y.id();
    let z_id = z.id();
    let region = context.create_region();
    let block = context.create_block(vec![x, y, z]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    fb.insert(mul);
    let add = ops::addi(&context, mul_result, z_id, i32_ty).build();
    let add_result = add.result();
    fb.insert(add);
    fb.insert(ops::r#return(&context, add_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), 1, emit_add),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new("mul", atomic_pattern(SymKind::Mul), 10, emit_mul),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    assert_eq!(body_ops, vec!["addi", "return"]);

    let mut buf = String::new();
    let mut fmt = IRFormatter::new(&mut buf);
    module.print(&mut fmt).expect("print lowered module");
    assert!(!buf.contains("muli"));
}

#[test]
fn rule_validation_rejects_missing_atomic_materializer() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let x = context.create_value(i32_ty, None);
    let y = context.create_value(i32_ty, None);
    let z = context.create_value(i32_ty, None);
    let x_id = x.id();
    let y_id = y.id();
    let z_id = z.id();
    let region = context.create_region();
    let block = context.create_block(vec![x, y, z]);
    region.add_block(block.id());

    // A standalone Mul that no rule can root and no parent match can consume:
    // the e-graph cover is infeasible, so selection fails naming the kind.
    let _ = z_id;
    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    fb.insert(mul);
    fb.insert(ops::r#return(&context, mul_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add)];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    let err = pm
        .run(&context, context.get_op(module.id()))
        .expect_err("incomplete rule set should be rejected");
    assert!(err.to_string().contains("Mul"));
}

/// A pure subexpression shared by two fused matches is *duplicated*: each
/// add-mul instruction recomputes the mul internally, and the mul op — no
/// longer needed as a register value — is consumed.
#[test]
fn pbqp_selector_duplicates_shared_pure_internal_nodes() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let x = context.create_value(i32_ty, None);
    let y = context.create_value(i32_ty, None);
    let z = context.create_value(i32_ty, None);
    let x_id = x.id();
    let y_id = y.id();
    let z_id = z.id();
    let region = context.create_region();
    let block = context.create_block(vec![x, y, z]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    fb.insert(mul);
    let add0 = ops::addi(&context, mul_result, z_id, i32_ty).build();
    let add0_result = add0.result();
    fb.insert(add0);
    let add1 = ops::addi(&context, mul_result, add0_result, i32_ty).build();
    let add1_result = add1.result();
    fb.insert(add1);
    fb.insert(ops::r#return(&context, add1_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), 1, emit_add),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new("mul", atomic_pattern(SymKind::Mul), 10, emit_mul),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    assert_eq!(body_ops, vec!["addi", "addi", "return"]);
}

/// A shared pure value with a use no match can cover (the return) must stay
/// materialized: the fused match still fires (recomputing the mul), but the
/// mul op itself is emitted rather than consumed.
#[test]
fn shared_value_with_uncoverable_use_stays_materialized() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let x = context.create_value(i32_ty, None);
    let y = context.create_value(i32_ty, None);
    let z = context.create_value(i32_ty, None);
    let x_id = x.id();
    let y_id = y.id();
    let z_id = z.id();
    let region = context.create_region();
    let block = context.create_block(vec![x, y, z]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    fb.insert(mul);
    let add = ops::addi(&context, mul_result, z_id, i32_ty).build();
    fb.insert(add);
    fb.insert(ops::r#return(&context, mul_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), 1, emit_add),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new("mul", atomic_pattern(SymKind::Mul), 10, emit_mul),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    assert_eq!(body_ops, vec!["muli", "addi", "return"]);
}

fn add_mul_add_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let a = symbol(&mut g, 0);
    let b = symbol(&mut g, 1);
    let inner = binary(&mut g, SymKind::Add, a, b);
    let c = symbol(&mut g, 2);
    let mul = binary(&mut g, SymKind::Mul, inner, c);
    let d = symbol(&mut g, 3);
    binary(&mut g, SymKind::Add, mul, d);
    g
}

/// A cost model that makes the fused `add-mul` rule prohibitively expensive,
/// so selection must fall back to the atomic `mul` + `add` cover.
struct NoFusionCostModel;

impl IselCostModel for NoFusionCostModel {
    fn node_cost(
        &self,
        _context: &Context,
        _op: &tir::OperationRef,
        rule: &Rule,
        _m: &RuleMatch,
    ) -> u64 {
        if rule.name == "add-mul" {
            1000
        } else {
            rule.base_cost as u64
        }
    }
}

#[test]
fn cost_model_override_changes_selection() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let x = context.create_value(i32_ty, None);
    let y = context.create_value(i32_ty, None);
    let z = context.create_value(i32_ty, None);
    let (x_id, y_id, z_id) = (x.id(), y.id(), z.id());
    let region = context.create_region();
    let block = context.create_block(vec![x, y, z]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    fb.insert(mul);
    let add = ops::addi(&context, mul_result, z_id, i32_ty).build();
    let add_result = add.result();
    fb.insert(add);
    fb.insert(ops::r#return(&context, add_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), 1, emit_add),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new("mul", atomic_pattern(SymKind::Mul), 10, emit_mul),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules).with_cost_model(Box::new(NoFusionCostModel)));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    // With fusion priced out, the default add-mul cost-1 win is overridden.
    assert_eq!(body_ops, vec!["muli", "addi", "return"]);
}

#[test]
fn composite_rule_falls_back_to_atomic_cover() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let a = context.create_value(i32_ty, None);
    let b = context.create_value(i32_ty, None);
    let c = context.create_value(i32_ty, None);
    let d = context.create_value(i32_ty, None);
    let (a_id, b_id, c_id, d_id) = (a.id(), b.id(), c.id(), d.id());
    let region = context.create_region();
    let block = context.create_block(vec![a, b, c, d]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let add0 = ops::addi(&context, a_id, b_id, i32_ty).build();
    let add0_result = add0.result();
    fb.insert(add0);
    let mul = ops::muli(&context, add0_result, c_id, i32_ty).build();
    let mul_result = mul.result();
    fb.insert(mul);
    let add1 = ops::addi(&context, mul_result, d_id, i32_ty).build();
    let add1_result = add1.result();
    fb.insert(add1);
    fb.insert(ops::r#return(&context, add1_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    // `add-mul-add` requires a `Mul(Add(_,_),_)` subpattern that no rule
    // provides; the pass synthesizes it. Selection must remain valid and, with
    // fusion priced high, fall back to the atomic cover.
    let rules = vec![
        Rule::new("add-mul-add", add_mul_add_pattern(), 100, emit_add),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new("mul", atomic_pattern(SymKind::Mul), 10, emit_mul),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    assert_eq!(body_ops, vec!["addi", "muli", "addi", "return"]);
}

/// A binary pattern constrained to a specific result type via the pattern
/// graph's actual-type annotation (the channel a typed rule would use).
fn typed_binary_pattern(kind: SymKind, ty: TypeId) -> SemGraph {
    let mut g = SemGraph::new();
    let lhs = symbol(&mut g, 0);
    let rhs = symbol(&mut g, 1);
    let root = binary(&mut g, kind, lhs, rhs);
    g.set_actual_type(root, ty);
    g
}

fn emit_sub(
    context: &Context,
    req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let op = req.op.expect("backed by an op");
    let result_ty = req.result_ty.expect("typed result");
    Ok(Box::new(
        ops::subi(context, op.op().operands[0], op.op().operands[1], result_ty).build(),
    ))
}

#[test]
fn selection_is_type_aware() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let i64_ty = IntegerType::new(&context, 64);
    let a32v = context.create_value(i32_ty, None);
    let b32v = context.create_value(i32_ty, None);
    let a64v = context.create_value(i64_ty, None);
    let b64v = context.create_value(i64_ty, None);
    let (a32, b32, a64, b64) = (a32v.id(), b32v.id(), a64v.id(), b64v.id());
    let region = context.create_region();
    let block = context.create_block(vec![a32v, b32v, a64v, b64v]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i64_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let add32 = ops::addi(&context, a32, b32, i32_ty).build();
    fb.insert(add32);
    let add64 = ops::addi(&context, a64, b64, i64_ty).build();
    let add64_result = add64.result();
    fb.insert(add64);
    fb.insert(ops::r#return(&context, add64_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    // Same opcode (Add), two result widths. The i32-constrained rule must only
    // fire on the i32 add; the i64 add falls back to the width-agnostic rule.
    let rules = vec![
        Rule::new(
            "add.i32",
            typed_binary_pattern(SymKind::Add, i32_ty),
            1,
            emit_sub,
        ),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    // i32 add -> the type-constrained rule (subi stand-in); i64 add -> fallback addi.
    assert_eq!(body_ops, vec!["subi", "addi", "return"]);
}

/// Build `add(add(a,b), c)` over i32 values and select it with a fused
/// `Add(Add(_,_),_)` rule whose *internal* node carries `inner_width` as a type
/// constraint (plus an untyped atomic `add` fallback). Returns the lowered op
/// names. Fusion (the `subi` marker) only happens when the inner constraint
/// agrees with the inferred i32 type of the inner add.
fn run_inner_typed_fusion(inner_width: Option<u32>) -> Vec<&'static str> {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let a = context.create_value(i32_ty, None);
    let b = context.create_value(i32_ty, None);
    let c = context.create_value(i32_ty, None);
    let (a_id, b_id, c_id) = (a.id(), b.id(), c.id());
    let region = context.create_region();
    let block = context.create_block(vec![a, b, c]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let add0 = ops::addi(&context, a_id, b_id, i32_ty).build();
    let add0_result = add0.result();
    fb.insert(add0);
    let add1 = ops::addi(&context, add0_result, c_id, i32_ty).build();
    let add1_result = add1.result();
    fb.insert(add1);
    fb.insert(ops::r#return(&context, add1_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    // Fused pattern Add(Add(s0, s1), s2); optionally constrain the inner Add.
    let mut pattern = SemGraph::new();
    let s0 = symbol(&mut pattern, 0);
    let s1 = symbol(&mut pattern, 1);
    let inner = binary(&mut pattern, SymKind::Add, s0, s1);
    let s2 = symbol(&mut pattern, 2);
    binary(&mut pattern, SymKind::Add, inner, s2);
    if let Some(width) = inner_width {
        pattern.set_actual_type(inner, IntegerType::new(&context, width));
    }

    let rules = vec![
        Rule::new("add-add", pattern, 1, emit_sub),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect()
}

#[test]
fn internal_node_type_constraint_is_enforced() {
    // Inner add inferred as i32 from i32 operands. A matching i32 constraint
    // (or no constraint) lets the fused rule consume it; an i64 constraint
    // forbids the match, falling back to two atomic adds.
    assert_eq!(run_inner_typed_fusion(Some(32)), vec!["subi", "return"]);
    assert_eq!(run_inner_typed_fusion(None), vec!["subi", "return"]);
    assert_eq!(
        run_inner_typed_fusion(Some(64)),
        vec!["addi", "addi", "return"]
    );
}

/// The square problem: a sub-word sign extension has no single RISC-V base
/// instruction. Equality saturation with the discovered `SExt -> slli/srai`
/// bridge must make the `SExt(v@i16, 64)` class selectable as an arithmetic
/// shift by `W - n = 48`, exactly the `srai` of the `add, slli, srai` idiom.
#[test]
fn saturation_bridges_sign_extension_to_shift_pair() {
    use super::SaturationLimits;
    use tir::graph::{OperandConstraint, Pattern, PatternExpr};
    use tir::sem::{SymKind, SymPayload};
    use tir_adt::APInt;

    let ctx = Context::with_default_dialects();
    let i16 = IntegerType::new(&ctx, 16);
    let i64 = IntegerType::new(&ctx, 64);

    // SExt(v @ i16, 64), typed i64 — the program graph node no RV64 base
    // instruction can root.
    let mut egraph = SemEGraph::new();
    let v = egraph.add(template_node(
        SymKind::Symbol,
        Some(SymPayload::SymbolId(0)),
        Some(i16),
    ));
    let width = egraph.add(template_node(
        SymKind::Constant,
        Some(SymPayload::Int(APInt::new(64, 64))),
        None,
    ));
    let mut sext_node = template_node(SymKind::SExt, None, Some(i64));
    sext_node.children = vec![v, width];
    let sext = egraph.add(sext_node);

    let rewrite = extension_rewrite(SymKind::SExt, SymKind::ShiftRightArithmetic);
    super::rewrites::saturate(
        &ctx,
        &mut egraph,
        std::slice::from_ref(&rewrite),
        SaturationLimits::default(),
    );

    // The sext class now also contains the shift-pair realization.
    assert!(
        egraph
            .nodes(sext)
            .iter()
            .any(|n| n.kind == SymKind::ShiftRightArithmetic),
        "saturation should add the arithmetic-shift bridge to the sext class"
    );

    // An immediate `srai` pattern matches the class, with shift amount 64-16=48.
    let mut srai = Pattern::<SemNode, ()>::new(());
    let rs1 = srai.add_node(PatternExpr::Boundary);
    srai.set_duplicable(rs1, true);
    let imm = srai.add_node(PatternExpr::Boundary);
    srai.set_duplicable(imm, true);
    srai.set_operand_constraint(imm, OperandConstraint::Immediate);
    let root = srai.add_node(PatternExpr::Node(template_node(
        SymKind::ShiftRightArithmetic,
        None,
        None,
    )));
    srai.add_edge(root, rs1);
    srai.add_edge(root, imm);
    srai.set_root(root);

    let matches = super::ematch::ematch(&egraph, &ctx, &srai);
    let m = matches
        .iter()
        .find(|m| egraph.find(m.root()) == egraph.find(sext))
        .expect("an immediate srai must match the sext class after saturation");
    let shift_amount = egraph
        .nodes(m.binding(imm))
        .iter()
        .find_map(|n| match n.payload.as_ref() {
            Some(super::SemPayload::Expr(SymPayload::Int(v))) => Some(v.to_u64()),
            _ => None,
        })
        .expect("the srai shift amount must be a constant");
    assert_eq!(shift_amount, 48);
}

fn shift_imm_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let rs1 = symbol(&mut g, 0);
    let imm = symbol(&mut g, 1);
    binary(&mut g, kind, rs1, imm);
    g
}

fn emit_shift_marker(
    marker: SymKind,
) -> impl Fn(&Context, &EmitRequest, &RuleMatch) -> Result<Box<dyn Operation>, tir::PassError> {
    move |context, req, m| {
        let rs1 = m
            .value_binding(0)
            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
        let result_ty = req.result_ty.expect("typed result");
        // The shift amount is an immediate (m.int_binding(1)); operands beyond the
        // mnemonic don't matter for this test, so the source register is reused.
        let built: Box<dyn Operation> = match marker {
            SymKind::ShiftLeft => Box::new(ops::shli(context, rs1, rs1, result_ty).build()),
            _ => Box::new(ops::shrsi(context, rs1, rs1, result_ty).build()),
        };
        Ok(built)
    }
}

fn emit_slli(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    emit_shift_marker(SymKind::ShiftLeft)(context, req, m)
}

fn emit_srai(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    emit_shift_marker(SymKind::ShiftRightArithmetic)(context, req, m)
}

/// End-to-end square: `extsi(addi(a, b) : i16) : i64` lowers to `add, slli, srai`.
/// The `add` covers the addi; saturation bridges the un-selectable sign extension
/// into a `slli`/`srai` pair, and multi-instruction emission materializes the
/// introduced `slli` (an e-class with no original op) before the `srai`.
#[test]
fn square_sign_extension_lowers_to_shift_pair() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i16_ty = IntegerType::new(&context, 16);
    let i64_ty = IntegerType::new(&context, 64);
    let a = context.create_value(i16_ty, None);
    let b = context.create_value(i16_ty, None);
    let (a_id, b_id) = (a.id(), b.id());
    let region = context.create_region();
    let block = context.create_block(vec![a, b]);
    region.add_block(block.id());

    let func = ops::func(&context, "square", i64_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let add = ops::addi(&context, a_id, b_id, i16_ty).build();
    let add_result = add.result();
    fb.insert(add);
    let ext = ops::extsi(&context, add_result, i64_ty).build();
    let ext_result = ext.result();
    fb.insert(ext);
    fb.insert(ops::r#return(&context, ext_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add", atomic_pattern(SymKind::Add), 1, emit_add),
        Rule::new("slli", shift_imm_pattern(SymKind::ShiftLeft), 1, emit_slli)
            .with_operand_constraints(vec![(1, OperandConstraint::Immediate)]),
        Rule::new(
            "srai",
            shift_imm_pattern(SymKind::ShiftRightArithmetic),
            1,
            emit_srai,
        )
        .with_operand_constraints(vec![(1, OperandConstraint::Immediate)]),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("square should select");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    // add (from the addi), then the slli/srai sign-extension idiom, then return.
    assert_eq!(body_ops, vec!["addi", "shli", "shrsi", "return"]);
}

/// Opaque leaves stand for *unknown* computations: two of them must never
/// hash-cons into the same e-class, or unrelated un-lowerable expressions
/// would be treated as equal.
#[test]
fn opaque_leaves_are_distinct() {
    use super::builder::SemDagBuilder;
    use std::collections::HashMap;

    let context = Context::with_default_dialects();
    let value_to_def = HashMap::new();
    let mut egraph = SemEGraph::new();
    let mut builder = SemDagBuilder::new(&context, &value_to_def, &mut egraph);
    let a = builder.add_opaque();
    let b = builder.add_opaque();
    assert_ne!(egraph.find(a), egraph.find(b));
}

/// A multi-operand pattern node (LoadMemory/StoreMemory shapes).
fn nary(g: &mut SemGraph, kind: SymKind, children: &[tir::graph::NodeId]) -> tir::graph::NodeId {
    let node = g.add_node(kind);
    for &child in children {
        g.add_edge(node, child);
    }
    node
}

/// `LoadMemory(Add(base, offset), bytes, metadata)` — the shape the builder
/// gives a zero-offset load, with every operand a boundary.
fn load_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let base = symbol(&mut g, 0);
    let offset = symbol(&mut g, 1);
    let addr = nary(&mut g, SymKind::Add, &[base, offset]);
    let bytes = symbol(&mut g, 3);
    let metadata = symbol(&mut g, 4);
    nary(&mut g, SymKind::LoadMemory, &[addr, bytes, metadata]);
    g
}

/// `StoreMemory(Add(base, offset), bytes, value, addrspace)`.
fn store_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let base = symbol(&mut g, 0);
    let offset = symbol(&mut g, 1);
    let addr = nary(&mut g, SymKind::Add, &[base, offset]);
    let bytes = symbol(&mut g, 3);
    let value = symbol(&mut g, 4);
    let addrspace = symbol(&mut g, 5);
    nary(
        &mut g,
        SymKind::StoreMemory,
        &[addr, bytes, value, addrspace],
    );
    g
}

fn emit_load_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let base = m
        .value_binding(0)
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(Box::new(ops::shli(context, base, base, result_ty).build()))
}

fn emit_store_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let value = m
        .value_binding(4)
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let result_ty = context.get_value(value).ty();
    Ok(Box::new(
        ops::muli(context, value, value, result_ty).build(),
    ))
}

/// Memory lowering is driven purely by the `MemoryRead`/`MemoryWrite` interfaces:
/// a `ptr.store` and a `ptr.load` of the same slot must lower to the target's
/// store/load patterns with the base pointer and stored value bound as operands.
/// The same-slot case also guards the addressing-wrapper uniqueness: were the
/// synthetic `addr + sext(0)` nodes shared, no block with two memory ops could
/// be covered at all.
#[test]
fn memory_ops_select_via_interfaces() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let param = context.create_value(i32_ty, None);
    let param_id = param.id();
    let region = context.create_region();
    let block = context.create_block(vec![param]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let slot_ty = tir::ptr::PtrType::typed(&context, i32_ty);
    let slot = fb.insert(tir::ptr::ops::alloca(&context, slot_ty).build());
    fb.insert(tir::ptr::ops::store(&context, param_id, slot.result()).build());
    let loaded = fb.insert(tir::ptr::ops::load(&context, slot.result(), i32_ty).build());
    fb.insert(ops::r#return(&context, loaded.result()).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("load", load_pattern(), 1, emit_load_marker),
        Rule::new("store", store_pattern(), 1, emit_store_marker),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("memory ops should select through their interfaces");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    // store -> muli marker, load -> shli marker; the alloca is untouched.
    assert_eq!(body_ops, vec!["alloca", "muli", "shli", "return"]);
}

/// When a rewrite proves two op results equal (their e-classes merge), operand
/// resolution must deterministically pick the earliest definition, and every
/// merged op must still receive a selection decision.
#[test]
fn merged_value_classes_resolve_to_earliest_def() {
    use super::{EMatch, IselRewrite};
    use tir::graph::{Pattern, PatternExpr};

    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let x = context.create_value(i32_ty, None);
    let y = context.create_value(i32_ty, None);
    let z = context.create_value(i32_ty, None);
    let (x_id, y_id, z_id) = (x.id(), y.id(), z.id());
    let region = context.create_region();
    let block = context.create_block(vec![x, y, z]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    fb.insert(mul);
    let add = ops::addi(&context, x_id, y_id, i32_ty).build();
    let add_result = add.result();
    fb.insert(add);
    let sub = ops::subi(&context, add_result, z_id, i32_ty).build();
    let sub_result = sub.result();
    fb.insert(sub);
    fb.insert(ops::r#return(&context, sub_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    // A test-only "proof" that x*y == x+y: union the Mul class with the Add
    // class, exactly the shape a discovered algebraic bridge produces.
    let mut searcher = Pattern::<SemNode, ()>::new(());
    let lhs = searcher.add_node(PatternExpr::Boundary);
    searcher.set_duplicable(lhs, true);
    let rhs = searcher.add_node(PatternExpr::Boundary);
    searcher.set_duplicable(rhs, true);
    let root = searcher.add_node(PatternExpr::Node(template_node(SymKind::Mul, None, None)));
    searcher.add_edge(root, lhs);
    searcher.add_edge(root, rhs);
    searcher.set_root(root);
    let union_mul_add = IselRewrite {
        name: "mul-equals-add".to_string(),
        searcher,
        apply: Box::new(|_ctx: &Context, egraph: &mut SemEGraph, m: &EMatch| {
            let add_class = egraph
                .classes()
                .find(|class| class.nodes().iter().any(|n| n.kind == SymKind::Add))
                .map(|class| class.id());
            if let Some(add_class) = add_class {
                egraph.union(m.root(), add_class);
            }
        }),
    };

    fn emit_sub_bound(
        context: &Context,
        req: &EmitRequest,
        m: &RuleMatch,
    ) -> Result<Box<dyn Operation>, tir::PassError> {
        let lhs = m
            .value_binding(0)
            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
        let rhs = m
            .value_binding(1)
            .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
        let result_ty = req.result_ty.expect("typed result");
        Ok(Box::new(ops::subi(context, lhs, rhs, result_ty).build()))
    }

    let rules = vec![
        Rule::new("mul", atomic_pattern(SymKind::Mul), 1, emit_mul),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new("sub", atomic_pattern(SymKind::Sub), 1, emit_sub_bound),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules).with_rewrites(vec![union_mul_add]));
    pm.run(&context, context.get_op(module.id()))
        .expect("merged classes should still select");

    let block_ref = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap();
    let body: Vec<_> = block_ref
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id))
        .collect();
    // Both the mul and the (merged) add lower through the cheaper mul rule.
    let names: Vec<_> = body.iter().map(|op| op.name).collect();
    assert_eq!(names, vec!["muli", "muli", "subi", "return"]);

    // The sub operand resolves to the *earliest* definition of the merged class
    // (the mul result, not the add result); `replace_op` then remapped it to the
    // result of the muli that replaced the original mul.
    let sub_op = &body[2];
    assert_eq!(sub_op.operands[0], body[0].results[0]);
}

/// At *equal* cost, the type-constrained rule must win the tie via dominance
/// pruning — specificity never reaches the PBQP objective.
#[test]
fn equal_cost_tie_breaks_to_more_specific_rule() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i32_ty = IntegerType::new(&context, 32);
    let a = context.create_value(i32_ty, None);
    let b = context.create_value(i32_ty, None);
    let (a_id, b_id) = (a.id(), b.id());
    let region = context.create_region();
    let block = context.create_block(vec![a, b]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let add = ops::addi(&context, a_id, b_id, i32_ty).build();
    let add_result = add.result();
    fb.insert(add);
    fb.insert(ops::r#return(&context, add_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    // Same opcode, same cost; only the type constraint differs. The typed rule
    // (subi marker) must be selected.
    let rules = vec![
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
        Rule::new(
            "add.i32",
            typed_binary_pattern(SymKind::Add, i32_ty),
            10,
            emit_sub,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("pass pipeline should succeed");

    let body_ops: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name)
        .collect();
    assert_eq!(body_ops, vec!["subi", "return"]);
}
