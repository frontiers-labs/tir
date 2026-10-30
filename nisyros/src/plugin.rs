//! Independently versioned execution-provider ABI and built-in backend selection.

use super::*;
use std::ffi::CString;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;

#[path = "rocm.rs"]
mod rocm;

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> i32;
}

type PluginGetApi = unsafe extern "C" fn(u32, *const PluginServices, u32, *mut c_void) -> i32;

struct DynamicLibrary(*mut c_void);

impl DynamicLibrary {
    fn open(path: &std::ffi::OsStr) -> Option<(Self, PluginGetApi)> {
        let path = CString::new(path.as_bytes()).ok()?;
        let handle = unsafe { dlopen(path.as_ptr(), 2) };
        if handle.is_null() {
            return None;
        }
        let library = Self(handle);
        let symbol = unsafe { dlsym(handle, c"nisyros_plugin_get_api".as_ptr()) };
        if symbol.is_null() {
            return None;
        }
        let getter = unsafe { std::mem::transmute::<*mut c_void, PluginGetApi>(symbol) };
        Some((library, getter))
    }
}

impl Drop for DynamicLibrary {
    fn drop(&mut self) {
        unsafe { dlclose(self.0) };
    }
}

const PLUGIN_ABI_VERSION: u32 = 2;
const SERVICES_ABI_VERSION: u32 = 1;

#[repr(C)]
pub struct PluginServices {
    struct_size: u32,
    abi_version: u32,
    context: *mut c_void,
    report_error: Option<unsafe extern "C" fn(*mut c_void, i32, i32, *const c_char, u32)>,
}

#[repr(C)]
struct PluginDeviceInfo {
    struct_size: u32,
    abi_version: u32,
    backend: u32,
    api_version: u32,
    vendor_id: u32,
    device_id: u32,
    device_type: u32,
    driver_id: u32,
    strict_profile_supported: u32,
    queue_family_count: u32,
    memory_heap_count: u32,
    cooperative_matrix_count: u32,
    features: u64,
    external_host_alignment: u64,
    cooperative_matrix_revision: u32,
    shader_bfloat16_revision: u32,
    shader_enqueue_revision: u32,
    cooperative_matrix_stages: u32,
    max_grid: [u32; 3],
    peer_memory_fd: u32,
    peer_timeline_fd: u32,
    timestamp_period_ns: f32,
    name: [c_char; 256],
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    driver_name: [c_char; 256],
}

#[repr(C)]
struct PluginQueueFamilyInfo {
    flags: u32,
    count: u32,
    timestamp_valid_bits: u32,
}

#[repr(C)]
struct PluginMemoryHeapInfo {
    bytes: u64,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
struct PluginQueueRequest {
    family: u32,
    count: u32,
    priority: f32,
}

#[repr(C)]
struct PluginAllocRequest {
    bytes: u64,
    alignment: u64,
    required_properties: u32,
    preferred_properties: u32,
    required_heap: u32,
    preferred_heap: u32,
    exclusive: u32,
    queue_family_count: u32,
    queue_families: *const u32,
}

#[repr(C)]
struct PluginAllocationInfo {
    base: u64,
    bytes: u64,
    backing_bytes: u64,
    heap_bytes: u64,
    memory_type_index: u32,
    heap_index: u32,
    properties: u32,
    exclusive: u32,
}

#[repr(C)]
struct PluginCommandOp {
    kind: u32,
    reserved: u32,
    args: [u64; 8],
}

#[repr(C)]
struct PluginSubmitPoint {
    timeline: *mut c_void,
    value: u64,
}

#[repr(C)]
struct PluginSubmitSegment {
    command: *mut c_void,
    wait_count: u32,
    waits: *const PluginSubmitPoint,
    signal_count: u32,
    signals: *const PluginSubmitPoint,
}

pub struct SubmitSegment<'a> {
    pub command: Option<&'a Command>,
    pub waits: Vec<(&'a Timeline, u64)>,
    pub signals: Vec<(&'a Timeline, u64)>,
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
    BarrierRangeScoped {
        address: u64,
        bytes: u64,
        src_stage: u32,
        src_access: u32,
        dst_stage: u32,
        dst_access: u32,
        src_family: u32,
        dst_family: u32,
    },
    Dispatch {
        pipeline: *mut c_void,
        grid: [u32; 3],
        arguments: u64,
    },
    DispatchIndirect {
        pipeline: *mut c_void,
        grid_address: u64,
        arguments: u64,
    },
    Timestamp {
        pool: *mut c_void,
        destination: u64,
    },
}

impl CommandOp {
    fn encode(self) -> PluginCommandOp {
        let mut args = [0; 8];
        let kind = match self {
            Self::Copy { dst, src, bytes } => {
                args[..3].copy_from_slice(&[dst, src, bytes]);
                1
            }
            Self::Fill {
                dst,
                bytes,
                pattern,
            } => {
                args[..3].copy_from_slice(&[dst, bytes, u64::from(pattern)]);
                2
            }
            Self::BarrierAll => 3,
            Self::BarrierRange { address, bytes } => {
                args[..2].copy_from_slice(&[address, bytes]);
                4
            }
            Self::BarrierRangeScoped {
                address,
                bytes,
                src_stage,
                src_access,
                dst_stage,
                dst_access,
                src_family,
                dst_family,
            } => {
                args = [
                    address,
                    bytes,
                    u64::from(src_stage),
                    u64::from(src_access),
                    u64::from(dst_stage),
                    u64::from(dst_access),
                    u64::from(src_family),
                    u64::from(dst_family),
                ];
                5
            }
            Self::Dispatch {
                pipeline,
                grid,
                arguments,
            } => {
                args[..5].copy_from_slice(&[
                    pipeline as u64,
                    u64::from(grid[0]),
                    u64::from(grid[1]),
                    u64::from(grid[2]),
                    arguments,
                ]);
                6
            }
            Self::DispatchIndirect {
                pipeline,
                grid_address,
                arguments,
            } => {
                args[..3].copy_from_slice(&[pipeline as u64, grid_address, arguments]);
                7
            }
            Self::Timestamp { pool, destination } => {
                args[..2].copy_from_slice(&[pool as u64, destination]);
                8
            }
        };
        PluginCommandOp {
            kind,
            reserved: 0,
            args,
        }
    }
}

type InstanceOpen = unsafe extern "C" fn(*const PluginServices, *mut *mut c_void) -> i32;
type InstanceClose = unsafe extern "C" fn(*mut c_void);
type DeviceCount = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;
type DeviceInfoQuery = unsafe extern "C" fn(*mut c_void, u32, *mut PluginDeviceInfo) -> i32;
type QueueFamilyQuery =
    unsafe extern "C" fn(*mut c_void, u32, u32, *mut PluginQueueFamilyInfo) -> i32;
type MemoryHeapQuery =
    unsafe extern "C" fn(*mut c_void, u32, u32, *mut PluginMemoryHeapInfo) -> i32;
type MatrixQuery = unsafe extern "C" fn(*mut c_void, u32, u32, *mut CooperativeMatrixTuple) -> i32;
type DeviceOpen =
    unsafe extern "C" fn(*mut c_void, u32, u32, *const PluginQueueRequest, *mut *mut c_void) -> i32;
type DeviceClose = unsafe extern "C" fn(*mut c_void);
type AllocationCreate = unsafe extern "C" fn(
    *mut c_void,
    *const PluginAllocRequest,
    *mut *mut c_void,
    *mut PluginAllocationInfo,
) -> i32;
type AllocationDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void);
type AllocationMap = unsafe extern "C" fn(*mut c_void, *mut c_void, u64, *mut *mut c_void) -> i32;
type AllocationUnmap = unsafe extern "C" fn(*mut c_void, *mut c_void);
type AllocationVisibility = unsafe extern "C" fn(*mut c_void, *mut c_void, u32) -> i32;
type AllocationRegisterHost = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    u64,
    *mut *mut c_void,
    *mut PluginAllocationInfo,
) -> i32;
type AllocationExportFd = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut i32, *mut u64) -> i32;
type AllocationImportFd = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    u64,
    u64,
    i32,
    *mut *mut c_void,
    *mut PluginAllocationInfo,
) -> i32;
type DeviceWaitIdle = unsafe extern "C" fn(*mut c_void, *mut i32) -> i32;
type TimelineCreate = unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void, *mut i32) -> i32;
type TimelineImportFd = unsafe extern "C" fn(*mut c_void, i32, *mut *mut c_void, *mut i32) -> i32;
type TimelineDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void);
type TimelineQuery = unsafe extern "C" fn(*mut c_void, *mut u64, *mut i32) -> i32;
type TimelineWait = unsafe extern "C" fn(*mut c_void, u64, u64, *mut u32, *mut i32) -> i32;
type TimelineSignal = unsafe extern "C" fn(*mut c_void, u64, *mut i32) -> i32;
type TimelineExportFd = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> i32;
type PipelineImport =
    unsafe extern "C" fn(*mut c_void, *const u8, u64, *mut *mut c_void, *mut i32) -> i32;
type PipelineDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void);
type QueryCreate = unsafe extern "C" fn(*mut c_void, *mut *mut c_void, *mut i32) -> i32;
type QueryDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void);
type CommandCreate = unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void, *mut i32) -> i32;
type CommandDestroy = unsafe extern "C" fn(*mut c_void, *mut c_void);
type CommandRecord =
    unsafe extern "C" fn(*mut c_void, u32, *const PluginCommandOp, *mut i32) -> i32;
type QueueSubmit =
    unsafe extern "C" fn(*mut c_void, u32, u32, *const PluginSubmitSegment, *mut i32) -> i32;
type InstanceNativeHandles =
    unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void, *mut *mut c_void) -> i32;
type DeviceNativeHandles =
    unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void, *mut *mut c_void) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct PluginApi {
    struct_size: u32,
    abi_version: u32,
    instance_open: Option<InstanceOpen>,
    instance_close: Option<InstanceClose>,
    device_count: Option<DeviceCount>,
    device_info: Option<DeviceInfoQuery>,
    queue_family_info: Option<QueueFamilyQuery>,
    memory_heap_info: Option<MemoryHeapQuery>,
    cooperative_matrix: Option<MatrixQuery>,
    device_open: Option<DeviceOpen>,
    device_close: Option<DeviceClose>,
    allocation_create: Option<AllocationCreate>,
    allocation_destroy: Option<AllocationDestroy>,
    allocation_map: Option<AllocationMap>,
    allocation_unmap: Option<AllocationUnmap>,
    allocation_visibility: Option<AllocationVisibility>,
    allocation_register_host: Option<AllocationRegisterHost>,
    allocation_export_fd: Option<AllocationExportFd>,
    allocation_import_fd: Option<AllocationImportFd>,
    device_wait_idle: Option<DeviceWaitIdle>,
    timeline_create: Option<TimelineCreate>,
    timeline_import_fd: Option<TimelineImportFd>,
    timeline_destroy: Option<TimelineDestroy>,
    timeline_query: Option<TimelineQuery>,
    timeline_wait: Option<TimelineWait>,
    timeline_signal: Option<TimelineSignal>,
    timeline_export_fd: Option<TimelineExportFd>,
    pipeline_import: Option<PipelineImport>,
    pipeline_destroy: Option<PipelineDestroy>,
    query_create: Option<QueryCreate>,
    query_destroy: Option<QueryDestroy>,
    command_create: Option<CommandCreate>,
    command_destroy: Option<CommandDestroy>,
    command_record: Option<CommandRecord>,
    queue_submit: Option<QueueSubmit>,
    instance_native_handles: Option<InstanceNativeHandles>,
    device_native_handles: Option<DeviceNativeHandles>,
}

struct VulkanInstance {
    instance: vulkan::Instance,
    devices: Vec<vulkan::DeviceRecord>,
}

#[derive(Clone, Copy)]
pub struct QueueFamilyProperties {
    pub flags: u32,
    pub count: u32,
    pub timestamp_valid_bits: u32,
}

pub struct QueueMetadata {
    pub family: u32,
    pub index: u32,
}

pub struct QueueRequest {
    pub family: u32,
    pub count: u32,
    pub priority: f32,
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

#[derive(Clone, Copy)]
pub enum AllocError {
    Invalid,
    Unsupported,
    OutOfMemory,
    Backend,
}

pub enum PipelineError {
    Incompatible,
    Unsupported,
    Backend(BackendError),
}

#[derive(Clone, Copy)]
pub struct BackendError {
    pub status: i32,
    pub code: i32,
}

pub struct DeviceRecord {
    pub index: u32,
    pub backend: u32,
    pub api_version: u32,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: u32,
    pub driver_id: u32,
    pub strict_profile_supported: bool,
    pub name: [c_char; 256],
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    pub driver_name: [c_char; 256],
    pub queue_families: Vec<QueueFamilyProperties>,
    pub memory_heaps: Vec<(u64, u32)>,
    pub features: u64,
    pub cooperative_matrix_revision: u32,
    pub shader_bfloat16_revision: u32,
    pub shader_enqueue_revision: u32,
    pub cooperative_matrix_stages: u32,
    pub cooperative_matrices: Vec<CooperativeMatrixTuple>,
    pub timestamp_period_ns: f32,
    pub external_host_alignment: Option<u64>,
    pub max_grid: [u32; 3],
    pub peer_memory_fd: bool,
    pub peer_timeline_fd: bool,
}

fn valid_services(services: *const PluginServices) -> bool {
    if services.is_null() {
        return false;
    }
    let services = unsafe { &*services };
    services.struct_size as usize >= std::mem::size_of::<PluginServices>()
        && services.abi_version == SERVICES_ABI_VERSION
}

fn report(services: *const PluginServices, status: i32, message: &CStr) {
    let services = unsafe { &*services };
    if let Some(callback) = services.report_error {
        unsafe {
            callback(
                services.context,
                status,
                0,
                message.as_ptr(),
                message.to_bytes().len() as u32,
            )
        };
    }
}

/// Negotiates the linked Vulkan execution provider's independent table.
///
/// # Safety
/// `services` must be readable and `table` must hold `table_size` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_plugin_get_api(
    abi_version: u32,
    services: *const PluginServices,
    table_size: u32,
    table: *mut c_void,
) -> i32 {
    if table.is_null() || table_size < 8 {
        return INVALID_ARGUMENT;
    }
    let provider_size = std::mem::size_of::<PluginApi>();
    unsafe {
        ptr::write_bytes(
            table.cast::<u8>(),
            0,
            (table_size as usize).min(provider_size),
        )
    };
    if abi_version != PLUGIN_ABI_VERSION {
        return UNSUPPORTED;
    }
    if !valid_services(services) {
        return INVALID_ARGUMENT;
    }
    unsafe {
        ptr::write(table.cast::<u32>(), provider_size as u32);
        ptr::write(table.cast::<u32>().add(1), PLUGIN_ABI_VERSION);
    }
    if (table_size as usize) < provider_size {
        return INCOMPLETE;
    }
    unsafe {
        let api = &mut *table.cast::<PluginApi>();
        api.instance_open = Some(instance_open);
        api.instance_close = Some(instance_close);
        api.device_count = Some(device_count);
        api.device_info = Some(device_info);
        api.queue_family_info = Some(queue_family_info);
        api.memory_heap_info = Some(memory_heap_info);
        api.cooperative_matrix = Some(cooperative_matrix);
        api.device_open = Some(device_open);
        api.device_close = Some(device_close);
        api.allocation_create = Some(allocation_create);
        api.allocation_destroy = Some(allocation_destroy);
        api.allocation_map = Some(allocation_map);
        api.allocation_unmap = Some(allocation_unmap);
        api.allocation_visibility = Some(allocation_visibility);
        api.allocation_register_host = Some(allocation_register_host);
        api.allocation_export_fd = Some(allocation_export_fd);
        api.allocation_import_fd = Some(allocation_import_fd);
        api.device_wait_idle = Some(device_wait_idle);
        api.timeline_create = Some(timeline_create);
        api.timeline_import_fd = Some(timeline_import_fd);
        api.timeline_destroy = Some(timeline_destroy);
        api.timeline_query = Some(timeline_query);
        api.timeline_wait = Some(timeline_wait);
        api.timeline_signal = Some(timeline_signal);
        api.timeline_export_fd = Some(timeline_export_fd);
        api.pipeline_import = Some(pipeline_import);
        api.pipeline_destroy = Some(pipeline_destroy);
        api.query_create = Some(query_create);
        api.query_destroy = Some(query_destroy);
        api.command_create = Some(command_create);
        api.command_destroy = Some(command_destroy);
        api.command_record = Some(command_record);
        api.queue_submit = Some(queue_submit);
        api.instance_native_handles = Some(instance_native_handles);
        api.device_native_handles = Some(device_native_handles);
    }
    SUCCESS
}

unsafe extern "C" fn instance_open(services: *const PluginServices, out: *mut *mut c_void) -> i32 {
    if !valid_services(services) || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let Ok(instance) = vulkan::Instance::open() else {
        report(services, NO_DEVICE, c"Vulkan loader unavailable");
        return NO_DEVICE;
    };
    let Ok(devices) = instance.devices() else {
        report(services, DEVICE_LOST, c"Vulkan device enumeration failed");
        return DEVICE_LOST;
    };
    let owner = Box::new(VulkanInstance { instance, devices });
    unsafe { *out = Box::into_raw(owner).cast() };
    SUCCESS
}

