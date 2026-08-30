//! E-graph instruction selection: PBQP covers, immediates, materializers,
//! saturation-backed lowering and memory interfaces.

use tir::{
    builtin::{ops, FloatType, IntegerType, ModuleOp},
    cfg::ops as cfg_ops,
    func::{ops as func_ops, FuncOp},
    graph::{MetaMutDag, MutDag, OperandConstraint},
    sem::{SemGraph, SymKind},
    Context, IRFormatter, Operation, PassError, PassManager, RegionId, TypeId, ValueId,
};

use tir::backend::isel::{
    EmitRequest, ImmRange, InstructionSelectPass, IselCostModel, RegisterCapability,
    RegisterRequirement, Rule, RuleEmitFn, RuleMatch, LATENCY_COST_SCALE,
};
use tir::sem::template_node;

use super::fixtures::{atomic_pattern, binary, nary, symbol};

// The instructions the test rules emit. A selection rule emits a machine
// instruction, whose operands are registers rather than typed mid-end values,
// so these markers say only what the assertions read: the mnemonic and the
// order they were emitted in.
macro_rules! marker_info {
    ($op:ident, $name:literal) => {
        impl tir::backend::MachineInstruction for $op {
            fn info(&self) -> &'static tir::backend::InstrInfo {
                static INFO: tir::backend::InstrInfo = tir::backend::InstrInfo {
                    name: $name,
                    mnemonic: $name,
                    ..tir::backend::InstrInfo::BASE
                };
                &INFO
            }

            fn instance(&self) -> &tir::OpHandle {
                &self.0
            }
        }
    };
}

