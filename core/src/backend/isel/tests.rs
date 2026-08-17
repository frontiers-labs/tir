use tir::{
    Context, IRFormatter, Operation, PassManager, TypeId,
    builtin::{FloatType, IntegerType, ops},
    cfg::ops as cfg_ops,
    func::{FuncOp, ops as func_ops},
    graph::{MetaMutDag, MutDag, OperandConstraint},
    sem::{FloatFormat, SemGraph, SemType, SymKind, SymPayload},
};

use super::{
    EmitRequest, ImmRange, InstructionSelectPass, IselCostModel, LATENCY_COST_SCALE,
    RegisterCapability, RegisterRequirement, Rule, RuleEmitFn, RuleMatch, SemEGraph, SemNode,
    prove_guarded_relaxations, template_node,
};

#[test]
fn overlapping_register_capability_accepts_integer_and_float_values() {
    let capability = RegisterCapability::any(64);

    assert!(capability.accepts(&SemType::bits(32)));
    assert!(capability.accepts(&SemType::bits(64)));
    assert!(capability.accepts(&SemType::Float(FloatFormat::new(11, 52))));
}

#[test]
fn whole_register_requirement_rejects_narrow_integer_values() {
    let requirement = RegisterRequirement::whole(RegisterCapability::integer(64));

    assert!(!requirement.accepts(&SemType::bits(32)));
    assert!(requirement.accepts(&SemType::bits(64)));
}

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
        .unwrap_or_else(|| op.op().operands().first().copied().unwrap());
    let rhs = m
        .value_binding(2)
        .or_else(|| m.value_binding(1))
        .unwrap_or_else(|| op.op().operands()[1]);
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
        ops::muli(
            context,
            op.op().operands()[0],
            op.op().operands()[1],
            result_ty,
        )
        .build(),
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add = ops::addi(&context, mul_result, z_id, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), LATENCY_COST_SCALE, emit_add),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "mul",
            atomic_pattern(SymKind::Mul),
            10 * LATENCY_COST_SCALE,
            emit_mul,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();
    assert_eq!(body_ops, vec!["addi", "return"]);

    let mut buf = String::new();
    let mut fmt = IRFormatter::new(&mut buf);
    module.print(&mut fmt).expect("print lowered module");
    assert!(!buf.contains("muli"));
}

/// A rule whose register class views its storage element at a nonzero bit
/// offset (x86 `ah`) is only compatible with values living at that same offset:
/// no instruction moves bits across views. Ordinary IR values live at offset 0,
/// so such a rule may neither read them as operands nor define them.
#[test]
fn shifted_register_view_rule_does_not_select_for_offset_zero_values() {
    let run = |operand_offset: u32, result_offset: u32| {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None).build();

        let i32_ty = IntegerType::new(&context, 32);
        let x = context.create_value(i32_ty, None);
        let y = context.create_value(i32_ty, None);
        let (x_id, y_id) = (x.id(), y.id());
        let region = context.create_region();
        let block = context.create_block(vec![x, y]);
        region.add_block(block.id());

        let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
        let add = ops::addi(&context, x_id, y_id, i32_ty).build();
        let add_result = add.result();
        func.body().append_op(add);
        func.body()
            .append_op(func_ops::r#return(&context, add_result).build());

        module.body().append_op(func);
        module.body().append_op(ops::module_end(&context).build());

        let capability = RegisterCapability::integer(32);
        let operand = RegisterRequirement::low_bits(capability).at_view_offset(operand_offset);
        let plain = RegisterRequirement::low_bits(capability);
        let rules = vec![
            // The cheaper rule, distinguished by emitting `muli`.
            Rule::new(
                "shifted-add",
                atomic_pattern(SymKind::Add),
                LATENCY_COST_SCALE,
                emit_mul,
            )
            .with_operand_registers(vec![(0, operand), (1, operand)])
            .with_result_register(
                RegisterRequirement::low_bits(capability).at_view_offset(result_offset),
            ),
            Rule::new(
                "add",
                atomic_pattern(SymKind::Add),
                10 * LATENCY_COST_SCALE,
                emit_add,
            )
            .with_operand_registers(vec![(0, plain), (1, plain)])
            .with_result_register(plain),
        ];

        let mut pm = PassManager::new();
        pm.nest::<FuncOp>()
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
            .map(|op_id| context.get_op(op_id).name().as_str().to_string())
            .next()
            .expect("a selected instruction")
    };

    assert_eq!(run(0, 0), "muli", "the cheaper offset-0 rule wins");
    assert_eq!(run(8, 0), "addi", "shifted operands cannot read the args");
    assert_eq!(
        run(0, 8),
        "addi",
        "a shifted result cannot define the value"
    );
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
    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    func.body()
        .append_op(func_ops::r#return(&context, mul_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![Rule::new(
        "add",
        atomic_pattern(SymKind::Add),
        10 * LATENCY_COST_SCALE,
        emit_add,
    )];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
        .add_pass(InstructionSelectPass::new(rules));
    let err = pm
        .run(&context, context.get_op(module.id()))
        .expect_err("incomplete rule set should be rejected");
    assert!(err.to_string().contains("Mul"));
}

