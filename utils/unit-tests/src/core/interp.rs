use proptest::prelude::*;
use tir::interp::{
    APInt, Args, Block, Builder, Const, Edge, EdgeId, Fault, Func, Function, Inst, Inst::*,
    Outcome, Reg,
};

fn block(params: &[Reg], code: Vec<Inst>) -> Block {
    Block {
        params: params.to_vec(),
        code,
    }
}

fn function(widths: &[u32], result_width: u32, blocks: Vec<Block>) -> Function {
    Function {
        widths: widths.to_vec(),
        result_width,
        blocks,
        edges: vec![],
        args: vec![],
    }
}

fn returned(outcome: Outcome<'_>) -> APInt {
    match outcome {
        Outcome::Returned(v) => v,
        Outcome::Load(_) => panic!("unexpected load"),
    }
}

fn swap_program() -> tir::interp::Program {
    let [x, y, n, one, zero, done] = [0, 1, 2, 3, 4, 5].map(Reg);
    let mut f = function(
        &[8, 8, 8, 8, 8, 1],
        8,
        vec![
            block(
                &[x, y, n],
                vec![
                    Constant(zero, Const(0)),
                    Eq(done, n, zero),
                    Branch(done, EdgeId(0), EdgeId(1)),
                ],
            ),
            block(
                &[x, y, n],
                vec![Constant(one, Const(1)), Sub(n, n, one), Jump(EdgeId(2))],
            ),
            block(&[x, y], vec![Sub(x, x, y), Return(x)]),
        ],
    );
    f.args = vec![vec![x, y], vec![x, y, n], vec![y, x, n]];
    f.edges = vec![
        Edge {
            block: 2,
            args: Args(0),
        },
        Edge {
            block: 1,
            args: Args(1),
        },
        Edge {
            block: 0,
            args: Args(2),
        },
    ];
    Builder {
        functions: vec![f],
        constants: vec![APInt::zero(8), APInt::one(8)],
    }
    .build()
    .unwrap()
}

proptest! {
    #[test]
    fn loop_transfers_both_swap_operands_before_writing(a in any::<u8>(), b in any::<u8>(), turns in 0u8..16) {
        let program = swap_program();
        let args = [a, b, turns].map(|n| APInt::new(8, n.into())).into();
        let actual = returned(program.run(Func(0), args, 128).unwrap());
        let expected = if turns % 2 == 0 { a.wrapping_sub(b) } else { b.wrapping_sub(a) };
        prop_assert_eq!(actual, APInt::new(8, expected.into()));
    }
}

#[test]
fn delayed_load_resumes_inside_a_wide_integer_call() {
    let [addr, x, one] = [0, 1, 2].map(Reg);
    let mut caller = function(
        &[32, 200],
        200,
        vec![block(&[addr], vec![Call(x, Func(1), Args(0)), Return(x)])],
    );
    caller.args = vec![vec![addr]];
    let callee = function(
        &[32, 200, 200],
        200,
        vec![block(
            &[addr],
            vec![
                Load(x, addr),
                Constant(one, Const(0)),
                Add(x, x, one),
                Return(x),
            ],
        )],
    );
    let program = Builder {
        constants: vec![APInt::one(200)],
        functions: vec![caller, callee],
    }
    .build()
    .unwrap();
    let issue = |fuel| match program
        .run(Func(0), vec![APInt::new(32, 123)], fuel)
        .unwrap()
    {
        Outcome::Load(load) => load,
        Outcome::Returned(_) => panic!("missing load"),
    };
    let load = issue(6);
    assert_eq!(load.address(), &APInt::new(32, 123));
    assert_eq!(load.width(), 200);
    assert_eq!(
        returned(issue(6).resume(APInt::zero(200)).unwrap()),
        APInt::one(200)
    );
    let input = APInt::from_limbs(200, &[u64::MAX, u64::MAX, u64::MAX, 0x80]);
    let expected = APInt::from_limbs(200, &[0, 0, 0, 0x81]);
    assert_eq!(returned(load.resume(input.clone()).unwrap()), expected);
    assert_eq!(issue(5).resume(input).err(), Some(Fault::FuelExhausted));
    assert_eq!(
        issue(6).resume(APInt::zero(64)).err(),
        Some(Fault::InvalidLoadWidth)
    );
}

