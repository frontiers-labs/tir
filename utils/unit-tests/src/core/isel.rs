//! Instruction selection: synthesized covers, immediates, materializers,
//! saturation-backed lowering and memory interfaces.

use tir::{
    builtin::{IntegerType, ModuleOp},
    func::FuncOp,
    sem::{SemGraph, SymKind},
    Context, Operation, PassError, PassManager, RegionId, TypeId,
};

use tir::backend::isel::{
    BranchEmitters, EmitRequest, Emitter, ImmRange, InstructionSelectPass, OperandConstraint,
    RegisterCapability, RegisterRequirement, Rule, RuleEmitFn, RuleMatch, LATENCY_COST_SCALE,
};

use super::fixtures::{self, atomic_pattern, binary, marker_op, symbol};

// The instructions the test rules emit. A selection rule emits a machine
// instruction, whose operands are registers rather than typed mid-end values,
// so these markers say only what the assertions read: the mnemonic and the
// order they were emitted in.
marker_op!(ShlMarkerOp, "shli");
marker_op!(ShrsMarkerOp, "shrsi");
marker_op!(AddMarkerOp, "addi");
marker_op!(SubMarkerOp, "subi");
marker_op!(MulMarkerOp, "muli");

tir::helpers::dialect! {
    TestDialect {
        name: "test",
        operations: [ShlMarkerOp, ShrsMarkerOp, AddMarkerOp, SubMarkerOp, MulMarkerOp],
    }
}

/// Build one of the marker instructions over `lhs` and `rhs`, producing `result_ty`.
macro_rules! marker {
    ($op:ident, $builder:ident, $context:expr, $lhs:expr, $rhs:expr, $result_ty:expr) => {{
        $op::register_interfaces($context);
        Box::new(
            $builder::new($context)
                .a($lhs)
                .b($rhs)
                .result_types(vec![$result_ty])
                .build(),
        )
    }};
}

/// The parsed module of `source`, its function's body region, and the dialect
/// the test rules emit into registered.
fn function(source: &str) -> (Context, ModuleOp, RegionId) {
    let (context, module, _, region) = fixtures::parse_function(source);
    context.register_dialect::<TestDialect>();
    (context, module, region)
}

/// Runs `pass` nested on the module's functions.
fn run_pass(
    context: &Context,
    module: &ModuleOp,
    pass: InstructionSelectPass,
) -> Result<(), PassError> {
    let mut pm = PassManager::new();
    let functions = pm.nest::<FuncOp>();
    functions.add_pass(tir::passes::RestructureNodesPass::new());
    functions.add_boxed_pass(Box::new(pass));
    pm.run(context, context.get_op(module.id()))
}

fn selection_pass(context: &Context, rules: Vec<Rule>) -> InstructionSelectPass {
    selection_pass_with(InstructionSelectPass::new(context, rules), context)
}

fn selection_pass_with(pass: InstructionSelectPass, context: &Context) -> InstructionSelectPass {
    pass.with_branch_emitters(BranchEmitters {
        uncond: |_, _, _| unreachable!("the bodies have no branches"),
        cond_nonzero: |_, _, _| unreachable!("the bodies have no branches"),
    })
    .synthesized(context)
}

fn select(context: &Context, module: &ModuleOp, rules: Vec<Rule>) {
    run_pass(context, module, selection_pass(context, rules))
        .expect("pass pipeline should succeed");
}

/// The op names of the selected body, in the order selection left them.
/// Returns are left to the target's op lowerings, which these tests lack.
fn body_names(context: &Context, region: RegionId) -> Vec<&'static str> {
    body_ops(context, region)
        .iter()
        .map(|op| op.name().as_str())
        .filter(|name| *name != "return")
        .collect()
}

fn body_ops(context: &Context, region: RegionId) -> Vec<tir::OpHandle> {
    context
        .get_region(region)
        .op_ids()
        .into_iter()
        .map(|op_id| context.get_op(op_id))
        .collect()
}

/// `demo(%a, %b) -> a + b`, the smallest body a rule over `Add` can cover.
const ADD_OF_TWO_ARGUMENTS: &str = r#"module {
func.func @demo(%a: !i32, %b: !i32) -> !i32 {
  %add = addi %a, %b : !i32
  func.return %add
}
module_end
}"#;

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
    let lhs = m
        .value_binding(0)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let rhs = m
        .value_binding(2)
        .or_else(|| m.value_binding(1))
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(marker!(
        AddMarkerOp,
        AddMarkerOpBuilder,
        context,
        lhs,
        rhs,
        result_ty
    ))
}

