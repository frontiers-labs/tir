use tir::backend::exec::{EffectRequest, MemoryEffect, RequestId, ResponseValue};
use tir::sem::{AtomicRmwOp, MemOrdering};
use tir_sim::{MemoryAccess, MemoryService, Permissions};

#[test]
fn read_to_clear_runs_once_after_backpressure() {
    let mut service = MemoryService::new();
    service
        .map_read_to_clear(0x1000, 8, 7, Permissions::READ)
        .unwrap();
    service.begin_instruction(3).unwrap();
    let request = EffectRequest {
        id: RequestId {
            instruction: 3,
            sequence: 0,
        },
        pc: 0x80,
        effect: MemoryEffect::Read {
            address: 0x1000,
            size: 8,
        },
    };

    let stale = EffectRequest {
        id: RequestId {
            instruction: 3,
            sequence: 1,
        },
        ..request.clone()
    };
    assert!(service.service(&stale, false).is_err());
    assert_eq!(service.read_to_clear_count(0x1000), Some(0));

    assert!(service.service(&request, false).unwrap().is_none());
    assert_eq!(service.read_to_clear_value(0x1000), Some(7));
    assert_eq!(service.read_to_clear_count(0x1000), Some(0));
    let response = service.service(&request, true).unwrap().unwrap();
    assert!(matches!(response.result, Ok(ResponseValue::Word(7))));
    assert_eq!(service.read_to_clear_value(0x1000), Some(0));
    assert_eq!(service.read_to_clear_count(0x1000), Some(1));
    assert!(service.service(&request, true).is_err());
    assert_eq!(service.read_to_clear_value(0x1000), Some(0));
    assert_eq!(service.read_to_clear_count(0x1000), Some(1));
}

#[test]
fn unmapping_one_read_to_clear_alias_preserves_the_device() {
    let mut service = MemoryService::new();
    service
        .map_read_to_clear(0x1000, 8, 7, Permissions::READ)
        .unwrap();
    let backing = service.address_space().mapping_at(0x1000).unwrap().backing;
    let (memory, address_space) = service.memory_and_space_mut();
    memory
        .map(address_space, 0x2000, 8, backing, 0, Permissions::READ)
        .unwrap();

    service.unmap(0x1000, 8).unwrap();

    assert_eq!(service.read_to_clear_value(0x2000), Some(7));
}

#[test]
fn raw_unmap_retires_read_to_clear_device_before_later_reuse() {
    let mut service = MemoryService::new();
    service
        .map_read_to_clear(0x1000, 8, 7, Permissions::READ)
        .unwrap();
    let backing = service.address_space().mapping_at(0x1000).unwrap().backing;
    let (memory, address_space) = service.memory_and_space_mut();
    memory.unmap(address_space, 0x1000, 8).unwrap();

    service.begin_instruction(1).unwrap();
    service.abort_instruction(1).unwrap();

    let (memory, address_space) = service.memory_and_space_mut();
    memory
        .map(address_space, 0x2000, 8, backing, 0, Permissions::READ)
        .unwrap();
    service.begin_instruction(2).unwrap();
    let response = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 2,
                    sequence: 0,
                },
                pc: 0x80,
                effect: MemoryEffect::Read {
                    address: 0x2000,
                    size: 8,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(matches!(response.result, Ok(ResponseValue::Word(0))));
}

#[test]
fn staged_stores_publish_together_at_commit() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(8).unwrap();
    memory
        .map(
            address_space,
            0x2000,
            8,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();

    service.begin_instruction(1).unwrap();
    for (sequence, address, byte) in [(0, 0x2000, 4), (1, 0x2001, 5)] {
        let response = service
            .service(
                &EffectRequest {
                    id: RequestId {
                        instruction: 1,
                        sequence,
                    },
                    pc: 0x80,
                    effect: MemoryEffect::Write {
                        address,
                        bytes: vec![byte],
                    },
                },
                true,
            )
            .unwrap()
            .unwrap();
        assert!(matches!(response.result, Ok(ResponseValue::Done)));
    }
    let mut bytes = [0; 2];
    service.read(0x2000, &mut bytes).unwrap();
    assert_eq!(bytes, [0, 0]);

    service.commit_instruction(1).unwrap();
    service.read(0x2000, &mut bytes).unwrap();
    assert_eq!(bytes, [4, 5]);
}

#[test]
fn faulted_instruction_accepts_only_abort() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(4).unwrap();
    memory
        .map(address_space, 0x3000, 4, backing, 0, Permissions::READ)
        .unwrap();
    service.begin_instruction(9).unwrap();
    let fault = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 9,
                    sequence: 0,
                },
                pc: 0x84,
                effect: MemoryEffect::Write {
                    address: 0x3000,
                    bytes: vec![1],
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(fault.result.is_err());

    let next = EffectRequest {
        id: RequestId {
            instruction: 9,
            sequence: 1,
        },
        pc: 0x84,
        effect: MemoryEffect::Read {
            address: 0x3000,
            size: 1,
        },
    };
    assert!(service.service(&next, true).is_err());
    assert!(service.commit_instruction(9).is_err());
    service.abort_instruction(9).unwrap();
}

