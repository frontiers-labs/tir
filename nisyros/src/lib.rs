//! Nisyros C ABI. The only backend implemented by this crate is Vulkan.

pub mod package;
mod vulkan;

/// Captures an implementation-compatible compute pipeline package from SPIR-V.
/// This is an offline packaging operation outside the C execution ABI.
pub fn capture_vulkan_package(
    device_index: usize,
    words: &[u32],
    mut template: package::Package,
) -> Result<package::Package, String> {
    let instance = vulkan::Instance::open().map_err(|()| "Vulkan loader unavailable")?;
    let devices = instance
        .devices()
        .map_err(|()| "Vulkan device enumeration failed")?;
    let record = devices
        .get(device_index)
        .ok_or("device index is not present")?;
    if !record.strict_profile_supported {
        return Err("device does not support the strict Vulkan profile".into());
    }
    let family = record
        .queue_families
        .iter()
        .position(|family| family.flags & 2 != 0 && family.count != 0)
        .ok_or("device has no compute queue")? as u32;
    let device = instance
        .open_device(
            record,
            &[vulkan::QueueRequest {
                family,
                count: 1,
                priority: 1.0,
            }],
        )
        .map_err(|()| "Vulkan device creation failed")?;
    template.vendor_id = record.vendor_id;
    template.device_id = record.device_id;
    template.api_version = record.api_version;
    device
        .capture_pipeline(words, template)
        .map_err(|error| format!("pipeline capture failed: {error:?}"))
}

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SUCCESS: i32 = 0;
const INCOMPLETE: i32 = 1;
const INVALID_ARGUMENT: i32 = -1;
const UNSUPPORTED: i32 = -2;
const NO_DEVICE: i32 = -3;
const DEVICE_LOST: i32 = -6;
const OUT_OF_MEMORY: i32 = -4;
const BUSY: i32 = -5;
const TIMEOUT: i32 = -7;
const SUBMISSION_FAILED: i32 = -8;
const INCOMPATIBLE_BINARY: i32 = -9;
const ABI_VERSION: u32 = 1;
const BACKEND_VULKAN: u32 = 1;

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
pub struct DescriptorHeader {
    struct_size: u32,
    abi_version: u32,
    extensions: *const Extension,
}

#[repr(C)]
pub struct Extension {
    struct_size: u32,
    type_id: u32,
    flags: u32,
    reserved: u32,
    next: *const Extension,
}

