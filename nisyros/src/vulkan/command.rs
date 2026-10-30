use super::{Device, GetDeviceProcAddr, VK_SUCCESS, VkDevice, VkQueue, VkResult, load_device};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::ptr;

type VkCommandPool = u64;
type VkCommandBuffer = *mut c_void;
type VkSemaphore = u64;

const VK_TIMEOUT: VkResult = 2;
const VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO: i32 = 9;
const VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO: i32 = 39;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO: i32 = 40;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO: i32 = 42;
const VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO: i32 = 1_000_207_002;
const VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO: i32 = 1_000_207_004;
const VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO: i32 = 1_000_207_005;
const VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO: i32 = 1_000_077_000;
const VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR: i32 = 1_000_079_000;
const VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR: i32 = 1_000_079_001;
const VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT: u32 = 1;
const VK_STRUCTURE_TYPE_MEMORY_BARRIER_2: i32 = 1_000_314_000;
const VK_STRUCTURE_TYPE_DEPENDENCY_INFO: i32 = 1_000_314_003;
const VK_STRUCTURE_TYPE_SUBMIT_INFO_2: i32 = 1_000_314_004;
const VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO: i32 = 1_000_314_005;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO: i32 = 1_000_314_006;
const VK_STRUCTURE_TYPE_DEVICE_MEMORY_COPY_KHR: i32 = 1_000_318_000;
const VK_STRUCTURE_TYPE_COPY_DEVICE_MEMORY_INFO_KHR: i32 = 1_000_318_001;
const VK_STRUCTURE_TYPE_DISPATCH_INDIRECT_2_INFO_KHR: i32 = 1_000_318_011;
const VK_STRUCTURE_TYPE_MEMORY_RANGE_BARRIERS_INFO_KHR: i32 = 1_000_318_004;
const VK_STRUCTURE_TYPE_MEMORY_RANGE_BARRIER_KHR: i32 = 1_000_318_005;
const VK_SEMAPHORE_TYPE_TIMELINE: i32 = 1;
const VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT: u64 = 0x1_0000;
const VK_ACCESS_2_MEMORY_READ_BIT: u64 = 0x8000;
const VK_ACCESS_2_MEMORY_WRITE_BIT: u64 = 0x1_0000;
const VK_ADDRESS_COMMAND_STORAGE_BUFFER_USAGE_BIT_KHR: u32 = 4;
const VK_QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
const VK_PIPELINE_BIND_POINT_COMPUTE: i32 = 1;
const VK_SHADER_STAGE_COMPUTE_BIT: u32 = 0x20;

type CreateCommandPool = unsafe extern "system" fn(
    VkDevice,
    *const CommandPoolCreateInfo,
    *const c_void,
    *mut VkCommandPool,
) -> VkResult;
type DestroyCommandPool = unsafe extern "system" fn(VkDevice, VkCommandPool, *const c_void);
type AllocateCommandBuffers = unsafe extern "system" fn(
    VkDevice,
    *const CommandBufferAllocateInfo,
    *mut VkCommandBuffer,
) -> VkResult;
type BeginCommandBuffer =
    unsafe extern "system" fn(VkCommandBuffer, *const CommandBufferBeginInfo) -> VkResult;
type EndCommandBuffer = unsafe extern "system" fn(VkCommandBuffer) -> VkResult;
type CmdCopyMemory = unsafe extern "system" fn(VkCommandBuffer, *const CopyDeviceMemoryInfo);
type CmdFillMemory =
    unsafe extern "system" fn(VkCommandBuffer, *const DeviceAddressRange, u32, u32);
type CmdPipelineBarrier2 = unsafe extern "system" fn(VkCommandBuffer, *const DependencyInfo);
type CmdBindPipeline = unsafe extern "system" fn(VkCommandBuffer, i32, u64);
type CmdPushConstants =
    unsafe extern "system" fn(VkCommandBuffer, u64, u32, u32, u32, *const c_void);
type CmdDispatch = unsafe extern "system" fn(VkCommandBuffer, u32, u32, u32);
type CmdDispatchIndirect2 =
    unsafe extern "system" fn(VkCommandBuffer, *const DispatchIndirect2Info);
