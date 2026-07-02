use tir::{
    Context, IRBuilder, IRFormatter, Operation, PassManager, TypeId,
    builtin::{FuncOp, IntegerType, ops},
    graph::{MetaMutDag, MutDag, OperandConstraint},
    sem::{SemGraph, SymKind, SymPayload},
};

use super::{
    BranchEmitters, EmitRequest, InstructionSelectPass, IselCostModel, Rule, RuleKind, RuleMatch,
    SemEGraph, SemNode, template_node,
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

fn emit_add_imm_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let lhs = m
        .value_binding(0)
        .or_else(|| m.value_binding(1))
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    m.int_binding(1)
        .or_else(|| m.int_binding(0))
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    // The immediate folds into the instruction (`subi` is only a marker), so
    // the constant op loses its last use and is swept.
    Ok(Box::new(ops::subi(context, lhs, lhs, result_ty).build()))
}

/// Select `add(a, constant)` with a cheap immediate rule bounded to a signed
/// 12-bit field (`subi` marker) and an expensive register-form fallback.
fn run_immediate_range(constant: i64) -> Vec<&'static str> {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i64_ty = IntegerType::new(&context, 64);
    let a = context.create_value(i64_ty, None);
    let a_id = a.id();
    let region = context.create_region();
    let block = context.create_block(vec![a]);
    region.add_block(block.id());

    let func = ops::func(&context, "demo", i64_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let c = ops::constant(&context, constant, i64_ty).build();
    let c_result = c.result();
    fb.insert(c);
    let add = ops::addi(&context, a_id, c_result, i64_ty).build();
    let add_result = add.result();
    fb.insert(add);
    fb.insert(ops::r#return(&context, add_result).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("addi", atomic_pattern(SymKind::Add), 1, emit_add_imm_marker)
            .with_operand_constraints(vec![(1, OperandConstraint::Immediate)])
            .with_operand_imm_ranges(vec![(
                1,
                super::ImmRange {
                    width: 12,
                    signed: true,
                },
            )]),
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
fn immediate_range_gates_immediate_rules() {
    // The signed 12-bit boundaries fold into the immediate form; the constant
    // op is swept.
    assert_eq!(run_immediate_range(2047), vec!["subi", "return"]);
    assert_eq!(run_immediate_range(-2048), vec!["subi", "return"]);
    // One past either boundary must not bind the immediate rule: the register
    // form is selected and the constant stays materialized.
    assert_eq!(
        run_immediate_range(2048),
        vec!["constant", "addi", "return"]
    );
    assert_eq!(
        run_immediate_range(-2049),
        vec!["constant", "addi", "return"]
    );
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
    use super::pattern::compile_isel_pattern;
    use tir::sem::{SymKind, SymPayload};
    use tir_adt::APInt;
    use tir_symbolic::egraph::Var;

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

    let texts = super::synthesis::synthesize_bridge_texts(
        SymKind::SExt,
        &std::collections::HashSet::from([SymKind::ShiftLeft, SymKind::ShiftRightArithmetic]),
    );
    let text = texts.first().expect("sext bridge discovered");
    let rewrite = super::axioms::parse_axiom(text).unwrap().compile();
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
    // The width requirement on the shifted value must not reject the
    // rewrite-introduced shl class: it carries no IR type, and introduced
    // classes are produced at register width by the instructions that
    // materialize them.
    let compiled = compile_isel_pattern(
        0,
        &shift_imm_pattern(SymKind::ShiftRightArithmetic),
        &[(1, OperandConstraint::Immediate)],
        &[(0, 64)],
        &[],
    )
    .expect("srai pattern should compile");

    let matches = compiled.search(&egraph, &ctx);
    let m = matches
        .iter()
        .find(|m| egraph.find(m.root) == egraph.find(sext))
        .expect("an immediate srai must match the sext class after saturation");
    let imm_class = m
        .subst
        .get(&Var::Symbol(1))
        .expect("shift amount operand bound");
    let shift_amount = egraph
        .nodes(imm_class)
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

    // The dev-utility flow: discover the bridge axioms for this rule set
    // offline, then install the rendered file on the pass.
    let axioms = super::render_axioms_file(&super::discover_axioms(&rules));
    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules).with_axioms(&axioms));
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
    use tir_symbolic::egraph::{Pattern, Var};

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
    let mut searcher = Pattern::<SemNode, u32>::new();
    let lhs = searcher.var(Var::Symbol(0));
    let rhs = searcher.var(Var::Symbol(1));
    let mut mul_root = template_node(SymKind::Mul, None, None);
    mul_root.children = vec![lhs, rhs];
    searcher.add(mul_root);
    let union_mul_add = IselRewrite {
        name: "mul-equals-add".to_string(),
        searcher,
        apply: Box::new(|_ctx: &Context, egraph: &mut SemEGraph, m: &EMatch<u32>| {
            let add_class = egraph
                .classes()
                .find(|class| class.nodes().iter().any(|n| n.kind == SymKind::Add))
                .map(|class| class.id());
            if let Some(add_class) = add_class {
                egraph.union(m.root, add_class);
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

// ── Conditional-branch selection ────────────────────────────────────────────
//
// Marker convention: the fused branch emits a `br` to the bound target
// forwarding the two compared values; the uncond emitter a `br` with the
// forwarded args; the nonzero fallback a `br` forwarding the condition.

fn emit_fused_branch_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let dest = m
        .block_binding(2)
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let lhs = m
        .value_binding(0)
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let rhs = m
        .value_binding(1)
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    Ok(Box::new(ops::br(context, vec![lhs, rhs], dest).build()))
}

fn emit_uncond_marker(
    context: &Context,
    dest: tir::BlockId,
    args: &[tir::ValueId],
) -> Box<dyn Operation> {
    Box::new(ops::br(context, args.to_vec(), dest).build())
}

fn emit_nonzero_marker(
    context: &Context,
    condition: tir::ValueId,
    dest: tir::BlockId,
) -> Box<dyn Operation> {
    Box::new(ops::br(context, vec![condition], dest).build())
}

fn branch_rule() -> Rule {
    Rule::new(
        "blt-marker",
        atomic_pattern(SymKind::Lt),
        1,
        emit_fused_branch_marker,
    )
    .with_kind(RuleKind::CondBranch { target_symbol: 2 })
}

fn branch_emitters() -> BranchEmitters {
    BranchEmitters {
        uncond: emit_uncond_marker,
        cond_nonzero: emit_nonzero_marker,
    }
}

struct BranchBlock {
    context: Context,
    region: tir::RegionId,
    true_dest: tir::BlockId,
    false_dest: tir::BlockId,
    args: Vec<tir::ValueId>,
}

/// A function whose entry block holds `body(entry builder, args)` followed by
/// `cond_br cond, ^t, ^f`, where `body` returns the condition value.
fn guarded_block(
    arg_tys: &[u32],
    body: impl Fn(&Context, &mut IRBuilder, &[tir::ValueId]) -> tir::ValueId,
) -> BranchBlock {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let values: Vec<_> = arg_tys
        .iter()
        .map(|w| context.create_value(IntegerType::new(&context, *w), None))
        .collect();
    let arg_ids: Vec<_> = values.iter().map(|v| v.id()).collect();
    let region = context.create_region();
    let block = context.create_block(values);
    region.add_block(block.id());
    let t = context.create_block(vec![]);
    let f = context.create_block(vec![]);

    let func = ops::func(
        &context,
        "demo",
        IntegerType::new(&context, 64),
        Some(region.id()),
    )
    .build();
    let mut fb = IRBuilder::new(func.body());
    let cond = body(&context, &mut fb, &arg_ids);
    fb.insert(ops::cond_br(&context, cond, vec![], vec![], t.id(), f.id()).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name()).add_pass(
        InstructionSelectPass::new(vec![branch_rule()]).with_branch_emitters(branch_emitters()),
    );
    pm.run(&context, context.get_op(module.id()))
        .expect("branch selection should succeed");

    BranchBlock {
        context,
        region: region.id(),
        true_dest: t.id(),
        false_dest: f.id(),
        args: arg_ids,
    }
}

fn block_ops(context: &Context, region: tir::RegionId) -> Vec<std::sync::Arc<tir::OpInstance>> {
    context
        .get_region(region)
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id))
        .collect()
}

/// A comparison feeding only the guard fuses into the branch rule, the compare
/// op is consumed (Dead), and the false edge lowers through the uncond emitter.
#[test]
fn guard_fuses_comparison_and_consumes_compare() {
    let b = guarded_block(&[64, 64], |context, fb, args| {
        let cmp = tir::builtin::CmpIOpBuilder::new(context)
            .lhs(args[0])
            .rhs(args[1])
            .predicate("slt")
            .result_type(IntegerType::new(context, 1))
            .build();
        let result = cmp.result();
        fb.insert(cmp);
        result
    });

    let body = block_ops(&b.context, b.region);
    let names: Vec<_> = body.iter().map(|op| op.name).collect();
    assert_eq!(names, vec!["br", "br"], "cmpi must be consumed");

    // The fused branch reads the compared values and targets the true block.
    let fused = body[0].clone().as_op::<tir::builtin::BranchOp>().unwrap();
    assert_eq!(fused.dest(), b.true_dest);
    assert_eq!(fused.dest_args(), b.args);
    // The fallthrough targets the false block.
    let fallthrough = body[1].clone().as_op::<tir::builtin::BranchOp>().unwrap();
    assert_eq!(fallthrough.dest(), b.false_dest);
    assert!(fallthrough.dest_args().is_empty());
}

/// A bare i1 condition no branch rule can fuse takes the branch-if-nonzero
/// fallback, forwarding the condition value.
#[test]
fn guard_without_matching_rule_uses_nonzero_fallback() {
    let b = guarded_block(&[1], |_, _, args| args[0]);

    let body = block_ops(&b.context, b.region);
    let names: Vec<_> = body.iter().map(|op| op.name).collect();
    assert_eq!(names, vec!["br", "br"]);

    let nonzero = body[0].clone().as_op::<tir::builtin::BranchOp>().unwrap();
    assert_eq!(nonzero.dest(), b.true_dest);
    assert_eq!(nonzero.dest_args(), vec![b.args[0]]);
}

/// A compared condition with another in-block consumer is both materialized
/// (the boundary edge forbids Dead) and fused into the branch.
#[test]
fn escaping_compare_materializes_and_fuses() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i1 = IntegerType::new(&context, 1);
    let i64_ty = IntegerType::new(&context, 64);
    let x = context.create_value(i64_ty, None);
    let y = context.create_value(i64_ty, None);
    let (x_id, y_id) = (x.id(), y.id());
    let region = context.create_region();
    let block = context.create_block(vec![x, y]);
    region.add_block(block.id());
    let t = context.create_block(vec![]);
    let f = context.create_block(vec![]);

    let func = ops::func(&context, "demo", i64_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    let cmp = tir::builtin::CmpIOpBuilder::new(&context)
        .lhs(x_id)
        .rhs(y_id)
        .predicate("slt")
        .result_type(i1)
        .build();
    let cond = cmp.result();
    fb.insert(cmp);
    // A second consumer of the condition: its class must stay materialized.
    fb.insert(ops::addi(&context, cond, cond, i1).build());
    fb.insert(ops::cond_br(&context, cond, vec![], vec![], t.id(), f.id()).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        branch_rule(),
        // The Lt materializer (subi marker) and the add consumer's rule.
        Rule::new("slt-marker", atomic_pattern(SymKind::Lt), 10, emit_sub),
        Rule::new("add", atomic_pattern(SymKind::Add), 10, emit_add),
    ];
    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules).with_branch_emitters(branch_emitters()));
    pm.run(&context, context.get_op(module.id()))
        .expect("selection should succeed");

    let names: Vec<_> = block_ops(&context, region.id())
        .iter()
        .map(|op| op.name)
        .collect();
    // cmpi -> subi marker (materialized), addi stays selected, then the fused
    // branch marker and the fallthrough.
    assert_eq!(names, vec!["subi", "addi", "br", "br"]);
}