#[test]
fn poison_propagates_until_observed() {
    let [x, condition, p] = [0, 1, 2].map(Reg);
    for (end, flag, expected) in [
        (Return(p), 0, Ok(APInt::zero(1))),
        (Return(x), 1, Ok(APInt::one(1))),
        (
            Branch(p, EdgeId(0), EdgeId(0)),
            1,
            Err(Fault::ObservedPoison),
        ),
        (Require(condition), 0, Err(Fault::FailedPrecondition)),
        (Trap, 1, Err(Fault::Trap)),
    ] {
        let mut code = vec![PoisonIf(p, x, condition), Add(p, x, p), end];
        if matches!(end, Require(_)) {
            code.push(Return(x));
        }
        let mut f = function(
            &[1, 1, 1],
            1,
            vec![block(&[x, condition], code), block(&[x], vec![Return(x)])],
        );
        f.edges.push(Edge {
            block: 1,
            args: Args(0),
        });
        f.args.push(vec![x]);
        let program = Builder {
            functions: vec![f],
            constants: vec![],
        }
        .build()
        .unwrap();
        let result = program
            .run(Func(0), vec![APInt::one(1), APInt::new(1, flag)], 10)
            .map(returned);
        assert_eq!(result, expected);
    }
}

#[test]
fn builder_rejects_reads_missing_a_value_or_matching_width() {
    let [x, y] = [0, 1].map(Reg);
    for params in [vec![x], vec![x, y]] {
        let f = function(
            &[8, 1],
            8,
            vec![block(&params, vec![Add(x, x, y), Return(x)])],
        );
        let error = Builder {
            functions: vec![f],
            constants: vec![],
        }
        .build()
        .err()
        .unwrap();
        assert_eq!((error.block, error.instruction), (Some(0), Some(0)));
    }
}

#[test]
fn compiled_triangle_matches_before_and_after_restructuring() {
    use tir::{func::FuncOp, Operation, PassManager};
    let (cfg, graph) = {
        let (ctx, module, func, _) = super::fixtures::parse_function(include_str!(
            "../../../../core/checks/Restructure/triangle.tir"
        ));
        let cfg = tir::interp::build(&func).unwrap();
        let mut passes = PassManager::new();
        passes
            .nest::<FuncOp>()
            .add_pass(tir::passes::RestructureNodesPass::new());
        passes.run(&ctx, module.id()).unwrap();
        assert!(func.body_region().is_nodes());
        (cfg, tir::interp::build(&func).unwrap())
    };
    for program in [cfg, graph] {
        for (cond, expected) in [(0, 13), (1, 26)] {
            let args = vec![APInt::new(1, cond), APInt::new(32, 13)];
            assert_eq!(
                returned(program.run(Func(0), args, 128).unwrap()).limbs(),
                [expected]
            );
        }
    }
}

#[test]
fn compiled_loop_swaps_feedback_simultaneously() {
    let program = {
        let (_, _, func, _) = super::fixtures::parse_function(
            r#"
module {
  %f = func.func @swap(%n: !i8, %x: !i8, %y: !i8) -> !i8 {
    %zero = constant {value = 0} : !i8
    %one = constant {value = 1} : !i8
    %count, %left, %right = scf.loop (%i = %zero, %a = %x, %b = %y) {
      %more = cmpi %i, %n {predicate = "ult"} : !i1
      %next = addi %i, %one : !i8
      -> %more, %next, %b, %a, %i, %a, %b
    }
    %diff = subi %left, %right : !i8
    -> %diff
  }
  module_end
}"#,
        );
        tir::interp::build(&func).unwrap()
    };
    for n in 0..5 {
        let args = [n, 10, 20].map(|a| APInt::new(8, a)).into();
        assert_eq!(
            returned(program.run(Func(0), args, 1000).unwrap()).limbs(),
            [if n % 2 == 0 { 246 } else { 10 }]
        );
    }
}