fn emit_mul(
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
    Ok(marker!(
        MulMarkerOp,
        MulMarkerOpBuilder,
        context,
        lhs,
        rhs,
        result_ty
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
    Ok(marker!(
        SubMarkerOp,
        SubMarkerOpBuilder,
        context,
        lhs,
        rhs,
        result_ty
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
fn fused_rule_consumes_internal_nodes_of_selected_pattern() {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%x: !i32, %y: !i32, %z: !i32) -> !i32 {
  %mul = muli %x, %y : !i32
  %add = addi %mul, %z : !i32
  func.return %add
}
module_end
}"#,
    );

    select(&context, &module, add_mul_rules());

    assert_eq!(body_names(&context, region), vec!["addi"]);
}

/// A rule whose register class views its storage element at a nonzero bit
/// offset (x86 `ah`) is only compatible with values living at that same offset:
/// no instruction moves bits across views. Ordinary IR values live at offset 0,
/// so such a rule may neither read them as operands nor define them.
#[test]
fn shifted_register_view_rule_does_not_select_for_offset_zero_values() {
    let run = |operand_offset: u32, result_offset: u32| {
        let (context, module, region) = function(ADD_OF_TWO_ARGUMENTS);

        let capability = RegisterCapability::integer(32);
        let operand = RegisterRequirement::low_bits(capability).at_view_offset(operand_offset);
        let plain = RegisterRequirement::low_bits(capability);
        let rules = vec![
            // The cheaper rule, distinguished by emitting `muli`.
            Rule {
                operand_registers: vec![(0, operand), (1, operand)],
                result_register: Some(
                    RegisterRequirement::low_bits(capability).at_view_offset(result_offset),
                ),
                ..Rule::new(
                    "shifted-add",
                    atomic_pattern(SymKind::Add),
                    LATENCY_COST_SCALE,
                    emit_mul,
                )
            },
            Rule {
                operand_registers: vec![(0, plain), (1, plain)],
                result_register: Some(plain),
                ..Rule::new(
                    "add",
                    atomic_pattern(SymKind::Add),
                    10 * LATENCY_COST_SCALE,
                    emit_add,
                )
            },
        ];

        select(&context, &module, rules);
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
fn operation_no_rule_covers_is_rejected() {
    // A standalone Mul that no rule can root and no parent match can consume:
    // selection fails naming the operation.
    let (context, module, _region) = function(
        r#"module {
func.func @demo(%x: !i32, %y: !i32) -> !i32 {
  %mul = muli %x, %y : !i32
  func.return %mul
}
module_end
}"#,
    );

    let rules = vec![Rule::new(
        "add",
        atomic_pattern(SymKind::Add),
        10 * LATENCY_COST_SCALE,
        emit_add,
    )];

    let err = run_pass(&context, &module, selection_pass(&context, rules))
        .expect_err("incomplete rule set should be rejected");
    assert!(err.to_string().contains("builtin.muli has no instruction"));
}

/// A pure subexpression shared by two fused matches is *duplicated*: each
/// add-mul instruction recomputes the mul internally, and the mul op — no
/// longer needed as a register value — is consumed.
#[test]
fn fused_rule_duplicates_shared_pure_internal_nodes() {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%x: !i32, %y: !i32, %z: !i32) -> !i32 {
  %mul = muli %x, %y : !i32
  %add0 = addi %mul, %z : !i32
  %add1 = addi %mul, %add0 : !i32
  func.return %add1
}
module_end
}"#,
    );

    select(&context, &module, add_mul_rules());

    assert_eq!(body_names(&context, region), vec!["addi", "addi"]);
}

/// An unused consumer does not demand its result or operands.
#[test]
fn unused_consumer_does_not_create_demand() {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%x: !i32, %y: !i32, %z: !i32) -> !i32 {
  %mul = muli %x, %y : !i32
  %dead = addi %mul, %z : !i32
  func.return %mul
}
module_end
}"#,
    );

    select(&context, &module, add_mul_rules());

    assert_eq!(body_names(&context, region), vec!["muli"]);
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

