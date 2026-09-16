use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Size of a sparse memory page.
pub const MEMORY_PAGE_SIZE: u64 = 4096;

static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);

/// Stable identity of a memory backing object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BackingId(u64);

/// Stable identity of an address space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AddressSpaceId(u64);

/// Permission bits on a guest mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions(u8);

impl Permissions {
    pub const NONE: Self = Self(0);
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const READ_WRITE: Self = Self(Self::READ.0 | Self::WRITE.0);
    pub const READ_EXECUTE: Self = Self(Self::READ.0 | Self::EXECUTE.0);
    pub const WRITE_EXECUTE: Self = Self(Self::WRITE.0 | Self::EXECUTE.0);
    pub const ALL: Self = Self(Self::READ.0 | Self::WRITE.0 | Self::EXECUTE.0);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Permissions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

/// Operation which failed while accessing or changing a mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryAccess {
    Read,
    Write,
    Fetch,
    Map,
    Protect,
    Unmap,
}

/// Cause of a memory or mapping failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryErrorReason {
    Unmapped,
    PermissionDenied,
    OverlappingMapping,
    InvalidRange,
    UnknownBacking,
    BackingOutOfBounds,
    DeviceAccess,
    IdentityExhausted,
}

/// A precise memory failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryError {
    pub address: u64,
    pub access: MemoryAccess,
    pub reason: MemoryErrorReason,
}

impl MemoryError {
    fn new(address: u64, access: MemoryAccess, reason: MemoryErrorReason) -> Self {
        Self {
            address,
            access,
            reason,
        }
    }
}

impl fmt::Display for MemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "memory {:?} at {:#x} failed: {:?}",
            self.access, self.address, self.reason
        )
    }
}

impl std::error::Error for MemoryError {}

/// Converts a mapped-memory error into the common simulator trap format.
pub fn memory_trap(error: MemoryError, size: usize) -> tir::backend::SimTrap {
    tir::backend::SimTrap::MemoryFault {
        address: error.address,
        size,
        access: match error.access {
            MemoryAccess::Read => "read",
            MemoryAccess::Write => "write",
            MemoryAccess::Fetch => "fetch",
            MemoryAccess::Map => "map",
            MemoryAccess::Protect => "protect",
            MemoryAccess::Unmap => "unmap",
        },
        reason: match error.reason {
            MemoryErrorReason::Unmapped => "unmapped",
            MemoryErrorReason::PermissionDenied => "permission denied",
            MemoryErrorReason::OverlappingMapping => "overlapping mapping",
            MemoryErrorReason::InvalidRange => "invalid range",
            MemoryErrorReason::UnknownBacking => "unknown backing",
            MemoryErrorReason::BackingOutOfBounds => "backing out of bounds",
            MemoryErrorReason::DeviceAccess => "device access",
            MemoryErrorReason::IdentityExhausted => "identity exhausted",
        },
    }
}

#[derive(Clone, Debug)]
struct Mapping {
    end: u64,
    backing: BackingId,
    backing_offset: u64,
    permissions: Permissions,
    generation: u64,
}

/// Public details of the mapping containing an address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingInfo {
    pub start: u64,
    pub end: u64,
    pub backing: BackingId,
    pub backing_offset: u64,
    pub permissions: Permissions,
    pub generation: u64,
}

/// Guest virtual mappings, kept separate from their backing storage.
#[derive(Debug)]
pub struct AddressSpace {
    id: AddressSpaceId,
    generation: u64,
    mappings: BTreeMap<u64, Mapping>,
}

