use std::collections::HashMap;
use tir::backend::SimTrap;

use tir::backend::exec::{EffectRequest, EffectResponse, MemoryEffect, RequestId, ResponseValue};
use tir::utils::{APInt, RawBits};

use crate::{
    AddressSpace, BackingId, Memory, MemoryAccess, MemoryError, MemoryErrorReason, Permissions,
    memory_trap,
};

struct PendingInstruction {
    id: u64,
    next_sequence: u64,
    stores: Vec<PendingStore>,
    failed: bool,
    mapping_generation: u64,
}

struct PendingStore {
    id: RequestId,
    address: u64,
    bytes: Vec<u8>,
}

struct ReadToClear {
    backing: BackingId,
    value: u64,
    reads: u64,
}

/// Serializes semantic memory effects and owns their shared memory state.
pub struct MemoryService {
    memory: Memory,
    address_space: AddressSpace,
    active: Option<PendingInstruction>,
    last_instruction: Option<u64>,
    reservation: Option<(u64, usize, u64)>,
    read_to_clear: Vec<ReadToClear>,
    next_memory_order: u64,
    effect_orders: HashMap<RequestId, u64>,
}

impl MemoryService {
    pub fn new() -> Self {
        Self {
            memory: Memory::new(),
            address_space: AddressSpace::new(),
            active: None,
            last_instruction: None,
            reservation: None,
            read_to_clear: Vec::new(),
            next_memory_order: 0,
            effect_orders: HashMap::new(),
        }
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    pub fn address_space(&self) -> &AddressSpace {
        &self.address_space
    }

    pub fn memory_and_space_mut(&mut self) -> (&mut Memory, &mut AddressSpace) {
        (&mut self.memory, &mut self.address_space)
    }

    pub fn read(&self, address: u64, destination: &mut [u8]) -> Result<(), MemoryError> {
        self.memory.read(&self.address_space, address, destination)
    }

    pub fn write(&mut self, address: u64, source: &[u8]) -> Result<(), MemoryError> {
        self.memory.write(&self.address_space, address, source)
    }

    pub fn unmap(&mut self, address: u64, size: u64) -> Result<(), MemoryError> {
        self.memory.unmap(&mut self.address_space, address, size)?;
        self.prune_read_to_clear();
        Ok(())
    }

    pub fn begin_instruction(&mut self, id: u64) -> Result<(), SimTrap> {
        if self.active.is_some() || self.last_instruction.is_some_and(|last| id <= last) {
            return Err(protocol_trap(
                "instruction identity is stale or already active",
            ));
        }
        self.prune_read_to_clear();
        self.last_instruction = Some(id);
        self.effect_orders.clear();
        self.active = Some(PendingInstruction {
            id,
            next_sequence: 0,
            stores: Vec::new(),
            failed: false,
            mapping_generation: self.address_space.generation(),
        });
        Ok(())
    }

    pub fn service(
        &mut self,
        request: &EffectRequest,
        ready: bool,
    ) -> Result<Option<EffectResponse>, SimTrap> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| protocol_trap("effect request has no active instruction"))?;
        if request.id.instruction != active.id || request.id.sequence != active.next_sequence {
            return Err(protocol_trap(
                "effect request identity is stale or out of sequence",
            ));
        }
        if active.failed {
            return Err(protocol_trap("faulted instruction must be aborted"));
        }
        if active.mapping_generation != self.address_space.generation() {
            return Err(protocol_trap(
                "address-space mappings changed during instruction",
            ));
        }
        if !active.stores.is_empty()
            && matches!(
                request.effect,
                MemoryEffect::Read { .. }
                    | MemoryEffect::LoadReserved { .. }
                    | MemoryEffect::StoreConditional { .. }
                    | MemoryEffect::AtomicRmw { .. }
                    | MemoryEffect::Fence { .. }
            )
        {
            return Err(protocol_trap(
                "staged store must commit before a later observing effect",
            ));
        }
        if !ready {
            return Ok(None);
        }