#[test]
fn composite_rule_falls_back_to_atomic_cover() {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%a: !i32, %b: !i32, %c: !i32, %d: !i32) -> !i32 {
  %add0 = addi %a, %b : !i32
  %mul = muli %add0, %c : !i32
  %add1 = addi %mul, %d : !i32
  func.return %add1
}
module_end
}"#,
    );

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

    select(&context, &module, rules);

    assert_eq!(body_names(&context, region), vec!["addi", "muli", "addi"]);
}

/// A binary pattern constrained to a specific result type via the pattern
/// graph's actual-type annotation (the channel a typed rule would use).
fn typed_binary_pattern(kind: SymKind, ty: TypeId) -> SemGraph {
    let mut g = SemGraph::new();
    let lhs = symbol(&mut g, 0);
    let rhs = symbol(&mut g, 1);
    let root = binary(&mut g, kind, lhs, rhs);
    g.annotation_mut(root).actual_type = Some(ty);
    g
}

#[test]
fn unused_typed_operation_is_not_selected() {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%a32: !i32, %b32: !i32, %a64: !i64, %b64: !i64) -> !i64 {
  %add32 = addi %a32, %b32 : !i32
  %add64 = addi %a64, %b64 : !i64
  func.return %add64
}
module_end
}"#,
    );
    let i32_ty = IntegerType::new(&context, 32);

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

    select(&context, &module, rules);

    assert_eq!(body_names(&context, region), vec!["addi"]);
}

/// Build `add(add(a,b), c)` over i32 values and select it with a fused
/// `Add(Add(_,_),_)` rule whose *internal* node carries `inner_width` as a type
/// constraint (plus an untyped atomic `add` fallback). Returns the lowered op
/// names. Fusion (the `subi` marker) only happens when the inner constraint
/// agrees with the inferred i32 type of the inner add.
fn run_inner_typed_fusion(inner_width: Option<u32>) -> Vec<&'static str> {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%a: !i32, %b: !i32, %c: !i32) -> !i32 {
  %add0 = addi %a, %b : !i32
  %add1 = addi %add0, %c : !i32
  func.return %add1
}
module_end
}"#,
    );

    // Fused pattern Add(Add(s0, s1), s2); optionally constrain the inner Add.
    let mut pattern = SemGraph::new();
    let s0 = symbol(&mut pattern, 0);
    let s1 = symbol(&mut pattern, 1);
    let inner = binary(&mut pattern, SymKind::Add, s0, s1);
    let s2 = symbol(&mut pattern, 2);
    binary(&mut pattern, SymKind::Add, inner, s2);
    if let Some(width) = inner_width {
        pattern.annotation_mut(inner).actual_type =
            Some(IntegerType::new(&context, u64::from(width)));
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

    select(&context, &module, rules);
    body_names(&context, region)
}

#[test]
fn internal_node_type_constraint_is_enforced() {
    // Inner add inferred as i32 from i32 operands. A matching i32 constraint
    // (or no constraint) lets the fused rule consume it; an i64 constraint
    // forbids the match, falling back to two atomic adds.
    assert_eq!(run_inner_typed_fusion(Some(32)), vec!["subi"]);
    assert_eq!(run_inner_typed_fusion(None), vec!["subi"]);
    assert_eq!(run_inner_typed_fusion(Some(64)), vec!["addi", "addi"]);
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
    Ok(marker!(
        SubMarkerOp,
        SubMarkerOpBuilder,
        context,
        lhs,
        lhs,
        result_ty
    ))
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
    let result_ty = req.result_ty.expect("typed result");
    MulMarkerOp::register_interfaces(context);
    Ok(Box::new(
        MulMarkerOpBuilder::new(context)
            .result_types(vec![result_ty])
            .build(),
    ))
}

fn materializer_rule(emit: RuleEmitFn) -> Rule {
    Rule {
        operand_constraints: vec![(1, OperandConstraint::Immediate)],
        operand_imm_ranges: vec![(
            1,
            ImmRange {
                width: 12,
                signed: true,
                align: 1,
                nonzero: false,
            },
        )],
        ..Rule::new(
            "li",
            zero_materializer_pattern(),
            5 * LATENCY_COST_SCALE,
            emit,
        )
    }
}