#[test]
fn atomic_rejects_device_before_reading_it() {
    let mut service = MemoryService::new();
    service
        .map_read_to_clear(0x4000, 8, 11, Permissions::ALL)
        .unwrap();
    service.begin_instruction(5).unwrap();

    let response = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 5,
                    sequence: 0,
                },
                pc: 0x88,
                effect: MemoryEffect::AtomicRmw {
                    op: AtomicRmwOp::Add,
                    address: 0x4000,
                    size: 8,
                    value: 1,
                    ordering: MemOrdering::SeqCst,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();

    assert!(response.result.is_err());
    assert_eq!(service.read_to_clear_value(0x4000), Some(11));
    assert_eq!(service.read_to_clear_count(0x4000), Some(0));
}

#[test]
fn access_crossing_from_ram_into_device_is_rejected_without_observation() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let ram = memory.create_backing(4).unwrap();
    memory
        .map(address_space, 0x1000, 4, ram, 0, Permissions::READ_WRITE)
        .unwrap();
    service
        .map_read_to_clear(0x1004, 4, 11, Permissions::READ)
        .unwrap();

    assert!(service
        .validate_ram_access(0x1000, 8, MemoryAccess::Read)
        .is_err());

    service.begin_instruction(1).unwrap();
    let response = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 1,
                    sequence: 0,
                },
                pc: 0x8c,
                effect: MemoryEffect::Read {
                    address: 0x1000,
                    size: 8,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();

    assert!(response.result.is_err());
    assert_eq!(service.read_to_clear_value(0x1004), Some(11));
    assert_eq!(service.read_to_clear_count(0x1004), Some(0));
}

#[test]
fn memory_order_follows_observation_and_visibility_events() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(8).unwrap();
    memory
        .map(
            address_space,
            0x5000,
            8,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();

    let load_id = RequestId {
        instruction: 1,
        sequence: 0,
    };
    service.begin_instruction(1).unwrap();
    service
        .service(
            &EffectRequest {
                id: load_id,
                pc: 0x90,
                effect: MemoryEffect::Read {
                    address: 0x5000,
                    size: 1,
                },
            },
            true,
        )
        .unwrap();
    assert_eq!(service.effect_order(load_id), Some(0));
    service.commit_instruction(1).unwrap();

    let store_id = RequestId {
        instruction: 2,
        sequence: 0,
    };
    service.begin_instruction(2).unwrap();
    service
        .service(
            &EffectRequest {
                id: store_id,
                pc: 0x94,
                effect: MemoryEffect::Write {
                    address: 0x5000,
                    bytes: vec![3],
                },
            },
            true,
        )
        .unwrap();
    assert_eq!(service.effect_order(store_id), None);
    service.commit_instruction(2).unwrap();
    assert_eq!(service.effect_order(store_id), Some(1));

    let final_load_id = RequestId {
        instruction: 3,
        sequence: 0,
    };
    service.begin_instruction(3).unwrap();
    service
        .service(
            &EffectRequest {
                id: final_load_id,
                pc: 0x98,
                effect: MemoryEffect::Read {
                    address: 0x5000,
                    size: 1,
                },
            },
            true,
        )
        .unwrap();
    assert_eq!(service.effect_order(final_load_id), Some(2));

    let fault_id = RequestId {
        instruction: 3,
        sequence: 1,
    };
    let response = service
        .service(
            &EffectRequest {
                id: fault_id,
                pc: 0x98,
                effect: MemoryEffect::Read {
                    address: 0x6000,
                    size: 1,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(response.result.is_err());
    assert_eq!(service.effect_order(fault_id), None);
}

#[test]
fn plain_store_does_not_clear_lr_sc_reservation() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(8).unwrap();
    memory
        .map(
            address_space,
            0x7000,
            8,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();

    service.begin_instruction(1).unwrap();
    service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 1,
                    sequence: 0,
                },
                pc: 0xa0,
                effect: MemoryEffect::LoadReserved {
                    address: 0x7000,
                    size: 8,
                    ordering: MemOrdering::Acquire,
                },
            },
            true,
        )
        .unwrap();
    service.commit_instruction(1).unwrap();

    service.begin_instruction(2).unwrap();
    service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 2,
                    sequence: 0,
                },
                pc: 0xa4,
                effect: MemoryEffect::Write {
                    address: 0x7000,
                    bytes: 1_u64.to_le_bytes().to_vec(),
                },
            },
            true,
        )
        .unwrap();
    service.commit_instruction(2).unwrap();

    service.begin_instruction(3).unwrap();
    let response = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 3,
                    sequence: 0,
                },
                pc: 0xa8,
                effect: MemoryEffect::StoreConditional {
                    address: 0x7000,
                    size: 8,
                    value: 2,
                    ordering: MemOrdering::Release,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(matches!(response.result, Ok(ResponseValue::Word(1))));
    service.commit_instruction(3).unwrap();

    let mut bytes = [0; 8];
    service.read(0x7000, &mut bytes).unwrap();
    assert_eq!(u64::from_le_bytes(bytes), 2);
}

