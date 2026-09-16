use tir_sim::{
    AddressSpace, Memory, MemoryAccess, MemoryErrorReason, Permissions, MEMORY_PAGE_SIZE,
};

#[test]
fn sparse_memory_reads_and_writes_mapped_bytes() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(0x2000).unwrap();
    let mut address_space = AddressSpace::new();
    memory
        .map(
            &mut address_space,
            0x1000,
            0x2000,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();

    memory.write(&address_space, 0x1ffe, &[1, 2, 3, 4]).unwrap();

    let mut bytes = [0; 4];
    memory.read(&address_space, 0x1ffe, &mut bytes).unwrap();
    assert_eq!(bytes, [1, 2, 3, 4]);
}

#[test]
fn failed_cross_mapping_write_changes_no_bytes() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(MEMORY_PAGE_SIZE * 2).unwrap();
    let mut address_space = AddressSpace::new();
    memory
        .map(
            &mut address_space,
            0x1000,
            MEMORY_PAGE_SIZE,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();
    memory
        .map(
            &mut address_space,
            0x2000,
            MEMORY_PAGE_SIZE,
            backing,
            MEMORY_PAGE_SIZE,
            Permissions::READ,
        )
        .unwrap();

    let error = memory
        .write(&address_space, 0x1ffe, &[1, 2, 3, 4])
        .unwrap_err();
    assert_eq!(error.address, 0x2000);
    assert_eq!(error.access, MemoryAccess::Write);
    assert_eq!(error.reason, MemoryErrorReason::PermissionDenied);

    let mut bytes = [9; 2];
    memory.read(&address_space, 0x1ffe, &mut bytes).unwrap();
    assert_eq!(bytes, [0, 0]);
}

#[test]
fn aliases_share_sparse_backing_bytes() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(16).unwrap();
    let mut address_space = AddressSpace::new();
    for address in [0x1003, 0x3007] {
        memory
            .map(
                &mut address_space,
                address,
                7,
                backing,
                4,
                Permissions::READ_WRITE,
            )
            .unwrap();
    }

    memory.write(&address_space, 0x1005, &[4, 5, 6]).unwrap();
    let mut bytes = [0; 3];
    memory.read(&address_space, 0x3009, &mut bytes).unwrap();
    assert_eq!(bytes, [4, 5, 6]);

    let error = memory.read(&address_space, 0x1002, &mut [0]).unwrap_err();
    assert_eq!(error.address, 0x1002);
    assert_eq!(error.reason, MemoryErrorReason::Unmapped);
    let error = memory.read(&address_space, 0x100a, &mut [0]).unwrap_err();
    assert_eq!(error.address, 0x100a);
    assert_eq!(error.reason, MemoryErrorReason::Unmapped);
}

#[test]
fn fetch_requires_execute_permission() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(4).unwrap();
    let mut address_space = AddressSpace::new();
    memory
        .map(
            &mut address_space,
            0x40,
            4,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();
    memory.write(&address_space, 0x40, &[1, 2, 3, 4]).unwrap();

    let error = memory.fetch(&address_space, 0x40, &mut [0; 4]).unwrap_err();
    assert_eq!(error.access, MemoryAccess::Fetch);
    assert_eq!(error.reason, MemoryErrorReason::PermissionDenied);

    memory
        .protect(&mut address_space, 0x40, 4, Permissions::READ_EXECUTE)
        .unwrap();
    let mut instruction = [0; 4];
    memory
        .fetch(&address_space, 0x40, &mut instruction)
        .unwrap();
    assert_eq!(instruction, [1, 2, 3, 4]);
}

#[test]
fn protect_and_unmap_split_exact_mapping_windows() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(12).unwrap();
    let mut address_space = AddressSpace::new();
    memory
        .map(
            &mut address_space,
            0x100,
            12,
            backing,
            0,
            Permissions::READ_WRITE,
        )
        .unwrap();
    let initial_generation = address_space.generation();

    memory
        .protect(&mut address_space, 0x104, 4, Permissions::READ)
        .unwrap();
    assert!(address_space.generation() > initial_generation);
    memory.write(&address_space, 0x100, &[1]).unwrap();
    assert_eq!(
        memory
            .write(&address_space, 0x104, &[2])
            .unwrap_err()
            .reason,
        MemoryErrorReason::PermissionDenied
    );
    memory.write(&address_space, 0x108, &[3]).unwrap();

    memory.unmap(&mut address_space, 0x102, 8).unwrap();
    assert_eq!(
        memory
            .read(&address_space, 0x102, &mut [0])
            .unwrap_err()
            .reason,
        MemoryErrorReason::Unmapped
    );
    memory.read(&address_space, 0x100, &mut [0]).unwrap();
    memory.read(&address_space, 0x10a, &mut [0]).unwrap();
}

#[test]
fn new_mapping_records_its_address_space_generation() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(MEMORY_PAGE_SIZE * 2).unwrap();
    let mut address_space = AddressSpace::new();
    memory
        .map(
            &mut address_space,
            0x1000,
            MEMORY_PAGE_SIZE * 2,
            backing,
            0,
            Permissions::ALL,
        )
        .unwrap();
    let mapping_generation = address_space.mapping_at(0x1000).unwrap().generation;
    assert_eq!(mapping_generation, address_space.generation());
}

#[test]
fn map_rejects_overlap_and_backing_overflow() {
    let mut memory = Memory::new();
    let backing = memory.create_backing(8).unwrap();
    let mut address_space = AddressSpace::new();
    memory
        .map(&mut address_space, 0x100, 4, backing, 0, Permissions::READ)
        .unwrap();
    assert_eq!(
        memory
            .map(&mut address_space, 0x102, 2, backing, 4, Permissions::READ)
            .unwrap_err()
            .reason,
        MemoryErrorReason::OverlappingMapping
    );
    assert_eq!(
        memory
            .map(&mut address_space, 0x200, 2, backing, 7, Permissions::READ)
            .unwrap_err()
            .reason,
        MemoryErrorReason::BackingOutOfBounds
    );
}