fn validate_header(header: &DescriptorHeader, expected_size: usize) -> i32 {
    if (header.struct_size as usize) < expected_size || header.abi_version != ABI_VERSION {
        return INVALID_ARGUMENT;
    }
    let mut seen = BTreeSet::new();
    let mut extension = header.extensions;
    while !extension.is_null() {
        if seen.len() >= 32 || !seen.insert(extension as usize) {
            return INVALID_ARGUMENT;
        }
        // SAFETY: Descriptor extension chains are readable caller-owned C data.
        let current = unsafe { &*extension };
        if (current.struct_size as usize) < std::mem::size_of::<Extension>()
            || current.type_id == 0
            || current.flags & !1 != 0
            || current.reserved != 0
        {
            return INVALID_ARGUMENT;
        }
        if current.flags & 1 == 0 {
            return UNSUPPORTED;
        }
        extension = current.next;
    }
    SUCCESS
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DeviceId {
    epoch: u64,
    index: u32,
    backend: u32,
}

#[repr(C)]
pub struct DeviceInfo {
    header: DescriptorHeader,
    backend: u32,
    api_version: u32,
    vendor_id: u32,
    device_id: u32,
    queue_family_count: u32,
    strict_profile_supported: u32,
    name: [c_char; 256],
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    driver_id: u32,
    device_type: u32,
    driver_name: [c_char; 256],
}

#[repr(C)]
pub struct QueueFamilyInfo {
    family: u32,
    count: u32,
    flags: u32,
    engine_identity_known: u32,
    affinity_enforceable: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct QueueRequest {
    family: u32,
    count: u32,
    priority: f32,
    reserved: u32,
}

#[repr(C)]
pub struct DeviceOpenDesc {
    header: DescriptorHeader,
    queue_request_count: u32,
    queue_requests: *const QueueRequest,
}

#[repr(C)]
pub struct AllocDesc {
    header: DescriptorHeader,
    bytes: u64,
    alignment: u64,
    required_properties: u32,
    preferred_properties: u32,
    reserved: [u32; 2],
}

#[repr(C)]
pub struct HostAllocDesc {
    header: DescriptorHeader,
    bytes: u64,
    alignment: u64,
    reserved: [u32; 2],
}

pub struct Cluster {
    instance: vulkan::Instance,
    devices: Vec<vulkan::DeviceRecord>,
    epoch: u64,
    live_devices: AtomicUsize,
}

pub struct Device {
    cluster: *mut Cluster,
    queues: Vec<Queue>,
    allocations: Mutex<BTreeMap<u64, TrackedAllocation>>,
    host_allocations: Mutex<BTreeMap<usize, TrackedHostAllocation>>,
    live_commands: AtomicUsize,
    live_events: AtomicUsize,
    live_executables: AtomicUsize,
    submission_lock: Mutex<()>,
    vendor_id: u32,
    device_id: u32,
    api_version: u32,
    backend: vulkan::Device,
}

pub struct Executable {
    owner: *mut Device,
    package: package::Package,
    kernel: Kernel,
    pipeline: vulkan::Pipeline,
    recordings: AtomicUsize,
}

pub struct Kernel {
    executable: *mut Executable,
}

pub struct Queue {
    parent: *mut Device,
    backend_index: usize,
    family: u32,
    index: u32,
    completion: Event,
    state: Mutex<QueueState>,
}

pub struct Event {
    timeline: vulkan::Timeline,
    owner: *mut Device,
    queue_owned: bool,
    signal_domain: u32,
    recordings: AtomicUsize,
    signal_state: Mutex<SignalState>,
    inflight: Mutex<Vec<Point>>,
    failure: AtomicI32,
}

struct SignalState {
    last_reserved: u64,
    last_queue: *mut Queue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Point {
    event: *mut Event,
    value: u64,
}

#[repr(C)]
pub struct EventDesc {
    header: DescriptorHeader,
    signaling_domain: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EventBinding {
    slot: u32,
    reserved: u32,
    point: Point,
}

#[repr(C)]
pub struct SubmitDesc {
    header: DescriptorHeader,
    binding_count: u32,
    bindings: *const EventBinding,
}

#[repr(C)]
pub struct CommandInfo {
    header: DescriptorHeader,
    native_segment_count: u32,
    requires_host_progress: u32,
    native_record_count: u64,
}

#[repr(C)]
pub struct Image {
    header: DescriptorHeader,
    format: u32,
    reserved: u32,
    byte_size: u64,
    bytes: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Grid {
    x: u32,
    y: u32,
    z: u32,
}

struct QueueState {
    next_value: u64,
    poisoned: bool,
}

struct TrackedAllocation {
    backend: vulkan::Allocation,
    recordings: usize,
}

struct TrackedHostAllocation {
    backend: vulkan::Allocation,
    bytes: u64,
    recordings: usize,
    registered: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordingState {
    Initial,
    Recording,
    Executable,
    Pending,
    Invalid,
}

struct CommandState {
    state: RecordingState,
    actions: Vec<RecordedAction>,
    plans: Vec<SegmentPlan>,
    references: BTreeSet<u64>,
    host_references: BTreeSet<usize>,
    event_references: BTreeSet<usize>,
    executable_references: BTreeSet<usize>,
    signal_slots: BTreeSet<u32>,
    inline_arguments: Vec<vulkan::Allocation>,
    native: Vec<vulkan::Command>,
    native_record_count: u64,
    pending: Option<Point>,
}

#[derive(Clone, Copy)]
enum RecordedAction {
    Operation(vulkan::CommandOp),
    Wait(EventRef),
    Signal(EventRef),
}

#[derive(Clone, Copy)]
enum EventRef {
    Point(Point),
    Slot(u32),
}

struct SegmentPlan {
    operations: Vec<vulkan::CommandOp>,
    waits: Vec<EventRef>,
    signals: Vec<EventRef>,
}

impl SegmentPlan {
    fn new() -> Self {
        Self {
            operations: Vec::new(),
            waits: Vec::new(),
            signals: Vec::new(),
        }
    }
}

pub struct Command {
    device: *mut Device,
    family: u32,
    inner: Mutex<CommandState>,
}

/// Opens the local Vulkan inventory.
///
/// # Safety
/// `out` must point to writable storage for one cluster handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cluster_open_local(out: *mut *mut Cluster) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The caller supplied writable output storage.
    unsafe { *out = ptr::null_mut() };
    let Ok(instance) = vulkan::Instance::open() else {
        return NO_DEVICE;
    };
    let Ok(devices) = instance.devices() else {
        return NO_DEVICE;
    };
    let cluster = Box::new(Cluster {
        instance,
        devices,
        epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
        live_devices: AtomicUsize::new(0),
    });
    // SAFETY: The caller supplied writable output storage.
    unsafe { *out = Box::into_raw(cluster) };
    SUCCESS
}

/// Enumerates IDs in this cluster epoch. `count` is input capacity and output
/// total. A null `ids` pointer queries the required capacity.
///
/// # Safety
/// `cluster` must be a live handle, `count` must be writable, and nonnull
/// `ids` must hold the input capacity of `DeviceId` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cluster_devices(
    cluster: *mut Cluster,
    count: *mut u32,
    ids: *mut DeviceId,
) -> i32 {
    if cluster.is_null() || count.is_null() {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The C caller owns the live handle and output storage.
    let cluster = unsafe { &*cluster };
    let capacity = unsafe { *count };
    let Ok(total) = u32::try_from(cluster.devices.len()) else {
        return INVALID_ARGUMENT;
    };
    unsafe { *count = total };
    if ids.is_null() {
        return SUCCESS;
    }
    for index in 0..capacity.min(total) {
        // SAFETY: The caller promised `capacity` writable entries.
        unsafe {
            *ids.add(index as usize) = DeviceId {
                epoch: cluster.epoch,
                index,
                backend: BACKEND_VULKAN,
            };
        }
    }
    if capacity < total {
        INCOMPLETE
    } else {
        SUCCESS
    }
}

/// Returns the Vulkan device identity and strict-profile availability.
///
/// # Safety
/// `cluster` must be live and `out` must point to a complete `DeviceInfo`
/// with its descriptor header initialized for this ABI version.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cluster_device_info(
    cluster: *mut Cluster,
    id: DeviceId,
    out: *mut DeviceInfo,
) -> i32 {
    if cluster.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The caller owns the live handle and initialized output header.
    let cluster = unsafe { &*cluster };
    let info = unsafe { &mut *out };
    let status = validate_header(&info.header, std::mem::size_of::<DeviceInfo>());
    if status != SUCCESS {
        return status;
    }
    if id.epoch != cluster.epoch || id.backend != BACKEND_VULKAN {
        return INVALID_ARGUMENT;
    }
    let Some(device) = cluster.devices.get(id.index as usize) else {
        return INVALID_ARGUMENT;
    };
    info.backend = BACKEND_VULKAN;
    info.api_version = device.api_version;
    info.vendor_id = device.vendor_id;
    info.device_id = device.device_id;
    info.queue_family_count = device.queue_families.len() as u32;
    info.strict_profile_supported = u32::from(device.strict_profile_supported);
    info.name = device.name;
    info.device_uuid = device.device_uuid;
    info.driver_uuid = device.driver_uuid;
    info.driver_id = device.driver_id;
    info.device_type = device.device_type;
    info.driver_name = device.driver_name;
    SUCCESS
}

/// Queries a Vulkan queue family. Physical engine identity and enforceable
/// affinity are reported as unknown because Vulkan does not expose them.
///
/// # Safety
/// `cluster` must be live and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cluster_queue_family_info(
    cluster: *mut Cluster,
    id: DeviceId,
    family: u32,
    out: *mut QueueFamilyInfo,
) -> i32 {
    if cluster.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The caller owns the live handle and output storage.
    let cluster = unsafe { &*cluster };
    if id.epoch != cluster.epoch || id.backend != BACKEND_VULKAN {
        return INVALID_ARGUMENT;
    }
    let Some(info) = cluster
        .devices
        .get(id.index as usize)
        .and_then(|device| device.queue_families.get(family as usize))
    else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *out = QueueFamilyInfo {
            family,
            count: info.count,
            flags: info.flags,
            engine_identity_known: 0,
            affinity_enforceable: 0,
        };
    }
    SUCCESS
}

/// Closes a cluster after all of its children have been released.
///
/// # Safety
/// `cluster` must be a live handle returned by `nisyros_cluster_open_local`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cluster_close(cluster: *mut Cluster) -> i32 {
    if cluster.is_null() {
        return INVALID_ARGUMENT;
    }
    if unsafe { (*cluster).live_devices.load(Ordering::Acquire) } != 0 {
        return BUSY;
    }
    // SAFETY: Ownership of the live handle returns to Rust exactly once.
    unsafe { drop(Box::from_raw(cluster)) };
    SUCCESS
}

/// Opens a strict-profile Vulkan session with exactly the requested queues.
///
/// # Safety
/// `cluster` and `id` must be live, `desc` must point to a complete descriptor,
/// its queue array must be readable, and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_device_open(
    cluster: *mut Cluster,
    id: DeviceId,
    desc: *const DeviceOpenDesc,
    out: *mut *mut Device,
) -> i32 {
    if cluster.is_null() || desc.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    // SAFETY: The caller provided live objects and a complete descriptor.
    let cluster_ref = unsafe { &*cluster };
    let desc = unsafe { &*desc };
    let status = validate_header(&desc.header, std::mem::size_of::<DeviceOpenDesc>());
    if status != SUCCESS {
        return status;
    }
    if id.epoch != cluster_ref.epoch
        || id.backend != BACKEND_VULKAN
        || desc.queue_request_count == 0
        || desc.queue_requests.is_null()
    {
        return INVALID_ARGUMENT;
    }
    let Some(record) = cluster_ref.devices.get(id.index as usize) else {
        return INVALID_ARGUMENT;
    };
    if !record.strict_profile_supported {
        return UNSUPPORTED;
    }
    let requests = unsafe {
        std::slice::from_raw_parts(desc.queue_requests, desc.queue_request_count as usize)
    };
    let mut seen = vec![false; record.queue_families.len()];
    let mut backend_requests = Vec::with_capacity(requests.len());
    for request in requests {
        let Some(family) = record.queue_families.get(request.family as usize) else {
            return INVALID_ARGUMENT;
        };
        if request.count == 0
            || request.count > family.count
            || !request.priority.is_finite()
            || !(0.0..=1.0).contains(&request.priority)
            || request.reserved != 0
            || seen[request.family as usize]
        {
            return INVALID_ARGUMENT;
        }
        seen[request.family as usize] = true;
        backend_requests.push(vulkan::QueueRequest {
            family: request.family,
            count: request.count,
            priority: request.priority,
        });
    }
    let Ok(backend) = cluster_ref.instance.open_device(record, &backend_requests) else {
        return DEVICE_LOST;
    };
    let mut queues = Vec::new();
    for (backend_index, queue) in backend.queues.iter().enumerate() {
        let Ok(timeline) = backend.create_timeline() else {
            return DEVICE_LOST;
        };
        queues.push(Queue {
            parent: ptr::null_mut(),
            backend_index,
            family: queue.family,
            index: queue.index,
            completion: Event {
                timeline,
                owner: ptr::null_mut(),
                queue_owned: true,
                signal_domain: 1,
                recordings: AtomicUsize::new(0),
                signal_state: Mutex::new(SignalState {
                    last_reserved: 0,
                    last_queue: ptr::null_mut(),
                }),
                inflight: Mutex::new(Vec::new()),
                failure: AtomicI32::new(0),
            },
            state: Mutex::new(QueueState {
                next_value: 0,
                poisoned: false,
            }),
        });
    }
    let mut device = Box::new(Device {
        cluster,
        queues,
        allocations: Mutex::new(BTreeMap::new()),
        host_allocations: Mutex::new(BTreeMap::new()),
        live_commands: AtomicUsize::new(0),
        live_events: AtomicUsize::new(0),
        live_executables: AtomicUsize::new(0),
        submission_lock: Mutex::new(()),
        vendor_id: record.vendor_id,
        device_id: record.device_id,
        api_version: record.api_version,
        backend,
    });
    let device_ptr: *mut Device = &raw mut *device;
    for queue in &mut device.queues {
        queue.parent = device_ptr;
        queue.completion.owner = device_ptr;
    }
    cluster_ref.live_devices.fetch_add(1, Ordering::Release);
    unsafe { *out = Box::into_raw(device) };
    SUCCESS
}

/// Returns a borrowed queue handle provisioned by `nisyros_device_open`.
///
/// # Safety
/// `device` must be live and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_queue_get(
    device: *mut Device,
    family: u32,
    index: u32,
    out: *mut *mut Queue,
) -> i32 {
    if device.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    // SAFETY: The caller owns the live device and writable output storage.
    let device = unsafe { &*device };
    let Some(queue) = device
        .queues
        .iter()
        .find(|queue| queue.family == family && queue.index == index)
    else {
        return INVALID_ARGUMENT;
    };
    unsafe { *out = (queue as *const Queue).cast_mut() };
    SUCCESS
}

