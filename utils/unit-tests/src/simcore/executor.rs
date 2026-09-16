use tir::utils::APInt;
use tir::Context;
use tir_sim::{Executor, MemAccess, MemAccessKind};

use super::support::riscv_program;

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
        "first",
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
        "first",
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
        "first",
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
fn fence_records_its_kind() {
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
        "first",
    );

    let mut executor = Executor::new_at(4096, 0x8000_0000);
    executor.enable_trace_recording();
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
        "first",
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

/// `bl` writes two destinations: the link register (x30 = pc + 4) and PC. Both
/// used to be silently dropped, because the multi-assignment behaviors only
/// ever emitted one write.
#[test]
fn arm64_branch_link_writes_the_link_register_and_pc() {
    use tir::attributes::AttributeValue;
    use tir::backend::{MachineContext, MachineInstruction};
    use tir::Operation;

    let context = Context::with_default_dialects();
    context.register_dialect::<tir::backend::AsmDialect>();
    context.register_dialect::<tir_arm64::Arm64Dialect>();

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

#[test]
fn fault_record_preserves_previous_store_and_faulting_destination() {
    use tir::backend::MachineContext;
    let context = Context::with_default_dialects();
    let base = 0x8000_0000;
    let program = riscv_program(
        &context,
        ".global done\ndone:\n add x0, x0, x0\n.global first\nfirst:\n sw x2, 0(x1)\n lw x3, 0(x4)\n",
        "first",
    );
    let mut executor = Executor::new_at(4096, base);
    for (index, value) in [(1, base + 128), (2, 42), (3, 99), (4, base + 4096)] {
        executor
            .write_register("GPR", index, APInt::new(64, value))
            .unwrap();
    }
    executor.load(program).unwrap();
    assert!(executor.run(base + 8, 10).is_err());
    assert_eq!(executor.read_memory(base + 128, 4).unwrap(), 42);
    assert_eq!(executor.read_register("GPR", 3).unwrap().to_u64(), 99);
    let fault = executor.last_fault().expect("precise fault record");
    assert_eq!(fault.pc, base + 4);
    assert_eq!(fault.sequence, 0);
}

#[test]
fn semantic_frame_yields_load_before_destination_commit() {
    use tir::backend::exec::{
        EffectResponse, FrameYield, MemoryEffect, ResponseValue, SemanticFrame,
    };
    use tir::backend::{MachineContext, MachineInstruction};
    let context = Context::with_default_dialects();
    let program = riscv_program(&context, ".global first\nfirst:\n lw x2, 0(x1)\n", "first");
    let block = program.block_at(0x8000_0000).unwrap();
    let op = context.get_op(block.instructions[0]);
    let instruction = op.clone().as_interface::<dyn MachineInstruction>().unwrap();
    let mut executor = Executor::new(64);
    executor
        .write_register("GPR", 1, APInt::new(64, 16))
        .unwrap();
    executor
        .write_register("GPR", 2, APInt::new(64, 99))
        .unwrap();
    let mut frame =
        SemanticFrame::new(7, 0x8000_0000, instruction.as_ref(), &mut executor).unwrap();
    let FrameYield::Effect(request) = frame.resume(None).unwrap() else {
        panic!("load must yield")
    };
    assert_eq!(request.pc, 0x8000_0000);
    assert_eq!(request.id.sequence, 0);
    assert!(matches!(
        request.effect,
        MemoryEffect::Read {
            address: 16,
            size: 4
        }
    ));
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 99);
    assert!(matches!(
        frame
            .resume(Some(EffectResponse {
                id: request.id,
                result: Ok(ResponseValue::Word(42)),
            }))
            .unwrap(),
        FrameYield::Complete
    ));
    frame.commit(&mut executor).unwrap();
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 42);
}

#[test]
fn semantic_frame_describes_pair_accesses_before_execution() {
    use tir::backend::exec::SemanticFrame;
    use tir::backend::{MachineContext, MachineInstruction};
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let module = target
        .asm_parser(&context)
        .parse_asm(&context, ".global first\nfirst:\n stp x2, x3, [x1, 16]\n")
        .unwrap();
    let program = tir_sim::ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let op = context.get_op(program.blocks[0].instructions[0]);
    let instruction = op.as_interface::<dyn MachineInstruction>().unwrap();
    let mut executor = Executor::new(64);
    executor
        .write_register("GPRsp", 1, APInt::new(64, 4096))
        .unwrap();
    let frame = SemanticFrame::new(1, 0, instruction.as_ref(), &mut executor).unwrap();
    let ranges = frame.preflight().unwrap();
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0].address, 4112);
    assert_eq!(ranges[1].address, 4120);
    assert!(ranges.iter().all(|range| range.size == 8 && range.is_write));
}

