use super::{Device, GetDeviceProcAddr, VK_SUCCESS, VkDevice, VkResult, load_device};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::ptr;

type VkBuffer = u64;
type VkDeviceMemory = u64;

const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO: i32 = 5;
const VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE: i32 = 6;
const VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO: i32 = 12;
const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO: i32 = 1_000_060_000;
const VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO: i32 = 1_000_244_001;
const VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO: i32 = 1_000_072_000;
const VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT: i32 = 1_000_178_000;
const VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT: i32 = 1_000_178_001;
const VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO: i32 = 1_000_072_002;
const VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR: i32 = 1_000_074_000;
const VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR: i32 = 1_000_074_002;
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT: u32 = 1;
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT: u32 = 0x80;
const VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT: u32 = 2;
const VK_BUFFER_USAGE_TRANSFER_SRC_BIT: u32 = 1;
const VK_BUFFER_USAGE_TRANSFER_DST_BIT: u32 = 2;
const VK_BUFFER_USAGE_STORAGE_BUFFER_BIT: u32 = 0x20;
const VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT: u32 = 0x100;
const VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT: u32 = 0x20_000;
pub(super) const BUFFER_USAGE: u32 = VK_BUFFER_USAGE_TRANSFER_SRC_BIT
    | VK_BUFFER_USAGE_TRANSFER_DST_BIT
    | VK_BUFFER_USAGE_STORAGE_BUFFER_BIT
    | VK_BUFFER_USAGE_INDIRECT_BUFFER_BIT
    | VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT;
const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: u32 = 2;
const VK_MEMORY_PROPERTY_DEVICE_COHERENT_BIT_AMD: u32 = 0x40;
const VK_SHARING_MODE_EXCLUSIVE: i32 = 0;
const VK_SHARING_MODE_CONCURRENT: i32 = 1;
const VK_WHOLE_SIZE: u64 = u64::MAX;

type CreateBuffer = unsafe extern "system" fn(
    VkDevice,
    *const BufferCreateInfo,
    *const c_void,
    *mut VkBuffer,
) -> VkResult;
type DestroyBuffer = unsafe extern "system" fn(VkDevice, VkBuffer, *const c_void);
type GetBufferMemoryRequirements =
    unsafe extern "system" fn(VkDevice, VkBuffer, *mut MemoryRequirements);
type AllocateMemory = unsafe extern "system" fn(
    VkDevice,
    *const MemoryAllocateInfo,
    *const c_void,
    *mut VkDeviceMemory,
) -> VkResult;
type FreeMemory = unsafe extern "system" fn(VkDevice, VkDeviceMemory, *const c_void);
type BindBufferMemory =
    unsafe extern "system" fn(VkDevice, VkBuffer, VkDeviceMemory, u64) -> VkResult;
type GetBufferDeviceAddress =
    unsafe extern "system" fn(VkDevice, *const BufferDeviceAddressInfo) -> u64;
type MapMemory = unsafe extern "system" fn(
    VkDevice,
    VkDeviceMemory,
    u64,
    u64,
    u32,
    *mut *mut c_void,
) -> VkResult;
type UnmapMemory = unsafe extern "system" fn(VkDevice, VkDeviceMemory);
type FlushMappedMemoryRanges =
    unsafe extern "system" fn(VkDevice, u32, *const MappedMemoryRange) -> VkResult;
type InvalidateMappedMemoryRanges =
    unsafe extern "system" fn(VkDevice, u32, *const MappedMemoryRange) -> VkResult;
type GetMemoryHostPointerProperties = unsafe extern "system" fn(
    VkDevice,
    u32,
    *const c_void,
    *mut MemoryHostPointerProperties,
) -> VkResult;
type GetMemoryFd =
    unsafe extern "system" fn(VkDevice, *const MemoryGetFdInfo, *mut i32) -> VkResult;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryType {
    property_flags: u32,
    heap_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MemoryHeap {
    size: u64,
    flags: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct MemoryProperties {
    type_count: u32,
    types: [MemoryType; 32],
    heap_count: u32,
    heaps: [MemoryHeap; 16],
}

impl MemoryProperties {
    pub(super) fn heaps(&self) -> Vec<(u64, u32)> {
        self.heaps[..self.heap_count.min(16) as usize]
            .iter()
            .map(|heap| (heap.size, heap.flags))
            .collect()
    }
}

#[repr(C)]
struct BufferCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    size: u64,
    usage: u32,
    sharing_mode: i32,
    queue_family_index_count: u32,
    queue_family_indices: *const u32,
}

#[repr(C)]
struct ExternalMemoryBufferCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    handle_types: u32,
}