#[test]
fn compiled_theta_loads_only_the_selected_yields() {
    let program = {
        let (_, _, func, _) = super::fixtures::parse_function(
            r#"
#layout_spec = #layout.spec<!ptr.p = {size = 64, abi = 64}>
module {layout.spec = #layout_spec} {
  %f = func.func @read(%n: !i32, %p: !ptr.p<!i32>, %q: !ptr.p<!i32>) -> !i32 {
    %zero = constant {value = 0} : !i32
    %one = constant {value = 1} : !i32
    %end, %value = scf.loop (%i = %zero, %v = %zero) {
      %more = cmpi %i, %n {predicate = "slt"} : !i1
      %next = addi %i, %one : !i32
      %a = ptr.load %p : !i32
      %b = ptr.load %q : !i32
      %acc = addi %a, %v : !i32
      %sum = addi %b, %v : !i32
      -> %more, %next, %acc, %i, %sum
    }
    -> %value
  }
  module_end
}"#,
        );
        tir::interp::build(&func).unwrap()
    };
    for n in [0, 2] {
        let mut outcome = program
            .run(
                Func(0),
                vec![APInt::new(32, n), APInt::new(64, 16), APInt::new(64, 32)],
                500,
            )
            .unwrap();
        for address in std::iter::repeat_n(16, n as usize).chain([32]) {
            let Outcome::Load(load) = outcome else {
                panic!("missing load")
            };
            assert_eq!(load.address().limbs(), [address]);
            outcome = load.resume(APInt::new(32, address)).unwrap();
        }
        assert_eq!(returned(outcome).limbs(), [16 * n + 32]);
    }
}

#[test]
fn captured_read_is_lazy_and_shared_across_iterations() {
    for capture in [
        "%captured = ptr.load %addr : !i32",
        r#"%captured = scf.loop (%inner = %zero) {
      %again = cmpi %inner, %one {predicate = "ult"} : !i1
      %word = ptr.load %addr : !i32
      -> %again, %one, %word
    }"#,
    ] {
        let program = {
            let (_, _, func, _) = super::fixtures::parse_function(
                &r#"
#layout_spec = #layout.spec<!ptr.p = {size = 64, abi = 64}>
module {layout.spec = #layout_spec} {
  %f = func.func @sum(%n: !i32, %addr: !ptr.p<!i32>) -> !i32 {
    %zero = constant {value = 0} : !i32
    %one = constant {value = 1} : !i32
    CAPTURE
    %last, %result = scf.loop (%i = %zero, %v = %zero) {
      %more = cmpi %i, %n {predicate = "ult"} : !i1
      %next = addi %i, %one : !i32
      %sum = addi %v, %captured : !i32
      -> %more, %next, %sum, %i, %v
    }
    -> %result
  }
  module_end
}"#
                .replace("CAPTURE", capture),
            );
            tir::interp::build(&func).unwrap()
        };
        let run = |n| {
            program
                .run(Func(0), vec![APInt::new(32, n), APInt::new(64, 99)], 500)
                .unwrap()
        };
        assert_eq!(returned(run(0)).limbs(), [0]);
        let Outcome::Load(load) = run(3) else {
            panic!("expected captured load")
        };
        assert_eq!(load.address().limbs(), [99]);
        assert_eq!(
            returned(load.resume(APInt::new(32, 7)).unwrap()).limbs(),
            [21]
        );
    }
}

#[test]
fn compiled_overflow_observes_the_dialect_flags() {
    for (op, flag) in [
        ("addi", "nsw"),
        ("addi", "nuw"),
        ("subi", "nsw"),
        ("subi", "nuw"),
    ] {
        let program = {
            let text=format!("module {{ %f = func.func @f(%x: !i4, %y: !i4) -> !i4 {{\n %r = {op} %x, %y {{{flag} = true}} : !i4\n func.return %r\n }}\n module_end\n}}");
            let (_, _, func, _) = super::fixtures::parse_function(&text);
            tir::interp::build(&func).unwrap()
        };
        for x in 0..16u64 {
            for y in 0..16u64 {
                let extend = |n: u64| {
                    if flag == "nsw" && n >= 8 {
                        n as i32 - 16
                    } else {
                        n as i32
                    }
                };
                let full = if op == "addi" {
                    extend(x) + extend(y)
                } else {
                    extend(x) - extend(y)
                };
                let range = if flag == "nsw" { -8..=7 } else { 0..=15 };
                let result = program.run(Func(0), vec![APInt::new(4, x), APInt::new(4, y)], 32);
                if range.contains(&full) {
                    assert_eq!(returned(result.unwrap()).limbs(), [(full & 15) as u64]);
                } else {
                    assert!(
                        matches!(result, Err(Fault::ObservedPoison)),
                        "{op} {flag} {x} {y}"
                    );
                }
            }
        }
    }
}