#[test]
fn protected_instruction_fetch_faults_before_register_writes() {
    use tir::backend::MachineContext;
    let context = Context::with_default_dialects();
    let base = 0x8000_0000;
    let program = riscv_program(
        &context,
        ".global done\ndone:\n add x0, x0, x0\n.global first\nfirst:\n addi x2, x0, 42\n",
        "first",
    );
    let mut executor = Executor::new_at(4096, base);
    executor.load(program).unwrap();
    let (memory, space) = executor.memory_service_mut().memory_and_space_mut();
    memory
        .protect(space, base, 4, tir_sim::Permissions::READ)
        .unwrap();
    assert!(executor.run(base + 4, 10).is_err());
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 0);
    assert_eq!(executor.last_fault().unwrap().pc, base);
    assert!(matches!(
        executor.last_fault().unwrap().trap,
        tir::backend::SimTrap::MemoryFault {
            access: "fetch",
            reason: "permission denied",
            ..
        }
    ));
}

#[test]
fn decode_on_fetch_accepts_execute_only_mapping() {
    use tir::backend::MachineContext;
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let base = 0x8000_0000;
    let mut executor = Executor::new_at(4, base);
    executor
        .write_bytes(base, &0xd280_0542_u32.to_le_bytes())
        .unwrap();
    let (memory, space) = executor.memory_service_mut().memory_and_space_mut();
    memory
        .protect(space, base, 4, tir_sim::Permissions::EXECUTE)
        .unwrap();
    executor.set_decoder(context, target.instruction_decoder().unwrap());
    executor.set_entry(base);
    executor.run(base + 4, 2).unwrap();
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 42);
}

#[test]
fn decode_cache_observes_remapped_instruction_bytes() {
    use tir::backend::MachineContext;
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let mut executor = Executor::new(4);
    executor
        .write_bytes(0, &0xd280_0542_u32.to_le_bytes())
        .unwrap();
    executor.set_decoder(context, target.instruction_decoder().unwrap());
    executor.run(4, 2).unwrap();
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 42);
    let (memory, space) = executor.memory_service_mut().memory_and_space_mut();
    memory.unmap(space, 0, 4).unwrap();
    let replacement = memory.create_backing(4).unwrap();
    memory
        .map(space, 0, 4, replacement, 0, tir_sim::Permissions::ALL)
        .unwrap();
    executor
        .write_bytes(0, &0xd280_0562_u32.to_le_bytes())
        .unwrap();
    executor.set_entry(0);
    executor.run(4, 2).unwrap();
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 43);
}

#[test]
fn effect_records_include_store_visibility_order() {
    use tir::backend::MachineContext;
    let context = Context::with_default_dialects();
    let base = 0x8000_0000;
    let program = riscv_program(&context,
        ".global done\ndone:\n add x0, x0, x0\n.global first\nfirst:\n sw x2, 0(x1)\n lw x3, 0(x1)\n", "first");
    let mut executor = Executor::new_at(4096, base);
    executor
        .write_register("GPR", 1, APInt::new(64, base + 128))
        .unwrap();
    executor
        .write_register("GPR", 2, APInt::new(64, 42))
        .unwrap();
    executor.enable_effect_recording();
    executor.load(program).unwrap();
    executor.run(base + 8, 10).unwrap();
    let events = executor.effect_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].memory_order, Some(0));
    assert_eq!(events[1].memory_order, Some(1));
    assert_eq!(executor.read_register("GPR", 3).unwrap().to_u64(), 42);
}