/// Closes an idle device. Its borrowed queue handles become invalid.
///
/// # Safety
/// `device` must be a live handle returned by `nisyros_device_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_device_close(device: *mut Device) -> i32 {
    if device.is_null() {
        return INVALID_ARGUMENT;
    }
    let device_ref = unsafe { &*device };
    if device_ref.live_commands.load(Ordering::Acquire) != 0 {
        return BUSY;
    }
    if device_ref.live_events.load(Ordering::Acquire) != 0 {
        return BUSY;
    }
    if device_ref.live_executables.load(Ordering::Acquire) != 0 {
        return BUSY;
    }
    let Ok(allocations) = device_ref.allocations.lock() else {
        return DEVICE_LOST;
    };
    if !allocations.is_empty() {
        return BUSY;
    }
    drop(allocations);
    let Ok(host_allocations) = device_ref.host_allocations.lock() else {
        return DEVICE_LOST;
    };
    if !host_allocations.is_empty() {
        return BUSY;
    }
    drop(host_allocations);
    let cluster = device_ref.cluster;
    // SAFETY: Ownership of the live device returns to Rust exactly once.
    unsafe { drop(Box::from_raw(device)) };
    unsafe { (*cluster).live_devices.fetch_sub(1, Ordering::Release) };
    SUCCESS
}

fn allocation_status(error: vulkan::AllocError) -> i32 {
    match error {
        vulkan::AllocError::Invalid => INVALID_ARGUMENT,
        vulkan::AllocError::Unsupported => UNSUPPORTED,
        vulkan::AllocError::OutOfMemory => OUT_OF_MEMORY,
        vulkan::AllocError::Backend => DEVICE_LOST,
    }
}

/// Allocates a device-addressable range. Zero is reserved as a null address.
///
/// # Safety
/// `device` must be live, `desc` must point to a complete descriptor, and
/// `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_alloc(
    device: *mut Device,
    desc: *const AllocDesc,
    out: *mut u64,
) -> i32 {
    if device.is_null() || desc.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = 0 };
    let device = unsafe { &*device };
    let desc = unsafe { &*desc };
    let status = validate_header(&desc.header, std::mem::size_of::<AllocDesc>());
    if status != SUCCESS {
        return status;
    }
    if desc.reserved != [0; 2]
        || desc.bytes == 0
        || !desc.alignment.is_power_of_two()
        || (desc.required_properties | desc.preferred_properties) & !7 != 0
    {
        return INVALID_ARGUMENT;
    }
    let mut families: Vec<u32> = device.queues.iter().map(|queue| queue.family).collect();
    families.sort_unstable();
    families.dedup();
    let request = vulkan::AllocRequest {
        bytes: desc.bytes,
        alignment: desc.alignment,
        required_properties: desc.required_properties,
        preferred_properties: desc.preferred_properties,
        queue_families: &families,
    };
    let allocation = match device.backend.allocate(&request) {
        Ok(allocation) => allocation,
        Err(error) => return allocation_status(error),
    };
    let base = allocation.base;
    let Ok(mut allocations) = device.allocations.lock() else {
        device.backend.free_allocation(allocation);
        return DEVICE_LOST;
    };
    if allocations.contains_key(&base) {
        device.backend.free_allocation(allocation);
        return DEVICE_LOST;
    }
    allocations.insert(
        base,
        TrackedAllocation {
            backend: allocation,
            recordings: 0,
        },
    );
    unsafe { *out = base };
    SUCCESS
}

/// Releases an unmapped allocation once no recording or submission uses it.
///
/// # Safety
/// `device` must be live and `base` must belong to it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_free(device: *mut Device, base: u64) -> i32 {
    if device.is_null() || base == 0 {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let Ok(mut allocations) = device.allocations.lock() else {
        return DEVICE_LOST;
    };
    let Some(allocation) = allocations.get(&base) else {
        return INVALID_ARGUMENT;
    };
    if allocation.backend.is_mapped() || allocation.recordings != 0 {
        return BUSY;
    }
    let allocation = allocations.remove(&base).expect("checked allocation key");
    device.backend.free_allocation(allocation.backend);
    SUCCESS
}

/// Maps an interior device address to an ordinary host pointer.
///
/// # Safety
/// `device` must be live and `host` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_map(
    device: *mut Device,
    address: u64,
    bytes: u64,
    host: *mut *mut c_void,
) -> i32 {
    if device.is_null() || host.is_null() || bytes == 0 {
        return INVALID_ARGUMENT;
    }
    unsafe { *host = ptr::null_mut() };
    let device = unsafe { &*device };
    let Ok(mut allocations) = device.allocations.lock() else {
        return DEVICE_LOST;
    };
    let Some((_, allocation)) = allocations.range_mut(..=address).next_back() else {
        return INVALID_ARGUMENT;
    };
    if !allocation.backend.contains(address, bytes) {
        return INVALID_ARGUMENT;
    }
    match device
        .backend
        .map_allocation(&mut allocation.backend, address)
    {
        Ok(mapped) => {
            unsafe { *host = mapped };
            SUCCESS
        }
        Err(error) => allocation_status(error),
    }
}

/// Ends the mapping returned by `nisyros_map`.
///
/// # Safety
/// `device` must be live and `host` must be a live pointer from its mapping.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_unmap(device: *mut Device, host: *mut c_void) -> i32 {
    if device.is_null() || host.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let Ok(mut allocations) = device.allocations.lock() else {
        return DEVICE_LOST;
    };
    let Some(allocation) = allocations
        .values_mut()
        .find(|allocation| allocation.backend.mapped_host() == host)
    else {
        return INVALID_ARGUMENT;
    };
    device.backend.unmap_allocation(&mut allocation.backend);
    SUCCESS
}

fn visibility(device: *mut Device, address: u64, bytes: u64, flush: bool) -> i32 {
    if device.is_null() || bytes == 0 {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The caller supplied a live device handle.
    let device = unsafe { &*device };
    let Ok(allocations) = device.allocations.lock() else {
        return DEVICE_LOST;
    };
    let Some((_, allocation)) = allocations.range(..=address).next_back() else {
        return INVALID_ARGUMENT;
    };
    if !allocation.backend.contains(address, bytes) {
        return INVALID_ARGUMENT;
    }
    match device.backend.visibility(&allocation.backend, flush) {
        Ok(()) => SUCCESS,
        Err(error) => allocation_status(error),
    }
}

/// Flushes host writes to a mapped allocation before device reads.
///
/// # Safety
/// `device` must be live and the address range must belong to it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_flush(device: *mut Device, address: u64, bytes: u64) -> i32 {
    visibility(device, address, bytes, true)
}

/// Invalidates host caches after device completion and before host reads.
///
/// # Safety
/// `device` must be live and the address range must belong to it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_invalidate(device: *mut Device, address: u64, bytes: u64) -> i32 {
    visibility(device, address, bytes, false)
}

/// Allocates coherent host memory backed by a device-addressable Vulkan range.
/// Its returned value is an ordinary host pointer.
///
/// # Safety
/// `device` must be live, `desc` complete, and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_host_alloc(
    device: *mut Device,
    desc: *const HostAllocDesc,
    out: *mut *mut c_void,
) -> i32 {
    if device.is_null() || desc.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device };
    let desc = unsafe { &*desc };
    let status = validate_header(&desc.header, std::mem::size_of::<HostAllocDesc>());
    if status != SUCCESS {
        return status;
    }
    if desc.bytes == 0 || !desc.alignment.is_power_of_two() || desc.reserved != [0; 2] {
        return INVALID_ARGUMENT;
    }
    let Ok(host_alignment) = usize::try_from(desc.alignment) else {
        return INVALID_ARGUMENT;
    };
    let mut families: Vec<u32> = device.queues.iter().map(|queue| queue.family).collect();
    families.sort_unstable();
    families.dedup();
    let request = vulkan::AllocRequest {
        bytes: desc.bytes,
        alignment: desc.alignment.max(4),
        required_properties: 2 | 4,
        preferred_properties: 1,
        queue_families: &families,
    };
    let mut allocation = match device.backend.allocate(&request) {
        Ok(allocation) => allocation,
        Err(error) => return allocation_status(error),
    };
    let address = allocation.base;
    let host = match device.backend.map_allocation(&mut allocation, address) {
        Ok(host) => host,
        Err(error) => {
            device.backend.free_allocation(allocation);
            return allocation_status(error);
        }
    };
    if !(host as usize).is_multiple_of(host_alignment) {
        device.backend.free_allocation(allocation);
        return UNSUPPORTED;
    }
    let Ok(mut hosts) = device.host_allocations.lock() else {
        device.backend.free_allocation(allocation);
        return DEVICE_LOST;
    };
    if hosts.contains_key(&(host as usize)) {
        device.backend.free_allocation(allocation);
        return DEVICE_LOST;
    }
    hosts.insert(
        host as usize,
        TrackedHostAllocation {
            backend: allocation,
            bytes: desc.bytes,
            recordings: 0,
            registered: false,
        },
    );
    unsafe { *out = host };
    SUCCESS
}

