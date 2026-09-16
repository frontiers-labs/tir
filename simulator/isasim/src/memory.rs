//! Memory-image setup for snippet runs: either an explicit JSON memory config,
//! or the default test allocation small ASM samples rely on. Both are stop-gaps
//! until programs arrive as ELF images with their own segments.

use serde::Deserialize;
use tir::backend::MachineContext;
use tir::utils::APInt;
use tir_sim::{Executor, Permissions};

use crate::parse_addr;

const DEFAULT_TEST_MEMORY_OFFSET: usize = 0x1000;
const DEFAULT_TEST_MEMORY_SIZE: usize = 0x1000;
const DEFAULT_TEST_MEMORY_BASE_REG: u16 = 10; // RISC-V a0
const DEFAULT_TEST_MEMORY_ALT_REG: u16 = 11; // RISC-V a1

#[derive(Deserialize)]
struct MemoryConfig {
    #[serde(default)]
    regions: Vec<MemoryRegionConfig>,
    #[serde(default)]
    mappings: Option<Vec<MemoryMappingConfig>>,
    #[serde(default)]
    devices: Vec<MemoryDeviceConfig>,
}

#[derive(Deserialize)]
struct MemoryMappingConfig {
    start: String,
    size: u64,
    permissions: String,
}

#[derive(Deserialize)]
struct MemoryDeviceConfig {
    start: String,
    size: u64,
    value: u64,
    permissions: String,
}

#[derive(Deserialize)]
struct MemoryRegionConfig {
    start: String,
    #[serde(default)]
    bytes: Option<Vec<u8>>,
    #[serde(default)]
    hex: Option<String>,
}

pub fn load_memory_config(executor: &mut Executor, path: &str) {
    let text = std::fs::read_to_string(path).expect("failed to read memory config");
    let config: MemoryConfig = serde_json::from_str(&text).expect("failed to parse memory config");
    if let Some(mappings) = config.mappings {
        let existing: Vec<_> = executor
            .memory_service()
            .address_space()
            .mappings()
            .map(|mapping| (mapping.start, mapping.end - mapping.start))
            .collect();
        for (start, size) in existing {
            executor
                .memory_service_mut()
                .unmap(start, size)
                .expect("failed to clear default memory mappings");
        }
        let (memory, address_space) = executor.memory_service_mut().memory_and_space_mut();
        for mapping in mappings {
            let start = parse_addr(&mapping.start);
            let permissions = parse_permissions(&mapping.permissions);
            let backing = memory
                .create_backing(mapping.size)
                .expect("failed to create configured memory backing");
            memory
                .map(address_space, start, mapping.size, backing, 0, permissions)
                .expect("failed to map configured memory region");
        }
    }
    for device in config.devices {
        executor
            .memory_service_mut()
            .map_read_to_clear(
                parse_addr(&device.start),
                device.size,
                device.value,
                parse_permissions(&device.permissions),
            )
            .expect("failed to map configured memory device");
    }
    for region in config.regions {
        let start = parse_addr(&region.start);
        let bytes = match (region.bytes, region.hex) {
            (Some(bytes), None) => bytes,
            (None, Some(hex)) => parse_hex_bytes(&hex),
            (Some(_), Some(_)) => {
                panic!("memory region must specify either bytes or hex, not both")
            }
            (None, None) => panic!("memory region must specify bytes or hex"),
        };
        for (offset, byte) in bytes.into_iter().enumerate() {
            let address = start
                .checked_add(offset as u64)
                .expect("memory region address overflow");
            executor
                .write_memory(address, 1, u64::from(byte))
                .expect("memory region does not fit configured memory window");
        }
    }
}

fn parse_permissions(permissions: &str) -> Permissions {
    let mut result = Permissions::NONE;
    for permission in permissions.chars() {
        let bit = match permission {
            'r' => Permissions::READ,
            'w' => Permissions::WRITE,
            'x' => Permissions::EXECUTE,
            _ => panic!("invalid memory permissions: {permissions:?}"),
        };
        if result.contains(bit) {
            panic!("duplicate memory permission: {permission}");
        }
        result = result | bit;
    }
    result
}

pub fn install_default_test_memory(
    executor: &mut Executor,
    target: &str,
    memory_base: u64,
    memory_size: usize,
) {
    let Some((start, size)) = default_test_memory_region(memory_base, memory_size) else {
        return;
    };

    for offset in 0..size {
        let byte = (offset & 0xff) as u8;
        executor
            .write_memory(start + offset as u64, 1, u64::from(byte))
            .expect("default memory allocation must fit configured memory window");
    }

    // Project convention for quick RISC-V snippets:
    //   a0/x10 = start of the default allocation
    //   a1/x11 = midpoint, useful as a separate store destination
    if target.starts_with("riscv") || target.starts_with("rv") {
        executor
            .write_register("GPR", DEFAULT_TEST_MEMORY_BASE_REG, APInt::new(64, start))
            .expect("failed to initialize default memory base register");
        executor
            .write_register(
                "GPR",
                DEFAULT_TEST_MEMORY_ALT_REG,
                APInt::new(64, start + (size / 2) as u64),
            )
            .expect("failed to initialize default memory alternate register");
    }
}

fn default_test_memory_region(memory_base: u64, memory_size: usize) -> Option<(u64, usize)> {
    if memory_size == 0 {
        return None;
    }
    let offset = if memory_size > DEFAULT_TEST_MEMORY_OFFSET {
        DEFAULT_TEST_MEMORY_OFFSET
    } else {
        0
    };
    let size = (memory_size - offset).min(DEFAULT_TEST_MEMORY_SIZE);
    Some((memory_base + offset as u64, size))
}

fn parse_hex_bytes(hex: &str) -> Vec<u8> {
    let hex = hex
        .trim()
        .strip_prefix("0x")
        .or_else(|| hex.trim().strip_prefix("0X"))
        .unwrap_or_else(|| hex.trim());
    let mut compact = String::new();
    for ch in hex.chars() {
        if ch.is_ascii_hexdigit() {
            compact.push(ch);
        } else if ch.is_whitespace() || ch == '_' {
            continue;
        } else {
            panic!("invalid character in memory hex data");
        }
    }
    if !compact.len().is_multiple_of(2) {
        panic!("memory hex data must contain an even number of digits");
    }
    (0..compact.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).expect("invalid memory hex byte"))
        .collect()
}