impl AddressSpace {
    pub fn new() -> Self {
        let id = NEXT_ADDRESS_SPACE_ID.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, u64::MAX, "address-space identity exhausted");
        Self {
            id: AddressSpaceId(id),
            generation: 0,
            mappings: BTreeMap::new(),
        }
    }

    pub fn id(&self) -> AddressSpaceId {
        self.id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Mappings in ascending virtual-address order.
    pub fn mappings(&self) -> impl Iterator<Item = MappingInfo> + '_ {
        self.mappings.iter().map(|(&start, mapping)| MappingInfo {
            start,
            end: mapping.end,
            backing: mapping.backing,
            backing_offset: mapping.backing_offset,
            permissions: mapping.permissions,
            generation: mapping.generation,
        })
    }

    pub fn mapping_at(&self, address: u64) -> Option<MappingInfo> {
        let (&start, mapping) = self.mappings.range(..=address).next_back()?;
        (address < mapping.end).then_some(MappingInfo {
            start,
            end: mapping.end,
            backing: mapping.backing,
            backing_offset: mapping.backing_offset,
            permissions: mapping.permissions,
            generation: mapping.generation,
        })
    }

    fn next_generation(&mut self, address: u64, access: MemoryAccess) -> Result<u64, MemoryError> {
        self.generation = self.generation.checked_add(1).ok_or_else(|| {
            MemoryError::new(address, access, MemoryErrorReason::IdentityExhausted)
        })?;
        Ok(self.generation)
    }

    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.mappings
            .range(..end)
            .next_back()
            .is_some_and(|(&other_start, other)| other_start < end && other.end > start)
    }

    fn mapped_range(&self, start: u64, end: u64) -> bool {
        let mut cursor = start;
        while cursor < end {
            let Some((_, mapping)) = self.mappings.range(..=cursor).next_back() else {
                return false;
            };
            if cursor >= mapping.end {
                return false;
            }
            cursor = mapping.end.min(end);
        }
        true
    }

    fn replace_range(
        &mut self,
        start: u64,
        end: u64,
        permissions: Option<Permissions>,
        access: MemoryAccess,
    ) -> Result<(), MemoryError> {
        if !self.mapped_range(start, end) {
            return Err(MemoryError::new(start, access, MemoryErrorReason::Unmapped));
        }
        let generation = self.next_generation(start, access)?;
        let affected: Vec<_> = self
            .mappings
            .range(..end)
            .filter(|(_, mapping)| mapping.end > start)
            .map(|(&mapping_start, mapping)| (mapping_start, mapping.clone()))
            .collect();

        for (mapping_start, mapping) in affected {
            self.mappings.remove(&mapping_start);
            if mapping_start < start {
                self.mappings.insert(
                    mapping_start,
                    Mapping {
                        end: start,
                        ..mapping.clone()
                    },
                );
            }
            let middle_start = mapping_start.max(start);
            let middle_end = mapping.end.min(end);
            if let Some(new_permissions) = permissions {
                self.mappings.insert(
                    middle_start,
                    Mapping {
                        end: middle_end,
                        backing_offset: mapping.backing_offset + (middle_start - mapping_start),
                        permissions: new_permissions,
                        generation,
                        ..mapping.clone()
                    },
                );
            }
            if mapping.end > end {
                self.mappings.insert(
                    end,
                    Mapping {
                        backing_offset: mapping.backing_offset + (end - mapping_start),
                        ..mapping
                    },
                );
            }
        }
        Ok(())
    }
}

impl Default for AddressSpace {
    fn default() -> Self {
        Self::new()
    }
}

struct Backing {
    size: u64,
    pages: HashMap<u64, Box<[u8; MEMORY_PAGE_SIZE as usize]>>,
}

/// Owner of sparse backing pages and the service for mapped memory accesses.
pub struct Memory {
    next_backing_id: u64,
    backings: HashMap<BackingId, Backing>,
}

impl Memory {
    pub fn new() -> Self {
        Self {
            next_backing_id: 1,
            backings: HashMap::new(),
        }
    }