/// Releases a runtime-owned host allocation after all recordings let go of it.
///
/// # Safety
/// `device` must be live and `host` must be its allocation base pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_host_free(device: *mut Device, host: *mut c_void) -> i32 {
    if device.is_null() || host.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let Ok(mut hosts) = device.host_allocations.lock() else {
        return DEVICE_LOST;
    };
    let Some(allocation) = hosts.get(&(host as usize)) else {
        return INVALID_ARGUMENT;
    };
    if allocation.registered {
        return INVALID_ARGUMENT;
    }
    if allocation.recordings != 0 {
        return BUSY;
    }
    let allocation = hosts
        .remove(&(host as usize))
        .expect("checked host allocation");
    device.backend.free_allocation(allocation.backend);
    SUCCESS
}

/// Queries the pointer and size alignment required for host registration.
///
/// # Safety
/// `device` must be live and `alignment` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_host_registration_alignment(
    device: *mut Device,
    alignment: *mut u64,
) -> i32 {
    if device.is_null() || alignment.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let Some(required) = device.backend.external_host_alignment else {
        return UNSUPPORTED;
    };
    unsafe { *alignment = required };
    SUCCESS
}

/// Registers an aligned caller-owned host range with the local Vulkan device.
/// The caller retains ownership and keeps it alive until unregister succeeds.
///
/// # Safety
/// `device` must be live and `host` must point to `bytes` writable, aligned
/// bytes that remain allocated through unregister.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_host_register(
    device: *mut Device,
    host: *mut c_void,
    bytes: u64,
) -> i32 {
    if device.is_null() || host.is_null() || bytes == 0 {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let Ok(mut hosts) = device.host_allocations.lock() else {
        return DEVICE_LOST;
    };
    let start = host as usize;
    let Ok(length) = usize::try_from(bytes) else {
        return INVALID_ARGUMENT;
    };
    let Some(end) = start.checked_add(length) else {
        return INVALID_ARGUMENT;
    };
    if hosts
        .range(..end)
        .next_back()
        .is_some_and(|(&base, allocation)| {
            base.checked_add(allocation.bytes as usize)
                .is_none_or(|last| last > start)
        })
    {
        return INVALID_ARGUMENT;
    }
    let allocation = match device.backend.register_host_pointer(host, bytes) {
        Ok(allocation) => allocation,
        Err(error) => return allocation_status(error),
    };
    hosts.insert(
        start,
        TrackedHostAllocation {
            backend: allocation,
            bytes,
            recordings: 0,
            registered: true,
        },
    );
    SUCCESS
}

/// Unregisters a caller-owned host range after every recording releases it.
///
/// # Safety
/// `device` must be live and `host` the base of a live registration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_host_unregister(device: *mut Device, host: *mut c_void) -> i32 {
    if device.is_null() || host.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let Ok(mut hosts) = device.host_allocations.lock() else {
        return DEVICE_LOST;
    };
    let Some(allocation) = hosts.get(&(host as usize)) else {
        return INVALID_ARGUMENT;
    };
    if !allocation.registered {
        return INVALID_ARGUMENT;
    }
    if allocation.recordings != 0 {
        return BUSY;
    }
    let allocation = hosts
        .remove(&(host as usize))
        .expect("checked registration");
    device.backend.free_allocation(allocation.backend);
    SUCCESS
}

/// Loads a complete native Vulkan pipeline-binary package without shader compilation.
///
/// # Safety
/// `device` must be live, `image` complete with readable bytes, and `out`
/// writable. The input bytes may be released after this function returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_executable_load(
    device: *mut Device,
    image: *const Image,
    out: *mut *mut Executable,
) -> i32 {
    if device.is_null() || image.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device_ref = unsafe { &*device };
    let image = unsafe { &*image };
    let status = validate_header(&image.header, std::mem::size_of::<Image>());
    if status != SUCCESS {
        return status;
    }
    if image.format != 1
        || image.reserved != 0
        || image.bytes.is_null()
        || image.byte_size == 0
        || image.byte_size > 128 * 1024 * 1024
    {
        return INVALID_ARGUMENT;
    }
    let Ok(length) = usize::try_from(image.byte_size) else {
        return INVALID_ARGUMENT;
    };
    let bytes = unsafe { std::slice::from_raw_parts(image.bytes.cast::<u8>(), length) };
    let Ok(package) = package::Package::parse(bytes) else {
        return INCOMPATIBLE_BINARY;
    };
    if package.vendor_id != device_ref.vendor_id
        || package.device_id != device_ref.device_id
        || package.api_version > device_ref.api_version
    {
        return INCOMPATIBLE_BINARY;
    }
    let pipeline = match device_ref.backend.import_pipeline(&package) {
        Ok(pipeline) => pipeline,
        Err(vulkan::PipelineError::Backend(-4)) => return DEVICE_LOST,
        Err(_) => return INCOMPATIBLE_BINARY,
    };
    let mut executable = Box::new(Executable {
        owner: device,
        package,
        kernel: Kernel {
            executable: ptr::null_mut(),
        },
        pipeline,
        recordings: AtomicUsize::new(0),
    });
    executable.kernel.executable = &raw mut *executable;
    device_ref.live_executables.fetch_add(1, Ordering::Release);
    unsafe { *out = Box::into_raw(executable) };
    SUCCESS
}

/// Returns a borrowed prepared entry point from a loaded executable.
///
/// # Safety
/// `executable` must be live, `entry` a terminated C string, and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_kernel_lookup(
    executable: *mut Executable,
    entry: *const c_char,
    out: *mut *mut Kernel,
) -> i32 {
    if executable.is_null() || entry.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let executable = unsafe { &*executable };
    let name = unsafe { CStr::from_ptr(entry) };
    if name.to_bytes() != executable.package.entry.as_bytes() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = (&raw const executable.kernel).cast_mut() };
    SUCCESS
}

/// Releases an executable after every recording that references it is reset.
///
/// # Safety
/// `executable` must be a live handle returned by `nisyros_executable_load`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_executable_destroy(executable: *mut Executable) -> i32 {
    if executable.is_null() {
        return INVALID_ARGUMENT;
    }
    let executable_ref = unsafe { &*executable };
    if executable_ref.recordings.load(Ordering::Acquire) != 0 {
        return BUSY;
    }
    let owner = executable_ref.owner;
    unsafe { drop(Box::from_raw(executable)) };
    unsafe { (*owner).live_executables.fetch_sub(1, Ordering::Release) };
    SUCCESS
}

fn refresh_command(state: &mut CommandState) -> i32 {
    if state.state != RecordingState::Pending {
        return SUCCESS;
    }
    let point = state
        .pending
        .expect("pending command has a completion point");
    // SAFETY: The command retains its device, whose queue owns this event.
    let event = unsafe { &*point.event };
    let failure = event.failure.load(Ordering::Acquire);
    if failure != 0 {
        return failure;
    }
    match event.timeline.query() {
        Ok(value) if value >= point.value => {
            state.state = RecordingState::Executable;
            state.pending = None;
            SUCCESS
        }
        Ok(_) => BUSY,
        Err(_) => DEVICE_LOST,
    }
}

/// Creates a recording for one provisioned Vulkan queue family.
///
/// # Safety
/// `device` must be live and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_create(
    device: *mut Device,
    family: u32,
    out: *mut *mut Command,
) -> i32 {
    if device.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device_ref = unsafe { &*device };
    if !device_ref.queues.iter().any(|queue| queue.family == family) {
        return INVALID_ARGUMENT;
    }
    let command = Box::new(Command {
        device,
        family,
        inner: Mutex::new(CommandState {
            state: RecordingState::Initial,
            actions: Vec::new(),
            plans: Vec::new(),
            references: BTreeSet::new(),
            host_references: BTreeSet::new(),
            event_references: BTreeSet::new(),
            executable_references: BTreeSet::new(),
            signal_slots: BTreeSet::new(),
            inline_arguments: Vec::new(),
            native: Vec::new(),
            native_record_count: 0,
            pending: None,
        }),
    });
    device_ref.live_commands.fetch_add(1, Ordering::Release);
    unsafe { *out = Box::into_raw(command) };
    SUCCESS
}

/// Starts recording an initial command buffer.
///
/// # Safety
/// `cmd` must be a live command handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_begin(cmd: *mut Command) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    let cmd = unsafe { &*cmd };
    let Ok(mut inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Initial {
        return INVALID_ARGUMENT;
    }
    inner.state = RecordingState::Recording;
    SUCCESS
}