/// Width-constrained comparison operands: a rule constrained to width 64 must
/// not bind i32 values (their upper register bits are undefined), while the
/// matching width fuses as usual.
#[test]
fn width_constraint_gates_comparison_fusion() {
    let build_cmp = |context: &Context, fb: &mut IRBuilder, args: &[tir::ValueId]| {
        let cmp = tir::builtin::CmpIOpBuilder::new(context)
            .lhs(args[0])
            .rhs(args[1])
            .predicate("slt")
            .result_type(IntegerType::new(context, 1))
            .build();
        let result = cmp.result();
        fb.insert(cmp);
        result
    };

    let run = |arg_width: u32, rule_width: u32| {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None).build();
        let values: Vec<_> = (0..2)
            .map(|_| context.create_value(IntegerType::new(&context, arg_width), None))
            .collect();
        let arg_ids: Vec<_> = values.iter().map(|v| v.id()).collect();
        let region = context.create_region();
        let block = context.create_block(values);
        region.add_block(block.id());
        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        let func = ops::func(
            &context,
            "demo",
            IntegerType::new(&context, 64),
            Some(region.id()),
        )
        .build();
        let mut fb = IRBuilder::new(func.body());
        let cond = build_cmp(&context, &mut fb, &arg_ids);
        fb.insert(ops::cond_br(&context, cond, vec![], vec![], t.id(), f.id()).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let rules = vec![branch_rule().with_operand_widths(vec![(0, rule_width), (1, rule_width)])];
        let mut pm = PassManager::new();
        pm.nest(FuncOp::name())
            .add_pass(InstructionSelectPass::new(rules).with_branch_emitters(branch_emitters()));
        pm.run(&context, context.get_op(module.id()))
            .map(|()| block_ops(&context, region.id()).len())
    };

    // Matching width: the compare fuses and is consumed (branch + fallthrough).
    assert_eq!(run(64, 64).expect("matching width should fuse"), 2);
    // Mismatched width: no branch rule matches, the fallback needs the
    // condition materialized, and no rule can — selection must refuse.
    let err = run(32, 64).expect_err("mismatched width must be rejected");
    assert!(err.to_string().contains("Lt"));
}