#[repr(C)]
struct ImportMemoryHostPointerInfo {
    s_type: i32,
    p_next: *const c_void,
    handle_type: u32,
    host_pointer: *mut c_void,
}

#[repr(C)]
struct ExportMemoryAllocateInfo {
    s_type: i32,
    p_next: *const c_void,
    handle_types: u32,
}

#[repr(C)]
struct ImportMemoryFdInfo {
    s_type: i32,
    p_next: *const c_void,
    handle_type: u32,
    fd: i32,
}

#[repr(C)]
struct MemoryGetFdInfo {
    s_type: i32,
    p_next: *const c_void,
    memory: VkDeviceMemory,
    handle_type: u32,
}

#[repr(C)]
struct MemoryHostPointerProperties {
    s_type: i32,
    p_next: *mut c_void,
    memory_type_bits: u32,
}

#[repr(C)]
#[derive(Default)]
struct MemoryRequirements {
    size: u64,
    alignment: u64,
    memory_type_bits: u32,
}

#[repr(C)]
struct MemoryAllocateFlagsInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    device_mask: u32,
}

#[repr(C)]
struct MemoryAllocateInfo {
    s_type: i32,
    p_next: *const c_void,
    allocation_size: u64,
    memory_type_index: u32,
}

#[repr(C)]
struct BufferDeviceAddressInfo {
    s_type: i32,
    p_next: *const c_void,
    buffer: VkBuffer,
}

#[repr(C)]
struct MappedMemoryRange {
    s_type: i32,
    p_next: *const c_void,
    memory: VkDeviceMemory,
    offset: u64,
    size: u64,
}

pub(super) struct MemoryFns {
    create_buffer: CreateBuffer,
    destroy_buffer: DestroyBuffer,
    get_requirements: GetBufferMemoryRequirements,
    allocate_memory: AllocateMemory,
    free_memory: FreeMemory,
    bind_buffer_memory: BindBufferMemory,
    get_buffer_device_address: GetBufferDeviceAddress,
    map_memory: MapMemory,
    unmap_memory: UnmapMemory,
    flush: FlushMappedMemoryRanges,
    invalidate: InvalidateMappedMemoryRanges,
    get_host_pointer_properties: Option<GetMemoryHostPointerProperties>,
    get_memory_fd: Option<GetMemoryFd>,
}