fn emit_integer_materializer_marker(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let ty = req.result_ty.ok_or(PassError::RewriteFailed(req.op_id()))?;
    if IntegerType::view(context, ty).is_none() {
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

#[test]
fn introduced_integer_materializer_uses_its_class_type_under_float_bitcast() {
    let (context, module, _region) = function(
        r#"module {
func.func @demo() -> !f32 {
  %value = fp.constant {bits = 0} : !f32
  func.return %value
}
module_end
}"#,
    );

    let rules = vec![
        Rule {
            float_constant_width: Some(32),
            ..Rule::new(
                "bitcast",
                bitcast_pattern(),
                LATENCY_COST_SCALE,
                emit_materializer_marker,
            )
        },
        materializer_rule(emit_integer_materializer_marker),
    ];

    run_pass(&context, &module, selection_pass(&context, rules))
        .expect("integer materialization under a bitcast should stay integer typed");
}

#[test]
fn constant_materialization_skips_unevaluated_shift_operands() {
    for kind in [SymKind::ShiftLeft, SymKind::ShiftRightLogic] {
        let (context, module, region) = function(
            r#"module {
func.func @demo() -> !i64 {
  %value = constant {value = 7} : !i64
  func.return %value
}
module_end
}"#,
        );
        let mut pattern = SemGraph::new();
        let value = symbol(&mut pattern, 0);
        let divisor = fixtures::constant(&mut pattern, 2, 64);
        let quotient = binary(&mut pattern, SymKind::UDiv, value, divisor);
        let amount = symbol(&mut pattern, 1);
        binary(&mut pattern, kind, quotient, amount);
        let rules = vec![
            Rule {
                operand_constraints: vec![(1, OperandConstraint::Immediate)],
                ..Rule::new("div-shift", pattern, 0, emit_materializer_marker)
            },
            materializer_rule(emit_materializer_marker),
        ];

        select(&context, &module, rules);
        assert_eq!(body_names(&context, region), vec!["muli"]);
    }
}

/// Select `add(a, constant)` with a cheap immediate rule bounded to a signed
/// 12-bit field (`subi` marker) and an expensive register-form fallback.
fn run_immediate_range(constant: i64) -> Vec<&'static str> {
    let (context, module, region) = function(&format!(
        r#"module {{
func.func @demo(%a: !i64) -> !i64 {{
  %c = constant {{value = {constant}}} : !i64
  %add = addi %a, %c : !i64
  func.return %add
}}
module_end
}}"#
    ));

    let rules = vec![
        Rule {
            operand_constraints: vec![(1, OperandConstraint::Immediate)],
            operand_imm_ranges: vec![(
                1,
                ImmRange {
                    width: 12,
                    signed: true,
                    align: 1,
                    nonzero: false,
                },
            )],
            ..Rule::new(
                "addi",
                atomic_pattern(SymKind::Add),
                LATENCY_COST_SCALE,
                emit_add_imm_marker,
            )
        },
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            10 * LATENCY_COST_SCALE,
            emit_add,
        ),
    ];

    select(&context, &module, rules);
    body_names(&context, region)
}

#[test]
fn immediate_range_gates_immediate_rules() {
    // The signed 12-bit boundaries fold into the immediate form; the constant
    // op is swept.
    assert_eq!(run_immediate_range(2047), vec!["subi"]);
    assert_eq!(run_immediate_range(-2048), vec!["subi"]);
    // One past either boundary must not bind the immediate rule whole: the
    // field's extreme and the one left over are two immediates that fit.
    assert_eq!(run_immediate_range(2048), vec!["subi", "subi"]);
    assert_eq!(run_immediate_range(-2049), vec!["subi", "subi"]);
}

fn shift_imm_pattern(kind: SymKind) -> SemGraph {
    let mut g = SemGraph::new();
    let rs1 = symbol(&mut g, 0);
    let imm = symbol(&mut g, 1);
    binary(&mut g, kind, rs1, imm);
    g
}

/// Emit the marker instruction standing for `kind` over the match's register
/// operand. The shift amount is an immediate (`m.int_binding(1)`); operands
/// beyond the mnemonic don't matter here, so the source register is reused.
fn emit_marker(
    kind: SymKind,
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    let rs1 = m
        .value_binding(0)
        .ok_or(PassError::RewriteFailed(req.op_id()))?;
    let result_ty = req.result_ty.expect("typed result");
    Ok(match kind {
        SymKind::ShiftLeft => marker!(
            ShlMarkerOp,
            ShlMarkerOpBuilder,
            context,
            rs1,
            rs1,
            result_ty
        ),
        _ => marker!(
            ShrsMarkerOp,
            ShrsMarkerOpBuilder,
            context,
            rs1,
            rs1,
            result_ty
        ),
    })
}