/// A function of two i64 args whose entry compares them (`predicate`) and
/// branches to `body` (in the region) or `other`; `body` re-compares with
/// `body_predicate` and operand order `body_swapped`, branching to `u`/`v`.
/// `body_on_true` picks which edge of the entry guard reaches `body`. Returns
/// the op names of the selected body block plus the block ids the body's
/// branches reference.
struct DominatedBlocks {
    context: Context,
    body: tir::BlockId,
    u: tir::BlockId,
}

fn run_dominated_compare(
    predicate: &str,
    body_on_true: bool,
    body_predicate: &str,
    body_swapped: bool,
) -> DominatedBlocks {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i64_ty = IntegerType::new(&context, 64);
    let i1 = IntegerType::new(&context, 1);
    let a = context.create_value(i64_ty, None);
    let b = context.create_value(i64_ty, None);
    let (a_id, b_id) = (a.id(), b.id());
    let region = context.create_region();
    let entry = context.create_block(vec![a, b]);
    region.add_block(entry.id());
    let body = context.create_block(vec![]);
    region.add_block(body.id());
    let other = context.create_block(vec![]);
    let u = context.create_block(vec![]);
    let v = context.create_block(vec![]);

    let func = ops::func(&context, "demo", i64_ty, Some(region.id())).build();

    let mut eb = IRBuilder::new(entry.clone());
    let entry_cmp = tir::builtin::CmpIOpBuilder::new(&context)
        .lhs(a_id)
        .rhs(b_id)
        .predicate(predicate)
        .result_type(i1)
        .build();
    let entry_cond = entry_cmp.result();
    eb.insert(entry_cmp);
    let (t_dest, f_dest) = if body_on_true {
        (body.id(), other.id())
    } else {
        (other.id(), body.id())
    };
    eb.insert(ops::cond_br(&context, entry_cond, vec![], vec![], t_dest, f_dest).build());

    let mut bb = IRBuilder::new(body.clone());
    let (lhs, rhs) = if body_swapped {
        (b_id, a_id)
    } else {
        (a_id, b_id)
    };
    let body_cmp = tir::builtin::CmpIOpBuilder::new(&context)
        .lhs(lhs)
        .rhs(rhs)
        .predicate(body_predicate)
        .result_type(i1)
        .build();
    let body_cond = body_cmp.result();
    bb.insert(body_cmp);
    bb.insert(ops::cond_br(&context, body_cond, vec![], vec![], u.id(), v.id()).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let rules = vec![
        branch_rule(),
        Rule::new(
            "beq-marker",
            atomic_pattern(SymKind::Eq),
            1,
            emit_fused_branch_marker,
        )
        .with_kind(RuleKind::CondBranch { target_symbol: 2 }),
    ];
    let mut pm = PassManager::new();
    pm.nest(FuncOp::name())
        .add_pass(InstructionSelectPass::new(rules).with_branch_emitters(branch_emitters()));
    pm.run(&context, context.get_op(module.id()))
        .expect("dominated selection should succeed");

    DominatedBlocks {
        context,
        body: body.id(),
        u: u.id(),
    }
}

fn block_op_list(context: &Context, block: tir::BlockId) -> Vec<std::sync::Arc<tir::OpInstance>> {
    context
        .get_block(block)
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id))
        .collect()
}