#[test]
fn axioms_chain_through_unselectable_kinds() {
    fn sub_chain_pattern() -> SemGraph {
        let mut graph = SemGraph::new();
        let lhs = symbol(&mut graph, 0);
        let rhs = symbol(&mut graph, 1);
        let zero = binary(&mut graph, SymKind::Sub, rhs, rhs);
        let neg = binary(&mut graph, SymKind::Sub, zero, rhs);
        binary(&mut graph, SymKind::Sub, lhs, neg);
        graph
    }

    let context = Context::with_default_dialects();
    let ty = IntegerType::new(&context, 32);
    let mut egraph = SemEGraph::new();
    let lhs = egraph.add(template_node(
        SymKind::Symbol,
        Some(SymPayload::SymbolId(0)),
        Some(ty),
    ));
    let rhs = egraph.add(template_node(
        SymKind::Symbol,
        Some(SymPayload::SymbolId(1)),
        Some(ty),
    ));
    let mut add = template_node(SymKind::Add, None, Some(ty));
    add.children = vec![lhs, rhs];
    let root = egraph.add(add);

    let axioms = "
        (axiom add-via-neg
          (vars (a w) (b w)) (root w)
          (lhs (add a b)) (rhs (sub a (neg b))))
        (axiom neg-via-sub
          (vars (x w)) (root w)
          (lhs (neg x)) (rhs (sub (sub x x) x)))
    ";
    let pass = InstructionSelectPass::new(vec![Rule::new(
        "sub-chain",
        sub_chain_pattern(),
        LATENCY_COST_SCALE,
        emit_sub,
    )])
    .with_axioms(axioms);
    super::rewrites::saturate(&context, &mut egraph, &pass.rewrites, Default::default());

    assert!(
        pass.compiled_patterns[0]
            .search(&egraph, &context)
            .iter()
            .any(|m| egraph.find(m.root) == egraph.find(root))
    );
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add0 = ops::addi(&context, mul_result, z_id, i32_ty).build();
    let add0_result = add0.result();
    func.body().append_op(add0);
    let add1 = ops::addi(&context, mul_result, add0_result, i32_ty).build();
    let add1_result = add1.result();
    func.body().append_op(add1);
    func.body()
        .append_op(func_ops::r#return(&context, add1_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), LATENCY_COST_SCALE, emit_add),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "mul",
            atomic_pattern(SymKind::Mul),
            10 * LATENCY_COST_SCALE,
            emit_mul,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();
    assert_eq!(body_ops, vec!["addi", "addi", "return"]);
}

/// An unused consumer does not demand its result or operands.
#[test]
fn unused_consumer_does_not_create_demand() {
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add = ops::addi(&context, mul_result, z_id, i32_ty).build();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, mul_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), LATENCY_COST_SCALE, emit_add),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "mul",
            atomic_pattern(SymKind::Mul),
            10 * LATENCY_COST_SCALE,
            emit_mul,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();
    assert_eq!(body_ops, vec!["muli", "return"]);
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add = ops::addi(&context, mul_result, z_id, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("add-mul", add_mul_pattern(), LATENCY_COST_SCALE, emit_add),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "mul",
            atomic_pattern(SymKind::Mul),
            10 * LATENCY_COST_SCALE,
            emit_mul,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let add0 = ops::addi(&context, a_id, b_id, i32_ty).build();
    let add0_result = add0.result();
    func.body().append_op(add0);
    let mul = ops::muli(&context, add0_result, c_id, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add1 = ops::addi(&context, mul_result, d_id, i32_ty).build();
    let add1_result = add1.result();
    func.body().append_op(add1);
    func.body()
        .append_op(func_ops::r#return(&context, add1_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    // `add-mul-add` requires a `Mul(Add(_,_),_)` subpattern that no rule
    // provides; the pass synthesizes it. Selection must remain valid and, with
    // fusion priced high, fall back to the atomic cover.
    let rules = vec![
        Rule::new(
            "add-mul-add",
            add_mul_add_pattern(),
            100 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "mul",
            atomic_pattern(SymKind::Mul),
            10 * LATENCY_COST_SCALE,
            emit_mul,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
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
        ops::subi(
            context,
            op.op().operands()[0],
            op.op().operands()[1],
            result_ty,
        )
        .build(),
    ))
}

#[test]
fn unused_typed_operation_is_not_selected() {
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

    let func = func_ops::func(&context, "demo", i64_ty, Some(region.id())).build();
    let add32 = ops::addi(&context, a32, b32, i32_ty).build();
    func.body().append_op(add32);
    let add64 = ops::addi(&context, a64, b64, i64_ty).build();
    let add64_result = add64.result();
    func.body().append_op(add64);
    func.body()
        .append_op(func_ops::r#return(&context, add64_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new(
            "add.i32",
            typed_binary_pattern(SymKind::Add, i32_ty),
            LATENCY_COST_SCALE,
            emit_sub,
        ),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();
    assert_eq!(body_ops, vec!["addi", "return"]);
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let add0 = ops::addi(&context, a_id, b_id, i32_ty).build();
    let add0_result = add0.result();
    func.body().append_op(add0);
    let add1 = ops::addi(&context, add0_result, c_id, i32_ty).build();
    let add1_result = add1.result();
    func.body().append_op(add1);
    func.body()
        .append_op(func_ops::r#return(&context, add1_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

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
        Rule::new("add-add", pattern, LATENCY_COST_SCALE, emit_sub),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
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

fn zero_materializer_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let zero = g.add_node(SymKind::Constant);
    g.set_leaf_data(zero, tir::sem::int_payload(1, 0, false));
    let width = symbol(&mut g, 2);
    let zext = binary(&mut g, SymKind::ZExt, zero, width);
    let immediate = symbol(&mut g, 1);
    binary(&mut g, SymKind::Add, zext, immediate);
    g
}

fn emit_materializer_marker(
    context: &Context,
    req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let result = *req
        .results
        .first()
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(Box::new(
        ops::muli(context, result, result, result_ty).build(),
    ))
}

fn materializer_rule(emit: RuleEmitFn) -> Rule {
    Rule::new(
        "li",
        zero_materializer_pattern(),
        5 * LATENCY_COST_SCALE,
        emit,
    )
    .with_operand_constraints(vec![(1, OperandConstraint::Immediate)])
    .with_operand_imm_ranges(vec![(
        1,
        ImmRange {
            width: 12,
            signed: true,
        },
    )])
}

fn emit_integer_materializer_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let ty = req
        .result_ty
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let data = context.get_type_data(ty);
    if (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .is_none()
    {
        return Err(tir::PassError::InvalidRuleSet(
            "integer materializer received a non-integer result type".into(),
        ));
    }
    emit_materializer_marker(context, req, m)
}

fn bitcast_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let input = symbol(&mut g, 0);
    let root = g.add_node(SymKind::Bitcast);
    g.add_edge(root, input);
    g
}

fn emit_float_marker(
    context: &Context,
    req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let ty = req
        .result_ty
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    Ok(Box::new(ops::constantf(context, 0.0, ty).build()))
}

#[test]
fn introduced_integer_materializer_uses_its_class_type_under_float_bitcast() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();
    let f32_ty = FloatType::f32(&context);
    let region = context.create_region();
    let block = context.create_block(vec![]);
    region.add_block(block.id());
    let func = func_ops::func(&context, "demo", f32_ty, Some(region.id())).build();

    let value = ops::constantf(&context, 0.0, f32_ty).build();
    let result = value.result();
    func.body().append_op(value);
    func.body()
        .append_op(func_ops::r#return(&context, result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new(
            "bitcast",
            bitcast_pattern(),
            LATENCY_COST_SCALE,
            emit_float_marker,
        ),
        materializer_rule(emit_integer_materializer_marker),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("integer materialization under a bitcast should stay integer typed");
}

#[test]
fn immediate_rule_materializes_an_unannotated_constant_register_operand() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();
    let i64_ty = IntegerType::new(&context, 64);
    let region = context.create_region();
    let block = context.create_block(vec![]);
    region.add_block(block.id());
    let func = func_ops::func(&context, "demo", i64_ty, Some(region.id())).build();

    let lhs = ops::constant(&context, 5, i64_ty).build();
    let lhs_result = lhs.result();
    func.body().append_op(lhs);
    let rhs = ops::constant(&context, 7, i64_ty).build();
    let rhs_result = rhs.result();
    func.body().append_op(rhs);
    let add = ops::addi(&context, lhs_result, rhs_result, i64_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new(
            "addi",
            atomic_pattern(SymKind::Add),
            LATENCY_COST_SCALE,
            emit_add_imm_marker,
        )
        .with_operand_constraints(vec![(1, OperandConstraint::Immediate)])
        .with_operand_imm_ranges(vec![(
            1,
            ImmRange {
                width: 12,
                signed: true,
            },
        )]),
        materializer_rule(emit_materializer_marker),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("selection should materialize the register operand");

    let names: Vec<_> = context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op| context.get_op(op).name().as_str())
        .collect();
    assert_eq!(names, vec!["muli", "subi", "return"]);
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

    let func = func_ops::func(&context, "demo", i64_ty, Some(region.id())).build();
    let c = ops::constant(&context, constant, i64_ty).build();
    let c_result = c.result();
    func.body().append_op(c);
    let add = ops::addi(&context, a_id, c_result, i64_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new(
            "addi",
            atomic_pattern(SymKind::Add),
            LATENCY_COST_SCALE,
            emit_add_imm_marker,
        )
        .with_operand_constraints(vec![(1, OperandConstraint::Immediate)])
        .with_operand_imm_ranges(vec![(
            1,
            super::ImmRange {
                width: 12,
                signed: true,
            },
        )]),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
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

    let rewrite = tir::sem::theory::axioms()
        .into_iter()
        .map(tir::sem::axioms::Axiom::compile)
        .find(|rewrite| rewrite.name == "axiom-sext-bridge")
        .expect("sext bridge enabled");
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
    // The rewrite-introduced shift classes carry the register width used by the
    // invariant.
    let compiled = compile_isel_pattern(
        0,
        &shift_imm_pattern(SymKind::ShiftRightArithmetic),
        &[(1, OperandConstraint::Immediate)],
        &[(
            0,
            RegisterRequirement::whole(RegisterCapability::integer(64)),
        )],
        &[],
        None,
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

fn emit_shift_prelude(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    let value = m
        .value_binding(0)
        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(Box::new(
        ops::subi(context, value, value, result_ty).build(),
    ))
}

fn emit_srai(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    emit_shift_marker(SymKind::ShiftRightArithmetic)(context, req, m)
}

fn select_sign_extension(slli_rule: Rule) -> Vec<&'static str> {
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

    let func = func_ops::func(&context, "square", i64_ty, Some(region.id())).build();
    let add = ops::addi(&context, a_id, b_id, i16_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    let ext = ops::extsi(&context, add_result, i64_ty).build();
    let ext_result = ext.result();
    func.body().append_op(ext);
    func.body()
        .append_op(func_ops::r#return(&context, ext_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            LATENCY_COST_SCALE,
            emit_add,
        ),
        slli_rule,
        Rule::new(
            "srai",
            shift_imm_pattern(SymKind::ShiftRightArithmetic),
            LATENCY_COST_SCALE,
            emit_srai,
        )
        .with_operand_constraints(vec![(1, OperandConstraint::Immediate)]),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("sign extension should select");

    context
        .get_region(region.id())
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect()
}

/// End-to-end square: `extsi(addi(a, b) : i16) : i64` lowers to `add, slli, srai`.
/// The `add` covers the addi; saturation bridges the un-selectable sign extension
/// into a `slli`/`srai` pair, and multi-instruction emission materializes the
/// introduced `slli` (an e-class with no original op) before the `srai`.
#[test]
fn square_sign_extension_lowers_to_shift_pair() {
    let slli_rule = Rule::new(
        "slli",
        shift_imm_pattern(SymKind::ShiftLeft),
        LATENCY_COST_SCALE,
        emit_slli,
    )
    .with_operand_constraints(vec![(1, OperandConstraint::Immediate)]);
    let body_ops = select_sign_extension(slli_rule);

    // add (from the addi), then the slli/srai sign-extension idiom, then return.
    assert_eq!(body_ops, vec!["addi", "shli", "shrsi", "return"]);
}

#[test]
fn introduced_rule_emits_prelude_before_instruction() {
    let slli_rule = Rule::new(
        "slli",
        shift_imm_pattern(SymKind::ShiftLeft),
        LATENCY_COST_SCALE,
        emit_slli,
    )
    .with_operand_constraints(vec![(1, OperandConstraint::Immediate)])
    .with_prelude_emitter(emit_shift_prelude);
    let body_ops = select_sign_extension(slli_rule);

    assert_eq!(body_ops, vec!["addi", "subi", "shli", "shrsi", "return"]);
}

/// Opaque leaves stand for *unknown* computations: two of them must never
/// hash-cons into the same e-class, or unrelated un-lowerable expressions
/// would be treated as equal.
#[test]
fn opaque_leaves_are_distinct() {
    use super::builder::SemDagBuilder;
    use std::collections::HashMap;

    let context = Context::with_default_dialects();
    let i32 = IntegerType::new(&context, 32);
    let region = context.create_region();
    func_ops::func(&context, "empty", i32, Some(region.id())).build();
    let value_to_def = HashMap::new();
    let mut egraph = SemEGraph::new();
    let mut builder = SemDagBuilder::new(&context, &value_to_def, &mut egraph, None);
    let a = builder.add_opaque();
    let b = builder.add_opaque();
    assert_ne!(egraph.find(a), egraph.find(b));
}

/// A block argument is an anchor: the merge it names is control flow the
/// mid-end already optimized, so isel lowers it to an input leaf and leaves the
/// incoming edges to carry the value.
#[test]
fn builder_anchors_join_block_argument() {
    use super::builder::SemDagBuilder;
    use std::collections::HashMap;

    let context = Context::with_default_dialects();
    let i1 = IntegerType::new(&context, 1);
    let i32 = IntegerType::new(&context, 32);
    let cond = context.create_value(i1, None);
    let x = context.create_value(i32, None);
    let y = context.create_value(i32, None);
    let cond_id = cond.id();
    let x_id = x.id();
    let y_id = y.id();
    let entry = context.create_block(vec![cond, x, y]);
    let then_block = context.create_block(vec![]);
    let else_block = context.create_block(vec![]);
    let merged = context.create_value(i32, None);
    let merged_id = merged.id();
    let join = context.create_block(vec![merged]);
    let region = context.create_region();
    for block in [&entry, &then_block, &else_block, &join] {
        region.add_block(block.id());
    }
    func_ops::func(&context, "gamma", i32, Some(region.id())).build();

    entry.append_op(
        cfg_ops::cond_br(
            &context,
            cond_id,
            vec![],
            vec![],
            then_block.id(),
            else_block.id(),
        )
        .build(),
    );
    then_block.append_op(cfg_ops::br(&context, vec![x_id], join.id()).build());
    else_block.append_op(cfg_ops::br(&context, vec![y_id], join.id()).build());
    join.append_op(func_ops::r#return(&context, merged_id).build());

    let value_to_def = HashMap::new();
    let mut egraph = SemEGraph::new();
    let mut builder = SemDagBuilder::new(&context, &value_to_def, &mut egraph, None);
    let class = builder.build_from_value(merged_id);
    drop(builder);

    let nodes = egraph.nodes(class);
    assert!(nodes.iter().all(|node| node.kind != SymKind::If));
    assert!(nodes.iter().any(|node| {
        node.kind == SymKind::Symbol
            && matches!(
                node.payload,
                Some(tir::sem::SemPayload::Expr(SymPayload::Value(value))) if value == merged_id
            )
    }));
}

type SeededFunction = (
    SemEGraph,
    std::collections::HashMap<tir::ValueId, tir_symbolic::egraph::Id>,
    tir::OpId,
);

/// Seed the single function of `src` under the region path: the graph, the class
/// each value was seeded with, and the function op.
fn seed_regions(context: &Context, src: &str) -> SeededFunction {
    use super::builder::SemDagBuilder;
    use std::collections::{HashMap, HashSet};

    let module =
        crate::parse::ir::parse_ir::<tir::builtin::ModuleOp>(context, src).expect("parse module");
    let module_region = context.get_op(module.id()).regions()[0];
    let func = context
        .get_region(module_region)
        .iter(context.clone())
        .next()
        .and_then(|block| block.op_ids().first().copied())
        .expect("a function in the module");
    let blocks: Vec<tir::BlockId> = context
        .get_region(context.get_op(func).regions()[0])
        .iter(context.clone())
        .map(|block| block.id())
        .collect();
    let mut value_to_def = HashMap::new();
    for &block in &blocks {
        for op_id in context.get_block(block).op_ids() {
            for result in context.get_op(op_id).results() {
                value_to_def.insert(result, op_id);
            }
        }
    }
    let mut egraph = SemEGraph::new();
    let mut builder = SemDagBuilder::new(context, &value_to_def, &mut egraph, None);
    builder.build_blocks(&blocks, &HashSet::new());
    let value_to_class = builder.value_to_class.clone();
    drop(builder);
    (egraph, value_to_class, func)
}

/// The operations of a region's single block.
fn region_ops(context: &Context, region: tir::RegionId) -> Vec<tir::OpHandle> {
    context
        .get_region(region)
        .iter(context.clone())
        .next()
        .expect("a block in the region")
        .op_ids()
        .into_iter()
        .map(|op| context.get_op(op))
        .collect()
}

/// A loop's carried port is the θ over what the loop was entered with and what
/// each edge back into the port carries — the term a region-scoped cover reads;
/// the port's own argument stays in the class so it can still be read as the
/// register the loop leaves it in.
#[test]
fn builder_seeds_a_loop_port_as_a_theta_over_its_edges() {
    let context = Context::with_default_dialects();
    let (egraph, value_to_class, func) = seed_regions(
        &context,
        "module {
  func.func @accumulate(%0: !i1, %1: !i64) -> !i64 {
    %2 = scf.while iter_args(%3 = %1) -> !i64 {
      scf.condition %0, %3
    } do scope(%4)(%5) {
      %6 = addi %5, %5 : !i64
      scf.yield %6
    }
    func.return %2
  }
  module_end
}",
    );

    let loop_op = region_ops(&context, context.get_op(func).regions()[0])[0].clone();
    let condition = region_ops(&context, loop_op.regions()[0]).pop().unwrap();
    let latch = region_ops(&context, loop_op.regions()[1]).pop().unwrap();
    let class_of = |value| egraph.find(value_to_class[&value]);
    let head = class_of(condition.operands()[1]);

    let theta = egraph
        .nodes(head)
        .iter()
        .find(|node| node.kind == SymKind::Theta)
        .expect("the tested port is a theta")
        .clone();
    assert_eq!(theta.children.len(), 2);
    assert_eq!(
        egraph.find(theta.children[0]),
        class_of(loop_op.operands()[0])
    );
    assert_eq!(
        egraph.find(theta.children[1]),
        class_of(latch.operands()[0])
    );
    // The port is still readable as the register the loop leaves it in.
    assert!(
        egraph
            .nodes(head)
            .iter()
            .any(|node| node.kind == SymKind::Symbol)
    );
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
/// The same-slot case also guards what keeps the two accesses apart: the store
/// takes the chain to a state the load then reads, so the rules' arity-3/4
/// memory patterns must still match terms carrying that state operand.
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let slot_ty = tir::ptr::PtrType::typed(&context, i32_ty);
    let slot = func
        .body()
        .append_op(tir::ptr::ops::alloca(&context, 4u64, 4u64, slot_ty).build());
    func.body()
        .append_op(tir::ptr::ops::store(&context, param_id, slot.result()).build());
    let loaded = func
        .body()
        .append_op(tir::ptr::ops::load(&context, slot.result(), i32_ty).build());
    func.body()
        .append_op(func_ops::r#return(&context, loaded.result()).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new("load", load_pattern(), LATENCY_COST_SCALE, emit_load_marker),
        Rule::new(
            "store",
            store_pattern(),
            LATENCY_COST_SCALE,
            emit_store_marker,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();
    // store -> muli marker, load -> shli marker; the alloca is untouched.
    assert_eq!(body_ops, vec!["alloca", "muli", "shli", "return"]);
}

/// Equivalent definitions extract to one tile.
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let mul = ops::muli(&context, x_id, y_id, i32_ty).build();
    func.body().append_op(mul);
    let add = ops::addi(&context, x_id, y_id, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    let sub = ops::subi(&context, add_result, z_id, i32_ty).build();
    let sub_result = sub.result();
    func.body().append_op(sub);
    func.body()
        .append_op(func_ops::r#return(&context, sub_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

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
        post_saturation: false,
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
        Rule::new(
            "mul",
            atomic_pattern(SymKind::Mul),
            LATENCY_COST_SCALE,
            emit_mul,
        ),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "sub",
            atomic_pattern(SymKind::Sub),
            LATENCY_COST_SCALE,
            emit_sub_bound,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
    let names: Vec<_> = body.iter().map(|op| op.name().as_str()).collect();
    assert_eq!(names, vec!["muli", "subi", "return"]);

    let sub_op = &body[1];
    assert_eq!(sub_op.operands()[0], body[0].results()[0]);
}

fn block_op_list(context: &Context, block: tir::BlockId) -> Vec<tir::OpHandle> {
    context
        .get_block(block)
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id))
        .collect()
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

    let func = func_ops::func(&context, "demo", i32_ty, Some(region.id())).build();
    let add = ops::addi(&context, a_id, b_id, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    // Same opcode, same cost; only the type constraint differs. The typed rule
    // (subi marker) must be selected.
    let rules = vec![
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
        Rule::new(
            "add.i32",
            typed_binary_pattern(SymKind::Add, i32_ty),
            10 * LATENCY_COST_SCALE,
            emit_sub,
        ),
    ];

    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
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
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect();
    assert_eq!(body_ops, vec!["subi", "return"]);
}

/// A demanded class is extracted in the block containing its live definition.
#[test]
fn refuses_cross_block_binding_to_non_escaping_value() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();
    let i64_ty = IntegerType::new(&context, 64);
    let a = context.create_value(i64_ty, None);
    let b = context.create_value(i64_ty, None);
    let m = context.create_value(i64_ty, None);
    let (a_id, b_id, m_id) = (a.id(), b.id(), m.id());
    let region = context.create_region();
    let entry = context.create_block(vec![a, b, m]);
    let bb1 = context.create_block(vec![]);
    for block in [&entry, &bb1] {
        region.add_block(block.id());
    }

    let func = func_ops::func(&context, "demo", i64_ty, Some(region.id())).build();

    // %d = a - b is used only within the entry block, so it never escapes.
    let d = ops::subi(&context, a_id, b_id, i64_ty).build();
    let d_res = d.result();
    entry.append_op(d);
    let g = ops::subi(&context, d_res, m_id, i64_ty).build();
    entry.append_op(g);
    entry.append_op(cfg_ops::br(&context, vec![], bb1.id()).build());

    // %e = a - b recomputes the same expression (CSE-merged with %d); the add
    // consumes it, resolving its operand under the binding rule.
    let e = ops::subi(&context, a_id, b_id, i64_ty).build();
    let e_res = e.result();
    bb1.append_op(e);
    let r = ops::addi(&context, e_res, m_id, i64_ty).build();
    let r_res = r.result();
    bb1.append_op(r);
    bb1.append_op(func_ops::r#return(&context, r_res).build());

    module.body().append_op(func);
    module.body().append_op(ops::module_end(&context).build());

    let rules = vec![
        Rule::new(
            "sub",
            atomic_pattern(SymKind::Sub),
            LATENCY_COST_SCALE,
            emit_sub,
        ),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            LATENCY_COST_SCALE,
            emit_add,
        ),
    ];
    let mut pm = PassManager::new();
    pm.nest::<FuncOp>()
        .add_pass(InstructionSelectPass::new(rules));
    pm.run(&context, context.get_op(module.id()))
        .expect("selection should succeed");

    let entry_names: Vec<_> = block_op_list(&context, entry.id())
        .iter()
        .map(|op| op.name().as_str())
        .collect();
    assert_eq!(entry_names, vec!["br"]);
    let bb1_ops = block_op_list(&context, bb1.id());
    let names: Vec<_> = bb1_ops.iter().map(|op| op.name().as_str()).collect();
    assert_eq!(names, vec!["subi", "addi", "return"]);
    let bb1_sub = bb1_ops[0].results()[0];
    let addi = &bb1_ops[1];
    assert!(
        addi.operands().contains(&bb1_sub),
        "the add binds the block-local recomputation"
    );
}

// ── Guarded-relaxation gate (Stage 3) ───────────────────────────────────────

fn constant(g: &mut SemGraph, value: u64, width: u32) -> tir::graph::NodeId {
    let node = g.add_node(SymKind::Constant);
    g.set_leaf_data(node, SymPayload::Int(tir_adt::APInt::new(width, value)));
    node
}

fn ternary(
    g: &mut SemGraph,
    kind: SymKind,
    a: tir::graph::NodeId,
    b: tir::graph::NodeId,
    c: tir::graph::NodeId,
) -> tir::graph::NodeId {
    let node = g.add_node(kind);
    g.add_edge(node, a);
    g.add_edge(node, b);
    g.add_edge(node, c);
    node
}

fn div_pattern() -> SemGraph {
    let mut g = SemGraph::new();
    let a = symbol(&mut g, 0);
    let b = symbol(&mut g, 1);
    binary(&mut g, SymKind::Div, a, b);
    g
}

fn emit_unreachable(
    _context: &Context,
    _req: &EmitRequest,
    _m: &RuleMatch,
) -> Result<Box<dyn Operation>, tir::PassError> {
    unreachable!("the relaxation gate never emits")
}

/// `If(Eq(b, guard_rhs), ones, Div(lhs, rhs))` — the riscv-div shape, with the
/// guard constant and the else-arm operand order made configurable so the tests
/// can build both the sound rule and the two unsound variants.
fn guarded_div(guard_rhs: u64, else_swapped: bool) -> SemGraph {
    let mut g = SemGraph::new();
    let a = symbol(&mut g, 0);
    let b = symbol(&mut g, 1);
    let (num, den) = if else_swapped { (b, a) } else { (a, b) };
    let div = binary(&mut g, SymKind::Div, num, den);
    let guard_const = constant(&mut g, guard_rhs, 64);
    let cond = binary(&mut g, SymKind::Eq, b, guard_const);
    let ones = constant(&mut g, u64::MAX, 64);
    ternary(&mut g, SymKind::If, cond, ones, div);
    g
}

#[test]
fn guarded_div_rule_with_correct_relaxation_is_accepted() {
    let rule = Rule::new("div", div_pattern(), LATENCY_COST_SCALE, emit_unreachable)
        .with_guarded_semantics(guarded_div(0, false));
    assert!(prove_guarded_relaxations(&[rule]).is_ok());
}

#[test]
fn guarded_div_rule_with_wrong_guard_region_is_rejected() {
    // Guarding on `b == 1` instead of `b == 0` leaves the pure `div` unequal to
    // the behavior at `b == 1` (where `div(a,1) == a`, not all-ones).
    let rule = Rule::new("div", div_pattern(), LATENCY_COST_SCALE, emit_unreachable)
        .with_guarded_semantics(guarded_div(1, false));
    match prove_guarded_relaxations(&[rule]) {
        Err(tir::PassError::InvalidRuleSet(msg)) => assert!(msg.contains("div")),
        Err(other) => panic!("expected InvalidRuleSet, got {other:?}"),
        Ok(_) => panic!("expected InvalidRuleSet, rule was accepted"),
    }
}

#[test]
fn guarded_div_rule_with_mismatched_else_arm_is_rejected() {
    // The else arm computes `div(b, a)` while the selection pattern is `div(a, b)`.
    let rule = Rule::new("div", div_pattern(), LATENCY_COST_SCALE, emit_unreachable)
        .with_guarded_semantics(guarded_div(0, true));
    match prove_guarded_relaxations(&[rule]) {
        Err(tir::PassError::InvalidRuleSet(msg)) => assert!(msg.contains("div")),
        Err(other) => panic!("expected InvalidRuleSet, got {other:?}"),
        Ok(_) => panic!("expected InvalidRuleSet, rule was accepted"),
    }
}

/// The `eq-zero` axiom is what puts a comparison against zero into the shape a
/// zero-register rule is written in, so the compiled pattern must find it.
#[test]
fn comparison_zero_shape_matches_zero_register_rule() {
    use tir::sem::{SaturationLimits, con, op, sym, theory};
    use tir_adt::APInt;

    let ctx = Context::with_default_dialects();
    let i1 = IntegerType::new(&ctx, 1);
    let i64 = IntegerType::new(&ctx, 64);
    let mut egraph = SemEGraph::new();
    let value = egraph.add(template_node(
        SymKind::Symbol,
        Some(SymPayload::SymbolId(0)),
        Some(i64),
    ));
    let zero = egraph.add(template_node(
        SymKind::Constant,
        Some(SymPayload::Int(APInt::new(64, 0))),
        Some(i64),
    ));
    let mut comparison = template_node(SymKind::Eq, None, Some(i1));
    comparison.children = vec![value, zero];
    let root = egraph.add(comparison);
    let rewrite = theory::axioms()
        .into_iter()
        .find(|axiom| axiom.name == "eq-zero")
        .expect("core theory must declare eq-zero")
        .compile();
    super::rewrites::saturate(
        &ctx,
        &mut egraph,
        std::slice::from_ref(&rewrite),
        SaturationLimits::default(),
    );

    let mut pattern = SemGraph::new();
    let value = sym(&mut pattern, 0);
    let zero = con(&mut pattern, 0, 1);
    let width = sym(&mut pattern, 1);
    let zext = op(&mut pattern, SymKind::ZExt, &[zero, width]);
    op(&mut pattern, SymKind::Eq, &[value, zext]);
    let compiled = super::pattern::compile_isel_pattern(0, &pattern, &[], &[], &[], None).unwrap();

    assert!(
        compiled
            .search(&egraph, &ctx)
            .iter()
            .any(|m| egraph.find(m.root) == egraph.find(root))
    );
}