fn record_addresses(cmd: &Command, operation: vulkan::CommandOp, ranges: &[(u64, u64)]) -> i32 {
    let Ok(mut inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Recording {
        return INVALID_ARGUMENT;
    }
    // SAFETY: A live command retains its parent device.
    let device = unsafe { &*cmd.device };
    let Ok(mut allocations) = device.allocations.lock() else {
        inner.state = RecordingState::Invalid;
        return DEVICE_LOST;
    };
    let mut bases = BTreeSet::new();
    for &(address, bytes) in ranges {
        let Some((&base, allocation)) = allocations.range(..=address).next_back() else {
            inner.state = RecordingState::Invalid;
            return INVALID_ARGUMENT;
        };
        if !allocation.backend.contains(address, bytes) {
            inner.state = RecordingState::Invalid;
            return INVALID_ARGUMENT;
        }
        bases.insert(base);
    }
    for base in bases {
        if inner.references.insert(base) {
            allocations
                .get_mut(&base)
                .expect("validated base")
                .recordings += 1;
        }
    }
    inner.actions.push(RecordedAction::Operation(operation));
    SUCCESS
}

fn invalid_recording_argument(cmd: *mut Command) -> i32 {
    if !cmd.is_null() {
        // SAFETY: A nonnull command pointer must be a live caller-owned handle.
        if let Ok(mut inner) = unsafe { &*cmd }.inner.lock()
            && inner.state == RecordingState::Recording
        {
            inner.state = RecordingState::Invalid;
        }
    }
    INVALID_ARGUMENT
}

fn record_noop(cmd: &Command) -> i32 {
    let Ok(inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state == RecordingState::Recording {
        SUCCESS
    } else {
        INVALID_ARGUMENT
    }
}

/// Records an address-to-address copy. Copy ranges must not overlap.
///
/// # Safety
/// `cmd` must be live and the ranges must belong to its device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_copy(
    cmd: *mut Command,
    dst: u64,
    src: u64,
    bytes: u64,
) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    if bytes == 0 {
        return record_noop(unsafe { &*cmd });
    }
    let Some(src_end) = src.checked_add(bytes) else {
        return invalid_recording_argument(cmd);
    };
    let Some(dst_end) = dst.checked_add(bytes) else {
        return invalid_recording_argument(cmd);
    };
    if src < dst_end && dst < src_end {
        return invalid_recording_argument(cmd);
    }
    let cmd = unsafe { &*cmd };
    record_addresses(
        cmd,
        vulkan::CommandOp::Copy { dst, src, bytes },
        &[(src, bytes), (dst, bytes)],
    )
}

fn record_host_transfer(
    cmd: *mut Command,
    host: *const c_void,
    device_address: u64,
    bytes: u64,
    to_device: bool,
) -> i32 {
    if cmd.is_null() || host.is_null() {
        return invalid_recording_argument(cmd);
    }
    if bytes == 0 {
        return record_noop(unsafe { &*cmd });
    }
    let Ok(byte_count) = usize::try_from(bytes) else {
        return invalid_recording_argument(cmd);
    };
    let host_address = host as usize;
    if host_address.checked_add(byte_count).is_none() || device_address.checked_add(bytes).is_none()
    {
        return invalid_recording_argument(cmd);
    }
    let cmd_ref = unsafe { &*cmd };
    let Ok(mut inner) = cmd_ref.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Recording {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*cmd_ref.device };
    let Ok(mut hosts) = device.host_allocations.lock() else {
        inner.state = RecordingState::Invalid;
        return DEVICE_LOST;
    };
    let Some((&host_base, host_allocation)) = hosts.range(..=host_address).next_back() else {
        inner.state = RecordingState::Invalid;
        return INVALID_ARGUMENT;
    };
    let offset = (host_address - host_base) as u64;
    if offset
        .checked_add(bytes)
        .is_none_or(|end| end > host_allocation.bytes)
    {
        inner.state = RecordingState::Invalid;
        return INVALID_ARGUMENT;
    }
    let Some(host_device_address) = host_allocation.backend.base.checked_add(offset) else {
        inner.state = RecordingState::Invalid;
        return INVALID_ARGUMENT;
    };
    let Ok(mut allocations) = device.allocations.lock() else {
        inner.state = RecordingState::Invalid;
        return DEVICE_LOST;
    };
    let Some((&device_base, allocation)) = allocations.range(..=device_address).next_back() else {
        inner.state = RecordingState::Invalid;
        return INVALID_ARGUMENT;
    };
    if !allocation.backend.contains(device_address, bytes) {
        inner.state = RecordingState::Invalid;
        return INVALID_ARGUMENT;
    }
    if inner.host_references.insert(host_base) {
        hosts
            .get_mut(&host_base)
            .expect("validated host allocation")
            .recordings += 1;
    }
    if inner.references.insert(device_base) {
        allocations
            .get_mut(&device_base)
            .expect("validated allocation")
            .recordings += 1;
    }
    let (dst, src) = if to_device {
        (device_address, host_device_address)
    } else {
        (host_device_address, device_address)
    };
    inner
        .actions
        .push(RecordedAction::Operation(vulkan::CommandOp::Copy {
            dst,
            src,
            bytes,
        }));
    SUCCESS
}

/// Records an asynchronous copy from a live runtime host allocation.
///
/// # Safety
/// `cmd` and `src` must remain live through submission and completion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_copy_h2d(
    cmd: *mut Command,
    dst: u64,
    src: *const c_void,
    bytes: u64,
) -> i32 {
    record_host_transfer(cmd, src, dst, bytes, true)
}

/// Records an asynchronous copy into a live runtime host allocation.
///
/// # Safety
/// `cmd` and `dst` must remain live through submission and completion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_copy_d2h(
    cmd: *mut Command,
    dst: *mut c_void,
    src: u64,
    bytes: u64,
) -> i32 {
    record_host_transfer(cmd, dst.cast_const(), src, bytes, false)
}

/// Records a repeated 32-bit fill over a four-byte-aligned range.
///
/// # Safety
/// `cmd` must be live and the range must belong to its device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_fill(
    cmd: *mut Command,
    dst: u64,
    bytes: u64,
    pattern: u32,
) -> i32 {
    if cmd.is_null() || !dst.is_multiple_of(4) || bytes == 0 || !bytes.is_multiple_of(4) {
        return invalid_recording_argument(cmd);
    }
    if dst.checked_add(bytes).is_none() {
        return invalid_recording_argument(cmd);
    }
    let cmd = unsafe { &*cmd };
    record_addresses(
        cmd,
        vulkan::CommandOp::Fill {
            dst,
            bytes,
            pattern,
        },
        &[(dst, bytes)],
    )
}

/// Records a compute dispatch whose packed argument block is already in a
/// device allocation. Its address remains stable for replay.
///
/// # Safety
/// `cmd`, `kernel`, and `grid` must be live, and `arguments` must cover the
/// kernel's declared argument block in the command device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_dispatch_args(
    cmd: *mut Command,
    kernel: *mut Kernel,
    grid: *const Grid,
    arguments: u64,
) -> i32 {
    if cmd.is_null() || kernel.is_null() || grid.is_null() {
        return invalid_recording_argument(cmd);
    }
    let cmd_ref = unsafe { &*cmd };
    let executable = unsafe { &*(*kernel).executable };
    if executable.owner != cmd_ref.device {
        return invalid_recording_argument(cmd);
    }
    let grid = unsafe { *grid };
    let dimensions = [grid.x, grid.y, grid.z];
    let device = unsafe { &*cmd_ref.device };
    if dimensions
        .iter()
        .zip(device.backend.max_grid)
        .any(|(&count, limit)| count == 0 || count > limit)
    {
        return invalid_recording_argument(cmd);
    }
    let operation = vulkan::CommandOp::Dispatch {
        pipeline: executable.pipeline.handle,
        layout: executable.pipeline.layout,
        grid: dimensions,
        arguments,
    };
    let status = record_addresses(
        cmd_ref,
        operation,
        &[(arguments, u64::from(executable.package.argument_size))],
    );
    if status != SUCCESS {
        return status;
    }
    let Ok(mut inner) = cmd_ref.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner
        .executable_references
        .insert(executable as *const Executable as usize)
    {
        executable.recordings.fetch_add(1, Ordering::Release);
    }
    SUCCESS
}

/// Records a compute dispatch that reads three workgroup counts from a device
/// address at execution. A producer must establish a memory dependency first.
///
/// # Safety
/// `cmd` and `kernel` must be live; both address ranges must belong to the
/// command device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_dispatch_indirect_args(
    cmd: *mut Command,
    kernel: *mut Kernel,
    grid_address: u64,
    arguments: u64,
) -> i32 {
    if cmd.is_null() || kernel.is_null() || !grid_address.is_multiple_of(4) {
        return invalid_recording_argument(cmd);
    }
    let cmd_ref = unsafe { &*cmd };
    let executable = unsafe { &*(*kernel).executable };
    if executable.owner != cmd_ref.device {
        return invalid_recording_argument(cmd);
    }
    let operation = vulkan::CommandOp::DispatchIndirect {
        pipeline: executable.pipeline.handle,
        layout: executable.pipeline.layout,
        grid_address,
        arguments,
    };
    let status = record_addresses(
        cmd_ref,
        operation,
        &[
            (grid_address, 12),
            (arguments, u64::from(executable.package.argument_size)),
        ],
    );
    if status != SUCCESS {
        return status;
    }
    let Ok(mut inner) = cmd_ref.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner
        .executable_references
        .insert(executable as *const Executable as usize)
    {
        executable.recordings.fetch_add(1, Ordering::Release);
    }
    SUCCESS
}