#[test]
fn faulting_store_conditional_consumes_reservation() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(8).unwrap();
    memory
        .map(address_space, 0x7800, 8, backing, 0, Permissions::READ)
        .unwrap();

    service.begin_instruction(1).unwrap();
    service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 1,
                    sequence: 0,
                },
                pc: 0xaa,
                effect: MemoryEffect::LoadReserved {
                    address: 0x7800,
                    size: 8,
                    ordering: MemOrdering::Acquire,
                },
            },
            true,
        )
        .unwrap();
    service.commit_instruction(1).unwrap();

    service.begin_instruction(2).unwrap();
    let fault = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 2,
                    sequence: 0,
                },
                pc: 0xac,
                effect: MemoryEffect::StoreConditional {
                    address: 0x7800,
                    size: 8,
                    value: 1,
                    ordering: MemOrdering::Release,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(fault.result.is_err());
    service.abort_instruction(2).unwrap();

    service.begin_instruction(3).unwrap();
    let retry = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 3,
                    sequence: 0,
                },
                pc: 0xb0,
                effect: MemoryEffect::StoreConditional {
                    address: 0x7800,
                    size: 8,
                    value: 2,
                    ordering: MemOrdering::Release,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(matches!(retry.result, Ok(ResponseValue::Word(0))));
}

#[test]
fn remapping_reserved_address_makes_store_conditional_fail() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let original = memory.create_backing(8).unwrap();
    memory
        .map(
            address_space,
            0x7000,
            8,
            original,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();

    service.begin_instruction(1).unwrap();
    service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 1,
                    sequence: 0,
                },
                pc: 0xac,
                effect: MemoryEffect::LoadReserved {
                    address: 0x7000,
                    size: 8,
                    ordering: MemOrdering::Acquire,
                },
            },
            true,
        )
        .unwrap();
    service.commit_instruction(1).unwrap();

    let (memory, address_space) = service.memory_and_space_mut();
    memory.unmap(address_space, 0x7000, 8).unwrap();
    let replacement = memory.create_backing(8).unwrap();
    memory
        .map(
            address_space,
            0x7000,
            8,
            replacement,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();
    service.write(0x7000, &9_u64.to_le_bytes()).unwrap();

    service.begin_instruction(2).unwrap();
    let response = service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 2,
                    sequence: 0,
                },
                pc: 0xb0,
                effect: MemoryEffect::StoreConditional {
                    address: 0x7000,
                    size: 8,
                    value: 2,
                    ordering: MemOrdering::Release,
                },
            },
            true,
        )
        .unwrap()
        .unwrap();
    assert!(matches!(response.result, Ok(ResponseValue::Word(0))));
    service.commit_instruction(2).unwrap();

    let mut bytes = [0; 8];
    service.read(0x7000, &mut bytes).unwrap();
    assert_eq!(u64::from_le_bytes(bytes), 9);
}

#[test]
fn staged_store_rejects_a_later_observing_effect() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(2).unwrap();
    memory
        .map(
            address_space,
            0x8000,
            2,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();
    service.begin_instruction(1).unwrap();
    service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 1,
                    sequence: 0,
                },
                pc: 0xb0,
                effect: MemoryEffect::Write {
                    address: 0x8000,
                    bytes: vec![1],
                },
            },
            true,
        )
        .unwrap();

    let later_read = EffectRequest {
        id: RequestId {
            instruction: 1,
            sequence: 1,
        },
        pc: 0xb0,
        effect: MemoryEffect::Read {
            address: 0x8000,
            size: 1,
        },
    };
    assert!(service.service(&later_read, true).is_err());
    assert_eq!(service.effect_order(later_read.id), None);
    service.abort_instruction(1).unwrap();
}

#[test]
fn mapping_change_invalidates_active_instruction() {
    let mut service = MemoryService::new();
    let (memory, address_space) = service.memory_and_space_mut();
    let backing = memory.create_backing(2).unwrap();
    memory
        .map(
            address_space,
            0x9000,
            2,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();
    service.begin_instruction(1).unwrap();
    service
        .service(
            &EffectRequest {
                id: RequestId {
                    instruction: 1,
                    sequence: 0,
                },
                pc: 0xb4,
                effect: MemoryEffect::Write {
                    address: 0x9000,
                    bytes: vec![1],
                },
            },
            true,
        )
        .unwrap();
    let (memory, address_space) = service.memory_and_space_mut();
    memory
        .protect(address_space, 0x9000, 2, Permissions::READ)
        .unwrap();
    assert!(service.commit_instruction(1).is_err());
    service.abort_instruction(1).unwrap();

    service.begin_instruction(2).unwrap();
    let (memory, address_space) = service.memory_and_space_mut();
    memory
        .protect(address_space, 0x9000, 2, Permissions::READ_WRITE)
        .unwrap();
    let request = EffectRequest {
        id: RequestId {
            instruction: 2,
            sequence: 0,
        },
        pc: 0xb8,
        effect: MemoryEffect::Read {
            address: 0x9000,
            size: 1,
        },
    };
    assert!(service.service(&request, false).is_err());
    service.abort_instruction(2).unwrap();
}