tir::helpers::operation! {
    ShlMarkerOp {
        name: "shli",
        dialect: "test",
        operands: O { a: "?tir::Any", b: "?tir::Any", },
        results: R { regs: "*tir::Any" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

tir::helpers::operation! {
    ShrsMarkerOp {
        name: "shrsi",
        dialect: "test",
        operands: O { a: "?tir::Any", b: "?tir::Any", },
        results: R { regs: "*tir::Any" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

tir::helpers::operation! {
    SubMarkerOp {
        name: "subi",
        dialect: "test",
        operands: O { a: "?tir::Any", b: "?tir::Any", },
        results: R { regs: "*tir::Any" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

tir::helpers::operation! {
    MulMarkerOp {
        name: "muli",
        dialect: "test",
        operands: O { a: "?tir::Any", b: "?tir::Any", },
        results: R { regs: "*tir::Any" },
        interfaces: [tir::backend::MachineInstruction],
    }
}

marker_info!(ShlMarkerOp, "shli");
marker_info!(ShrsMarkerOp, "shrsi");
marker_info!(SubMarkerOp, "subi");
marker_info!(MulMarkerOp, "muli");

/// Build one of the marker instructions over `value`, producing `result_ty`.
macro_rules! marker {
    ($op:ident, $builder:ident, $context:expr, $value:expr, $result_ty:expr) => {{
        $op::register_interfaces($context);
        Box::new(
            $builder::new($context)
                .a($value)
                .b($value)
                .result_types(vec![$result_ty])
                .build(),
        )
    }};
}

/// `module { func demo(args…) -> ret }` with an empty body block; the module is
/// not yet closed so the test can append the body it needs.
fn function(
    context: &Context,
    args: &[TypeId],
    ret: TypeId,
) -> (ModuleOp, FuncOp, RegionId, Vec<ValueId>) {
    let module = ops::module(context, None).build();
    let values: Vec<_> = args
        .iter()
        .map(|&t| context.create_value(t, None))
        .collect();
    let ids: Vec<_> = values.iter().map(|v| v.id()).collect();
    let region = context.create_region();
    let block = context.create_block(values);
    region.add_block(block.id());
    let func = func_ops::lambda(context, "demo", ret, &region).build();
    (module, func, region.id(), ids)
}

/// Closes the module around `func` and runs `pass` nested on functions.
fn run_pass(
    context: &Context,
    module: &ModuleOp,
    func: FuncOp,
    pass: InstructionSelectPass,
) -> Result<(), PassError> {
    module.body().append_op(func);
    module.body().append_op(ops::module_end(context).build());
    let mut pm = PassManager::new();
    pm.nest::<FuncOp>().add_pass(pass);
    pm.run(context, context.get_op(module.id()))
}

fn select(context: &Context, module: &ModuleOp, func: FuncOp, rules: Vec<Rule>) {
    run_pass(context, module, func, InstructionSelectPass::new(rules))
        .expect("pass pipeline should succeed");
}

/// The op names of the region's first block.
fn body_names(context: &Context, region: RegionId) -> Vec<&'static str> {
    context
        .get_region(region)
        .iter(context.clone())
        .next()
        .unwrap()
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id).name().as_str())
        .collect()
}

fn block_op_list(context: &Context, block: tir::BlockId) -> Vec<tir::OpHandle> {
    context
        .get_block(block)
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id))
        .collect()
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
) -> Result<Box<dyn Operation>, PassError> {
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
) -> Result<Box<dyn Operation>, PassError> {
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

// A rule reads its operands from the match, not from the op it covers: a fused
// pattern covers ops whose results the cover retires.
fn emit_sub(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let lhs = m
        .value_binding(0)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let rhs = m
        .value_binding(1)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    SubMarkerOp::register_interfaces(context);
    Ok(Box::new(
        SubMarkerOpBuilder::new(context)
            .a(lhs)
            .b(rhs)
            .result_types(vec![result_ty])
            .build(),
    ))
}

fn add_mul_rules() -> Vec<Rule> {
    vec![
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
    ]
}

#[test]
fn pbqp_selector_consumes_internal_nodes_of_selected_pattern() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);
    let (x, y, z) = (args[0], args[1], args[2]);

    let mul = ops::muli(&context, x, y, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add = ops::addi(&context, mul_result, z, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    select(&context, &module, func, add_mul_rules());

    assert_eq!(body_names(&context, region), vec!["addi", "return"]);

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
        let i32_ty = IntegerType::new(&context, 32);
        let (module, func, region, args) = function(&context, &[i32_ty, i32_ty], i32_ty);

        let add = ops::addi(&context, args[0], args[1], i32_ty).build();
        let add_result = add.result();
        func.body().append_op(add);
        func.body()
            .append_op(func_ops::r#return(&context, add_result).build());

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

        select(&context, &module, func, rules);
        body_names(&context, region)[0]
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
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, _region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);

    // A standalone Mul that no rule can root and no parent match can consume:
    // the e-graph cover is infeasible, so selection fails naming the kind.
    let mul = ops::muli(&context, args[0], args[1], i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    func.body()
        .append_op(func_ops::r#return(&context, mul_result).build());

    let rules = vec![Rule::new(
        "add",
        atomic_pattern(SymKind::Add),
        10 * LATENCY_COST_SCALE,
        emit_add,
    )];

    let err = run_pass(&context, &module, func, InstructionSelectPass::new(rules))
        .expect_err("incomplete rule set should be rejected");
    assert!(err.to_string().contains("Mul"));
}

/// A pure subexpression shared by two fused matches is *duplicated*: each
/// add-mul instruction recomputes the mul internally, and the mul op — no
/// longer needed as a register value — is consumed.
#[test]
fn pbqp_selector_duplicates_shared_pure_internal_nodes() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);
    let (x, y, z) = (args[0], args[1], args[2]);

    let mul = ops::muli(&context, x, y, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add0 = ops::addi(&context, mul_result, z, i32_ty).build();
    let add0_result = add0.result();
    func.body().append_op(add0);
    let add1 = ops::addi(&context, mul_result, add0_result, i32_ty).build();
    let add1_result = add1.result();
    func.body().append_op(add1);
    func.body()
        .append_op(func_ops::r#return(&context, add1_result).build());

    select(&context, &module, func, add_mul_rules());

    assert_eq!(body_names(&context, region), vec!["addi", "addi", "return"]);
}

/// An unused consumer does not demand its result or operands.
#[test]
fn unused_consumer_does_not_create_demand() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);
    let (x, y, z) = (args[0], args[1], args[2]);

    let mul = ops::muli(&context, x, y, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add = ops::addi(&context, mul_result, z, i32_ty).build();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, mul_result).build());

    select(&context, &module, func, add_mul_rules());

    assert_eq!(body_names(&context, region), vec!["muli", "return"]);
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
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);
    let (x, y, z) = (args[0], args[1], args[2]);

    let mul = ops::muli(&context, x, y, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add = ops::addi(&context, mul_result, z, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

    let pass =
        InstructionSelectPass::new(add_mul_rules()).with_cost_model(Box::new(NoFusionCostModel));
    run_pass(&context, &module, func, pass).expect("pass pipeline should succeed");

    // With fusion priced out, the default add-mul cost-1 win is overridden.
    assert_eq!(body_names(&context, region), vec!["muli", "addi", "return"]);
}

#[test]
fn composite_rule_falls_back_to_atomic_cover() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) =
        function(&context, &[i32_ty, i32_ty, i32_ty, i32_ty], i32_ty);
    let (a, b, c, d) = (args[0], args[1], args[2], args[3]);

    let add0 = ops::addi(&context, a, b, i32_ty).build();
    let add0_result = add0.result();
    func.body().append_op(add0);
    let mul = ops::muli(&context, add0_result, c, i32_ty).build();
    let mul_result = mul.result();
    func.body().append_op(mul);
    let add1 = ops::addi(&context, mul_result, d, i32_ty).build();
    let add1_result = add1.result();
    func.body().append_op(add1);
    func.body()
        .append_op(func_ops::r#return(&context, add1_result).build());

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

    select(&context, &module, func, rules);

    assert_eq!(
        body_names(&context, region),
        vec!["addi", "muli", "addi", "return"]
    );
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

#[test]
fn unused_typed_operation_is_not_selected() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let i64_ty = IntegerType::new(&context, 64);
    let (module, func, region, args) =
        function(&context, &[i32_ty, i32_ty, i64_ty, i64_ty], i64_ty);
    let (a32, b32, a64, b64) = (args[0], args[1], args[2], args[3]);

    let add32 = ops::addi(&context, a32, b32, i32_ty).build();
    func.body().append_op(add32);
    let add64 = ops::addi(&context, a64, b64, i64_ty).build();
    let add64_result = add64.result();
    func.body().append_op(add64);
    func.body()
        .append_op(func_ops::r#return(&context, add64_result).build());

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

    select(&context, &module, func, rules);

    assert_eq!(body_names(&context, region), vec!["addi", "return"]);
}

/// Build `add(add(a,b), c)` over i32 values and select it with a fused
/// `Add(Add(_,_),_)` rule whose *internal* node carries `inner_width` as a type
/// constraint (plus an untyped atomic `add` fallback). Returns the lowered op
/// names. Fusion (the `subi` marker) only happens when the inner constraint
/// agrees with the inferred i32 type of the inner add.
fn run_inner_typed_fusion(inner_width: Option<u32>) -> Vec<&'static str> {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);
    let (a, b, c) = (args[0], args[1], args[2]);

    let add0 = ops::addi(&context, a, b, i32_ty).build();
    let add0_result = add0.result();
    func.body().append_op(add0);
    let add1 = ops::addi(&context, add0_result, c, i32_ty).build();
    let add1_result = add1.result();
    func.body().append_op(add1);
    func.body()
        .append_op(func_ops::r#return(&context, add1_result).build());

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

    select(&context, &module, func, rules);
    body_names(&context, region)
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

fn emit_add_imm_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let lhs = m
        .value_binding(0)
        .or_else(|| m.value_binding(1))
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    m.int_binding(1)
        .or_else(|| m.int_binding(0))
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
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
) -> Result<Box<dyn Operation>, PassError> {
    let result = *req
        .results
        .first()
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
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
            align: 1,
            nonzero: false,
        },
    )])
}

fn emit_integer_materializer_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let ty = req.result_ty.ok_or(PassError::RewriteFailed(req.op_id()))?;
    let data = context.get_type_data(ty);
    if (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .is_none()
    {
        return Err(PassError::InvalidRuleSet(
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
) -> Result<Box<dyn Operation>, PassError> {
    let ty = req.result_ty.ok_or(PassError::RewriteFailed(req.op_id()))?;
    Ok(Box::new(ops::constantf(context, 0.0, ty).build()))
}

#[test]
fn introduced_integer_materializer_uses_its_class_type_under_float_bitcast() {
    let context = Context::with_default_dialects();
    let f32_ty = FloatType::f32(&context);
    let (module, func, _region, _args) = function(&context, &[], f32_ty);

    let value = ops::constantf(&context, 0.0, f32_ty).build();
    let result = value.result();
    func.body().append_op(value);
    func.body()
        .append_op(func_ops::r#return(&context, result).build());

    let rules = vec![
        Rule::new(
            "bitcast",
            bitcast_pattern(),
            LATENCY_COST_SCALE,
            emit_float_marker,
        ),
        materializer_rule(emit_integer_materializer_marker),
    ];

    run_pass(&context, &module, func, InstructionSelectPass::new(rules))
        .expect("integer materialization under a bitcast should stay integer typed");
}

#[test]
fn immediate_rule_materializes_an_unannotated_constant_register_operand() {
    let context = Context::with_default_dialects();
    let i64_ty = IntegerType::new(&context, 64);
    let (module, func, region, _args) = function(&context, &[], i64_ty);

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
                align: 1,
                nonzero: false,
            },
        )]),
        materializer_rule(emit_materializer_marker),
    ];

    run_pass(&context, &module, func, InstructionSelectPass::new(rules))
        .expect("selection should materialize the register operand");

    assert_eq!(body_names(&context, region), vec!["muli", "subi", "return"]);
}