type CreateSemaphore = unsafe extern "system" fn(
    VkDevice,
    *const SemaphoreCreateInfo,
    *const c_void,
    *mut VkSemaphore,
) -> VkResult;
type DestroySemaphore = unsafe extern "system" fn(VkDevice, VkSemaphore, *const c_void);
type GetSemaphoreCounterValue =
    unsafe extern "system" fn(VkDevice, VkSemaphore, *mut u64) -> VkResult;
type WaitSemaphores =
    unsafe extern "system" fn(VkDevice, *const SemaphoreWaitInfo, u64) -> VkResult;
type SignalSemaphore = unsafe extern "system" fn(VkDevice, *const SemaphoreSignalInfo) -> VkResult;
type QueueSubmit2 = unsafe extern "system" fn(VkQueue, u32, *const SubmitInfo2, u64) -> VkResult;
type GetSemaphoreFd =
    unsafe extern "system" fn(VkDevice, *const SemaphoreGetFdInfo, *mut i32) -> VkResult;
type ImportSemaphoreFd =
    unsafe extern "system" fn(VkDevice, *const ImportSemaphoreFdInfo) -> VkResult;

#[repr(C)]
struct CommandPoolCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_family_index: u32,
}

#[repr(C)]
struct CommandBufferAllocateInfo {
    s_type: i32,
    p_next: *const c_void,
    command_pool: VkCommandPool,
    level: i32,
    command_buffer_count: u32,
}

#[repr(C)]
struct CommandBufferBeginInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    inheritance_info: *const c_void,
}

#[repr(C)]
struct DeviceAddressRange {
    address: u64,
    size: u64,
}

#[repr(C)]
struct DeviceMemoryCopy {
    s_type: i32,
    p_next: *const c_void,
    src_range: DeviceAddressRange,
    src_flags: u32,
    dst_range: DeviceAddressRange,
    dst_flags: u32,
}

#[repr(C)]
struct CopyDeviceMemoryInfo {
    s_type: i32,
    p_next: *const c_void,
    region_count: u32,
    regions: *const DeviceMemoryCopy,
}

#[repr(C)]
struct MemoryBarrier2 {
    s_type: i32,
    p_next: *const c_void,
    src_stage: u64,
    src_access: u64,
    dst_stage: u64,
    dst_access: u64,
}

#[repr(C)]
struct DependencyInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    memory_barrier_count: u32,
    memory_barriers: *const MemoryBarrier2,
    buffer_memory_barrier_count: u32,
    buffer_memory_barriers: *const c_void,
    image_memory_barrier_count: u32,
    image_memory_barriers: *const c_void,
}

#[repr(C)]
struct MemoryRangeBarrier {
    s_type: i32,
    p_next: *const c_void,
    src_stage: u64,
    src_access: u64,
    dst_stage: u64,
    dst_access: u64,
    src_queue_family: u32,
    dst_queue_family: u32,
    range: DeviceAddressRange,
    address_flags: u32,
}

#[repr(C)]
struct MemoryRangeBarriersInfo {
    s_type: i32,
    p_next: *const c_void,
    barrier_count: u32,
    barriers: *const MemoryRangeBarrier,
}

#[repr(C)]
struct DispatchIndirect2Info {
    s_type: i32,
    p_next: *const c_void,
    range: DeviceAddressRange,
    address_flags: u32,
}

#[repr(C)]
struct SemaphoreTypeCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    semaphore_type: i32,
    initial_value: u64,
}

#[repr(C)]
struct SemaphoreCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
}

#[repr(C)]
struct ExportSemaphoreCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    handle_types: u32,
}

#[repr(C)]
struct SemaphoreGetFdInfo {
    s_type: i32,
    p_next: *const c_void,
    semaphore: VkSemaphore,
    handle_type: u32,
}

#[repr(C)]
struct ImportSemaphoreFdInfo {
    s_type: i32,
    p_next: *const c_void,
    semaphore: VkSemaphore,
    flags: u32,
    handle_type: u32,
    fd: i32,
}

#[repr(C)]
struct SemaphoreWaitInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    semaphore_count: u32,
    semaphores: *const VkSemaphore,
    values: *const u64,
}