unsafe extern "C" fn instance_close(instance: *mut c_void) {
    if !instance.is_null() {
        unsafe { drop(Box::from_raw(instance.cast::<VulkanInstance>())) };
    }
}

unsafe extern "C" fn device_count(instance: *mut c_void, out: *mut u32) -> i32 {
    if instance.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let owner = unsafe { &*instance.cast::<VulkanInstance>() };
    let Ok(count) = u32::try_from(owner.devices.len()) else {
        return INVALID_ARGUMENT;
    };
    unsafe { *out = count };
    SUCCESS
}

unsafe extern "C" fn device_info(
    instance: *mut c_void,
    index: u32,
    out: *mut PluginDeviceInfo,
) -> i32 {
    if instance.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let out = unsafe { &mut *out };
    if (out.struct_size as usize) < std::mem::size_of::<PluginDeviceInfo>()
        || out.abi_version != PLUGIN_ABI_VERSION
    {
        return INVALID_ARGUMENT;
    }
    let owner = unsafe { &*instance.cast::<VulkanInstance>() };
    let Some(record) = owner.devices.get(index as usize) else {
        return INVALID_ARGUMENT;
    };
    out.backend = BACKEND_VULKAN;
    out.api_version = record.api_version;
    out.vendor_id = record.vendor_id;
    out.device_id = record.device_id;
    out.device_type = record.device_type;
    out.driver_id = record.driver_id;
    out.strict_profile_supported = u32::from(record.strict_profile_supported);
    out.queue_family_count = record.queue_families.len() as u32;
    out.memory_heap_count = record.memory_heaps.len() as u32;
    out.cooperative_matrix_count = record.cooperative_matrices.len() as u32;
    out.features = record.features;
    out.external_host_alignment = record.external_host_alignment.unwrap_or(0);
    out.cooperative_matrix_revision = record.cooperative_matrix_revision;
    out.shader_bfloat16_revision = record.shader_bfloat16_revision;
    out.shader_enqueue_revision = record.shader_enqueue_revision;
    out.cooperative_matrix_stages = record.cooperative_matrix_stages;
    out.max_grid = record.max_grid;
    out.peer_memory_fd = u32::from(record.peer_memory_fd || record.peer_dma_buf_fd);
    out.peer_timeline_fd = u32::from(record.peer_timeline_fd);
    out.timestamp_period_ns = record.timestamp_period_ns;
    out.name = record.name;
    out.device_uuid = record.device_uuid;
    out.driver_uuid = record.driver_uuid;
    out.driver_name = record.driver_name;
    SUCCESS
}

unsafe extern "C" fn queue_family_info(
    instance: *mut c_void,
    device_index: u32,
    family: u32,
    out: *mut PluginQueueFamilyInfo,
) -> i32 {
    if instance.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let owner = unsafe { &*instance.cast::<VulkanInstance>() };
    let Some(queue) = owner
        .devices
        .get(device_index as usize)
        .and_then(|device| device.queue_families.get(family as usize))
    else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *out = PluginQueueFamilyInfo {
            flags: queue.flags,
            count: queue.count,
            timestamp_valid_bits: queue.timestamp_valid_bits,
        }
    };
    SUCCESS
}

unsafe extern "C" fn memory_heap_info(
    instance: *mut c_void,
    device_index: u32,
    heap: u32,
    out: *mut PluginMemoryHeapInfo,
) -> i32 {
    if instance.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let owner = unsafe { &*instance.cast::<VulkanInstance>() };
    let Some(&(bytes, flags)) = owner
        .devices
        .get(device_index as usize)
        .and_then(|device| device.memory_heaps.get(heap as usize))
    else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *out = PluginMemoryHeapInfo {
            bytes,
            flags,
            reserved: 0,
        }
    };
    SUCCESS
}

unsafe extern "C" fn cooperative_matrix(
    instance: *mut c_void,
    device_index: u32,
    tuple: u32,
    out: *mut CooperativeMatrixTuple,
) -> i32 {
    if instance.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let owner = unsafe { &*instance.cast::<VulkanInstance>() };
    let Some(&matrix) = owner
        .devices
        .get(device_index as usize)
        .and_then(|device| device.cooperative_matrices.get(tuple as usize))
    else {
        return INVALID_ARGUMENT;
    };
    unsafe { *out = matrix };
    SUCCESS
}

