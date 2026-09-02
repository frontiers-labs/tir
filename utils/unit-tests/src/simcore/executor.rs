use tir::backend::AsmDialect;
use tir::utils::APInt;
use tir::Context;
use tir_riscv::RiscvDialect;
use tir_sim::{Executor, MemAccess, MemAccessKind, ProgramImage};

fn riscv_program(context: &Context, asm: &str) -> ProgramImage {
    context.register_dialect::<AsmDialect>();
    context.register_dialect::<RiscvDialect>();
    let dialect = context.find_dialect::<RiscvDialect>().unwrap();
    let module = dialect.get_asm_parser().parse_asm(context, asm).unwrap();
    ProgramImage::from_module(context, module, 0x8000_0000, Some("first")).unwrap()
}

#[test]
fn mem_trace_records_loads_and_stores_parallel_to_trace() {
    use tir::backend::MachineContext;

    let context = Context::with_default_dialects();
    // Reverse declaration order: `first` executes at 0x8000_0000 and falls
    // through to `last` at 0x8000_000c after three instructions.
    let program = riscv_program(
        &context,
        "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              lw  x2, 0(x1)
              sw  x2, 4(x1)
              add x3, x2, x2
        ",
    );

    let base = 0x8000_0000;
    let data = base + 0x100;
    let mut executor = Executor::new_at(4096, base);
    executor.enable_trace_recording();
    MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, data)).unwrap();
    MachineContext::write_memory(&mut executor, data, 4, 0x1234_5678).unwrap();
    executor.load(program).unwrap();
    executor.run(0x8000_000c, 10).unwrap();

    assert_eq!(executor.trace().len(), 3);
    assert_eq!(executor.mem_trace().len(), executor.trace().len());
    assert_eq!(
        executor.mem_trace()[0],
        vec![MemAccess {
            addr: data,
            size: 4,
            is_write: false,
            ..Default::default()
        }]
    );
    assert_eq!(
        executor.mem_trace()[1],
        vec![MemAccess {
            addr: data + 4,
            size: 4,
            is_write: true,
            ..Default::default()
        }]
    );
    assert!(executor.mem_trace()[2].is_empty(), "add touches no memory");
}

#[test]
fn pc_advances_by_each_instructions_encoded_length() {
    // A compressed instruction is two bytes and an uncompressed one four, so
    // the program counter follows what each instruction encodes to rather than
    // one width per opcode.
    let context = Context::with_default_dialects();
    let program = riscv_program(
        &context,
        "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              c.addi a0, 1
              add    a1, a0, a0
              c.addi a0, 1
        ",
    );

    let base = 0x8000_0000;
    let mut executor = Executor::new_at(4096, base);
    executor.enable_trace_recording();
    executor.load(program).unwrap();
    executor.run(base + 8, 10).unwrap();

    let pcs: Vec<u64> = executor.trace().iter().map(|(_, pc)| *pc).collect();
    assert_eq!(pcs, vec![base, base + 2, base + 6]);
}

#[test]
fn mem_trace_records_atomic_kinds() {
    let context = Context::with_default_dialects();
    let program = riscv_program(
        &context,
        "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              lr.w      t0, (a0)
              sc.w      t1, t0, (a0)
              amoadd.w  t2, t0, (a0)
        ",
    );

    let base = 0x8000_0000;
    let mut executor = Executor::new_at(4096, base);
    executor.enable_trace_recording();
    tir::backend::MachineContext::write_register(&mut executor, "GPR", 10, APInt::new(64, base))
        .unwrap();
    executor.load(program).unwrap();
    executor.run(0x8000_000c, 10).unwrap();

    let kinds: Vec<_> = executor
        .mem_trace()
        .iter()
        .flatten()
        .map(|access| access.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            MemAccessKind::LoadReserved,
            MemAccessKind::StoreConditional { success: true },
            // The AMO records its read-modify-write plus the sc's store.
            MemAccessKind::AtomicRmw,
        ]
    );
}

#[test]
fn fence_records_kind_and_changes_nothing() {
    use tir::backend::MachineContext;

    let context = Context::with_default_dialects();
    let program = riscv_program(
        &context,
        "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              fence 3, 3
              fence.i
        ",
    );

    let mut executor = Executor::new_at(4096, 0x8000_0000);
    executor.enable_trace_recording();
    MachineContext::write_register(&mut executor, "GPR", 1, APInt::new(64, 7)).unwrap();
    executor.load(program).unwrap();
    executor.run(0x8000_0008, 10).unwrap();

    let kinds: Vec<_> = executor
        .mem_trace()
        .iter()
        .flatten()
        .map(|access| access.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            MemAccessKind::Fence {
                pred: 0b0011,
                succ: 0b0011,
                ifence: false,
            },
            MemAccessKind::Fence {
                pred: 0,
                succ: 0,
                ifence: true,
            },
        ]
    );
    assert_eq!(
        MachineContext::read_register(&executor, "GPR", 1)
            .unwrap()
            .to_u64(),
        7,
        "fence leaves architectural state untouched"
    );
}