#[repr(C)]
struct SemaphoreSignalInfo {
    s_type: i32,
    p_next: *const c_void,
    semaphore: VkSemaphore,
    value: u64,
}

#[repr(C)]
struct SemaphoreSubmitInfo {
    s_type: i32,
    p_next: *const c_void,
    semaphore: VkSemaphore,
    value: u64,
    stage_mask: u64,
    device_index: u32,
}

#[repr(C)]
struct CommandBufferSubmitInfo {
    s_type: i32,
    p_next: *const c_void,
    command_buffer: VkCommandBuffer,
    device_mask: u32,
}

#[repr(C)]
struct SubmitInfo2 {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    wait_count: u32,
    waits: *const SemaphoreSubmitInfo,
    command_count: u32,
    commands: *const CommandBufferSubmitInfo,
    signal_count: u32,
    signals: *const SemaphoreSubmitInfo,
}

#[derive(Clone, Copy)]
pub(super) struct CommandFns {
    create_pool: CreateCommandPool,
    destroy_pool: DestroyCommandPool,
    allocate_buffers: AllocateCommandBuffers,
    begin_buffer: BeginCommandBuffer,
    end_buffer: EndCommandBuffer,
    copy_memory: CmdCopyMemory,
    fill_memory: CmdFillMemory,
    barrier2: CmdPipelineBarrier2,
    bind_pipeline: CmdBindPipeline,
    push_constants: CmdPushConstants,
    dispatch: CmdDispatch,
    dispatch_indirect2: CmdDispatchIndirect2,
    create_semaphore: CreateSemaphore,
    destroy_semaphore: DestroySemaphore,
    query_semaphore: GetSemaphoreCounterValue,
    wait_semaphores: WaitSemaphores,
    signal_semaphore: SignalSemaphore,
    queue_submit2: QueueSubmit2,
    get_semaphore_fd: Option<GetSemaphoreFd>,
    import_semaphore_fd: Option<ImportSemaphoreFd>,
}

impl CommandFns {
    pub(super) unsafe fn load(get_proc: GetDeviceProcAddr, device: VkDevice) -> Option<Self> {
        // SAFETY: These signatures match Vulkan 1.3 and the address extension.
        unsafe {
            Some(Self {
                create_pool: load_device(get_proc, device, c"vkCreateCommandPool")?,
                destroy_pool: load_device(get_proc, device, c"vkDestroyCommandPool")?,
                allocate_buffers: load_device(get_proc, device, c"vkAllocateCommandBuffers")?,
                begin_buffer: load_device(get_proc, device, c"vkBeginCommandBuffer")?,
                end_buffer: load_device(get_proc, device, c"vkEndCommandBuffer")?,
                copy_memory: load_device(get_proc, device, c"vkCmdCopyMemoryKHR")?,
                fill_memory: load_device(get_proc, device, c"vkCmdFillMemoryKHR")?,
                barrier2: load_device(get_proc, device, c"vkCmdPipelineBarrier2")?,
                bind_pipeline: load_device(get_proc, device, c"vkCmdBindPipeline")?,
                push_constants: load_device(get_proc, device, c"vkCmdPushConstants")?,
                dispatch: load_device(get_proc, device, c"vkCmdDispatch")?,
                dispatch_indirect2: load_device(get_proc, device, c"vkCmdDispatchIndirect2KHR")?,
                create_semaphore: load_device(get_proc, device, c"vkCreateSemaphore")?,
                destroy_semaphore: load_device(get_proc, device, c"vkDestroySemaphore")?,
                query_semaphore: load_device(get_proc, device, c"vkGetSemaphoreCounterValue")?,
                wait_semaphores: load_device(get_proc, device, c"vkWaitSemaphores")?,
                signal_semaphore: load_device(get_proc, device, c"vkSignalSemaphore")?,
                queue_submit2: load_device(get_proc, device, c"vkQueueSubmit2")?,
                get_semaphore_fd: load_device(get_proc, device, c"vkGetSemaphoreFdKHR"),
                import_semaphore_fd: load_device(get_proc, device, c"vkImportSemaphoreFdKHR"),
            })
        }
    }
}