/// Copies the packed argument block into stable device-accessible storage
/// during recording. The block can be released by the caller on return.
///
/// # Safety
/// `cmd`, `kernel`, and `grid` must be live, and `arguments` must point to
/// `argument_bytes` readable bytes matching the executable manifest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_dispatch(
    cmd: *mut Command,
    kernel: *mut Kernel,
    grid: *const Grid,
    arguments: *const c_void,
    argument_bytes: u64,
) -> i32 {
    if cmd.is_null() || kernel.is_null() || grid.is_null() || arguments.is_null() {
        return invalid_recording_argument(cmd);
    }
    let cmd_ref = unsafe { &*cmd };
    let executable = unsafe { &*(*kernel).executable };
    if executable.owner != cmd_ref.device
        || argument_bytes != u64::from(executable.package.argument_size)
    {
        return invalid_recording_argument(cmd);
    }
    let grid = unsafe { *grid };
    let dimensions = [grid.x, grid.y, grid.z];
    let device = unsafe { &*cmd_ref.device };
    if dimensions
        .iter()
        .zip(device.backend.max_grid)
        .any(|(&count, limit)| count == 0 || count > limit)
    {
        return invalid_recording_argument(cmd);
    }
    let Ok(mut inner) = cmd_ref.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Recording {
        return INVALID_ARGUMENT;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(arguments.cast::<u8>(), argument_bytes as usize) };
    let Ok(mut allocations) = device.allocations.lock() else {
        inner.state = RecordingState::Invalid;
        return DEVICE_LOST;
    };
    let mut pointer_bases = BTreeSet::new();
    for field in &executable.package.argument_fields {
        if field.kind != package::ARG_DEVICE_ADDRESS {
            continue;
        }
        let start = field.offset as usize;
        let address = u64::from_le_bytes(
            bytes[start..start + 8]
                .try_into()
                .expect("validated address field"),
        );
        if address == 0 {
            continue;
        }
        let Some((&base, allocation)) = allocations.range(..=address).next_back() else {
            inner.state = RecordingState::Invalid;
            return INVALID_ARGUMENT;
        };
        if !allocation.backend.contains(address, 1) {
            inner.state = RecordingState::Invalid;
            return INVALID_ARGUMENT;
        }
        pointer_bases.insert(base);
    }
    let mut families: Vec<u32> = device.queues.iter().map(|queue| queue.family).collect();
    families.sort_unstable();
    families.dedup();
    let request = vulkan::AllocRequest {
        bytes: argument_bytes,
        alignment: u64::from(executable.package.argument_alignment),
        required_properties: 2,
        preferred_properties: 4,
        queue_families: &families,
    };
    let mut storage = match device.backend.allocate(&request) {
        Ok(storage) => storage,
        Err(error) => {
            inner.state = RecordingState::Invalid;
            return allocation_status(error);
        }
    };
    let storage_base = storage.base;
    let mapped = match device.backend.map_allocation(&mut storage, storage_base) {
        Ok(mapped) => mapped,
        Err(error) => {
            device.backend.free_allocation(storage);
            inner.state = RecordingState::Invalid;
            return allocation_status(error);
        }
    };
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len()) };
    let status = device.backend.visibility(&storage, true);
    device.backend.unmap_allocation(&mut storage);
    if let Err(error) = status {
        device.backend.free_allocation(storage);
        inner.state = RecordingState::Invalid;
        return allocation_status(error);
    }
    for base in pointer_bases {
        if inner.references.insert(base) {
            allocations
                .get_mut(&base)
                .expect("validated pointer base")
                .recordings += 1;
        }
    }
    if inner
        .executable_references
        .insert(executable as *const Executable as usize)
    {
        executable.recordings.fetch_add(1, Ordering::Release);
    }
    inner
        .actions
        .push(RecordedAction::Operation(vulkan::CommandOp::Dispatch {
            pipeline: executable.pipeline.handle,
            layout: executable.pipeline.layout,
            grid: dimensions,
            arguments: storage.base,
        }));
    inner.inline_arguments.push(storage);
    SUCCESS
}

/// Records a full device-memory dependency between prior and later commands.
///
/// # Safety
/// `cmd` must be a live command handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_barrier_all(cmd: *mut Command) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    let cmd = unsafe { &*cmd };
    record_addresses(cmd, vulkan::CommandOp::BarrierAll, &[])
}

/// Records an all-stage dependency restricted to one device-address range.
///
/// # Safety
/// `cmd` must be live and the range must belong to its device.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_barrier_range(
    cmd: *mut Command,
    address: u64,
    bytes: u64,
) -> i32 {
    if cmd.is_null() || bytes == 0 || address.checked_add(bytes).is_none() {
        return invalid_recording_argument(cmd);
    }
    let cmd = unsafe { &*cmd };
    record_addresses(
        cmd,
        vulkan::CommandOp::BarrierRange { address, bytes },
        &[(address, bytes)],
    )
}

/// Creates a device-signaled timeline event, initially at value zero.
///
/// # Safety
/// `device` must be live, `desc` complete, and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_event_create(
    device: *mut Device,
    desc: *const EventDesc,
    out: *mut *mut Event,
) -> i32 {
    if device.is_null() || desc.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device_ref = unsafe { &*device };
    let desc = unsafe { &*desc };
    let status = validate_header(&desc.header, std::mem::size_of::<EventDesc>());
    if status != SUCCESS {
        return status;
    }
    if desc.reserved != 0 {
        return INVALID_ARGUMENT;
    }
    if desc.signaling_domain != 1 && desc.signaling_domain != 2 {
        return UNSUPPORTED;
    }
    let Ok(timeline) = device_ref.backend.create_timeline() else {
        return DEVICE_LOST;
    };
    let event = Box::new(Event {
        timeline,
        owner: device,
        queue_owned: false,
        signal_domain: desc.signaling_domain,
        recordings: AtomicUsize::new(0),
        signal_state: Mutex::new(SignalState {
            last_reserved: 0,
            last_queue: ptr::null_mut(),
        }),
        inflight: Mutex::new(Vec::new()),
        failure: AtomicI32::new(0),
    });
    device_ref.live_events.fetch_add(1, Ordering::Release);
    unsafe { *out = Box::into_raw(event) };
    SUCCESS
}

/// Destroys an idle event. A recording retains its referenced events.
///
/// # Safety
/// `event` must be a live explicit event returned by `nisyros_event_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_event_destroy(event: *mut Event) -> i32 {
    if event.is_null() {
        return INVALID_ARGUMENT;
    }
    let event_ref = unsafe { &*event };
    if event_ref.queue_owned {
        return INVALID_ARGUMENT;
    }
    let owner = unsafe { &*event_ref.owner };
    let Ok(_submission) = owner.submission_lock.lock() else {
        return DEVICE_LOST;
    };
    if event_ref.recordings.load(Ordering::Acquire) != 0 {
        return BUSY;
    }
    let Ok(mut inflight) = event_ref.inflight.lock() else {
        return DEVICE_LOST;
    };
    inflight.retain(|point| {
        // SAFETY: Queue-owned completion points outlive explicit events.
        match unsafe { &*point.event }.timeline.query() {
            Ok(completed) => completed < point.value,
            Err(_) => true,
        }
    });
    if !inflight.is_empty() {
        return BUSY;
    }
    drop(inflight);
    let Ok(state) = event_ref.signal_state.lock() else {
        return DEVICE_LOST;
    };
    if event_ref.failure.load(Ordering::Acquire) == 0 {
        let Ok(completed) = event_ref.timeline.query() else {
            return DEVICE_LOST;
        };
        if completed < state.last_reserved {
            return BUSY;
        }
    }
    drop(state);
    let owner = event_ref.owner;
    unsafe { drop(Box::from_raw(event)) };
    unsafe { (*owner).live_events.fetch_sub(1, Ordering::Release) };
    SUCCESS
}