#[test]
fn synchronous_execution_keeps_pair_destinations_provisional_on_fault() {
    use tir::backend::{MachineContext, MachineInstruction};
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let module = target
        .asm_parser(&context)
        .parse_asm(&context, ".global first\nfirst:\n ldp x2, x3, [x1, 0]\n")
        .unwrap();
    let program = tir_sim::ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let instruction = context
        .get_op(program.blocks[0].instructions[0])
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    let mut executor = Executor::new(64);
    executor
        .write_register("GPRsp", 1, APInt::new(64, 56))
        .unwrap();
    executor
        .write_register("GPR", 2, APInt::new(64, 99))
        .unwrap();
    executor.write_memory(56, 8, 42).unwrap();
    assert!(instruction.execute(&mut executor).is_err());
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 99);
}

#[test]
fn frame_backpressure_observes_read_to_clear_once() {
    use tir::backend::exec::{EffectResponse, FrameYield, RequestId, ResponseValue, SemanticFrame};
    use tir::backend::{MachineContext, MachineInstruction};
    use tir_sim::Permissions;
    let context = Context::with_default_dialects();
    let program = riscv_program(&context, ".global first\nfirst:\n lw x2, 0(x1)\n", "first");
    let instruction = context
        .get_op(program.blocks[0].instructions[0])
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    let mut executor = Executor::new(64);
    executor
        .write_register("GPR", 1, APInt::new(64, 4096))
        .unwrap();
    executor
        .memory_service_mut()
        .map_read_to_clear(4096, 4, 42, Permissions::READ)
        .unwrap();
    let mut frame = SemanticFrame::new(9, 0, instruction.as_ref(), &mut executor).unwrap();
    executor.memory_service_mut().begin_instruction(9).unwrap();
    let FrameYield::Effect(request) = frame.resume(None).unwrap() else {
        panic!("load must yield")
    };
    for _ in 0..3 {
        assert!(executor
            .memory_service_mut()
            .service(&request, false)
            .unwrap()
            .is_none());
        let FrameYield::Effect(repeated) = frame.resume(None).unwrap() else {
            panic!("load must wait")
        };
        assert_eq!(repeated, request);
    }
    assert_eq!(executor.memory_service().read_to_clear_count(4096), Some(0));
    let response = executor
        .memory_service_mut()
        .service(&request, true)
        .unwrap()
        .unwrap();
    assert!(executor
        .memory_service_mut()
        .service(&request, true)
        .is_err());
    assert!(frame
        .resume(Some(EffectResponse {
            id: RequestId {
                instruction: 8,
                sequence: 0
            },
            result: Ok(ResponseValue::Word(7))
        }))
        .is_err());
    assert!(matches!(
        frame.resume(Some(response)).unwrap(),
        FrameYield::Complete
    ));
    executor.memory_service_mut().commit_instruction(9).unwrap();
    frame.commit(&mut executor).unwrap();
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 42);
    assert_eq!(executor.memory_service().read_to_clear_count(4096), Some(1));
    assert_eq!(executor.memory_service().read_to_clear_value(4096), Some(0));
}

#[test]
fn pair_frame_resumes_each_load_without_publishing_partial_registers() {
    use tir::backend::exec::{EffectResponse, FrameYield, ResponseValue, SemanticFrame};
    use tir::backend::{MachineContext, MachineInstruction};
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let module = target
        .asm_parser(&context)
        .parse_asm(&context, ".global first\nfirst:\n ldp x2, x3, [x1, 0]\n")
        .unwrap();
    let program = tir_sim::ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let instruction = context
        .get_op(program.blocks[0].instructions[0])
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    let mut executor = Executor::new(64);
    let mut frame = SemanticFrame::new(1, 0, instruction.as_ref(), &mut executor).unwrap();
    let FrameYield::Effect(first) = frame.resume(None).unwrap() else {
        panic!("first load")
    };
    let FrameYield::Effect(second) = frame
        .resume(Some(EffectResponse {
            id: first.id,
            result: Ok(ResponseValue::Word(42)),
        }))
        .unwrap()
    else {
        panic!("second load")
    };
    assert_eq!(second.id.sequence, 1);
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 0);
    let FrameYield::Effect(waiting) = frame.resume(None).unwrap() else {
        panic!("wait")
    };
    assert_eq!(waiting, second);
    assert!(matches!(
        frame
            .resume(Some(EffectResponse {
                id: second.id,
                result: Ok(ResponseValue::Word(43))
            }))
            .unwrap(),
        FrameYield::Complete
    ));
    frame.commit(&mut executor).unwrap();
    assert_eq!(executor.read_register("GPR", 2).unwrap().to_u64(), 42);
    assert_eq!(executor.read_register("GPR", 3).unwrap().to_u64(), 43);
}