#[derive(Clone, Copy)]
pub enum CommandOp {
    Copy {
        dst: u64,
        src: u64,
        bytes: u64,
    },
    Fill {
        dst: u64,
        bytes: u64,
        pattern: u32,
    },
    BarrierAll,
    BarrierRange {
        address: u64,
        bytes: u64,
    },
    Dispatch {
        pipeline: u64,
        layout: u64,
        grid: [u32; 3],
        arguments: u64,
    },
    DispatchIndirect {
        pipeline: u64,
        layout: u64,
        grid_address: u64,
        arguments: u64,
    },
}

pub struct Command {
    device: VkDevice,
    pool: VkCommandPool,
    buffer: VkCommandBuffer,
    functions: CommandFns,
}

impl Command {
    pub fn buffer(&self) -> VkCommandBuffer {
        self.buffer
    }

    pub fn record(&self, operations: &[CommandOp]) -> Result<(), VkResult> {
        let begin = CommandBufferBeginInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            p_next: ptr::null(),
            flags: 0,
            inheritance_info: ptr::null(),
        };
        // SAFETY: The command buffer is initial and owned by this live device.
        unsafe {
            let result = (self.functions.begin_buffer)(self.buffer, &begin);
            if result != VK_SUCCESS {
                return Err(result);
            }
            for operation in operations {
                match *operation {
                    CommandOp::Copy { dst, src, bytes } => {
                        let region = DeviceMemoryCopy {
                            s_type: VK_STRUCTURE_TYPE_DEVICE_MEMORY_COPY_KHR,
                            p_next: ptr::null(),
                            src_range: DeviceAddressRange {
                                address: src,
                                size: bytes,
                            },
                            src_flags: VK_ADDRESS_COMMAND_STORAGE_BUFFER_USAGE_BIT_KHR,
                            dst_range: DeviceAddressRange {
                                address: dst,
                                size: bytes,
                            },
                            dst_flags: VK_ADDRESS_COMMAND_STORAGE_BUFFER_USAGE_BIT_KHR,
                        };
                        let info = CopyDeviceMemoryInfo {
                            s_type: VK_STRUCTURE_TYPE_COPY_DEVICE_MEMORY_INFO_KHR,
                            p_next: ptr::null(),
                            region_count: 1,
                            regions: &region,
                        };
                        (self.functions.copy_memory)(self.buffer, &info);
                    }
                    CommandOp::Fill {
                        dst,
                        bytes,
                        pattern,
                    } => {
                        let range = DeviceAddressRange {
                            address: dst,
                            size: bytes,
                        };
                        (self.functions.fill_memory)(
                            self.buffer,
                            &range,
                            VK_ADDRESS_COMMAND_STORAGE_BUFFER_USAGE_BIT_KHR,
                            pattern,
                        );
                    }
                    CommandOp::BarrierAll => {
                        let barrier = MemoryBarrier2 {
                            s_type: VK_STRUCTURE_TYPE_MEMORY_BARRIER_2,
                            p_next: ptr::null(),
                            src_stage: VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                            src_access: VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
                            dst_stage: VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                            dst_access: VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
                        };
                        let dependency = DependencyInfo {
                            s_type: VK_STRUCTURE_TYPE_DEPENDENCY_INFO,
                            p_next: ptr::null(),
                            flags: 0,
                            memory_barrier_count: 1,
                            memory_barriers: &barrier,
                            buffer_memory_barrier_count: 0,
                            buffer_memory_barriers: ptr::null(),
                            image_memory_barrier_count: 0,
                            image_memory_barriers: ptr::null(),
                        };
                        (self.functions.barrier2)(self.buffer, &dependency);
                    }
                    CommandOp::BarrierRange { address, bytes } => {
                        let barrier = MemoryRangeBarrier {
                            s_type: VK_STRUCTURE_TYPE_MEMORY_RANGE_BARRIER_KHR,
                            p_next: ptr::null(),
                            src_stage: VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                            src_access: VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
                            dst_stage: VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
                            dst_access: VK_ACCESS_2_MEMORY_READ_BIT | VK_ACCESS_2_MEMORY_WRITE_BIT,
                            src_queue_family: VK_QUEUE_FAMILY_IGNORED,
                            dst_queue_family: VK_QUEUE_FAMILY_IGNORED,
                            range: DeviceAddressRange {
                                address,
                                size: bytes,
                            },
                            address_flags: VK_ADDRESS_COMMAND_STORAGE_BUFFER_USAGE_BIT_KHR,
                        };
                        let ranges = MemoryRangeBarriersInfo {
                            s_type: VK_STRUCTURE_TYPE_MEMORY_RANGE_BARRIERS_INFO_KHR,
                            p_next: ptr::null(),
                            barrier_count: 1,
                            barriers: &barrier,
                        };
                        let dependency = DependencyInfo {
                            s_type: VK_STRUCTURE_TYPE_DEPENDENCY_INFO,
                            p_next: (&raw const ranges).cast(),
                            flags: 0,
                            memory_barrier_count: 0,
                            memory_barriers: ptr::null(),
                            buffer_memory_barrier_count: 0,
                            buffer_memory_barriers: ptr::null(),
                            image_memory_barrier_count: 0,
                            image_memory_barriers: ptr::null(),
                        };
                        (self.functions.barrier2)(self.buffer, &dependency);
                    }
                    CommandOp::Dispatch {
                        pipeline,
                        layout,
                        grid,
                        arguments,
                    } => {
                        (self.functions.bind_pipeline)(
                            self.buffer,
                            VK_PIPELINE_BIND_POINT_COMPUTE,
                            pipeline,
                        );
                        (self.functions.push_constants)(
                            self.buffer,
                            layout,
                            VK_SHADER_STAGE_COMPUTE_BIT,
                            0,
                            8,
                            (&raw const arguments).cast(),
                        );
                        (self.functions.dispatch)(self.buffer, grid[0], grid[1], grid[2]);
                    }
                    CommandOp::DispatchIndirect {
                        pipeline,
                        layout,
                        grid_address,
                        arguments,
                    } => {
                        (self.functions.bind_pipeline)(
                            self.buffer,
                            VK_PIPELINE_BIND_POINT_COMPUTE,
                            pipeline,
                        );
                        (self.functions.push_constants)(
                            self.buffer,
                            layout,
                            VK_SHADER_STAGE_COMPUTE_BIT,
                            0,
                            8,
                            (&raw const arguments).cast(),
                        );
                        let info = DispatchIndirect2Info {
                            s_type: VK_STRUCTURE_TYPE_DISPATCH_INDIRECT_2_INFO_KHR,
                            p_next: ptr::null(),
                            range: DeviceAddressRange {
                                address: grid_address,
                                size: 12,
                            },
                            address_flags: VK_ADDRESS_COMMAND_STORAGE_BUFFER_USAGE_BIT_KHR,
                        };
                        (self.functions.dispatch_indirect2)(self.buffer, &info);
                    }
                }
            }
            let result = (self.functions.end_buffer)(self.buffer);
            if result == VK_SUCCESS {
                Ok(())
            } else {
                Err(result)
            }
        }
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        // SAFETY: The caller has established that this command is not pending.
        unsafe { (self.functions.destroy_pool)(self.device, self.pool, ptr::null()) };
    }
}