    pub fn create_backing(&mut self, size: u64) -> Result<BackingId, MemoryError> {
        if size == 0 {
            return Err(MemoryError::new(
                0,
                MemoryAccess::Map,
                MemoryErrorReason::InvalidRange,
            ));
        }
        let id = BackingId(self.next_backing_id);
        self.next_backing_id = self.next_backing_id.checked_add(1).ok_or_else(|| {
            MemoryError::new(0, MemoryAccess::Map, MemoryErrorReason::IdentityExhausted)
        })?;
        self.backings.insert(
            id,
            Backing {
                size,
                pages: HashMap::new(),
            },
        );
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn map(
        &self,
        address_space: &mut AddressSpace,
        address: u64,
        size: u64,
        backing: BackingId,
        backing_offset: u64,
        permissions: Permissions,
    ) -> Result<(), MemoryError> {
        let end = checked_end(address, size, MemoryAccess::Map)?;
        let backing_end = checked_end(backing_offset, size, MemoryAccess::Map)?;
        let backing_size = self
            .backings
            .get(&backing)
            .ok_or_else(|| {
                MemoryError::new(
                    address,
                    MemoryAccess::Map,
                    MemoryErrorReason::UnknownBacking,
                )
            })?
            .size;
        if backing_end > backing_size {
            return Err(MemoryError::new(
                address,
                MemoryAccess::Map,
                MemoryErrorReason::BackingOutOfBounds,
            ));
        }
        if address_space.overlaps(address, end) {
            return Err(MemoryError::new(
                address,
                MemoryAccess::Map,
                MemoryErrorReason::OverlappingMapping,
            ));
        }
        let generation = address_space.next_generation(address, MemoryAccess::Map)?;
        address_space.mappings.insert(
            address,
            Mapping {
                end,
                backing,
                backing_offset,
                permissions,
                generation,
            },
        );
        Ok(())
    }

    pub fn protect(
        &self,
        address_space: &mut AddressSpace,
        address: u64,
        size: u64,
        permissions: Permissions,
    ) -> Result<(), MemoryError> {
        let end = checked_end(address, size, MemoryAccess::Protect)?;
        address_space.replace_range(address, end, Some(permissions), MemoryAccess::Protect)
    }

    pub fn unmap(
        &self,
        address_space: &mut AddressSpace,
        address: u64,
        size: u64,
    ) -> Result<(), MemoryError> {
        let end = checked_end(address, size, MemoryAccess::Unmap)?;
        address_space.replace_range(address, end, None, MemoryAccess::Unmap)
    }

    pub fn read(
        &self,
        address_space: &AddressSpace,
        address: u64,
        destination: &mut [u8],
    ) -> Result<(), MemoryError> {
        let spans = self.resolve(
            address_space,
            address,
            destination.len(),
            Permissions::READ,
            MemoryAccess::Read,
        )?;
        let mut destination_offset = 0;
        for span in spans {
            let backing = &self.backings[&span.backing];
            read_backing(
                backing,
                span.offset,
                &mut destination[destination_offset..destination_offset + span.len],
            );
            destination_offset += span.len;
        }
        Ok(())
    }

    pub fn fetch(
        &self,
        address_space: &AddressSpace,
        address: u64,
        destination: &mut [u8],
    ) -> Result<(), MemoryError> {
        let spans = self.resolve(
            address_space,
            address,
            destination.len(),
            Permissions::EXECUTE,
            MemoryAccess::Fetch,
        )?;
        let mut destination_offset = 0;
        for span in spans {
            let backing = &self.backings[&span.backing];
            read_backing(
                backing,
                span.offset,
                &mut destination[destination_offset..destination_offset + span.len],
            );
            destination_offset += span.len;
        }
        Ok(())
    }

    pub fn write(
        &mut self,
        address_space: &AddressSpace,
        address: u64,
        source: &[u8],
    ) -> Result<(), MemoryError> {
        let spans = self.resolve(
            address_space,
            address,
            source.len(),
            Permissions::WRITE,
            MemoryAccess::Write,
        )?;
        let mut source_offset = 0;
        for span in spans {
            let backing = self.backings.get_mut(&span.backing).unwrap();
            write_backing(
                backing,
                span.offset,
                &source[source_offset..source_offset + span.len],
            );
            source_offset += span.len;
        }
        Ok(())
    }

    /// Checks an entire access without reading or changing backing bytes.
    pub fn validate(
        &self,
        address_space: &AddressSpace,
        address: u64,
        size: usize,
        access: MemoryAccess,
    ) -> Result<(), MemoryError> {
        let permission = match access {
            MemoryAccess::Read => Permissions::READ,
            MemoryAccess::Write => Permissions::WRITE,
            MemoryAccess::Fetch => Permissions::EXECUTE,
            MemoryAccess::Map | MemoryAccess::Protect | MemoryAccess::Unmap => {
                return Err(MemoryError::new(
                    address,
                    access,
                    MemoryErrorReason::InvalidRange,
                ));
            }
        };
        self.resolve(address_space, address, size, permission, access)?;
        Ok(())
    }

    fn resolve(
        &self,
        address_space: &AddressSpace,
        address: u64,
        size: usize,
        permission: Permissions,
        access: MemoryAccess,
    ) -> Result<Vec<Span>, MemoryError> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let size = u64::try_from(size)
            .map_err(|_| MemoryError::new(address, access, MemoryErrorReason::InvalidRange))?;
        let end = checked_end(address, size, access)?;
        let mut spans = Vec::new();
        let mut cursor = address;
        while cursor < end {
            let (mapping_start, mapping) = address_space
                .mappings
                .range(..=cursor)
                .next_back()
                .filter(|(_, mapping)| cursor < mapping.end)
                .ok_or_else(|| MemoryError::new(cursor, access, MemoryErrorReason::Unmapped))?;
            if !mapping.permissions.contains(permission) {
                return Err(MemoryError::new(
                    cursor,
                    access,
                    MemoryErrorReason::PermissionDenied,
                ));
            }
            let span_end = mapping.end.min(end);
            spans.push(Span {
                backing: mapping.backing,
                offset: mapping.backing_offset + (cursor - *mapping_start),
                len: usize::try_from(span_end - cursor).unwrap(),
            });
            cursor = span_end;
        }
        Ok(spans)
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

struct Span {
    backing: BackingId,
    offset: u64,
    len: usize,
}

fn checked_end(address: u64, size: u64, access: MemoryAccess) -> Result<u64, MemoryError> {
    if size == 0 {
        return Err(MemoryError::new(
            address,
            access,
            MemoryErrorReason::InvalidRange,
        ));
    }
    address
        .checked_add(size)
        .ok_or_else(|| MemoryError::new(address, access, MemoryErrorReason::InvalidRange))
}

fn read_backing(backing: &Backing, offset: u64, destination: &mut [u8]) {
    let mut copied = 0;
    while copied < destination.len() {
        let current = offset + copied as u64;
        let page_number = current / MEMORY_PAGE_SIZE;
        let page_offset = (current % MEMORY_PAGE_SIZE) as usize;
        let len = (MEMORY_PAGE_SIZE as usize - page_offset).min(destination.len() - copied);
        if let Some(page) = backing.pages.get(&page_number) {
            destination[copied..copied + len]
                .copy_from_slice(&page[page_offset..page_offset + len]);
        } else {
            destination[copied..copied + len].fill(0);
        }
        copied += len;
    }
}

fn write_backing(backing: &mut Backing, offset: u64, source: &[u8]) {
    let mut copied = 0;
    while copied < source.len() {
        let current = offset + copied as u64;
        let page_number = current / MEMORY_PAGE_SIZE;
        let page_offset = (current % MEMORY_PAGE_SIZE) as usize;
        let len = (MEMORY_PAGE_SIZE as usize - page_offset).min(source.len() - copied);
        let bytes = &source[copied..copied + len];
        if bytes.iter().any(|byte| *byte != 0) || backing.pages.contains_key(&page_number) {
            let page = backing
                .pages
                .entry(page_number)
                .or_insert_with(|| Box::new([0; MEMORY_PAGE_SIZE as usize]));
            page[page_offset..page_offset + len].copy_from_slice(bytes);
        }
        copied += len;
    }
}