        let result = self.execute_effect(request.id, &request.effect);
        if result.is_ok() && self.linearizes_immediately(&request.effect) {
            let order = self.take_memory_order();
            self.effect_orders.insert(request.id, order);
        }
        let active = self.active.as_mut().unwrap();
        active.next_sequence += 1;
        active.failed = result.is_err();
        Ok(Some(EffectResponse {
            id: request.id,
            result,
        }))
    }

    pub fn commit_instruction(&mut self, id: u64) -> Result<(), SimTrap> {
        let active = self
            .active
            .as_ref()
            .filter(|active| {
                active.id == id
                    && !active.failed
                    && active.mapping_generation == self.address_space.generation()
            })
            .ok_or_else(|| protocol_trap("commit identity is stale"))?;
        for store in &active.stores {
            self.memory
                .validate(
                    &self.address_space,
                    store.address,
                    store.bytes.len(),
                    MemoryAccess::Write,
                )
                .map_err(|error| memory_trap(error, store.bytes.len()))?;
        }
        let stores = std::mem::take(&mut self.active.as_mut().unwrap().stores);
        for store in stores {
            self.memory
                .write(&self.address_space, store.address, &store.bytes)
                .map_err(|error| memory_trap(error, store.bytes.len()))?;
            let order = self.take_memory_order();
            self.effect_orders.insert(store.id, order);
        }
        self.active = None;
        Ok(())
    }

    pub fn abort_instruction(&mut self, id: u64) -> Result<(), SimTrap> {
        if self.active.as_ref().is_none_or(|active| active.id != id) {
            return Err(protocol_trap("abort identity is stale"));
        }
        self.active = None;
        Ok(())
    }

    pub fn map_read_to_clear(
        &mut self,
        address: u64,
        size: u64,
        value: u64,
        permissions: Permissions,
    ) -> Result<(), MemoryError> {
        self.prune_read_to_clear();
        if size == 0 || size > 8 {
            return self.map_invalid_device_range(address);
        }
        let backing = self.memory.create_backing(size)?;
        self.memory.map(
            &mut self.address_space,
            address,
            size,
            backing,
            0,
            permissions,
        )?;
        self.read_to_clear.push(ReadToClear {
            backing,
            value,
            reads: 0,
        });
        Ok(())
    }

    pub fn read_to_clear_value(&self, address: u64) -> Option<u64> {
        let mapping = self.address_space.mapping_at(address)?;
        self.read_to_clear
            .iter()
            .find_map(|device| (device.backing == mapping.backing).then_some(device.value))
    }

    pub fn read_to_clear_count(&self, address: u64) -> Option<u64> {
        let mapping = self.address_space.mapping_at(address)?;
        self.read_to_clear
            .iter()
            .find_map(|device| (device.backing == mapping.backing).then_some(device.reads))
    }

    /// Returns the global memory-order index assigned at this effect's linearization event.
    pub fn effect_order(&self, id: RequestId) -> Option<u64> {
        self.effect_orders.get(&id).copied()
    }

    /// Validates an access and rejects device mappings without observing them.
    pub fn validate_ram_access(
        &self,
        address: u64,
        size: usize,
        access: MemoryAccess,
    ) -> Result<(), MemoryError> {
        self.memory
            .validate(&self.address_space, address, size, access)?;
        if self.device_for_access(address, size, access)?.is_some() {
            return Err(MemoryError {
                address,
                access,
                reason: MemoryErrorReason::DeviceAccess,
            });
        }
        Ok(())
    }

    fn linearizes_immediately(&self, effect: &MemoryEffect) -> bool {
        match effect {
            MemoryEffect::Read { .. }
            | MemoryEffect::LoadReserved { .. }
            | MemoryEffect::StoreConditional { .. }
            | MemoryEffect::AtomicRmw { .. }
            | MemoryEffect::Fence { .. } => true,
            MemoryEffect::Write { .. } => false,
            MemoryEffect::Exception { .. } => false,
        }
    }

    fn take_memory_order(&mut self) -> u64 {
        let order = self.next_memory_order;
        self.next_memory_order = self
            .next_memory_order
            .checked_add(1)
            .expect("memory-order index exhausted");
        order
    }

    fn map_invalid_device_range(&mut self, address: u64) -> Result<(), MemoryError> {
        Err(MemoryError {
            address,
            access: MemoryAccess::Map,
            reason: MemoryErrorReason::InvalidRange,
        })
    }

    fn execute_effect(
        &mut self,
        id: RequestId,
        effect: &MemoryEffect,
    ) -> Result<ResponseValue, SimTrap> {
        match effect {
            MemoryEffect::Read { address, size } => self.read_effect(*address, *size),
            MemoryEffect::Write { address, bytes } => self.write_effect(id, *address, bytes),
            MemoryEffect::LoadReserved { address, size, .. } => {
                self.validate_ram_access(*address, *size, MemoryAccess::Read)
                    .map_err(|error| memory_trap(error, *size))?;
                let value = self.read_word(*address, *size)?;
                self.reservation = Some((*address, *size, self.address_space.generation()));
                Ok(ResponseValue::Word(value))
            }
            MemoryEffect::StoreConditional {
                address,
                size,
                value,
                ..
            } => {
                let success = self.reservation.take()
                    == Some((*address, *size, self.address_space.generation()));
                if success {
                    self.validate_ram_access(*address, *size, MemoryAccess::Write)
                        .map_err(|error| memory_trap(error, *size))?;
                }
                if success {
                    self.write_word(*address, *size, *value)?;
                }
                Ok(ResponseValue::Word(u64::from(success)))
            }
            MemoryEffect::AtomicRmw {
                op,
                address,
                size,
                value,
                ..
            } => {
                self.validate_ram_access(*address, *size, MemoryAccess::Read)
                    .map_err(|error| memory_trap(error, *size))?;
                self.validate_ram_access(*address, *size, MemoryAccess::Write)
                    .map_err(|error| memory_trap(error, *size))?;
                let old = self.read_word(*address, *size)?;
                let width = u32::try_from(*size)
                    .ok()
                    .and_then(|size| size.checked_mul(8))
                    .filter(|width| *width <= 64)
                    .ok_or_else(|| bad_address(*address, *size))?;
                let new = op
                    .apply(APInt::new(width, old), APInt::new(width, *value))
                    .to_u64();
                self.write_word(*address, *size, new)?;
                Ok(ResponseValue::Word(old))
            }
            MemoryEffect::Fence { .. } | MemoryEffect::Exception { .. } => Ok(ResponseValue::Done),
        }
    }

    fn read_effect(&mut self, address: u64, size: usize) -> Result<ResponseValue, SimTrap> {
        self.memory
            .validate(&self.address_space, address, size, MemoryAccess::Read)
            .map_err(|error| memory_trap(error, size))?;
        if let Some((index, offset)) = self
            .device_for_access(address, size, MemoryAccess::Read)
            .map_err(|error| memory_trap(error, size))?
        {
            let shift = offset * 8;
            let value = self.read_to_clear[index].value >> shift;
            self.read_to_clear[index].value = 0;
            self.read_to_clear[index].reads += 1;
            return Ok(ResponseValue::Word(value & width_mask(size)));
        }
        let mut bytes = vec![0; size];
        self.memory
            .read(&self.address_space, address, &mut bytes)
            .map_err(|error| memory_trap(error, size))?;
        if size <= 8 {
            Ok(ResponseValue::Word(word_from_bytes(&bytes)))
        } else {
            Ok(ResponseValue::Bytes(RawBits::from_bytes(bytes)))
        }
    }

    fn write_effect(
        &mut self,
        id: RequestId,
        address: u64,
        bytes: &[u8],
    ) -> Result<ResponseValue, SimTrap> {
        self.memory
            .validate(
                &self.address_space,
                address,
                bytes.len(),
                MemoryAccess::Write,
            )
            .map_err(|error| memory_trap(error, bytes.len()))?;
        if self
            .device_for_access(address, bytes.len(), MemoryAccess::Write)
            .map_err(|error| memory_trap(error, bytes.len()))?
            .is_some()
        {
            return Err(memory_trap(
                MemoryError {
                    address,
                    access: MemoryAccess::Write,
                    reason: MemoryErrorReason::DeviceAccess,
                },
                bytes.len(),
            ));
        }
        self.active.as_mut().unwrap().stores.push(PendingStore {
            id,
            address,
            bytes: bytes.to_vec(),
        });
        Ok(ResponseValue::Done)
    }

    fn prune_read_to_clear(&mut self) {
        let address_space = &self.address_space;
        self.read_to_clear.retain(|device| {
            address_space
                .mappings()
                .any(|mapping| mapping.backing == device.backing)
        });
    }

    fn read_word(&mut self, address: u64, size: usize) -> Result<u64, SimTrap> {
        if size > 8 {
            return Err(bad_address(address, size));
        }
        match self.read_effect(address, size)? {
            ResponseValue::Word(value) => Ok(value),
            _ => unreachable!(),
        }
    }

    fn write_word(&mut self, address: u64, size: usize, value: u64) -> Result<(), SimTrap> {
        if size == 0 || size > 8 {
            return Err(bad_address(address, size));
        }
        let bytes = &value.to_le_bytes()[..size];
        self.memory
            .write(&self.address_space, address, bytes)
            .map_err(|error| memory_trap(error, size))
    }

    fn device_for_access(
        &self,
        address: u64,
        size: usize,
        access: MemoryAccess,
    ) -> Result<Option<(usize, u64)>, MemoryError> {
        let size = u64::try_from(size).map_err(|_| MemoryError {
            address,
            access,
            reason: MemoryErrorReason::InvalidRange,
        })?;
        let end = address.checked_add(size).ok_or(MemoryError {
            address,
            access,
            reason: MemoryErrorReason::InvalidRange,
        })?;

        let mut cursor = address;
        while cursor < end {
            let Some(mapping) = self.address_space.mapping_at(cursor) else {
                return Ok(None);
            };
            if let Some(index) = self
                .read_to_clear
                .iter()
                .position(|device| device.backing == mapping.backing)
            {
                if address >= mapping.start && end <= mapping.end {
                    return Ok(Some((
                        index,
                        mapping.backing_offset + address - mapping.start,
                    )));
                }
                return Err(MemoryError {
                    address,
                    access,
                    reason: MemoryErrorReason::DeviceAccess,
                });
            }
            cursor = mapping.end.min(end);
        }
        Ok(None)
    }
}

impl Default for MemoryService {
    fn default() -> Self {
        Self::new()
    }
}

fn word_from_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().enumerate().fold(0, |value, (index, byte)| {
        value | (u64::from(*byte) << (index * 8))
    })
}

fn width_mask(size: usize) -> u64 {
    if size == 8 {
        u64::MAX
    } else {
        (1_u64 << (size * 8)) - 1
    }
}

fn bad_address(address: u64, size: usize) -> SimTrap {
    SimTrap::BadAddress { address, size }
}

fn protocol_trap(reason: &str) -> SimTrap {
    SimTrap::InvalidInstruction {
        op: "memory service",
        reason: reason.to_string(),
    }
}