#[test]
fn pair_store_fault_is_rejected_before_first_effect() {
    use tir::backend::MachineContext;
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let module = target
        .asm_parser(&context)
        .parse_asm(&context, ".global first\nfirst:\n stp x2, x3, [x1, 0]\n")
        .unwrap();
    let program = tir_sim::ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let mut executor = Executor::new(64);
    executor
        .write_register("GPRsp", 1, APInt::new(64, 56))
        .unwrap();
    executor
        .write_register("GPR", 2, APInt::new(64, 42))
        .unwrap();
    executor.write_memory(56, 8, 99).unwrap();
    executor.enable_effect_recording();
    executor.load(program).unwrap();
    assert!(executor.run(4, 10).is_err());
    assert_eq!(executor.read_memory(56, 8).unwrap(), 99);
    assert!(executor.effect_events().is_empty());
    assert_eq!(executor.last_fault().unwrap().pc, 0);
}

#[test]
fn unmapped_program_counter_has_precise_fault_record() {
    let context = Context::with_default_dialects();
    let program = riscv_program(
        &context,
        ".global first\nfirst:\n addi x2, x0, 42\n",
        "first",
    );
    let mut executor = Executor::new_at(64, 0x8000_0000);
    executor.load(program).unwrap();
    executor.set_entry(0x9000_0000);
    assert!(executor.run(0, 10).is_err());
    let fault = executor.last_fault().expect("fetch fault record");
    assert_eq!(fault.pc, 0x9000_0000);
    assert_eq!(fault.sequence, 0);
}

#[test]
fn wide_effect_retains_word_chunks_in_legacy_memory_trace() {
    use tir::backend::{exec::MemoryEffect, MachineContext};
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let module = target
        .asm_parser(&context)
        .parse_asm(
            &context,
            ".global first\nfirst:\n ldr q2, [x1, 0]\n add x0, x0, x0\n",
        )
        .unwrap();
    let program = tir_sim::ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let mut executor = Executor::new(128);
    executor.set_register_widths(target.register_widths());
    executor
        .write_register("GPRsp", 1, APInt::new(64, 60))
        .unwrap();
    executor.enable_trace_recording();
    executor.enable_effect_recording();
    executor.load(program).unwrap();
    executor.run(4, 10).unwrap();
    let chunks: Vec<_> = executor.mem_trace()[0]
        .iter()
        .map(|access| (access.addr, access.size))
        .collect();
    assert_eq!(chunks, [(60, 8), (68, 8)]);
    assert_eq!(executor.effect_events().len(), 1);
    assert!(matches!(
        executor.effect_events()[0].request.effect,
        MemoryEffect::Read {
            address: 60,
            size: 16
        }
    ));
}

#[test]
fn synchronous_wide_store_is_all_or_nothing() {
    use tir::backend::{MachineContext, MachineInstruction};
    use tir::utils::RawBits;
    let context = Context::with_default_dialects();
    let target = tir::backend::select_target("arm64", None, None).unwrap();
    target.register_dialects(&context);
    let module = target
        .asm_parser(&context)
        .parse_asm(&context, ".global first\nfirst:\n str q2, [x1, 0]\n")
        .unwrap();
    let program = tir_sim::ProgramImage::from_module(&context, module, 0, Some("first")).unwrap();
    let instruction = context
        .get_op(program.blocks[0].instructions[0])
        .as_interface::<dyn MachineInstruction>()
        .unwrap();
    for address in [48, 56] {
        let mut executor = Executor::new(64);
        executor.set_register_widths(target.register_widths());
        executor
            .write_register("GPRsp", 1, APInt::new(64, address))
            .unwrap();
        executor
            .write_register_bits("QPR", 2, RawBits::from_bytes(vec![42; 16]))
            .unwrap();
        executor.write_bytes(0, &[99; 64]).unwrap();
        let result = instruction.execute(&mut executor);
        assert_eq!(result.is_ok(), address == 48);
        let mut expected = [99; 64];
        if address == 48 {
            expected[48..].fill(42);
        }
        let mut actual = [0; 64];
        executor.memory_service().read(0, &mut actual).unwrap();
        assert_eq!(actual, expected);
    }
}