/// Select `add(a, constant)` with a cheap immediate rule bounded to a signed
/// 12-bit field (`subi` marker) and an expensive register-form fallback.
fn run_immediate_range(constant: i64) -> Vec<&'static str> {
    let context = Context::with_default_dialects();
    let i64_ty = IntegerType::new(&context, 64);
    let (module, func, region, args) = function(&context, &[i64_ty], i64_ty);

    let c = ops::constant(&context, constant, i64_ty).build();
    let c_result = c.result();
    func.body().append_op(c);
    let add = ops::addi(&context, args[0], c_result, i64_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

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
                align: 1,
                nonzero: false,
            },
        )]),
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
    ];

    select(&context, &module, func, rules);
    body_names(&context, region)
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

fn shift_imm_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let rs1 = symbol(&mut g, 0);
    let imm = symbol(&mut g, 1);
    binary(&mut g, kind, rs1, imm);
    g
}

fn emit_shift_marker(
    marker: SymKind,
) -> impl Fn(&Context, &EmitRequest, &RuleMatch) -> Result<Box<dyn Operation>, PassError> {
    move |context, req, m| {
        let rs1 = m
            .value_binding(0)
            .ok_or(PassError::RewriteFailed(req.op_id()))?;
        let result_ty = req.result_ty.expect("typed result");
        // The shift amount is an immediate (m.int_binding(1)); operands beyond the
        // mnemonic don't matter for this test, so the source register is reused.
        let built: Box<dyn Operation> = match marker {
            SymKind::ShiftLeft => marker!(ShlMarkerOp, ShlMarkerOpBuilder, context, rs1, result_ty),
            _ => marker!(ShrsMarkerOp, ShrsMarkerOpBuilder, context, rs1, result_ty),
        };
        Ok(built)
    }
}