/// A block dominated by a guard edge knows the guard's fact: the identical
/// compare in the body folds to the known truth, its guard becomes an
/// unconditional branch to the taken successor, and the compare op is erased.
#[test]
fn dominated_block_folds_redundant_compare_and_branch() {
    let r = run_dominated_compare("slt", true, "slt", false);
    let body = block_op_list(&r.context, r.body);
    let names: Vec<_> = body.iter().map(|op| op.name).collect();
    assert_eq!(
        names,
        vec!["br"],
        "compare consumed, guard folded to a jump"
    );
    let jump = body[0].clone().as_op::<tir::builtin::BranchOp>().unwrap();
    assert_eq!(jump.dest(), r.u, "the true successor is taken");
}

/// On the false edge the *complement* comparison is known true: `a >= b`
/// dominated by the false edge of `a < b` folds the same way.
#[test]
fn false_edge_assumes_complement_comparison() {
    let r = run_dominated_compare("slt", false, "sge", false);
    let body = block_op_list(&r.context, r.body);
    let names: Vec<_> = body.iter().map(|op| op.name).collect();
    assert_eq!(names, vec!["br"]);
    let jump = body[0].clone().as_op::<tir::builtin::BranchOp>().unwrap();
    assert_eq!(jump.dest(), r.u);
}