#[test]
fn exception_handler_controls_run_outcome() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let context = Context::with_default_dialects();
    let program = riscv_program(
        &context,
        "
            .global last
            last:
              add x0, x0, x0
            .global first
            first:
              ecall
              addi x1, x0, 7
              ebreak
              addi x2, x0, 9
        ",
    );

    let traps = Rc::new(RefCell::new(Vec::new()));
    let seen = traps.clone();
    let mut executor = Executor::new(4096);
    executor.set_exception_handler(Box::new(move |_executor, cause, pc| {
        seen.borrow_mut().push((cause, pc));
        // Resume after the ecall, stop at the ebreak.
        if cause == 11 {
            tir_sim::ExceptionAction::Continue
        } else {
            tir_sim::ExceptionAction::Halt
        }
    }));
    executor.load(program).unwrap();
    executor.run(0x8000_0010, 10).unwrap();

    assert!(executor.halted());
    assert_eq!(
        *traps.borrow(),
        vec![(11, 0x8000_0000), (3, 0x8000_0008)],
        "handler saw the ecall and the ebreak with their PCs"
    );
    let reg = |idx| {
        tir::backend::MachineContext::read_register(&executor, "GPR", idx)
            .unwrap()
            .to_u64()
    };
    assert_eq!(reg(1), 7, "execution resumed after the ecall");
    assert_eq!(reg(2), 0, "the halt stopped execution at the ebreak");
}

/// `cmp` writes all four AArch64 condition flags (`PSTATE` n/z/c/v), and a
/// conditional branch reads them back. Both used to be silently dropped: the
/// multi-assignment behaviors only emitted one write (or none), and flag paths
/// could not be lowered at all. Flags live in a register class with index-less
/// registers, so this also exercises the canonical-index support that ports to
/// any target with status/flag registers.
#[test]
fn arm64_compare_sets_flags_and_conditional_branch_reads_them() {
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::backend::{MachineContext, MachineInstruction};
    use tir::Operation;

    fn gpr(index: u16) -> AttributeValue {
        AttributeValue::Register(RegisterAttr::Physical {
            class: tir_arm64::RegClass::GPR.id(),
            index,
        })
    }

    // PSTATE flag slots, assigned by declaration order in the register class.
    const N: u16 = 0;
    const Z: u16 = 1;
    const C: u16 = 2;
    const V: u16 = 3;

    let context = Context::with_default_dialects();
    context.register_dialect::<tir::backend::AsmDialect>();
    context.register_dialect::<tir_arm64::Arm64Dialect>();

    let exec_cmp = |x0: u64, x1: u64| -> Executor {
        let mut ex = Executor::new(64);
        MachineContext::write_register(&mut ex, "GPR", 0, APInt::new(64, x0)).unwrap();
        MachineContext::write_register(&mut ex, "GPR", 1, APInt::new(64, x1)).unwrap();
        let cmp = tir_arm64::CompareOpBuilder::new(&context)
            .attr("rn", gpr(0))
            .attr("rm", gpr(1))
            .build();
        let mi = context
            .get_op(cmp.id())
            .as_interface::<dyn MachineInstruction>()
            .expect("cmp is a machine instruction");
        mi.execute(&mut ex).expect("cmp executes");
        ex
    };
    let flag = |ex: &Executor, idx: u16| {
        MachineContext::read_register(ex, "PSTATE", idx)
            .unwrap()
            .to_u64()
    };

    // Equal operands: Z and C set, N and V clear.
    let eq = exec_cmp(5, 5);
    assert_eq!(flag(&eq, Z), 1, "Z set when operands are equal");
    assert_eq!(flag(&eq, N), 0);
    assert_eq!(flag(&eq, C), 1, "C set: 5 >=u 5");
    assert_eq!(flag(&eq, V), 0);

    // 5 - 7 is negative and borrows: N set, Z and C clear.
    let lt = exec_cmp(5, 7);
    assert_eq!(flag(&lt, Z), 0);
    assert_eq!(flag(&lt, N), 1, "N set: 5 - 7 is negative");
    assert_eq!(flag(&lt, C), 0, "C clear: 5 <u 7");

    // A b.eq reads Z: taken when set, fall-through (pc + 4) when clear.
    let run_beq = |z: u64| -> u64 {
        let mut ex = Executor::new(64);
        MachineContext::write_pc(&mut ex, 0x1000);
        MachineContext::write_register(&mut ex, "PSTATE", Z, APInt::new(1, z)).unwrap();
        let beq = tir_arm64::BranchEqOpBuilder::new(&context)
            .attr("imm", AttributeValue::Int(4))
            .build();
        let mi = context
            .get_op(beq.id())
            .as_interface::<dyn MachineInstruction>()
            .expect("b.eq is a machine instruction");
        mi.execute(&mut ex).expect("b.eq executes");
        MachineContext::read_pc(&ex)
    };
    // imm=4, target = pc + (sext(imm) << 2) = 0x1000 + 16.
    assert_eq!(run_beq(1), 0x1010, "branch taken when Z is set");
    assert_eq!(
        run_beq(0),
        0x1004,
        "fall-through (pc + width) when Z is clear"
    );

    // `bl` writes two destinations: the link register (x30 = pc + 4) and PC.
    // Both used to be dropped because only one assignment was ever emitted.
    let mut ex = Executor::new(64);
    MachineContext::write_pc(&mut ex, 0x2000);
    let bl = tir_arm64::BranchLinkOpBuilder::new(&context)
        .attr("imm", AttributeValue::Int(3))
        .build();
    let mi = context
        .get_op(bl.id())
        .as_interface::<dyn MachineInstruction>()
        .expect("bl is a machine instruction");
    mi.execute(&mut ex).expect("bl executes");
    let x30 = MachineContext::read_register(&ex, "GPR", 30)
        .unwrap()
        .to_u64();
    assert_eq!(
        x30, 0x2004,
        "link register holds the return address (pc + 4)"
    );
    assert_eq!(
        MachineContext::read_pc(&ex),
        0x2000 + (3 << 2),
        "pc takes the branch target"
    );
}