/// Advances a host-signaled event after earlier values have completed.
///
/// # Safety
/// `event` must be a live host-signaled explicit event.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_event_host_signal(event: *mut Event, value: u64) -> i32 {
    if event.is_null() || value == 0 {
        return INVALID_ARGUMENT;
    }
    let event = unsafe { &*event };
    if event.queue_owned || event.signal_domain != 2 {
        return INVALID_ARGUMENT;
    }
    let failure = event.failure.load(Ordering::Acquire);
    if failure != 0 {
        return failure;
    }
    let device = unsafe { &*event.owner };
    let Ok(_submission) = device.submission_lock.lock() else {
        return DEVICE_LOST;
    };
    let Ok(mut state) = event.signal_state.lock() else {
        return DEVICE_LOST;
    };
    if value <= state.last_reserved {
        return INVALID_ARGUMENT;
    }
    let Ok(completed) = event.timeline.query() else {
        return DEVICE_LOST;
    };
    if completed < state.last_reserved {
        return BUSY;
    }
    if event.timeline.signal(value).is_err() {
        event.failure.store(DEVICE_LOST, Ordering::Release);
        return DEVICE_LOST;
    }
    state.last_reserved = value;
    state.last_queue = ptr::null_mut();
    SUCCESS
}

fn record_event(cmd: *mut Command, point: Point, signal: bool) -> i32 {
    if cmd.is_null() || point.event.is_null() || (signal && point.value == 0) {
        return invalid_recording_argument(cmd);
    }
    // SAFETY: The caller owns live command and event handles.
    let cmd_ref = unsafe { &*cmd };
    let event = unsafe { &*point.event };
    if event.owner != cmd_ref.device || (signal && (event.queue_owned || event.signal_domain != 1))
    {
        return invalid_recording_argument(cmd);
    }
    let Ok(mut inner) = cmd_ref.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Recording {
        return INVALID_ARGUMENT;
    }
    if inner.event_references.insert(point.event as usize) {
        event.recordings.fetch_add(1, Ordering::Release);
    }
    inner.actions.push(if signal {
        RecordedAction::Signal(EventRef::Point(point))
    } else {
        RecordedAction::Wait(EventRef::Point(point))
    });
    SUCCESS
}

/// Records a cross-queue acquire at this command boundary.
///
/// # Safety
/// `cmd` and `point.event` must remain live through recording and submission.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_wait(cmd: *mut Command, point: Point) -> i32 {
    record_event(cmd, point, false)
}

/// Records a release to a strictly increasing event value.
///
/// # Safety
/// `cmd` and `point.event` must remain live through recording and submission.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_signal(cmd: *mut Command, point: Point) -> i32 {
    record_event(cmd, point, true)
}

fn record_slot(cmd: *mut Command, slot: u32, signal: bool) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    let cmd = unsafe { &*cmd };
    let Ok(mut inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Recording {
        return INVALID_ARGUMENT;
    }
    if signal && !inner.signal_slots.insert(slot) {
        inner.state = RecordingState::Invalid;
        return INVALID_ARGUMENT;
    }
    inner.actions.push(if signal {
        RecordedAction::Signal(EventRef::Slot(slot))
    } else {
        RecordedAction::Wait(EventRef::Slot(slot))
    });
    SUCCESS
}

/// Records a wait slot whose point is supplied on each submission.
///
/// # Safety
/// `cmd` must be a live command handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_wait_slot(cmd: *mut Command, slot: u32) -> i32 {
    record_slot(cmd, slot, false)
}

/// Records a signal slot whose point is supplied on each submission.
///
/// # Safety
/// `cmd` must be a live command handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_signal_slot(cmd: *mut Command, slot: u32) -> i32 {
    record_slot(cmd, slot, true)
}

/// Seals a recording and prepares native Vulkan command segments.
///
/// # Safety
/// `cmd` must be a live command handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_end(cmd: *mut Command) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    let cmd = unsafe { &*cmd };
    let Ok(mut inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    if inner.state != RecordingState::Recording {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*cmd.device };
    let mut plans = Vec::new();
    let mut current = SegmentPlan::new();
    for action in &inner.actions {
        match *action {
            RecordedAction::Operation(operation) => {
                if !current.signals.is_empty() {
                    plans.push(current);
                    current = SegmentPlan::new();
                }
                current.operations.push(operation);
            }
            RecordedAction::Wait(point) => {
                if !current.operations.is_empty() || !current.signals.is_empty() {
                    plans.push(current);
                    current = SegmentPlan::new();
                }
                current.waits.push(point);
            }
            RecordedAction::Signal(point) => current.signals.push(point),
        }
    }
    plans.push(current);
    let mut native_segments = Vec::with_capacity(plans.len());
    for plan in &plans {
        let Ok(native) = device.backend.create_command(cmd.family) else {
            inner.state = RecordingState::Invalid;
            return DEVICE_LOST;
        };
        if native.record(&plan.operations).is_err() {
            inner.state = RecordingState::Invalid;
            return DEVICE_LOST;
        }
        native_segments.push(native);
    }
    inner.plans = plans;
    inner.native_record_count += native_segments.len() as u64;
    inner.native = native_segments;
    inner.state = RecordingState::Executable;
    SUCCESS
}

/// Queries prepared native segments and cumulative command-buffer recordings.
///
/// # Safety
/// `cmd` must be live and `out` must be a complete initialized output struct.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_query(cmd: *mut Command, out: *mut CommandInfo) -> i32 {
    if cmd.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let out = unsafe { &mut *out };
    let status = validate_header(&out.header, std::mem::size_of::<CommandInfo>());
    if status != SUCCESS {
        return status;
    }
    let cmd = unsafe { &*cmd };
    let Ok(inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    out.native_segment_count = inner.native.len() as u32;
    out.requires_host_progress = 0;
    out.native_record_count = inner.native_record_count;
    SUCCESS
}

fn reset_command(cmd: &Command) -> i32 {
    let Ok(mut inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    let status = refresh_command(&mut inner);
    if status != SUCCESS {
        return status;
    }
    inner.native.clear();
    inner.plans.clear();
    inner.actions.clear();
    inner.signal_slots.clear();
    let device = unsafe { &*cmd.device };
    for allocation in inner.inline_arguments.drain(..) {
        device.backend.free_allocation(allocation);
    }
    let Ok(mut allocations) = device.allocations.lock() else {
        return DEVICE_LOST;
    };
    for base in std::mem::take(&mut inner.references) {
        allocations
            .get_mut(&base)
            .expect("retained allocation")
            .recordings -= 1;
    }
    drop(allocations);
    let Ok(mut hosts) = device.host_allocations.lock() else {
        return DEVICE_LOST;
    };
    for base in std::mem::take(&mut inner.host_references) {
        hosts
            .get_mut(&base)
            .expect("retained host allocation")
            .recordings -= 1;
    }
    drop(hosts);
    for event in std::mem::take(&mut inner.event_references) {
        // SAFETY: Event destruction is rejected while a recording retains it.
        unsafe {
            (*(event as *mut Event))
                .recordings
                .fetch_sub(1, Ordering::Release)
        };
    }
    for executable in std::mem::take(&mut inner.executable_references) {
        // SAFETY: Executable destruction is rejected while retained.
        unsafe {
            (*(executable as *mut Executable))
                .recordings
                .fetch_sub(1, Ordering::Release)
        };
    }
    inner.state = RecordingState::Initial;
    SUCCESS
}

/// Clears an idle recording and releases its allocation references.
///
/// # Safety
/// `cmd` must be a live command handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_reset(cmd: *mut Command) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    reset_command(unsafe { &*cmd })
}

/// Destroys an idle command buffer without waiting for pending GPU work.
///
/// # Safety
/// `cmd` must be a live command handle returned by `nisyros_cmd_create`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_cmd_destroy(cmd: *mut Command) -> i32 {
    if cmd.is_null() {
        return INVALID_ARGUMENT;
    }
    let command = unsafe { &*cmd };
    let status = reset_command(command);
    if status != SUCCESS {
        return status;
    }
    let device = command.device;
    unsafe { drop(Box::from_raw(cmd)) };
    unsafe { (*device).live_commands.fetch_sub(1, Ordering::Release) };
    SUCCESS
}