pub struct Timeline {
    device: VkDevice,
    semaphore: VkSemaphore,
    functions: CommandFns,
    exportable: bool,
}

pub struct SubmitSegment<'a> {
    pub command: &'a Command,
    pub waits: Vec<(&'a Timeline, u64)>,
    pub signals: Vec<(&'a Timeline, u64)>,
}

impl Timeline {
    pub fn query(&self) -> Result<u64, VkResult> {
        let mut value = 0;
        // SAFETY: This timeline semaphore belongs to the live device.
        let result =
            unsafe { (self.functions.query_semaphore)(self.device, self.semaphore, &mut value) };
        if result == VK_SUCCESS {
            Ok(value)
        } else {
            Err(result)
        }
    }

    pub fn wait(&self, value: u64, timeout_ns: u64) -> Result<bool, VkResult> {
        let info = SemaphoreWaitInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
            p_next: ptr::null(),
            flags: 0,
            semaphore_count: 1,
            semaphores: &self.semaphore,
            values: &value,
        };
        // SAFETY: The semaphore and value arrays remain valid for the call.
        let result = unsafe { (self.functions.wait_semaphores)(self.device, &info, timeout_ns) };
        match result {
            VK_SUCCESS => Ok(true),
            VK_TIMEOUT => Ok(false),
            error => Err(error),
        }
    }

    pub fn signal(&self, value: u64) -> Result<(), VkResult> {
        let info = SemaphoreSignalInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO,
            p_next: ptr::null(),
            semaphore: self.semaphore,
            value,
        };
        // SAFETY: This host-signaled timeline belongs to the live device.
        let result = unsafe { (self.functions.signal_semaphore)(self.device, &info) };
        if result == VK_SUCCESS {
            Ok(())
        } else {
            Err(result)
        }
    }

    pub fn export_fd(&self) -> Result<OwnedFd, VkResult> {
        if !self.exportable {
            return Err(-8);
        }
        let Some(get_fd) = self.functions.get_semaphore_fd else {
            return Err(-8);
        };
        let info = SemaphoreGetFdInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR,
            p_next: ptr::null(),
            semaphore: self.semaphore,
            handle_type: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT,
        };
        let mut fd = -1;
        // SAFETY: The timeline was created with OPAQUE_FD export enabled.
        let result = unsafe { get_fd(self.device, &info, &mut fd) };
        if result != VK_SUCCESS || fd < 0 {
            return Err(result);
        }
        // SAFETY: Vulkan transfers a new owned descriptor to the caller.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        // SAFETY: The owning device is live and no pending submission uses this
        // semaphore when its queue owner is destroyed.
        unsafe { (self.functions.destroy_semaphore)(self.device, self.semaphore, ptr::null()) };
    }
}