impl MemoryFns {
    pub(super) unsafe fn load(get_proc: GetDeviceProcAddr, device: VkDevice) -> Option<Self> {
        // SAFETY: These signatures match the Vulkan 1.3 C declarations.
        unsafe {
            Some(Self {
                create_buffer: load_device(get_proc, device, c"vkCreateBuffer")?,
                destroy_buffer: load_device(get_proc, device, c"vkDestroyBuffer")?,
                get_requirements: load_device(get_proc, device, c"vkGetBufferMemoryRequirements")?,
                allocate_memory: load_device(get_proc, device, c"vkAllocateMemory")?,
                free_memory: load_device(get_proc, device, c"vkFreeMemory")?,
                bind_buffer_memory: load_device(get_proc, device, c"vkBindBufferMemory")?,
                get_buffer_device_address: load_device(
                    get_proc,
                    device,
                    c"vkGetBufferDeviceAddress",
                )?,
                map_memory: load_device(get_proc, device, c"vkMapMemory")?,
                unmap_memory: load_device(get_proc, device, c"vkUnmapMemory")?,
                flush: load_device(get_proc, device, c"vkFlushMappedMemoryRanges")?,
                invalidate: load_device(get_proc, device, c"vkInvalidateMappedMemoryRanges")?,
                get_host_pointer_properties: load_device(
                    get_proc,
                    device,
                    c"vkGetMemoryHostPointerPropertiesEXT",
                ),
                get_memory_fd: load_device(get_proc, device, c"vkGetMemoryFdKHR"),
            })
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AllocError {
    Invalid,
    Unsupported,
    OutOfMemory,
    Backend,
}

impl AllocError {
    fn from_vulkan(result: VkResult) -> Self {
        if result == -1 || result == -2 {
            Self::OutOfMemory
        } else {
            Self::Backend
        }
    }
}

pub struct AllocRequest<'a> {
    pub bytes: u64,
    pub alignment: u64,
    pub required_properties: u32,
    pub preferred_properties: u32,
    pub queue_families: &'a [u32],
    pub required_heap: Option<u32>,
    pub preferred_heap: Option<u32>,
    pub exclusive: bool,
}

pub struct Allocation {
    buffer: VkBuffer,
    memory: VkDeviceMemory,
    pub base: u64,
    pub bytes: u64,
    buffer_offset: u64,
    buffer_size: u64,
    allocation_size: u64,
    memory_type_index: u32,
    exportable: bool,
    memory_properties: u32,
    mapped: *mut c_void,
    mapped_host: *mut c_void,
    exclusive: bool,
}

impl Allocation {
    pub fn exclusive(&self) -> bool {
        self.exclusive
    }
    pub fn memory_info(&self, device: &Device) -> (u64, u64, u32, u32, u32) {
        let memory_type = self.memory_type_index;
        let heap = device.memory_properties.types[memory_type as usize].heap_index;
        (
            self.allocation_size,
            device.memory_properties.heaps[heap as usize].size,
            memory_type,
            heap,
            self.memory_properties & 7,
        )
    }

    pub fn is_mapped(&self) -> bool {
        !self.mapped.is_null()
    }
}

impl Device {
    pub fn memory_heap_count(&self) -> u32 {
        self.memory_properties.heap_count.min(16)
    }

    pub fn allocate(&self, request: &AllocRequest<'_>) -> Result<Allocation, AllocError> {
        if request.bytes == 0 || request.alignment == 0 || !request.alignment.is_power_of_two() {
            return Err(AllocError::Invalid);
        }
        let padded_size = request
            .bytes
            .checked_add(request.alignment - 1)
            .ok_or(AllocError::Invalid)?;
        let families = request.queue_families;
        let external = ExternalMemoryBufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
            p_next: ptr::null(),
            handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
        };
        let create_info = BufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            p_next: if self.peer_memory_fd {
                (&raw const external).cast()
            } else {
                ptr::null()
            },
            flags: 0,
            size: padded_size,
            usage: BUFFER_USAGE,
            sharing_mode: if families.len() > 1 && !request.exclusive {
                VK_SHARING_MODE_CONCURRENT
            } else {
                VK_SHARING_MODE_EXCLUSIVE
            },
            queue_family_index_count: if families.len() > 1 && !request.exclusive {
                families.len() as u32
            } else {
                0
            },
            queue_family_indices: if families.len() > 1 && !request.exclusive {
                families.as_ptr()
            } else {
                ptr::null()
            },
        };
        // SAFETY: The device and function table are live, and every Vulkan
        // output has its exact C layout. Failure paths release created objects.
        unsafe {
            let mut buffer = 0;
            let status =
                (self.memory.create_buffer)(self.handle, &create_info, ptr::null(), &mut buffer);
            if status != VK_SUCCESS {
                return Err(AllocError::from_vulkan(status));
            }
            let mut requirements = MemoryRequirements::default();
            (self.memory.get_requirements)(self.handle, buffer, &mut requirements);
            let Some((memory_type_index, memory_properties)) = self.choose_memory_type(
                requirements.memory_type_bits,
                request.required_properties,
                request.preferred_properties,
                request.required_heap,
                request.preferred_heap,
            ) else {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Unsupported);
            };
            let export = ExportMemoryAllocateInfo {
                s_type: VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
                p_next: ptr::null(),
                handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
            };
            let flags = MemoryAllocateFlagsInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
                p_next: if self.peer_memory_fd {
                    (&raw const export).cast()
                } else {
                    ptr::null()
                },
                flags: VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
                device_mask: 0,
            };
            let allocate_info = MemoryAllocateInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                p_next: (&raw const flags).cast(),
                allocation_size: requirements.size,
                memory_type_index,
            };
            let mut memory = 0;
            let status = (self.memory.allocate_memory)(
                self.handle,
                &allocate_info,
                ptr::null(),
                &mut memory,
            );
            if status != VK_SUCCESS {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::from_vulkan(status));
            }
            let status = (self.memory.bind_buffer_memory)(self.handle, buffer, memory, 0);
            if status != VK_SUCCESS {
                (self.memory.free_memory)(self.handle, memory, ptr::null());
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::from_vulkan(status));
            }
            let address_info = BufferDeviceAddressInfo {
                s_type: VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
                p_next: ptr::null(),
                buffer,
            };
            let raw_address = (self.memory.get_buffer_device_address)(self.handle, &address_info);
            let base = raw_address
                .checked_add(request.alignment - 1)
                .map(|address| address & !(request.alignment - 1));
            let Some((base, buffer_offset)) = base
                .filter(|&base| base != 0)
                .and_then(|base| base.checked_sub(raw_address).map(|offset| (base, offset)))
                .filter(|(_, offset)| {
                    offset
                        .checked_add(request.bytes)
                        .is_some_and(|end| end <= padded_size)
                })
            else {
                (self.memory.free_memory)(self.handle, memory, ptr::null());
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Backend);
            };
            Ok(Allocation {
                buffer,
                memory,
                base,
                bytes: request.bytes,
                buffer_offset,
                buffer_size: padded_size,
                allocation_size: requirements.size,
                memory_type_index,
                exportable: self.peer_memory_fd,
                memory_properties,
                mapped: ptr::null_mut(),
                mapped_host: ptr::null_mut(),
                exclusive: request.exclusive || families.len() <= 1,
            })
        }
    }

    pub fn register_host_pointer(
        &self,
        host: *mut c_void,
        bytes: u64,
    ) -> Result<Allocation, AllocError> {
        let Some(alignment) = self.external_host_alignment else {
            return Err(AllocError::Unsupported);
        };
        let Some(get_properties) = self.memory.get_host_pointer_properties else {
            return Err(AllocError::Unsupported);
        };
        if host.is_null()
            || alignment == 0
            || !(host as usize as u64).is_multiple_of(alignment)
            || bytes == 0
            || !bytes.is_multiple_of(alignment)
        {
            return Err(AllocError::Invalid);
        }
        let mut host_properties = MemoryHostPointerProperties {
            s_type: VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT,
            p_next: ptr::null_mut(),
            memory_type_bits: 0,
        };
        // SAFETY: The caller keeps the aligned host range alive through
        // unregister, and the extension was enabled at device creation.
        unsafe {
            let result = get_properties(
                self.handle,
                VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
                host,
                &mut host_properties,
            );
            if result != VK_SUCCESS {
                return Err(AllocError::from_vulkan(result));
            }
            let external = ExternalMemoryBufferCreateInfo {
                s_type: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
                p_next: ptr::null(),
                handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
            };
            let buffer_info = BufferCreateInfo {
                s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                p_next: (&raw const external).cast(),
                flags: 0,
                size: bytes,
                usage: BUFFER_USAGE,
                sharing_mode: VK_SHARING_MODE_CONCURRENT,
                queue_family_index_count: 0,
                queue_family_indices: ptr::null(),
            };
            // Imported ranges are made accessible to every provisioned family.
            // A single-family session uses exclusive sharing.
            let families: Vec<u32> = self.queues.iter().map(|queue| queue.family).collect();
            let mut unique_families = families;
            unique_families.sort_unstable();
            unique_families.dedup();
            let buffer_info = BufferCreateInfo {
                sharing_mode: if unique_families.len() > 1 {
                    VK_SHARING_MODE_CONCURRENT
                } else {
                    VK_SHARING_MODE_EXCLUSIVE
                },
                queue_family_index_count: if unique_families.len() > 1 {
                    unique_families.len() as u32
                } else {
                    0
                },
                queue_family_indices: if unique_families.len() > 1 {
                    unique_families.as_ptr()
                } else {
                    ptr::null()
                },
                ..buffer_info
            };
            let mut buffer = 0;
            let result =
                (self.memory.create_buffer)(self.handle, &buffer_info, ptr::null(), &mut buffer);
            if result != VK_SUCCESS {
                return Err(AllocError::from_vulkan(result));
            }
            let mut requirements = MemoryRequirements::default();
            (self.memory.get_requirements)(self.handle, buffer, &mut requirements);
            if requirements.size > bytes {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Invalid);
            }
            let Some((memory_type_index, memory_properties)) = self.choose_memory_type(
                requirements.memory_type_bits & host_properties.memory_type_bits,
                VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | 4,
                0,
                None,
                None,
            ) else {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Unsupported);
            };
            let import = ImportMemoryHostPointerInfo {
                s_type: VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
                p_next: ptr::null(),
                handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
                host_pointer: host,
            };
            let flags = MemoryAllocateFlagsInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
                p_next: (&raw const import).cast(),
                flags: VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
                device_mask: 0,
            };
            let allocate_info = MemoryAllocateInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                p_next: (&raw const flags).cast(),
                allocation_size: requirements.size,
                memory_type_index,
            };
            let mut memory = 0;
            let result = (self.memory.allocate_memory)(
                self.handle,
                &allocate_info,
                ptr::null(),
                &mut memory,
            );
            if result != VK_SUCCESS {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::from_vulkan(result));
            }
            let result = (self.memory.bind_buffer_memory)(self.handle, buffer, memory, 0);
            if result != VK_SUCCESS {
                (self.memory.free_memory)(self.handle, memory, ptr::null());
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::from_vulkan(result));
            }
            let address_info = BufferDeviceAddressInfo {
                s_type: VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
                p_next: ptr::null(),
                buffer,
            };
            let base = (self.memory.get_buffer_device_address)(self.handle, &address_info);
            if base == 0 {
                (self.memory.free_memory)(self.handle, memory, ptr::null());
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Backend);
            }
            Ok(Allocation {
                buffer,
                memory,
                base,
                bytes,
                buffer_offset: 0,
                buffer_size: bytes,
                allocation_size: requirements.size,
                memory_type_index,
                exportable: false,
                memory_properties,
                mapped: ptr::null_mut(),
                mapped_host: host,
                exclusive: unique_families.len() <= 1,
            })
        }
    }

    pub fn export_allocation_fd(&self, allocation: &Allocation) -> Result<OwnedFd, AllocError> {
        if !self.peer_memory_fd || !allocation.exportable {
            return Err(AllocError::Unsupported);
        }
        let Some(get_fd) = self.memory.get_memory_fd else {
            return Err(AllocError::Unsupported);
        };
        let info = MemoryGetFdInfo {
            s_type: VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
            p_next: ptr::null(),
            memory: allocation.memory,
            handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
        };
        let mut fd = -1;
        // SAFETY: The allocation is live and was created exportable.
        let result = unsafe { get_fd(self.handle, &info, &mut fd) };
        if result != VK_SUCCESS || fd < 0 {
            return Err(AllocError::from_vulkan(result));
        }
        // SAFETY: Vulkan transfers ownership of a newly created descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn import_peer_fd(
        &self,
        source: &Allocation,
        source_offset: u64,
        bytes: u64,
        fd: OwnedFd,
    ) -> Result<Allocation, AllocError> {
        if !self.peer_memory_fd || !source.exportable || bytes == 0 {
            return Err(AllocError::Unsupported);
        }
        let logical_offset = source
            .buffer_offset
            .checked_add(source_offset)
            .ok_or(AllocError::Invalid)?;
        if source_offset
            .checked_add(bytes)
            .is_none_or(|end| end > source.bytes)
            || logical_offset
                .checked_add(bytes)
                .is_none_or(|end| end > source.buffer_size)
        {
            return Err(AllocError::Invalid);
        }
        let external = ExternalMemoryBufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
            p_next: ptr::null(),
            handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
        };
        let mut families: Vec<u32> = self.queues.iter().map(|queue| queue.family).collect();
        families.sort_unstable();
        families.dedup();
        let create_info = BufferCreateInfo {
            s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
            p_next: (&raw const external).cast(),
            flags: 0,
            size: source.buffer_size,
            usage: BUFFER_USAGE,
            sharing_mode: if families.len() > 1 {
                VK_SHARING_MODE_CONCURRENT
            } else {
                VK_SHARING_MODE_EXCLUSIVE
            },
            queue_family_index_count: if families.len() > 1 {
                families.len() as u32
            } else {
                0
            },
            queue_family_indices: if families.len() > 1 {
                families.as_ptr()
            } else {
                ptr::null()
            },
        };
        // SAFETY: The descriptor and buffer metadata remain live through the
        // import; Vulkan consumes the FD only after successful allocation.
        unsafe {
            let mut buffer = 0;
            let result =
                (self.memory.create_buffer)(self.handle, &create_info, ptr::null(), &mut buffer);
            if result != VK_SUCCESS {
                return Err(AllocError::from_vulkan(result));
            }
            let mut requirements = MemoryRequirements::default();
            (self.memory.get_requirements)(self.handle, buffer, &mut requirements);
            let index = source.memory_type_index;
            if index >= self.memory_properties.type_count
                || index >= 32
                || requirements.memory_type_bits & (1 << index) == 0
                || requirements.size > source.allocation_size
            {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Unsupported);
            }
            let import = ImportMemoryFdInfo {
                s_type: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                p_next: ptr::null(),
                handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT,
                fd: fd.as_raw_fd(),
            };
            let flags = MemoryAllocateFlagsInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
                p_next: (&raw const import).cast(),
                flags: VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
                device_mask: 0,
            };
            let allocate_info = MemoryAllocateInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                p_next: (&raw const flags).cast(),
                allocation_size: source.allocation_size,
                memory_type_index: index,
            };
            let mut memory = 0;
            let result = (self.memory.allocate_memory)(
                self.handle,
                &allocate_info,
                ptr::null(),
                &mut memory,
            );
            if result != VK_SUCCESS {
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::from_vulkan(result));
            }
            let _consumed_fd = fd.into_raw_fd();
            let result = (self.memory.bind_buffer_memory)(self.handle, buffer, memory, 0);
            if result != VK_SUCCESS {
                (self.memory.free_memory)(self.handle, memory, ptr::null());
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::from_vulkan(result));
            }
            let address_info = BufferDeviceAddressInfo {
                s_type: VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
                p_next: ptr::null(),
                buffer,
            };
            let raw_address = (self.memory.get_buffer_device_address)(self.handle, &address_info);
            let Some(base) = raw_address
                .checked_add(logical_offset)
                .filter(|&base| base != 0)
            else {
                (self.memory.free_memory)(self.handle, memory, ptr::null());
                (self.memory.destroy_buffer)(self.handle, buffer, ptr::null());
                return Err(AllocError::Backend);
            };
            Ok(Allocation {
                buffer,
                memory,
                base,
                bytes,
                buffer_offset: logical_offset,
                buffer_size: source.buffer_size,
                allocation_size: source.allocation_size,
                memory_type_index: index,
                exportable: false,
                memory_properties: self.memory_properties.types[index as usize].property_flags,
                mapped: ptr::null_mut(),
                mapped_host: ptr::null_mut(),
                exclusive: families.len() <= 1,
            })
        }
    }

    fn choose_memory_type(
        &self,
        bits: u32,
        required: u32,
        preferred: u32,
        required_heap: Option<u32>,
        preferred_heap: Option<u32>,
    ) -> Option<(u32, u32)> {
        (0..self.memory_properties.type_count.min(32))
            .filter(|&index| bits & (1 << index) != 0)
            .filter_map(|index| {
                let memory_type = self.memory_properties.types[index as usize];
                let flags = memory_type.property_flags;
                (required_heap.is_none_or(|heap| heap == memory_type.heap_index)
                    && flags & required == required
                    && flags & VK_MEMORY_PROPERTY_DEVICE_COHERENT_BIT_AMD == 0)
                    .then_some((index, flags, memory_type.heap_index))
            })
            .max_by_key(|(_, flags, heap)| {
                (
                    preferred_heap == Some(*heap),
                    (flags & preferred).count_ones(),
                )
            })
            .map(|(index, flags, _)| (index, flags))
    }

    pub fn free_allocation(&self, allocation: Allocation) {
        // SAFETY: The allocation belongs to this device and no work references it.
        unsafe {
            if allocation.is_mapped() {
                (self.memory.unmap_memory)(self.handle, allocation.memory);
            }
            (self.memory.destroy_buffer)(self.handle, allocation.buffer, ptr::null());
            (self.memory.free_memory)(self.handle, allocation.memory, ptr::null());
        }
    }

    pub fn map_allocation(
        &self,
        allocation: &mut Allocation,
        address: u64,
    ) -> Result<*mut c_void, AllocError> {
        if allocation.is_mapped() {
            return Err(AllocError::Invalid);
        }
        if allocation.memory_properties & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT == 0 {
            return Err(AllocError::Unsupported);
        }
        let offset = address
            .checked_sub(allocation.base)
            .and_then(|offset| offset.checked_add(allocation.buffer_offset))
            .ok_or(AllocError::Invalid)?;
        let offset = usize::try_from(offset).map_err(|_| AllocError::Invalid)?;
        // SAFETY: The full host-visible allocation is mapped once and retained
        // until the matching unmap call.
        unsafe {
            let mut mapped = ptr::null_mut();
            let result = (self.memory.map_memory)(
                self.handle,
                allocation.memory,
                0,
                VK_WHOLE_SIZE,
                0,
                &mut mapped,
            );
            if result != VK_SUCCESS {
                return Err(AllocError::from_vulkan(result));
            }
            allocation.mapped = mapped;
            allocation.mapped_host = mapped.cast::<u8>().add(offset).cast();
            Ok(allocation.mapped_host)
        }
    }

    pub fn unmap_allocation(&self, allocation: &mut Allocation) {
        // SAFETY: The allocation has one active mapping on this device.
        unsafe { (self.memory.unmap_memory)(self.handle, allocation.memory) };
        allocation.mapped = ptr::null_mut();
        allocation.mapped_host = ptr::null_mut();
    }

    pub fn visibility(&self, allocation: &Allocation, flush: bool) -> Result<(), AllocError> {
        if !allocation.is_mapped() {
            return Err(AllocError::Invalid);
        }
        // Flush the whole mapped allocation. Offset zero and VK_WHOLE_SIZE
        // satisfy noncoherent atom alignment without exposing device limits.
        let range = MappedMemoryRange {
            s_type: VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
            p_next: ptr::null(),
            memory: allocation.memory,
            offset: 0,
            size: VK_WHOLE_SIZE,
        };
        // SAFETY: The range is currently mapped and belongs to this device.
        let result = unsafe {
            if flush {
                (self.memory.flush)(self.handle, 1, &range)
            } else {
                (self.memory.invalidate)(self.handle, 1, &range)
            }
        };
        if result == VK_SUCCESS {
            Ok(())
        } else {
            Err(AllocError::from_vulkan(result))
        }
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_tests {
    use super::*;
    use std::mem::{align_of, size_of};

    #[test]
    fn memory_layout_matches_c_header() {
        assert_eq!(
            (
                size_of::<MemoryProperties>(),
                align_of::<MemoryProperties>()
            ),
            (520, 8)
        );
        assert_eq!(size_of::<BufferCreateInfo>(), 56);
        assert_eq!(size_of::<MemoryRequirements>(), 24);
        assert_eq!(size_of::<MemoryAllocateInfo>(), 32);
        assert_eq!(size_of::<MemoryAllocateFlagsInfo>(), 24);
        assert_eq!(size_of::<MappedMemoryRange>(), 40);
        assert_eq!(size_of::<ExternalMemoryBufferCreateInfo>(), 24);
        assert_eq!(size_of::<ImportMemoryHostPointerInfo>(), 32);
        assert_eq!(size_of::<MemoryHostPointerProperties>(), 24);
        assert_eq!(
            (
                size_of::<ExportMemoryAllocateInfo>(),
                size_of::<ImportMemoryFdInfo>(),
                size_of::<MemoryGetFdInfo>()
            ),
            (24, 24, 32)
        );
    }
}