fn emit_slli(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    emit_shift_marker(SymKind::ShiftLeft)(context, req, m)
}

fn emit_shift_prelude(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let value = m
        .value_binding(0)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(marker!(
        SubMarkerOp,
        SubMarkerOpBuilder,
        context,
        value,
        result_ty
    ))
}

fn emit_srai(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    emit_shift_marker(SymKind::ShiftRightArithmetic)(context, req, m)
}

fn select_sign_extension(slli_rule: Rule) -> Vec<&'static str> {
    let context = Context::with_default_dialects();
    let i16_ty = IntegerType::new(&context, 16);
    let i64_ty = IntegerType::new(&context, 64);
    let (module, func, region, args) = function(&context, &[i16_ty, i16_ty], i64_ty);

    let add = ops::addi(&context, args[0], args[1], i16_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    let ext = ops::extsi(&context, add_result, i64_ty).build();
    let ext_result = ext.result();
    func.body().append_op(ext);
    func.body()
        .append_op(func_ops::r#return(&context, ext_result).build());

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

    run_pass(&context, &module, func, InstructionSelectPass::new(rules))
        .expect("sign extension should select");
    body_names(&context, region)
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
) -> Result<Box<dyn Operation>, PassError> {
    let base = m
        .value_binding(0)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(marker!(
        ShlMarkerOp,
        ShlMarkerOpBuilder,
        context,
        base,
        result_ty
    ))
}

fn emit_store_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let value = m
        .value_binding(4)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let result_ty = context.get_value(value).ty();
    Ok(marker!(
        MulMarkerOp,
        MulMarkerOpBuilder,
        context,
        value,
        result_ty
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
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty], i32_ty);

    // Threaded: an access names the chain it reads, and the slot's own chain
    // starts where it is allocated.
    let slot_ty = tir::ptr::PtrType::typed(&context, i32_ty);
    let slot = tir::ptr::ops::alloca(&context, 4u64, 4u64, slot_ty)
        .state_result()
        .build();
    let allocated = slot.state_result().expect("the allocation opens a chain");
    let store = tir::ptr::ops::store(&context, args[0], slot.result())
        .state(allocated)
        .state_result()
        .build();
    let stored = store.state_result().expect("the store publishes a state");
    let loaded = tir::ptr::ops::load(&context, slot.result(), i32_ty)
        .state(stored)
        .state_result()
        .build();
    let result = loaded.result();
    func.body().append_op(slot);
    func.body().append_op(store);
    func.body().append_op(loaded);
    func.body()
        .append_op(func_ops::r#return(&context, result).build());

    let rules = vec![
        Rule::new("load", load_pattern(), LATENCY_COST_SCALE, emit_load_marker),
        Rule::new(
            "store",
            store_pattern(),
            LATENCY_COST_SCALE,
            emit_store_marker,
        ),
    ];

    run_pass(&context, &module, func, InstructionSelectPass::new(rules))
        .expect("memory ops should select through their interfaces");

    // store -> muli marker, load -> shli marker; the alloca is untouched.
    assert_eq!(
        body_names(&context, region),
        vec!["alloca", "muli", "shli", "return"]
    );
}

/// Equivalent definitions extract to one tile.
#[test]
fn merged_value_classes_resolve_to_earliest_def() {
    use smallvec::smallvec;
    use tir::backend::isel::Theory;
    use tir_relational::{Atom, ClassId as Id, HeadOp, Plan, Query};

    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty, i32_ty], i32_ty);
    let (x, y, z) = (args[0], args[1], args[2]);

    let mul = ops::muli(&context, x, y, i32_ty).build();
    func.body().append_op(mul);
    let add = ops::addi(&context, x, y, i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    let sub = ops::subi(&context, add_result, z, i32_ty).build();
    let sub_result = sub.result();
    func.body().append_op(sub);
    func.body()
        .append_op(func_ops::r#return(&context, sub_result).build());

    // A test-only "proof" that x*y == x+y: union the Mul class with the Add
    // class over the same operands, exactly the shape a discovered algebraic
    // bridge produces.
    let template = |kind, class| {
        let mut node = template_node(kind, None, None);
        node.children = vec![Id::from_raw(1), Id::from_raw(2)];
        Atom::Node {
            template: node,
            args: smallvec![1, 2],
            class,
            row: None,
        }
    };
    let union_mul_add = tir_relational::Rule {
        name: "mul-equals-add".to_string(),
        plan: Plan::compile(Query::tree(
            4,
            0,
            vec![template(SymKind::Mul, 0), template(SymKind::Add, 3)],
        )),
        head: vec![HeadOp::Union(0, 3)],
        head_vars: 0,
        post_saturation: false,
    };

    fn emit_sub_bound(
        context: &Context,
        req: &EmitRequest,
        m: &RuleMatch,
    ) -> Result<Box<dyn Operation>, PassError> {
        let lhs = m
            .value_binding(0)
            .ok_or(PassError::RewriteFailed(req.op_id()))?;
        let rhs = m
            .value_binding(1)
            .ok_or(PassError::RewriteFailed(req.op_id()))?;
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

    let mut theory = Theory::default();
    theory.push_rule(union_mul_add);
    let pass = InstructionSelectPass::new(rules).with_theory(theory);
    run_pass(&context, &module, func, pass).expect("merged classes should still select");

    let block_ref = context
        .get_region(region)
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

/// At *equal* cost, the type-constrained rule must win the tie via dominance
/// pruning — specificity never reaches the PBQP objective.
#[test]
fn equal_cost_tie_breaks_to_more_specific_rule() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);
    let (module, func, region, args) = function(&context, &[i32_ty, i32_ty], i32_ty);

    let add = ops::addi(&context, args[0], args[1], i32_ty).build();
    let add_result = add.result();
    func.body().append_op(add);
    func.body()
        .append_op(func_ops::r#return(&context, add_result).build());

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

    select(&context, &module, func, rules);

    assert_eq!(body_names(&context, region), vec!["subi", "return"]);
}

/// A demanded class is extracted in the block containing its live definition.
#[test]
fn refuses_cross_block_binding_to_non_escaping_value() {
    let context = Context::with_default_dialects();
    let i64_ty = IntegerType::new(&context, 64);
    let module = ops::module(&context, None).build();
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

    let func = func_ops::lambda(&context, "demo", i64_ty, &region).build();

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
    run_pass(&context, &module, func, InstructionSelectPass::new(rules))
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