#[test]
fn sparse_switch_keeps_case_arguments_and_default() {
    use tir::{func::FuncOp, Operation, PassManager};
    let programs = {
        let (ctx, module, func, _) = super::fixtures::parse_function(include_str!(
            "../../../../core/checks/Restructure/switch-cases.tir"
        ));
        let before = tir::interp::build(&func).unwrap();
        let mut passes = PassManager::new();
        passes
            .nest::<FuncOp>()
            .add_pass(tir::passes::RestructureNodesPass::new());
        passes.run(&ctx, module.id()).unwrap();
        [before, tir::interp::build(&func).unwrap()]
    };
    for program in programs {
        for (x, expected) in [(10, 100), (0xffff_ffff, 200), (42, 42)] {
            assert_eq!(
                returned(program.run(Func(0), vec![APInt::new(32, x)], 128).unwrap()).limbs(),
                [expected]
            );
        }
    }
}

#[test]
fn direct_call_checks_the_callee_precondition() {
    use tir::{func::FuncOp, Operation};
    let program = {
        let (ctx, module) = super::fixtures::parse(
            r#"module {
  %callee = func.func @checked(%ok: !i1, %v: !i32) -> !i32 {
    assume %ok
    func.return %v
  }
  %caller = func.func @caller(%ok2: !i1, %a: !i32) -> !i32 {
    %r = func.call %callee(%ok2, %a : !i1, !i32) -> !i32
    func.return %r
  }
  module_end
}"#,
        );
        let caller = super::fixtures::module_ops(&ctx, module.id())
            .into_iter()
            .filter_map(|id| ctx.get_op(id).as_op::<FuncOp>())
            .last()
            .unwrap();
        tir::interp::build(&caller).unwrap()
    };
    let run = |b| program.run(Func(0), vec![APInt::new(1, b), APInt::new(32, 41)], 32);
    assert_eq!(returned(run(1).unwrap()).limbs(), [41]);
    assert!(matches!(run(0), Err(Fault::FailedPrecondition)));
}

#[test]
fn shared_predicate_load_runs_once_per_activation_in_state_order() {
    let program = {
        let (_, _, func, _) = super::fixtures::parse_function(
            r#"
#layout_spec = #layout.spec<!ptr.p = {size = 64, abi = 64}>
module {layout.spec = #layout_spec} {
  %f = func.func @read(%p: !ptr.p<!i32>, %q: !ptr.p<!i32>) -> !i32 {
    %zero = constant {value = 0} : !i32
    %entry = state.entry_state : !state<memory>
    %result, %final = scf.loop (%acc = %zero, %s = %entry) {
      %x, %read = ptr.load %p state(%s) : !i32
      %again = cmpi %x, %zero {predicate = "ne"} : !i1
      %y, %done = ptr.load %q state(%read) : !i32
      %sum = addi %acc, %x : !i32
      %sum2 = addi %sum, %y : !i32
      -> %again, %sum2, %done, %acc, %read
    }
    -> %result, %final
  }
  module_end
}"#,
        );
        tir::interp::build(&func).unwrap()
    };
    let mut outcome = program
        .run(Func(0), vec![APInt::new(64, 1), APInt::new(64, 2)], 500)
        .unwrap();
    for (address, value) in [(1, 3), (2, 7), (1, 0)] {
        let Outcome::Load(load) = outcome else {
            panic!("missing load")
        };
        assert_eq!(load.address().limbs(), [address]);
        outcome = load.resume(APInt::new(32, value)).unwrap();
    }
    assert_eq!(returned(outcome).limbs(), [10]);
}

#[test]
fn rejects_load_qualifiers_and_noninteger_results() {
    for (attr, ret) in [
        ("volatile = true", "!i32"),
        ("atomic = true", "!i32"),
        ("align = 4", "!i32"),
        ("", "!f32"),
    ] {
        let text=format!("#layout_spec = #layout.spec<!ptr.p = {{size = 64, abi = 64}}>\nmodule {{layout.spec = #layout_spec}} {{\n %f = func.func @f(%p: !ptr.p) -> {ret} {{\n %v = ptr.load %p {{{attr}}} : {ret}\n func.return %v\n }}\n module_end\n}}");
        let (_, _, func, _) = super::fixtures::parse_function(&text);
        assert!(matches!(
            tir::interp::build(&func),
            Err(tir::interp::BuildError::Unsupported(_))
        ));
    }
}