unsafe extern "C" fn device_open(
    instance: *mut c_void,
    index: u32,
    request_count: u32,
    requests: *const PluginQueueRequest,
    out: *mut *mut c_void,
) -> i32 {
    if instance.is_null()
        || out.is_null()
        || requests.is_null()
        || request_count == 0
        || request_count > 32
    {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let owner = unsafe { &*instance.cast::<VulkanInstance>() };
    let Some(record) = owner.devices.get(index as usize) else {
        return INVALID_ARGUMENT;
    };
    if !record.strict_profile_supported {
        return UNSUPPORTED;
    }
    let requests = unsafe { std::slice::from_raw_parts(requests, request_count as usize) };
    let mut seen = BTreeSet::new();
    let mut converted = Vec::with_capacity(requests.len());
    for request in requests {
        let Some(family) = record.queue_families.get(request.family as usize) else {
            return INVALID_ARGUMENT;
        };
        if !seen.insert(request.family)
            || request.count == 0
            || request.count > family.count
            || !request.priority.is_finite()
            || !(0.0..=1.0).contains(&request.priority)
        {
            return INVALID_ARGUMENT;
        }
        converted.push(vulkan::QueueRequest {
            family: request.family,
            count: request.count,
            priority: request.priority,
        });
    }
    let Ok(device) = owner.instance.open_device(record, &converted) else {
        return DEVICE_LOST;
    };
    unsafe { *out = Box::into_raw(Box::new(device)).cast() };
    SUCCESS
}

unsafe extern "C" fn device_close(device: *mut c_void) {
    if !device.is_null() {
        unsafe { drop(Box::from_raw(device.cast::<vulkan::Device>())) };
    }
}

fn vulkan_allocation_status(error: vulkan::AllocError) -> i32 {
    match error {
        vulkan::AllocError::Invalid => INVALID_ARGUMENT,
        vulkan::AllocError::Unsupported => UNSUPPORTED,
        vulkan::AllocError::OutOfMemory => OUT_OF_MEMORY,
        vulkan::AllocError::Backend => DEVICE_LOST,
    }
}

fn allocation_info(
    device: &vulkan::Device,
    allocation: &vulkan::Allocation,
) -> PluginAllocationInfo {
    let (backing_bytes, heap_bytes, memory_type_index, heap_index, properties) =
        allocation.memory_info(device);
    PluginAllocationInfo {
        base: allocation.base,
        bytes: allocation.bytes,
        backing_bytes,
        heap_bytes,
        memory_type_index,
        heap_index,
        properties,
        exclusive: u32::from(allocation.exclusive()),
    }
}

unsafe extern "C" fn allocation_create(
    device: *mut c_void,
    request: *const PluginAllocRequest,
    out: *mut *mut c_void,
    info: *mut PluginAllocationInfo,
) -> i32 {
    if device.is_null() || request.is_null() || out.is_null() || info.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let request = unsafe { &*request };
    if request.bytes == 0
        || !request.alignment.is_power_of_two()
        || request.exclusive > 1
        || request.queue_family_count == 0
        || request.queue_family_count > 32
        || request.queue_families.is_null()
        || (request.required_properties | request.preferred_properties) & !7 != 0
    {
        return INVALID_ARGUMENT;
    }
    let families = unsafe {
        std::slice::from_raw_parts(request.queue_families, request.queue_family_count as usize)
    };
    if families
        .iter()
        .any(|family| !device.queues.iter().any(|queue| queue.family == *family))
    {
        return INVALID_ARGUMENT;
    }
    let required_heap = (request.required_heap != u32::MAX).then_some(request.required_heap);
    let preferred_heap = (request.preferred_heap != u32::MAX).then_some(request.preferred_heap);
    if required_heap.is_some_and(|heap| heap >= device.memory_heap_count())
        || preferred_heap.is_some_and(|heap| heap >= device.memory_heap_count())
    {
        return UNSUPPORTED;
    }
    let backend_request = vulkan::AllocRequest {
        bytes: request.bytes,
        alignment: request.alignment,
        required_properties: request.required_properties,
        preferred_properties: request.preferred_properties,
        queue_families: families,
        required_heap,
        preferred_heap,
        exclusive: request.exclusive != 0,
    };
    let allocation = match device.allocate(&backend_request) {
        Ok(allocation) => allocation,
        Err(error) => return vulkan_allocation_status(error),
    };
    unsafe {
        *info = allocation_info(device, &allocation);
        *out = Box::into_raw(Box::new(allocation)).cast();
    }
    SUCCESS
}

unsafe extern "C" fn allocation_destroy(device: *mut c_void, allocation: *mut c_void) {
    if device.is_null() || allocation.is_null() {
        return;
    }
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = unsafe { *Box::from_raw(allocation.cast::<vulkan::Allocation>()) };
    device.free_allocation(allocation);
}

unsafe extern "C" fn allocation_map(
    device: *mut c_void,
    allocation: *mut c_void,
    address: u64,
    out: *mut *mut c_void,
) -> i32 {
    if device.is_null() || allocation.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = unsafe { &mut *allocation.cast::<vulkan::Allocation>() };
    match device.map_allocation(allocation, address) {
        Ok(host) => {
            unsafe { *out = host };
            SUCCESS
        }
        Err(error) => vulkan_allocation_status(error),
    }
}

unsafe extern "C" fn allocation_unmap(device: *mut c_void, allocation: *mut c_void) {
    if device.is_null() || allocation.is_null() {
        return;
    }
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = unsafe { &mut *allocation.cast::<vulkan::Allocation>() };
    device.unmap_allocation(allocation);
}

unsafe extern "C" fn allocation_visibility(
    device: *mut c_void,
    allocation: *mut c_void,
    flush: u32,
) -> i32 {
    if device.is_null() || allocation.is_null() || flush > 1 {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = unsafe { &*allocation.cast::<vulkan::Allocation>() };
    match device.visibility(allocation, flush != 0) {
        Ok(()) => SUCCESS,
        Err(error) => vulkan_allocation_status(error),
    }
}

unsafe extern "C" fn allocation_register_host(
    device: *mut c_void,
    host: *mut c_void,
    bytes: u64,
    out: *mut *mut c_void,
    info: *mut PluginAllocationInfo,
) -> i32 {
    if device.is_null() || host.is_null() || bytes == 0 || out.is_null() || info.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = match device.register_host_pointer(host, bytes) {
        Ok(allocation) => allocation,
        Err(error) => return vulkan_allocation_status(error),
    };
    unsafe {
        *info = allocation_info(device, &allocation);
        *out = Box::into_raw(Box::new(allocation)).cast();
    }
    SUCCESS
}

unsafe extern "C" fn allocation_export_fd(
    device: *mut c_void,
    allocation: *mut c_void,
    out_fd: *mut i32,
    out_offset: *mut u64,
) -> i32 {
    if device.is_null() || allocation.is_null() || out_fd.is_null() || out_offset.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out_fd = -1;
        *out_offset = 0;
    };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = unsafe { &*allocation.cast::<vulkan::Allocation>() };
    match device.export_allocation_fd(allocation) {
        Ok(fd) => {
            unsafe { *out_fd = fd.into_raw_fd() };
            SUCCESS
        }
        Err(error) => vulkan_allocation_status(error),
    }
}

unsafe extern "C" fn allocation_import_fd(
    device: *mut c_void,
    source_allocation: *mut c_void,
    source_offset: u64,
    bytes: u64,
    fd: i32,
    out: *mut *mut c_void,
    info: *mut PluginAllocationInfo,
) -> i32 {
    if fd < 0 {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The plugin call takes ownership of this descriptor on every path.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if device.is_null() || out.is_null() || info.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let allocation = match if source_allocation.is_null() {
        device.import_external_dmabuf_fd(source_offset, bytes, fd)
    } else {
        let source = unsafe { &*source_allocation.cast::<vulkan::Allocation>() };
        device.import_peer_fd(source, source_offset, bytes, fd)
    } {
        Ok(allocation) => allocation,
        Err(error) => return vulkan_allocation_status(error),
    };
    unsafe {
        *info = allocation_info(device, &allocation);
        *out = Box::into_raw(Box::new(allocation)).cast();
    }
    SUCCESS
}

fn timeline_status(error: i32, backend_code: *mut i32) -> i32 {
    if !backend_code.is_null() {
        unsafe { *backend_code = error };
    }
    match error {
        -1 | -2 => OUT_OF_MEMORY,
        -8 => UNSUPPORTED,
        _ => DEVICE_LOST,
    }
}

unsafe extern "C" fn device_wait_idle(device: *mut c_void, backend_code: *mut i32) -> i32 {
    if device.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    match device.wait_idle() {
        Ok(()) => SUCCESS,
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn timeline_create(
    device: *mut c_void,
    exportable: u32,
    out: *mut *mut c_void,
    backend_code: *mut i32,
) -> i32 {
    if device.is_null() || exportable > 1 || out.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    match device.create_timeline(exportable != 0) {
        Ok(timeline) => {
            unsafe { *out = Box::into_raw(Box::new(timeline)).cast() };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn timeline_import_fd(
    device: *mut c_void,
    fd: i32,
    out: *mut *mut c_void,
    backend_code: *mut i32,
) -> i32 {
    if fd < 0 {
        return INVALID_ARGUMENT;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if device.is_null() || out.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    match device.import_timeline_fd(fd) {
        Ok(timeline) => {
            unsafe { *out = Box::into_raw(Box::new(timeline)).cast() };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn timeline_destroy(_device: *mut c_void, timeline: *mut c_void) {
    if !timeline.is_null() {
        unsafe { drop(Box::from_raw(timeline.cast::<vulkan::Timeline>())) };
    }
}

unsafe extern "C" fn timeline_query(
    timeline: *mut c_void,
    out: *mut u64,
    backend_code: *mut i32,
) -> i32 {
    if timeline.is_null() || out.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    let timeline = unsafe { &*timeline.cast::<vulkan::Timeline>() };
    match timeline.query() {
        Ok(value) => {
            unsafe { *out = value };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn timeline_wait(
    timeline: *mut c_void,
    value: u64,
    timeout_ns: u64,
    completed: *mut u32,
    backend_code: *mut i32,
) -> i32 {
    if timeline.is_null() || completed.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    let timeline = unsafe { &*timeline.cast::<vulkan::Timeline>() };
    match timeline.wait(value, timeout_ns) {
        Ok(done) => {
            unsafe { *completed = u32::from(done) };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn timeline_signal(
    timeline: *mut c_void,
    value: u64,
    backend_code: *mut i32,
) -> i32 {
    if timeline.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    let timeline = unsafe { &*timeline.cast::<vulkan::Timeline>() };
    match timeline.signal(value) {
        Ok(()) => SUCCESS,
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn timeline_export_fd(
    timeline: *mut c_void,
    out_fd: *mut i32,
    backend_code: *mut i32,
) -> i32 {
    if timeline.is_null() || out_fd.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out_fd = -1 };
    let timeline = unsafe { &*timeline.cast::<vulkan::Timeline>() };
    match timeline.export_fd() {
        Ok(fd) => {
            unsafe { *out_fd = fd.into_raw_fd() };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn pipeline_import(
    device: *mut c_void,
    image: *const u8,
    image_bytes: u64,
    out: *mut *mut c_void,
    backend_code: *mut i32,
) -> i32 {
    if device.is_null()
        || image.is_null()
        || image_bytes == 0
        || image_bytes > 128 * 1024 * 1024
        || out.is_null()
        || backend_code.is_null()
    {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let image = unsafe { std::slice::from_raw_parts(image, image_bytes as usize) };
    let package = match package::Package::parse(image) {
        Ok(package) => package,
        Err(package::PackageError::UnsupportedFeature) => return UNSUPPORTED,
        Err(_) => return INCOMPATIBLE_BINARY,
    };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    match device.import_pipeline(&package) {
        Ok(pipeline) => {
            unsafe { *out = Box::into_raw(Box::new(pipeline)).cast() };
            SUCCESS
        }
        Err(vulkan::PipelineError::Unsupported) => UNSUPPORTED,
        Err(vulkan::PipelineError::Backend(error)) => {
            unsafe { *backend_code = error };
            if error == -4 {
                DEVICE_LOST
            } else {
                INCOMPATIBLE_BINARY
            }
        }
        Err(_) => INCOMPATIBLE_BINARY,
    }
}

unsafe extern "C" fn pipeline_destroy(_device: *mut c_void, pipeline: *mut c_void) {
    if !pipeline.is_null() {
        unsafe { drop(Box::from_raw(pipeline.cast::<vulkan::Pipeline>())) };
    }
}

unsafe extern "C" fn query_create(
    device: *mut c_void,
    out: *mut *mut c_void,
    backend_code: *mut i32,
) -> i32 {
    if device.is_null() || out.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    match device.create_timestamp_query() {
        Ok(query) => {
            unsafe { *out = Box::into_raw(Box::new(query)).cast() };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn query_destroy(_device: *mut c_void, query: *mut c_void) {
    if !query.is_null() {
        unsafe { drop(Box::from_raw(query.cast::<vulkan::QueryPool>())) };
    }
}

unsafe extern "C" fn command_create(
    device: *mut c_void,
    family: u32,
    out: *mut *mut c_void,
    backend_code: *mut i32,
) -> i32 {
    if device.is_null() || out.is_null() || backend_code.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    match device.create_command(family) {
        Ok(command) => {
            unsafe { *out = Box::into_raw(Box::new(command)).cast() };
            SUCCESS
        }
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn command_destroy(_device: *mut c_void, command: *mut c_void) {
    if !command.is_null() {
        unsafe { drop(Box::from_raw(command.cast::<vulkan::Command>())) };
    }
}

fn decode_command_op(op: &PluginCommandOp) -> Option<vulkan::CommandOp> {
    if op.reserved != 0 {
        return None;
    }
    let a = &op.args;
    let u32_arg = |index| u32::try_from(a[index]).ok();
    Some(match op.kind {
        1 => vulkan::CommandOp::Copy {
            dst: a[0],
            src: a[1],
            bytes: a[2],
        },
        2 => vulkan::CommandOp::Fill {
            dst: a[0],
            bytes: a[1],
            pattern: u32_arg(2)?,
        },
        3 => vulkan::CommandOp::BarrierAll,
        4 => vulkan::CommandOp::BarrierRange {
            address: a[0],
            bytes: a[1],
        },
        5 => vulkan::CommandOp::BarrierRangeScoped {
            address: a[0],
            bytes: a[1],
            src_stage: vulkan::scoped_stage(u32_arg(2)?)?,
            src_access: vulkan::scoped_access(u32_arg(3)?)?,
            dst_stage: vulkan::scoped_stage(u32_arg(4)?)?,
            dst_access: vulkan::scoped_access(u32_arg(5)?)?,
            src_family: u32_arg(6)?,
            dst_family: u32_arg(7)?,
        },
        6 | 7 => {
            if a[0] == 0 {
                return None;
            }
            let pipeline = unsafe { &*(usize::try_from(a[0]).ok()? as *const vulkan::Pipeline) };
            if op.kind == 6 {
                vulkan::CommandOp::Dispatch {
                    pipeline: pipeline.handle,
                    layout: pipeline.layout,
                    grid: [u32_arg(1)?, u32_arg(2)?, u32_arg(3)?],
                    arguments: a[4],
                }
            } else {
                vulkan::CommandOp::DispatchIndirect {
                    pipeline: pipeline.handle,
                    layout: pipeline.layout,
                    grid_address: a[1],
                    arguments: a[2],
                }
            }
        }
        8 => {
            if a[0] == 0 {
                return None;
            }
            let pool = unsafe { &*(usize::try_from(a[0]).ok()? as *const vulkan::QueryPool) };
            vulkan::CommandOp::Timestamp {
                pool: pool.handle(),
                destination: a[1],
            }
        }
        _ => return None,
    })
}

unsafe extern "C" fn command_record(
    command: *mut c_void,
    operation_count: u32,
    operations: *const PluginCommandOp,
    backend_code: *mut i32,
) -> i32 {
    if command.is_null()
        || backend_code.is_null()
        || operation_count > 1_000_000
        || (operation_count != 0 && operations.is_null())
    {
        return INVALID_ARGUMENT;
    }
    let operations = if operation_count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(operations, operation_count as usize) }
    };
    let Some(decoded) = operations
        .iter()
        .map(decode_command_op)
        .collect::<Option<Vec<_>>>()
    else {
        return INVALID_ARGUMENT;
    };
    let command = unsafe { &*command.cast::<vulkan::Command>() };
    match command.record(&decoded) {
        Ok(()) => SUCCESS,
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn queue_submit(
    device: *mut c_void,
    queue_index: u32,
    segment_count: u32,
    segments: *const PluginSubmitSegment,
    backend_code: *mut i32,
) -> i32 {
    if device.is_null()
        || backend_code.is_null()
        || segments.is_null()
        || segment_count == 0
        || segment_count > 65_536
    {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    if device.queues.get(queue_index as usize).is_none() {
        return INVALID_ARGUMENT;
    }
    let input = unsafe { std::slice::from_raw_parts(segments, segment_count as usize) };
    let mut waits = Vec::with_capacity(input.len());
    let mut signals = Vec::with_capacity(input.len());
    for segment in input {
        if segment.wait_count > 1_000_000
            || segment.signal_count > 1_000_000
            || (segment.wait_count != 0 && segment.waits.is_null())
            || (segment.signal_count != 0 && segment.signals.is_null())
        {
            return INVALID_ARGUMENT;
        }
        let wait_points = if segment.wait_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(segment.waits, segment.wait_count as usize) }
        };
        let signal_points = if segment.signal_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(segment.signals, segment.signal_count as usize) }
        };
        if wait_points
            .iter()
            .chain(signal_points)
            .any(|point| point.timeline.is_null())
        {
            return INVALID_ARGUMENT;
        }
        waits.push(
            wait_points
                .iter()
                .map(|point| {
                    (
                        unsafe { &*point.timeline.cast::<vulkan::Timeline>() },
                        point.value,
                    )
                })
                .collect::<Vec<_>>(),
        );
        signals.push(
            signal_points
                .iter()
                .map(|point| {
                    (
                        unsafe { &*point.timeline.cast::<vulkan::Timeline>() },
                        point.value,
                    )
                })
                .collect::<Vec<_>>(),
        );
    }
    let decoded: Vec<_> = input
        .iter()
        .zip(waits)
        .zip(signals)
        .map(|((segment, waits), signals)| vulkan::SubmitSegment {
            command: (!segment.command.is_null())
                .then(|| unsafe { &*segment.command.cast::<vulkan::Command>() }),
            waits,
            signals,
        })
        .collect();
    match device.submit_segments(queue_index as usize, &decoded) {
        Ok(()) => SUCCESS,
        Err(error) => timeline_status(error, backend_code),
    }
}

unsafe extern "C" fn instance_native_handles(
    instance: *mut c_void,
    index: u32,
    native_instance: *mut *mut c_void,
    native_physical_device: *mut *mut c_void,
) -> i32 {
    if instance.is_null() || native_instance.is_null() || native_physical_device.is_null() {
        return INVALID_ARGUMENT;
    }
    let instance = unsafe { &*instance.cast::<VulkanInstance>() };
    let Some(record) = instance.devices.get(index as usize) else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *native_instance = instance.instance.native_handle();
        *native_physical_device = record.physical;
    }
    SUCCESS
}

unsafe extern "C" fn device_native_handles(
    device: *mut c_void,
    queue_index: u32,
    native_device: *mut *mut c_void,
    native_queue: *mut *mut c_void,
) -> i32 {
    if device.is_null() || native_device.is_null() || native_queue.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<vulkan::Device>() };
    let Some(queue) = device.queues.get(queue_index as usize) else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *native_device = device.native_handle();
        *native_queue = queue._handle;
    }
    SUCCESS
}

unsafe extern "C" fn core_report_error(
    _context: *mut c_void,
    status: i32,
    backend_code: i32,
    _message: *const c_char,
    _message_bytes: u32,
) {
    set_backend_diagnostic(status, backend_code, "Execution provider failed");
}

/// Core-owned handle to the linked provider's inventory session.
pub struct Instance {
    api: PluginApi,
    handle: *mut c_void,
    _services: Box<PluginServices>,
    _library: Option<DynamicLibrary>,
}

fn backend_disabled(name: &str) -> bool {
    std::env::var("NISYROS_DISABLE_BACKENDS").is_ok_and(|value| {
        value
            .split(|character: char| {
                character == ',' || character == ';' || character.is_ascii_whitespace()
            })
            .any(|backend| backend.eq_ignore_ascii_case(name))
    })
}

impl Instance {
    pub fn open() -> Result<Self, ()> {
        if let Some(path) = std::env::var_os("NISYROS_PLUGIN_LIBRARY") {
            let (library, getter) = DynamicLibrary::open(&path).ok_or(())?;
            return Self::open_with_getter(Some(library), getter);
        }
        let gpu = if !backend_disabled("rocm")
            && let Ok(instance) = Self::open_with_getter(None, rocm::rocm_plugin_get_api)
            && instance
                .devices()
                .is_ok_and(|devices| devices.iter().any(|device| device.strict_profile_supported))
        {
            Some(instance)
        } else if !backend_disabled("vulkan") {
            Self::open_with_getter(None, nisyros_plugin_get_api).ok()
        } else {
            None
        };
        gpu.ok_or(())
    }

    fn open_with_getter(library: Option<DynamicLibrary>, getter: PluginGetApi) -> Result<Self, ()> {
        let services = Box::new(PluginServices {
            struct_size: std::mem::size_of::<PluginServices>() as u32,
            abi_version: SERVICES_ABI_VERSION,
            context: ptr::null_mut(),
            report_error: Some(core_report_error),
        });
        let mut api: PluginApi = unsafe { std::mem::zeroed() };
        let status = unsafe {
            getter(
                PLUGIN_ABI_VERSION,
                &*services,
                std::mem::size_of::<PluginApi>() as u32,
                (&raw mut api).cast(),
            )
        };
        if status != SUCCESS
            || api.struct_size as usize != std::mem::size_of::<PluginApi>()
            || api.abi_version != PLUGIN_ABI_VERSION
            || api.instance_open.is_none()
            || api.instance_close.is_none()
            || api.device_count.is_none()
            || api.device_info.is_none()
            || api.queue_family_info.is_none()
            || api.memory_heap_info.is_none()
            || api.cooperative_matrix.is_none()
            || api.device_open.is_none()
            || api.device_close.is_none()
            || api.allocation_create.is_none()
            || api.allocation_destroy.is_none()
            || api.allocation_map.is_none()
            || api.allocation_unmap.is_none()
            || api.allocation_visibility.is_none()
            || api.allocation_register_host.is_none()
            || api.allocation_export_fd.is_none()
            || api.allocation_import_fd.is_none()
            || api.device_wait_idle.is_none()
            || api.timeline_create.is_none()
            || api.timeline_import_fd.is_none()
            || api.timeline_destroy.is_none()
            || api.timeline_query.is_none()
            || api.timeline_wait.is_none()
            || api.timeline_signal.is_none()
            || api.timeline_export_fd.is_none()
            || api.pipeline_import.is_none()
            || api.pipeline_destroy.is_none()
            || api.query_create.is_none()
            || api.query_destroy.is_none()
            || api.command_create.is_none()
            || api.command_destroy.is_none()
            || api.command_record.is_none()
            || api.queue_submit.is_none()
            || api.instance_native_handles.is_none()
            || api.device_native_handles.is_none()
        {
            return Err(());
        }
        let mut handle = ptr::null_mut();
        let status = unsafe {
            api.instance_open.expect("checked provider callback")(&*services, &mut handle)
        };
        if status != SUCCESS || handle.is_null() {
            if !handle.is_null() {
                unsafe { api.instance_close.expect("checked provider callback")(handle) };
            }
            return Err(());
        }
        Ok(Self {
            api,
            handle,
            _services: services,
            _library: library,
        })
    }

    fn local_device_count(&self) -> Result<u32, ()> {
        let mut count = 0;
        if unsafe {
            self.api.device_count.expect("checked provider callback")(self.handle, &mut count)
        } != SUCCESS
            || count > 32
        {
            return Err(());
        }
        Ok(count)
    }

    pub fn devices(&self) -> Result<Vec<DeviceRecord>, ()> {
        let count = self.local_device_count()?;
        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut info: PluginDeviceInfo = unsafe { std::mem::zeroed() };
            info.struct_size = std::mem::size_of::<PluginDeviceInfo>() as u32;
            info.abi_version = PLUGIN_ABI_VERSION;
            if unsafe {
                self.api.device_info.expect("checked provider callback")(
                    self.handle,
                    index,
                    &mut info,
                )
            } != SUCCESS
                || info.backend == 0
                || info.strict_profile_supported > 1
                || info.peer_memory_fd > 1
                || info.peer_timeline_fd > 1
                || info.queue_family_count > 32
                || info.memory_heap_count > 16
                || info.cooperative_matrix_count > 256
                || info.max_grid.contains(&0)
                || !info.name.contains(&0)
                || !info.driver_name.contains(&0)
            {
                return Err(());
            }
            let mut queue_families = Vec::with_capacity(info.queue_family_count as usize);
            for family in 0..info.queue_family_count {
                let mut queue: PluginQueueFamilyInfo = unsafe { std::mem::zeroed() };
                if unsafe {
                    self.api
                        .queue_family_info
                        .expect("checked provider callback")(
                        self.handle, index, family, &mut queue
                    )
                } != SUCCESS
                    || queue.count == 0
                    || (queue.timestamp_valid_bits != 0
                        && !(36..=64).contains(&queue.timestamp_valid_bits))
                {
                    return Err(());
                }
                queue_families.push(QueueFamilyProperties {
                    flags: queue.flags,
                    count: queue.count,
                    timestamp_valid_bits: queue.timestamp_valid_bits,
                });
            }
            let mut memory_heaps = Vec::with_capacity(info.memory_heap_count as usize);
            for heap in 0..info.memory_heap_count {
                let mut value: PluginMemoryHeapInfo = unsafe { std::mem::zeroed() };
                if unsafe {
                    self.api
                        .memory_heap_info
                        .expect("checked provider callback")(
                        self.handle, index, heap, &mut value
                    )
                } != SUCCESS
                    || value.bytes == 0
                    || value.reserved != 0
                {
                    return Err(());
                }
                memory_heaps.push((value.bytes, value.flags));
            }
            let mut cooperative_matrices =
                Vec::with_capacity(info.cooperative_matrix_count as usize);
            for tuple in 0..info.cooperative_matrix_count {
                let mut value: CooperativeMatrixTuple = unsafe { std::mem::zeroed() };
                if unsafe {
                    self.api
                        .cooperative_matrix
                        .expect("checked provider callback")(
                        self.handle, index, tuple, &mut value
                    )
                } != SUCCESS
                    || value.m == 0
                    || value.n == 0
                    || value.k == 0
                    || value.saturating_accumulation > 1
                {
                    return Err(());
                }
                cooperative_matrices.push(value);
            }
            devices.push(DeviceRecord {
                index,
                backend: info.backend,
                api_version: info.api_version,
                vendor_id: info.vendor_id,
                device_id: info.device_id,
                device_type: info.device_type,
                driver_id: info.driver_id,
                strict_profile_supported: info.strict_profile_supported != 0,
                name: info.name,
                device_uuid: info.device_uuid,
                driver_uuid: info.driver_uuid,
                driver_name: info.driver_name,
                queue_families,
                memory_heaps,
                features: info.features,
                cooperative_matrix_revision: info.cooperative_matrix_revision,
                shader_bfloat16_revision: info.shader_bfloat16_revision,
                shader_enqueue_revision: info.shader_enqueue_revision,
                cooperative_matrix_stages: info.cooperative_matrix_stages,
                cooperative_matrices,
                timestamp_period_ns: info.timestamp_period_ns,
                external_host_alignment: (info.external_host_alignment != 0)
                    .then_some(info.external_host_alignment),
                max_grid: info.max_grid,
                peer_memory_fd: info.peer_memory_fd != 0,
                peer_timeline_fd: info.peer_timeline_fd != 0,
            });
        }
        Ok(devices)
    }

    pub fn native_handles(&self, index: usize) -> Option<(*mut c_void, *mut c_void)> {
        let mut instance = ptr::null_mut();
        let mut physical = ptr::null_mut();
        let status = unsafe {
            self.api
                .instance_native_handles
                .expect("checked provider callback")(
                self.handle,
                u32::try_from(index).ok()?,
                &mut instance,
                &mut physical,
            )
        };
        (status == SUCCESS && !instance.is_null() && !physical.is_null())
            .then_some((instance, physical))
    }

    pub fn open_device(
        &self,
        record: &DeviceRecord,
        requests: &[QueueRequest],
    ) -> Result<Device, ()> {
        let converted: Vec<_> = requests
            .iter()
            .map(|request| PluginQueueRequest {
                family: request.family,
                count: request.count,
                priority: request.priority,
            })
            .collect();
        let mut handle = ptr::null_mut();
        let status = unsafe {
            self.api.device_open.expect("checked provider callback")(
                self.handle,
                record.index,
                converted.len() as u32,
                converted.as_ptr(),
                &mut handle,
            )
        };
        if status != SUCCESS || handle.is_null() {
            return Err(());
        }
        let queues = requests
            .iter()
            .flat_map(|request| {
                (0..request.count).map(|index| QueueMetadata {
                    family: request.family,
                    index,
                })
            })
            .collect();
        Ok(Device {
            api: self.api,
            handle,
            queues,
            max_grid: record.max_grid,
            external_host_alignment: record.external_host_alignment,
            peer_memory_fd: record.peer_memory_fd,
            peer_timeline_fd: record.peer_timeline_fd,
            memory_heap_count: record.memory_heaps.len() as u32,
            timestamp_valid_bits: record
                .queue_families
                .iter()
                .map(|family| family.timestamp_valid_bits)
                .collect(),
        })
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe { self.api.instance_close.expect("checked provider callback")(self.handle) };
    }
}

/// Opaque provider device with negotiated inventory properties.
pub struct Device {
    api: PluginApi,
    handle: *mut c_void,
    pub queues: Vec<QueueMetadata>,
    pub max_grid: [u32; 3],
    pub external_host_alignment: Option<u64>,
    pub peer_memory_fd: bool,
    pub peer_timeline_fd: bool,
    memory_heap_count: u32,
    pub timestamp_valid_bits: Vec<u32>,
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { self.api.device_close.expect("checked provider callback")(self.handle) };
    }
}

pub struct Allocation {
    api: PluginApi,
    device: *mut c_void,
    handle: *mut c_void,
    pub base: u64,
    pub bytes: u64,
    backing_bytes: u64,
    heap_bytes: u64,
    memory_type_index: u32,
    heap_index: u32,
    properties: u32,
    exclusive: bool,
    mapped: bool,
    mapped_host: *mut c_void,
}

impl Allocation {
    fn new(
        api: PluginApi,
        device: *mut c_void,
        handle: *mut c_void,
        info: PluginAllocationInfo,
        mapped_host: *mut c_void,
    ) -> Self {
        Self {
            api,
            device,
            handle,
            base: info.base,
            bytes: info.bytes,
            backing_bytes: info.backing_bytes,
            heap_bytes: info.heap_bytes,
            memory_type_index: info.memory_type_index,
            heap_index: info.heap_index,
            properties: info.properties,
            exclusive: info.exclusive != 0,
            mapped: false,
            mapped_host,
        }
    }

    pub fn contains(&self, address: u64, bytes: u64) -> bool {
        address >= self.base
            && address
                .checked_sub(self.base)
                .and_then(|offset| offset.checked_add(bytes))
                .is_some_and(|end| end <= self.bytes)
    }

    pub fn is_mapped(&self) -> bool {
        self.mapped
    }

    pub fn mapped_host(&self) -> *mut c_void {
        self.mapped_host
    }

    pub fn exclusive(&self) -> bool {
        self.exclusive
    }

    pub fn memory_info(&self, _device: &Device) -> (u64, u64, u32, u32, u32) {
        (
            self.backing_bytes,
            self.heap_bytes,
            self.memory_type_index,
            self.heap_index,
            self.properties,
        )
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            self.api
                .allocation_destroy
                .expect("checked provider callback")(self.device, self.handle)
        };
    }
}

fn allocation_error(status: i32) -> AllocError {
    match status {
        INVALID_ARGUMENT => AllocError::Invalid,
        UNSUPPORTED => AllocError::Unsupported,
        OUT_OF_MEMORY => AllocError::OutOfMemory,
        _ => AllocError::Backend,
    }
}

impl Device {
    pub fn memory_heap_count(&self) -> u32 {
        self.memory_heap_count
    }

    pub fn native_handles(&self, queue_index: usize) -> Option<(*mut c_void, *mut c_void)> {
        let mut device = ptr::null_mut();
        let mut queue = ptr::null_mut();
        let status = unsafe {
            self.api
                .device_native_handles
                .expect("checked provider callback")(
                self.handle,
                u32::try_from(queue_index).ok()?,
                &mut device,
                &mut queue,
            )
        };
        (status == SUCCESS && !device.is_null() && !queue.is_null()).then_some((device, queue))
    }

    pub fn submit_segments(
        &self,
        queue_index: usize,
        segments: &[SubmitSegment<'_>],
    ) -> Result<(), BackendError> {
        let queue_index =
            u32::try_from(queue_index).map_err(|_| provider_error(INVALID_ARGUMENT, 0))?;
        let segment_count =
            u32::try_from(segments.len()).map_err(|_| provider_error(INVALID_ARGUMENT, 0))?;
        let waits: Vec<Vec<_>> = segments
            .iter()
            .map(|segment| {
                segment
                    .waits
                    .iter()
                    .map(|(timeline, value)| PluginSubmitPoint {
                        timeline: timeline.handle,
                        value: *value,
                    })
                    .collect()
            })
            .collect();
        let signals: Vec<Vec<_>> = segments
            .iter()
            .map(|segment| {
                segment
                    .signals
                    .iter()
                    .map(|(timeline, value)| PluginSubmitPoint {
                        timeline: timeline.handle,
                        value: *value,
                    })
                    .collect()
            })
            .collect();
        let input: Vec<_> = segments
            .iter()
            .enumerate()
            .map(|(index, segment)| PluginSubmitSegment {
                command: segment
                    .command
                    .map_or(ptr::null_mut(), |command| command.handle),
                wait_count: waits[index].len() as u32,
                waits: waits[index].as_ptr(),
                signal_count: signals[index].len() as u32,
                signals: signals[index].as_ptr(),
            })
            .collect();
        let mut backend_code = 0;
        let status = unsafe {
            self.api.queue_submit.expect("checked provider callback")(
                self.handle,
                queue_index,
                segment_count,
                input.as_ptr(),
                &mut backend_code,
            )
        };
        if status == SUCCESS {
            Ok(())
        } else {
            Err(provider_error(status, backend_code))
        }
    }

    pub fn create_timestamp_query(&self) -> Result<QueryPool, BackendError> {
        let mut handle = ptr::null_mut();
        let mut backend_code = 0;
        let status = unsafe {
            self.api.query_create.expect("checked provider callback")(
                self.handle,
                &mut handle,
                &mut backend_code,
            )
        };
        if status != SUCCESS || handle.is_null() {
            if !handle.is_null() {
                unsafe {
                    self.api.query_destroy.expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(provider_error(status, backend_code));
        }
        Ok(QueryPool {
            api: self.api,
            device: self.handle,
            handle,
        })
    }

    pub fn create_command(&self, family: u32) -> Result<Command, BackendError> {
        let mut handle = ptr::null_mut();
        let mut backend_code = 0;
        let status = unsafe {
            self.api.command_create.expect("checked provider callback")(
                self.handle,
                family,
                &mut handle,
                &mut backend_code,
            )
        };
        if status != SUCCESS || handle.is_null() {
            if !handle.is_null() {
                unsafe {
                    self.api.command_destroy.expect("checked provider callback")(
                        self.handle,
                        handle,
                    )
                };
            }
            return Err(provider_error(status, backend_code));
        }
        Ok(Command {
            api: self.api,
            device: self.handle,
            handle,
        })
    }

    pub fn import_pipeline(&self, image: &[u8]) -> Result<Pipeline, PipelineError> {
        let mut handle = ptr::null_mut();
        let mut backend_code = 0;
        let status = unsafe {
            self.api.pipeline_import.expect("checked provider callback")(
                self.handle,
                image.as_ptr(),
                image.len() as u64,
                &mut handle,
                &mut backend_code,
            )
        };
        if status != SUCCESS || handle.is_null() {
            if !handle.is_null() {
                unsafe {
                    self.api
                        .pipeline_destroy
                        .expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(if backend_code != 0 || status == DEVICE_LOST {
                PipelineError::Backend(provider_error(status, backend_code))
            } else if status == UNSUPPORTED {
                PipelineError::Unsupported
            } else {
                PipelineError::Incompatible
            });
        }
        Ok(Pipeline {
            api: self.api,
            device: self.handle,
            handle,
        })
    }

    pub fn wait_idle(&self) -> Result<(), BackendError> {
        let mut backend_code = 0;
        let status = unsafe {
            self.api
                .device_wait_idle
                .expect("checked provider callback")(self.handle, &mut backend_code)
        };
        if status == SUCCESS {
            Ok(())
        } else {
            Err(provider_error(status, backend_code))
        }
    }

    pub fn create_timeline(&self, exportable: bool) -> Result<Timeline, BackendError> {
        let mut handle = ptr::null_mut();
        let mut backend_code = 0;
        let status = unsafe {
            self.api.timeline_create.expect("checked provider callback")(
                self.handle,
                u32::from(exportable),
                &mut handle,
                &mut backend_code,
            )
        };
        if status != SUCCESS || handle.is_null() {
            if !handle.is_null() {
                unsafe {
                    self.api
                        .timeline_destroy
                        .expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(provider_error(status, backend_code));
        }
        Ok(Timeline {
            api: self.api,
            device: self.handle,
            handle,
        })
    }

    pub fn import_timeline_fd(&self, fd: OwnedFd) -> Result<Timeline, BackendError> {
        let mut handle = ptr::null_mut();
        let mut backend_code = 0;
        let status = unsafe {
            self.api
                .timeline_import_fd
                .expect("checked provider callback")(
                self.handle,
                fd.into_raw_fd(),
                &mut handle,
                &mut backend_code,
            )
        };
        if status != SUCCESS || handle.is_null() {
            if !handle.is_null() {
                unsafe {
                    self.api
                        .timeline_destroy
                        .expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(provider_error(status, backend_code));
        }
        Ok(Timeline {
            api: self.api,
            device: self.handle,
            handle,
        })
    }

    pub fn allocate(&self, request: &AllocRequest<'_>) -> Result<Allocation, AllocError> {
        let input = PluginAllocRequest {
            bytes: request.bytes,
            alignment: request.alignment,
            required_properties: request.required_properties,
            preferred_properties: request.preferred_properties,
            required_heap: request.required_heap.unwrap_or(u32::MAX),
            preferred_heap: request.preferred_heap.unwrap_or(u32::MAX),
            exclusive: u32::from(request.exclusive),
            queue_family_count: request.queue_families.len() as u32,
            queue_families: request.queue_families.as_ptr(),
        };
        let mut handle = ptr::null_mut();
        let mut info: PluginAllocationInfo = unsafe { std::mem::zeroed() };
        let status = unsafe {
            self.api
                .allocation_create
                .expect("checked provider callback")(
                self.handle, &input, &mut handle, &mut info
            )
        };
        if status != SUCCESS {
            return Err(allocation_error(status));
        }
        if handle.is_null() || info.base == 0 || info.bytes != request.bytes {
            if !handle.is_null() {
                unsafe {
                    self.api
                        .allocation_destroy
                        .expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(AllocError::Backend);
        }
        Ok(Allocation::new(
            self.api,
            self.handle,
            handle,
            info,
            ptr::null_mut(),
        ))
    }

    pub fn register_host_pointer(
        &self,
        host: *mut c_void,
        bytes: u64,
    ) -> Result<Allocation, AllocError> {
        let mut handle = ptr::null_mut();
        let mut info: PluginAllocationInfo = unsafe { std::mem::zeroed() };
        let status = unsafe {
            self.api
                .allocation_register_host
                .expect("checked provider callback")(
                self.handle,
                host,
                bytes,
                &mut handle,
                &mut info,
            )
        };
        if status != SUCCESS {
            return Err(allocation_error(status));
        }
        if handle.is_null() || info.base == 0 || info.bytes != bytes {
            if !handle.is_null() {
                unsafe {
                    self.api
                        .allocation_destroy
                        .expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(AllocError::Backend);
        }
        Ok(Allocation::new(self.api, self.handle, handle, info, host))
    }

    pub fn export_allocation_fd(
        &self,
        allocation: &Allocation,
    ) -> Result<(OwnedFd, u64), AllocError> {
        let mut fd = -1;
        let mut offset = 0;
        let status = unsafe {
            self.api
                .allocation_export_fd
                .expect("checked provider callback")(
                self.handle,
                allocation.handle,
                &mut fd,
                &mut offset,
            )
        };
        if status != SUCCESS {
            return Err(allocation_error(status));
        }
        if fd < 0 {
            return Err(AllocError::Backend);
        }
        Ok((unsafe { OwnedFd::from_raw_fd(fd) }, offset))
    }

    pub fn import_peer_fd(
        &self,
        source: &Allocation,
        backing_offset: u64,
        bytes: u64,
        fd: OwnedFd,
        same_provider: bool,
    ) -> Result<Allocation, AllocError> {
        let mut handle = ptr::null_mut();
        let mut info: PluginAllocationInfo = unsafe { std::mem::zeroed() };
        let status = unsafe {
            self.api
                .allocation_import_fd
                .expect("checked provider callback")(
                self.handle,
                if same_provider {
                    source.handle
                } else {
                    ptr::null_mut()
                },
                backing_offset,
                bytes,
                fd.into_raw_fd(),
                &mut handle,
                &mut info,
            )
        };
        if status != SUCCESS {
            return Err(allocation_error(status));
        }
        if handle.is_null() || info.base == 0 || info.bytes != bytes {
            if !handle.is_null() {
                unsafe {
                    self.api
                        .allocation_destroy
                        .expect("checked provider callback")(self.handle, handle)
                };
            }
            return Err(AllocError::Backend);
        }
        Ok(Allocation::new(
            self.api,
            self.handle,
            handle,
            info,
            ptr::null_mut(),
        ))
    }

    pub fn map_allocation(
        &self,
        allocation: &mut Allocation,
        address: u64,
    ) -> Result<*mut c_void, AllocError> {
        let mut host = ptr::null_mut();
        let status = unsafe {
            self.api.allocation_map.expect("checked provider callback")(
                self.handle,
                allocation.handle,
                address,
                &mut host,
            )
        };
        if status != SUCCESS {
            return Err(allocation_error(status));
        }
        if host.is_null() {
            return Err(AllocError::Backend);
        }
        allocation.mapped = true;
        allocation.mapped_host = host;
        Ok(host)
    }

    pub fn unmap_allocation(&self, allocation: &mut Allocation) {
        unsafe {
            self.api
                .allocation_unmap
                .expect("checked provider callback")(self.handle, allocation.handle)
        };
        allocation.mapped = false;
        allocation.mapped_host = ptr::null_mut();
    }

    pub fn visibility(&self, allocation: &Allocation, flush: bool) -> Result<(), AllocError> {
        let status = unsafe {
            self.api
                .allocation_visibility
                .expect("checked provider callback")(
                self.handle,
                allocation.handle,
                u32::from(flush),
            )
        };
        if status == SUCCESS {
            Ok(())
        } else {
            Err(allocation_error(status))
        }
    }

    pub fn free_allocation(&self, allocation: Allocation) {
        debug_assert_eq!(allocation.device, self.handle);
        drop(allocation);
    }
}

pub struct Pipeline {
    api: PluginApi,
    device: *mut c_void,
    handle: *mut c_void,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        unsafe {
            self.api
                .pipeline_destroy
                .expect("checked provider callback")(self.device, self.handle)
        };
    }
}

impl Pipeline {
    pub fn opaque_handle(&self) -> *mut c_void {
        self.handle
    }
}

pub struct QueryPool {
    api: PluginApi,
    device: *mut c_void,
    handle: *mut c_void,
}

impl QueryPool {
    pub fn opaque_handle(&self) -> *mut c_void {
        self.handle
    }
}

impl Drop for QueryPool {
    fn drop(&mut self) {
        unsafe {
            self.api.query_destroy.expect("checked provider callback")(self.device, self.handle)
        };
    }
}

pub struct Command {
    api: PluginApi,
    device: *mut c_void,
    handle: *mut c_void,
}

impl Command {
    pub fn record(&self, operations: &[CommandOp]) -> Result<(), BackendError> {
        let encoded: Vec<_> = operations.iter().copied().map(CommandOp::encode).collect();
        let mut backend_code = 0;
        let status = unsafe {
            self.api.command_record.expect("checked provider callback")(
                self.handle,
                encoded.len() as u32,
                encoded.as_ptr(),
                &mut backend_code,
            )
        };
        if status == SUCCESS {
            Ok(())
        } else {
            Err(provider_error(status, backend_code))
        }
    }
}

impl Drop for Command {
    fn drop(&mut self) {
        unsafe {
            self.api.command_destroy.expect("checked provider callback")(self.device, self.handle)
        };
    }
}

fn provider_error(status: i32, code: i32) -> BackendError {
    BackendError { status, code }
}

pub struct Timeline {
    api: PluginApi,
    device: *mut c_void,
    handle: *mut c_void,
}

impl Timeline {
    pub fn query(&self) -> Result<u64, BackendError> {
        let mut value = 0;
        let mut backend_code = 0;
        let status = unsafe {
            self.api.timeline_query.expect("checked provider callback")(
                self.handle,
                &mut value,
                &mut backend_code,
            )
        };
        if status == SUCCESS {
            Ok(value)
        } else {
            Err(provider_error(status, backend_code))
        }
    }

    pub fn wait(&self, value: u64, timeout_ns: u64) -> Result<bool, BackendError> {
        let mut completed = 0;
        let mut backend_code = 0;
        let status = unsafe {
            self.api.timeline_wait.expect("checked provider callback")(
                self.handle,
                value,
                timeout_ns,
                &mut completed,
                &mut backend_code,
            )
        };
        if status == SUCCESS && completed <= 1 {
            Ok(completed != 0)
        } else {
            Err(provider_error(status, backend_code))
        }
    }

    pub fn signal(&self, value: u64) -> Result<(), BackendError> {
        let mut backend_code = 0;
        let status = unsafe {
            self.api.timeline_signal.expect("checked provider callback")(
                self.handle,
                value,
                &mut backend_code,
            )
        };
        if status == SUCCESS {
            Ok(())
        } else {
            Err(provider_error(status, backend_code))
        }
    }

    pub fn export_fd(&self) -> Result<OwnedFd, BackendError> {
        let mut fd = -1;
        let mut backend_code = 0;
        let status = unsafe {
            self.api
                .timeline_export_fd
                .expect("checked provider callback")(
                self.handle, &mut fd, &mut backend_code
            )
        };
        if status != SUCCESS || fd < 0 {
            return Err(provider_error(status, backend_code));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        unsafe {
            self.api
                .timeline_destroy
                .expect("checked provider callback")(self.device, self.handle)
        };
    }
}