impl Device {
    pub fn create_command(&self, family: u32) -> Result<Command, VkResult> {
        let info = CommandPoolCreateInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            queue_family_index: family,
        };
        // SAFETY: The family was provisioned when this device opened.
        unsafe {
            let mut pool = 0;
            let result = (self.command.create_pool)(self.handle, &info, ptr::null(), &mut pool);
            if result != VK_SUCCESS {
                return Err(result);
            }
            let allocation = CommandBufferAllocateInfo {
                s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                p_next: ptr::null(),
                command_pool: pool,
                level: 0,
                command_buffer_count: 1,
            };
            let mut buffer = ptr::null_mut();
            let result = (self.command.allocate_buffers)(self.handle, &allocation, &mut buffer);
            if result != VK_SUCCESS || buffer.is_null() {
                (self.command.destroy_pool)(self.handle, pool, ptr::null());
                return Err(result);
            }
            Ok(Command {
                device: self.handle,
                pool,
                buffer,
                functions: self.command,
            })
        }
    }

    pub fn create_timeline(&self, exportable: bool) -> Result<Timeline, VkResult> {
        if exportable && !self.peer_timeline_fd {
            return Err(-8);
        }
        let external = ExportSemaphoreCreateInfo {
            s_type: VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO,
            p_next: ptr::null(),
            handle_types: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT,
        };
        let timeline = SemaphoreTypeCreateInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
            p_next: if exportable {
                (&raw const external).cast()
            } else {
                ptr::null()
            },
            semaphore_type: VK_SEMAPHORE_TYPE_TIMELINE,
            initial_value: 0,
        };
        let info = SemaphoreCreateInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
            p_next: (&raw const timeline).cast(),
            flags: 0,
        };
        // SAFETY: The timeline chain is live and has Vulkan C layout.
        unsafe {
            let mut semaphore = 0;
            let result =
                (self.command.create_semaphore)(self.handle, &info, ptr::null(), &mut semaphore);
            if result != VK_SUCCESS {
                return Err(result);
            }
            Ok(Timeline {
                device: self.handle,
                semaphore,
                functions: self.command,
                exportable,
            })
        }
    }

    pub fn import_timeline_fd(&self, fd: OwnedFd) -> Result<Timeline, VkResult> {
        if !self.peer_timeline_fd {
            return Err(-8);
        }
        let Some(import_fd) = self.command.import_semaphore_fd else {
            return Err(-8);
        };
        let timeline = self.create_timeline(false)?;
        let info = ImportSemaphoreFdInfo {
            s_type: VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR,
            p_next: ptr::null(),
            semaphore: timeline.semaphore,
            flags: 0,
            handle_type: VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT,
            fd: fd.as_raw_fd(),
        };
        // SAFETY: The new timeline and duplicated FD remain live through
        // import. On success Vulkan takes ownership of this FD.
        let result = unsafe { import_fd(self.handle, &info) };
        if result != VK_SUCCESS {
            return Err(result);
        }
        let _consumed_fd = fd.into_raw_fd();
        Ok(timeline)
    }

    pub fn submit_segments(
        &self,
        queue_index: usize,
        segments: &[SubmitSegment<'_>],
    ) -> Result<(), VkResult> {
        let to_info = |&(timeline, value): &(&Timeline, u64)| SemaphoreSubmitInfo {
            s_type: VK_STRUCTURE_TYPE_SEMAPHORE_SUBMIT_INFO,
            p_next: ptr::null(),
            semaphore: timeline.semaphore,
            value,
            stage_mask: VK_PIPELINE_STAGE_2_ALL_COMMANDS_BIT,
            device_index: 0,
        };
        let waits: Vec<Vec<_>> = segments
            .iter()
            .map(|segment| segment.waits.iter().map(&to_info).collect())
            .collect();
        let signals: Vec<Vec<_>> = segments
            .iter()
            .map(|segment| segment.signals.iter().map(&to_info).collect())
            .collect();
        let commands: Vec<_> = segments
            .iter()
            .map(|segment| CommandBufferSubmitInfo {
                s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_SUBMIT_INFO,
                p_next: ptr::null(),
                command_buffer: segment.command.buffer(),
                device_mask: 0,
            })
            .collect();
        let submits: Vec<_> = segments
            .iter()
            .enumerate()
            .map(|(index, _)| SubmitInfo2 {
                s_type: VK_STRUCTURE_TYPE_SUBMIT_INFO_2,
                p_next: ptr::null(),
                flags: 0,
                wait_count: waits[index].len() as u32,
                waits: waits[index].as_ptr(),
                command_count: 1,
                commands: &commands[index],
                signal_count: signals[index].len() as u32,
                signals: signals[index].as_ptr(),
            })
            .collect();
        let queue = self.queues[queue_index]._handle;
        // SAFETY: The arrays and native commands remain live through submission.
        let result = unsafe {
            (self.command.queue_submit2)(queue, submits.len() as u32, submits.as_ptr(), 0)
        };
        if result == VK_SUCCESS {
            Ok(())
        } else {
            Err(result)
        }
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn command_layout_matches_c_header() {
        assert_eq!(
            (
                size_of::<DeviceMemoryCopy>(),
                size_of::<CopyDeviceMemoryInfo>()
            ),
            (64, 32)
        );
        assert_eq!(
            (
                size_of::<MemoryRangeBarrier>(),
                size_of::<MemoryRangeBarriersInfo>()
            ),
            (80, 32)
        );
        assert_eq!(
            (size_of::<MemoryBarrier2>(), size_of::<DependencyInfo>()),
            (48, 64)
        );
        assert_eq!(size_of::<DispatchIndirect2Info>(), 40);
        assert_eq!(
            (
                size_of::<CommandPoolCreateInfo>(),
                size_of::<CommandBufferAllocateInfo>(),
                size_of::<CommandBufferBeginInfo>()
            ),
            (24, 32, 32)
        );
        assert_eq!(
            (
                size_of::<SemaphoreSubmitInfo>(),
                size_of::<CommandBufferSubmitInfo>(),
                size_of::<SubmitInfo2>()
            ),
            (48, 32, 64)
        );
        assert_eq!(size_of::<SemaphoreWaitInfo>(), 40);
        assert_eq!(
            (
                size_of::<ExportSemaphoreCreateInfo>(),
                size_of::<ImportSemaphoreFdInfo>(),
                size_of::<SemaphoreGetFdInfo>()
            ),
            (24, 40, 32)
        );
    }
}