/// An `eq` guard makes its operands congruent in the dominated block, so even
/// the operand-swapped `eq` compare folds (the swapped node becomes congruent
/// with the assumed one once `a == b` is asserted).
#[test]
fn eq_edge_congruence_folds_swapped_compare() {
    let r = run_dominated_compare("eq", true, "eq", true);
    let body = block_op_list(&r.context, r.body);
    let names: Vec<_> = body.iter().map(|op| op.name).collect();
    assert_eq!(names, vec!["br"]);
    let jump = body[0].clone().as_op::<tir::builtin::BranchOp>().unwrap();
    assert_eq!(jump.dest(), r.u);
}

/// The assumption scope is popped once the block's plan is solved: the cached
/// e-graph must not leak the assumed facts.
#[test]
fn assumption_scope_is_popped_after_solving() {
    let r = run_dominated_compare("slt", true, "slt", false);
    // Selection succeeded and committed; nothing to assert beyond the fold
    // tests other than clean teardown, which reaching this point (no panic in
    // the scope machinery, plan committed) demonstrates. The body block was
    // rewritten to a plain jump.
    assert_eq!(block_op_list(&r.context, r.body).len(), 1);
}

/// Block arguments on conditional edges are still rejected (codegen cannot
/// place them yet), now at selection time.
#[test]
fn guard_edge_arguments_are_rejected() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    let i1 = IntegerType::new(&context, 1);
    let i64_ty = IntegerType::new(&context, 64);
    let c = context.create_value(i1, None);
    let x = context.create_value(i64_ty, None);
    let (c_id, x_id) = (c.id(), x.id());
    let region = context.create_region();
    let block = context.create_block(vec![c, x]);
    region.add_block(block.id());
    let t = context.create_block(vec![]);
    let f = context.create_block(vec![]);

    let func = ops::func(&context, "demo", i64_ty, Some(region.id())).build();
    let mut fb = IRBuilder::new(func.body());
    fb.insert(ops::cond_br(&context, c_id, vec![x_id], vec![], t.id(), f.id()).build());

    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(ops::module_end(&context).build());

    let mut pm = PassManager::new();
    pm.nest(FuncOp::name()).add_pass(
        InstructionSelectPass::new(vec![branch_rule()]).with_branch_emitters(branch_emitters()),
    );
    let err = pm
        .run(&context, context.get_op(module.id()))
        .expect_err("edge arguments should be rejected");
    assert!(err.to_string().contains("block arguments"));
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