fn emit_slli(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    emit_marker(SymKind::ShiftLeft, context, req, m)
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
        value,
        result_ty
    ))
}

fn emit_srai(
    context: &Context,
    req: &EmitRequest,
    m: &RuleMatch,
) -> Result<Box<dyn Operation>, PassError> {
    emit_marker(SymKind::ShiftRightArithmetic, context, req, m)
}

fn select_sign_extension(slli_rule: Rule) -> Vec<&'static str> {
    let (context, module, region) = function(
        r#"module {
func.func @demo(%a: !i16, %b: !i16) -> !i64 {
  %add = addi %a, %b : !i16
  %ext = extsi %add : !i64
  func.return %ext
}
module_end
}"#,
    );

    let rules = vec![
        Rule::new(
            "add",
            atomic_pattern(SymKind::Add),
            LATENCY_COST_SCALE,
            emit_add,
        ),
        slli_rule,
        Rule {
            operand_constraints: vec![(1, OperandConstraint::Immediate)],
            ..Rule::new(
                "srai",
                shift_imm_pattern(SymKind::ShiftRightArithmetic),
                LATENCY_COST_SCALE,
                emit_srai,
            )
        },
    ];

    run_pass(&context, &module, selection_pass(&context, rules))
        .expect("sign extension should select");
    body_names(&context, region)
}

/// End-to-end square: `extsi(addi(a, b) : i16) : i64` lowers to `add, slli, srai`.
/// The `add` covers the addi; saturation bridges the un-selectable sign extension
/// into a `slli`/`srai` pair, and multi-instruction emission materializes the
/// introduced `slli` (an e-class with no original op) before the `srai`.
#[test]
fn square_sign_extension_lowers_to_shift_pair() {
    let slli_rule = Rule {
        operand_constraints: vec![(1, OperandConstraint::Immediate)],
        ..Rule::new(
            "slli",
            shift_imm_pattern(SymKind::ShiftLeft),
            LATENCY_COST_SCALE,
            emit_slli,
        )
    };
    let body_ops = select_sign_extension(slli_rule);

    // add (from the addi), then the slli/srai sign-extension idiom, then return.
    assert_eq!(body_ops, vec!["addi", "shli", "shrsi"]);
}

#[test]
fn introduced_rule_emits_its_plan_in_order() {
    let slli_rule = Rule {
        operand_constraints: vec![(1, OperandConstraint::Immediate)],
        emit: vec![
            Emitter::Custom(emit_shift_prelude),
            Emitter::Custom(emit_slli),
        ],
        ..Rule::new(
            "slli",
            shift_imm_pattern(SymKind::ShiftLeft),
            LATENCY_COST_SCALE,
            emit_slli,
        )
    };
    let body_ops = select_sign_extension(slli_rule);

    assert_eq!(body_ops, vec!["addi", "subi", "shli", "shrsi"]);
}

/// At *equal* cost, the type-constrained rule must win the tie.
#[test]
fn equal_cost_tie_breaks_to_more_specific_rule() {
    let (context, module, region) = function(ADD_OF_TWO_ARGUMENTS);
    let i32_ty = IntegerType::new(&context, 32);

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

    select(&context, &module, rules);

    assert_eq!(body_names(&context, region), vec!["subi"]);
}

/// A value recomputed in another block of the CFG is one class once the body
/// is unordered: the add binds the single selected definition, and the
/// recomputation nothing reads is swept.
#[test]
fn recomputation_across_blocks_binds_to_its_selected_definition() {
    // %d = a - b is used only within the entry block, so it never escapes;
    // %e = a - b recomputes the same expression (CSE-merged with %d) and the
    // add consumes it, resolving its operand under the binding rule.
    let (context, module, region) = function(
        r#"module {
func.func @demo(%a: !i64, %b: !i64, %m: !i64) -> !i64 {
  %d = subi %a, %b : !i64
  %g = subi %d, %m : !i64
  cfg.br ^bb1
^bb1:
  %e = subi %a, %b : !i64
  %r = addi %e, %m : !i64
  func.return %r
}
module_end
}"#,
    );

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
    run_pass(&context, &module, selection_pass(&context, rules)).expect("selection should succeed");

    let body = body_ops(&context, region);
    assert_eq!(body_names(&context, region), vec!["subi", "addi"]);
    let sub = body[0].results()[0];
    assert!(
        body[1].operands().contains(&sub),
        "the add binds the selected recomputation"
    );
}