/// Submits one sealed recording asynchronously on the selected queue.
///
/// # Safety
/// `queue` and `cmd` must be live, `out` writable, and a nonnull descriptor
/// must point to a complete binding array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_submit(
    queue: *mut Queue,
    cmd: *mut Command,
    submit_desc: *const SubmitDesc,
    out: *mut Point,
) -> i32 {
    if queue.is_null() || cmd.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = Point {
            event: ptr::null_mut(),
            value: 0,
        }
    };
    let queue = unsafe { &*queue };
    let cmd = unsafe { &*cmd };
    if queue.parent != cmd.device || queue.family != cmd.family {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*queue.parent };
    let Ok(_submission) = device.submission_lock.lock() else {
        return DEVICE_LOST;
    };
    let Ok(mut inner) = cmd.inner.lock() else {
        return DEVICE_LOST;
    };
    let status = refresh_command(&mut inner);
    if status != SUCCESS {
        return status;
    }
    if inner.state != RecordingState::Executable {
        return INVALID_ARGUMENT;
    }
    let mut used_slots = BTreeSet::new();
    for plan in &inner.plans {
        for reference in plan.waits.iter().chain(&plan.signals) {
            if let EventRef::Slot(slot) = reference {
                used_slots.insert(*slot);
            }
        }
    }
    let mut bindings = BTreeMap::new();
    if !submit_desc.is_null() {
        let desc = unsafe { &*submit_desc };
        let status = validate_header(&desc.header, std::mem::size_of::<SubmitDesc>());
        if status != SUCCESS {
            return status;
        }
        if desc.binding_count as usize != used_slots.len()
            || (desc.binding_count != 0 && desc.bindings.is_null())
        {
            return INVALID_ARGUMENT;
        }
        let binding_slice = if desc.binding_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(desc.bindings, desc.binding_count as usize) }
        };
        for binding in binding_slice {
            if binding.reserved != 0
                || binding.point.event.is_null()
                || !used_slots.contains(&binding.slot)
                || bindings.insert(binding.slot, binding.point).is_some()
            {
                return INVALID_ARGUMENT;
            }
        }
    }
    if bindings.len() != used_slots.len() {
        return INVALID_ARGUMENT;
    }
    let resolve = |reference: EventRef| match reference {
        EventRef::Point(point) => point,
        EventRef::Slot(slot) => bindings[&slot],
    };
    let mut resolved_plans = Vec::with_capacity(inner.plans.len());
    for plan in &inner.plans {
        let waits: Vec<Point> = plan.waits.iter().copied().map(&resolve).collect();
        let signals: Vec<Point> = plan.signals.iter().copied().map(&resolve).collect();
        for point in &waits {
            if unsafe { &*point.event }.owner != cmd.device {
                return INVALID_ARGUMENT;
            }
        }
        for point in &signals {
            let event = unsafe { &*point.event };
            if event.owner != cmd.device
                || event.queue_owned
                || event.signal_domain != 1
                || point.value == 0
            {
                return INVALID_ARGUMENT;
            }
        }
        resolved_plans.push((waits, signals));
    }
    let Ok(mut queue_state) = queue.state.lock() else {
        return DEVICE_LOST;
    };
    if queue_state.poisoned {
        return SUBMISSION_FAILED;
    }
    let Some(value) = queue_state.next_value.checked_add(1) else {
        return INVALID_ARGUMENT;
    };
    let mut expected_signals: BTreeMap<usize, (u64, *mut Queue)> = BTreeMap::new();
    let mut prior_waits: BTreeMap<usize, u64> = BTreeMap::new();
    for (waits, signals) in &resolved_plans {
        for point in waits {
            let entry = prior_waits.entry(point.event as usize).or_default();
            *entry = (*entry).max(point.value);
        }
        for point in signals {
            let key = point.event as usize;
            let previous = if let Some(&state) = expected_signals.get(&key) {
                state
            } else {
                let event = unsafe { &*point.event };
                let state = event
                    .signal_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                (state.last_reserved, state.last_queue)
            };
            if point.value <= previous.0
                || (previous.0 != 0
                    && !ptr::eq(previous.1, queue)
                    && prior_waits
                        .get(&key)
                        .is_none_or(|&waited| waited < previous.0))
            {
                return INVALID_ARGUMENT;
            }
            expected_signals.insert(key, (point.value, queue as *const Queue as *mut Queue));
        }
    }
    let mut segments = Vec::with_capacity(inner.plans.len());
    for ((wait_points, signal_points), native) in resolved_plans.iter().zip(&inner.native) {
        let waits = wait_points
            .iter()
            .map(|point| (unsafe { &(*point.event).timeline }, point.value))
            .collect();
        let signals = signal_points
            .iter()
            .map(|point| (unsafe { &(*point.event).timeline }, point.value))
            .collect();
        segments.push(vulkan::SubmitSegment {
            command: native,
            waits,
            signals,
        });
    }
    segments
        .last_mut()
        .expect("sealed recording has a segment")
        .signals
        .push((&queue.completion.timeline, value));
    for (&event_address, &(reserved, last_queue)) in &expected_signals {
        let event = unsafe { &*(event_address as *const Event) };
        let mut state = event
            .signal_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.last_reserved = reserved;
        state.last_queue = last_queue;
    }
    queue_state.next_value = value;
    if device
        .backend
        .submit_segments(queue.backend_index, &segments)
        .is_err()
    {
        queue_state.poisoned = true;
        queue
            .completion
            .failure
            .store(SUBMISSION_FAILED, Ordering::Release);
        for &event_address in expected_signals.keys() {
            unsafe {
                (*(event_address as *mut Event))
                    .failure
                    .store(SUBMISSION_FAILED, Ordering::Release)
            };
        }
        inner.state = RecordingState::Invalid;
        return SUBMISSION_FAILED;
    }
    let point = Point {
        event: (&raw const queue.completion).cast_mut(),
        value,
    };
    for bound in bindings.values() {
        let event = unsafe { &*bound.event };
        if !event.queue_owned {
            let mut inflight = event
                .inflight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inflight.retain(
                |previous| match unsafe { &*previous.event }.timeline.query() {
                    Ok(completed) => completed < previous.value,
                    Err(_) => true,
                },
            );
            inflight.push(point);
        }
    }
    inner.state = RecordingState::Pending;
    inner.pending = Some(point);
    unsafe { *out = point };
    SUCCESS
}

/// Waits for an event point without canceling work on timeout.
///
/// # Safety
/// `point.event` must remain live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_wait(point: Point, timeout_ns: u64) -> i32 {
    if point.event.is_null() {
        return INVALID_ARGUMENT;
    }
    let event = unsafe { &*point.event };
    let started = Instant::now();
    loop {
        let failure = event.failure.load(Ordering::Acquire);
        if failure != 0 {
            return failure;
        }
        let remaining = if timeout_ns == u64::MAX {
            u64::MAX
        } else {
            timeout_ns.saturating_sub(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64)
        };
        let slice = remaining.min(Duration::from_millis(100).as_nanos() as u64);
        match event.timeline.wait(point.value, slice) {
            Ok(true) => return SUCCESS,
            Ok(false) if remaining == 0 || slice == remaining => return TIMEOUT,
            Ok(false) => {}
            Err(_) => return DEVICE_LOST,
        }
    }
}

/// Waits for every point or the first satisfied point across local devices.
///
/// # Safety
/// `points` must contain `count` readable live points, and `satisfied_index`
/// must be writable when `mode` is `NISYROS_WAIT_ANY`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_wait_many(
    count: u32,
    points: *const Point,
    mode: u32,
    timeout_ns: u64,
    satisfied_index: *mut u32,
) -> i32 {
    if count == 0
        || count > 4096
        || points.is_null()
        || mode > 1
        || (mode == 1 && satisfied_index.is_null())
    {
        return INVALID_ARGUMENT;
    }
    let points = unsafe { std::slice::from_raw_parts(points, count as usize) };
    if points.iter().any(|point| point.event.is_null()) {
        return INVALID_ARGUMENT;
    }
    let started = Instant::now();
    loop {
        let mut satisfied = 0;
        let mut first = None;
        let mut failure = 0;
        for (index, point) in points.iter().enumerate() {
            let event = unsafe { &*point.event };
            let event_failure = event.failure.load(Ordering::Acquire);
            if event_failure != 0 {
                failure = event_failure;
                continue;
            }
            match event.timeline.query() {
                Ok(value) if value >= point.value => {
                    satisfied += 1;
                    first.get_or_insert(index as u32);
                }
                Ok(_) => {}
                Err(_) => failure = DEVICE_LOST,
            }
        }
        if (mode == 0 && satisfied == count) || (mode == 1 && first.is_some()) {
            if !satisfied_index.is_null() {
                unsafe { *satisfied_index = first.unwrap_or(u32::MAX) };
            }
            return SUCCESS;
        }
        if failure != 0 {
            return failure;
        }
        let elapsed = started.elapsed().as_nanos();
        if timeout_ns != u64::MAX && elapsed >= u128::from(timeout_ns) {
            return TIMEOUT;
        }
        let remaining = if timeout_ns == u64::MAX {
            u64::MAX
        } else {
            timeout_ns - elapsed as u64
        };
        std::thread::sleep(Duration::from_nanos(remaining.min(1_000_000)));
    }
}

/// Queries the completed value of an event counter.
///
/// # Safety
/// `event` must be live and `value` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_event_query(event: *mut Event, value: *mut u64) -> i32 {
    if event.is_null() || value.is_null() {
        return INVALID_ARGUMENT;
    }
    let event = unsafe { &*event };
    let failure = event.failure.load(Ordering::Acquire);
    if failure != 0 {
        return failure;
    }
    match event.timeline.query() {
        Ok(completed) => {
            unsafe { *value = completed };
            SUCCESS
        }
        Err(_) => DEVICE_LOST,
    }
}
