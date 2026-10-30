//! Built-in AMD XDNA provider using the Linux DRM accel UAPI.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BACKEND_XDNA: u32 = 3;
const DRM_IOCTL_VERSION: u64 = iowr(0, 64);
const DRM_IOCTL_AMDXDNA_GET_INFO: u64 = iowr(0x47, 16);
const DRM_IOCTL_AMDXDNA_CREATE_BO: u64 = iowr(0x43, 32);
const DRM_IOCTL_AMDXDNA_GET_BO_INFO: u64 = iowr(0x44, 48);
const DRM_IOCTL_AMDXDNA_SYNC_BO: u64 = iowr(0x45, 24);
const DRM_IOCTL_AMDXDNA_CREATE_HWCTX: u64 = iowr(0x40, 56);
const DRM_IOCTL_AMDXDNA_DESTROY_HWCTX: u64 = iowr(0x41, 8);
const DRM_IOCTL_AMDXDNA_CONFIG_HWCTX: u64 = iowr(0x42, 24);
const DRM_IOCTL_AMDXDNA_EXEC_CMD: u64 = iowr(0x46, 56);
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: u64 = iowr(0xca, 48);
const DRM_IOCTL_SYNCOBJ_DESTROY: u64 = iowr(0xc0, 8);
const DRM_IOCTL_PRIME_HANDLE_TO_FD: u64 = iowr(0x2d, 12);
const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = iowr(0x2e, 12);
const DRM_IOCTL_GEM_CLOSE: u64 = iow(0x09, 8);
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;
const HEAP_BYTES: usize = 64 * 1024 * 1024;

const fn iowr(number: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | (b'd' as u64) << 8 | number
}

const fn iow(number: u64, size: u64) -> u64 {
    (1 << 30) | (size << 16) | (b'd' as u64) << 8 | number
}

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, length: usize) -> i32;
    fn lseek(fd: i32, offset: i64, whence: i32) -> i64;
}

#[repr(C)]
#[derive(Default)]
struct DrmVersion {
    major: i32,
    minor: i32,
    patchlevel: i32,
    name_len: usize,
    name: *mut c_char,
    date_len: usize,
    date: *mut c_char,
    desc_len: usize,
    desc: *mut c_char,
}

#[repr(C)]
struct GetInfo {
    param: u32,
    buffer_size: u32,
    buffer: u64,
}

#[repr(C)]
struct CreateBo {
    flags: u64,
    vaddr: u64,
    size: u64,
    kind: u32,
    handle: u32,
}

#[repr(C)]
struct BoInfo {
    ext: u64,
    ext_flags: u64,
    handle: u32,
    pad: u32,
    map_offset: u64,
    vaddr: u64,
    xdna_addr: u64,
}

#[repr(C)]
struct SyncBo {
    handle: u32,
    direction: u32,
    offset: u64,
    size: u64,
}

#[repr(C)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct GemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct Qos {
    gops: u32,
    fps: u32,
    dma_bandwidth: u32,
    latency: u32,
    frame_exec_time: u32,
    priority: u32,
}

#[repr(C)]
#[derive(Default)]
struct CreateHwctx {
    ext: u64,
    ext_flags: u64,
    qos_p: u64,
    umq_bo: u32,
    log_buf_bo: u32,
    max_opc: u32,
    num_tiles: u32,
    mem_size: u32,
    umq_doorbell: u32,
    handle: u32,
    syncobj_handle: u32,
}

#[repr(C)]
struct DestroyHwctx {
    handle: u32,
    pad: u32,
}

#[repr(C)]
struct ConfigHwctx {
    handle: u32,
    param_type: u32,
    param_val: u64,
    param_val_size: u32,
    pad: u32,
}

#[repr(C)]
struct CuConfig {
    num_cus: u16,
    pad: [u16; 3],
    cu_bo: u32,
    cu_func: u8,
    reserved: [u8; 3],
}

#[repr(C)]
struct SyncobjDestroy {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct ExecCmd {
    ext: u64,
    ext_flags: u64,
    hwctx: u32,
    kind: u32,
    cmd_handles: u64,
    args: u64,
    cmd_count: u32,
    arg_count: u32,
    seq: u64,
}

#[repr(C)]
struct SyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline_nsec: u64,
}

struct XdnaImage {
    columns: u32,
    operations_per_cycle: u32,
    cu_func: u8,
    pdi: Vec<u8>,
}

fn range(bytes: &[u8], offset: usize, length: usize) -> Option<&[u8]> {
    bytes.get(offset..offset.checked_add(length)?)
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        range(bytes, offset, 2)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        range(bytes, offset, 4)?.try_into().ok()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        range(bytes, offset, 8)?.try_into().ok()?,
    ))
}

fn section(bytes: &[u8], kind: u32) -> Option<&[u8]> {
    if range(bytes, 0, 8)? != b"xclbin2\0" || u64_at(bytes, 304)? != bytes.len() as u64 {
        return None;
    }
    let count = usize::try_from(u32_at(bytes, 448)?).ok()?;
    if count > 256 || 456usize.checked_add(count.checked_mul(40)?)? > bytes.len() {
        return None;
    }
    for index in 0..count {
        let header = 456 + 40 * index;
        if u32_at(bytes, header)? == kind {
            let offset = usize::try_from(u64_at(bytes, header + 24)?).ok()?;
            let length = usize::try_from(u64_at(bytes, header + 32)?).ok()?;
            return range(bytes, offset, length);
        }
    }
    None
}

fn parse_xclbin(bytes: &[u8], entry: &str) -> Option<XdnaImage> {
    let layout = section(bytes, 8)?;
    let partition = section(bytes, 32)?;
    if partition.len() < 184 {
        return None;
    }
    let columns = u16_at(partition, 32)? as u32;
    let operations_per_cycle = u32_at(partition, 8)?;
    if columns == 0 || columns > 64 {
        return None;
    }
    let count = usize::try_from(u32_at(layout, 0)?).ok()?;
    if count > 256 || 8usize.checked_add(count.checked_mul(80)?)? > layout.len() {
        return None;
    }
    let mut kernel = None;
    for index in 0..count {
        let offset = 8 + 80 * index;
        if u32_at(layout, offset)? != 7 {
            continue;
        }
        let name = range(layout, offset + 16, 64)?;
        let name = std::str::from_utf8(name.split(|byte| *byte == 0).next()?).ok()?;
        if name == entry || name.split(':').next()? == entry {
            let properties = u32_at(layout, offset + 4)?;
            kernel = Some(((properties >> 16) & 0xfff, ((properties >> 4) & 3) as u8));
            break;
        }
    }
    let (kernel_id, cu_func) = kernel?;
    let pdi_count = usize::try_from(u32_at(partition, 120)?).ok()?;
    let pdi_offset = usize::try_from(u32_at(partition, 124)?).ok()?;
    if pdi_count == 0 || pdi_count > 64 {
        return None;
    }
    for index in 0..pdi_count {
        let pdi = pdi_offset.checked_add(index.checked_mul(96)?)?;
        range(partition, pdi, 96)?;
        let image_size = usize::try_from(u32_at(partition, pdi + 16)?).ok()?;
        let image_offset = usize::try_from(u32_at(partition, pdi + 20)?).ok()?;
        let image = range(partition, image_offset, image_size)?;
        let group_count = usize::try_from(u32_at(partition, pdi + 24)?).ok()?;
        let group_offset = usize::try_from(u32_at(partition, pdi + 28)?).ok()?;
        if group_count > 128 {
            return None;
        }
        for group_index in 0..group_count {
            let group = group_offset.checked_add(group_index.checked_mul(96)?)?;
            range(partition, group, 96)?;
            let kernel_count = usize::try_from(u32_at(partition, group + 16)?).ok()?;
            let kernel_offset = usize::try_from(u32_at(partition, group + 20)?).ok()?;
            if kernel_count > 256 {
                return None;
            }
            for kernel_index in 0..kernel_count {
                let offset = kernel_offset.checked_add(kernel_index.checked_mul(8)?)?;
                if u64_at(partition, offset)? == u64::from(kernel_id) {
                    return Some(XdnaImage {
                        columns,
                        operations_per_cycle,
                        cu_func,
                        pdi: image.to_vec(),
                    });
                }
            }
        }
    }
    None
}

#[derive(Clone)]
struct XdnaInfo {
    path: PathBuf,
    device_id: u32,
    revision: u32,
    memory_bytes: u64,
    name: [c_char; 256],
}

struct XdnaInstance {
    devices: Vec<XdnaInfo>,
}

fn read_hex(path: PathBuf) -> Option<u32> {
    let value = fs::read_to_string(path).ok()?;
    u32::from_str_radix(value.trim().trim_start_matches("0x"), 16).ok()
}

fn device_version(file: &File) -> Option<(i32, i32)> {
    let mut name = [0i8; 64];
    let mut version = DrmVersion {
        name_len: name.len(),
        name: name.as_mut_ptr(),
        ..Default::default()
    };
    let status = unsafe { ioctl(file.as_raw_fd(), DRM_IOCTL_VERSION, &mut version) };
    if status != 0 || !name.starts_with(&[b'a' as i8, b'm' as i8, b'd' as i8, b'x' as i8]) {
        return None;
    }
    Some((version.major, version.minor))
}

fn memory_bytes() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|data| {
            data.lines()
                .find_map(|line| line.strip_prefix("MemTotal:")?.split_whitespace().next())
                .and_then(|number| number.parse::<u64>().ok())
        })
        .and_then(|kib| kib.checked_mul(1024))
        .unwrap_or(1)
}

fn inventory() -> Vec<XdnaInfo> {
    let Ok(entries) = fs::read_dir("/sys/class/accel") else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("accel") || !name[5..].bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let sysfs = entry.path().join("device");
        let (Some(0x1022), Some(device_id), Some(revision)) = (
            read_hex(sysfs.join("vendor")),
            read_hex(sysfs.join("device")),
            read_hex(sysfs.join("revision")),
        ) else {
            continue;
        };
        let path = PathBuf::from("/dev/accel").join(name);
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        if device_version(&file) != Some((0, 10)) {
            continue;
        }
        let mut version = [0u32; 2];
        let mut query = GetInfo {
            param: 2,
            buffer_size: std::mem::size_of_val(&version) as u32,
            buffer: version.as_mut_ptr() as u64,
        };
        if unsafe { ioctl(file.as_raw_fd(), DRM_IOCTL_AMDXDNA_GET_INFO, &mut query) } != 0 {
            continue;
        }
        let mut display_name = [0; 256];
        for (dst, src) in display_name.iter_mut().zip(b"AMD XDNA\0") {
            *dst = *src as c_char;
        }
        devices.push(XdnaInfo {
            path,
            device_id,
            revision,
            memory_bytes: memory_bytes(),
            name: display_name,
        });
    }
    devices.sort_by(|left, right| left.path.cmp(&right.path));
    devices
}

pub(super) unsafe extern "C" fn xdna_plugin_get_api(
    abi_version: u32,
    services: *const PluginServices,
    table_size: u32,
    table: *mut c_void,
) -> i32 {
    if table.is_null() || table_size < 8 || !valid_services(services) {
        return INVALID_ARGUMENT;
    }
    let size = std::mem::size_of::<PluginApi>();
    unsafe { ptr::write_bytes(table.cast::<u8>(), 0, (table_size as usize).min(size)) };
    if abi_version != PLUGIN_ABI_VERSION {
        return UNSUPPORTED;
    }
    unsafe {
        ptr::write(table.cast::<u32>(), size as u32);
        ptr::write(table.cast::<u32>().add(1), PLUGIN_ABI_VERSION);
    }
    if table_size as usize != size {
        return INCOMPLETE;
    }
    unsafe {
        *table.cast::<PluginApi>() = PluginApi {
            struct_size: size as u32,
            abi_version: PLUGIN_ABI_VERSION,
            instance_open: Some(instance_open),
            instance_close: Some(instance_close),
            device_count: Some(device_count),
            device_info: Some(device_info),
            queue_family_info: Some(queue_family_info),
            memory_heap_info: Some(memory_heap_info),
            cooperative_matrix: Some(cooperative_matrix),
            device_open: Some(device_open),
            device_close: Some(device_close),
            allocation_create: Some(allocation_create),
            allocation_destroy: Some(allocation_destroy),
            allocation_map: Some(allocation_map),
            allocation_unmap: Some(allocation_unmap),
            allocation_visibility: Some(allocation_visibility),
            allocation_register_host: Some(allocation_register_host),
            allocation_export_fd: Some(allocation_export_fd),
            allocation_import_fd: Some(allocation_import_fd),
            device_wait_idle: Some(device_wait_idle),
            timeline_create: Some(timeline_create),
            timeline_import_fd: Some(timeline_import_fd),
            timeline_destroy: Some(timeline_destroy),
            timeline_query: Some(timeline_query),
            timeline_wait: Some(timeline_wait),
            timeline_signal: Some(timeline_signal),
            timeline_export_fd: Some(timeline_export_fd),
            pipeline_import: Some(pipeline_import),
            pipeline_destroy: Some(pipeline_destroy),
            query_create: Some(query_create),
            query_destroy: Some(query_destroy),
            command_create: Some(command_create),
            command_destroy: Some(command_destroy),
            command_record: Some(command_record),
            queue_submit: Some(queue_submit),
            instance_native_handles: Some(instance_native_handles),
            device_native_handles: Some(device_native_handles),
        };
    }
    SUCCESS
}

unsafe extern "C" fn instance_open(_: *const PluginServices, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let devices = inventory();
    if devices.is_empty() {
        return NO_DEVICE;
    }
    unsafe { *out = Box::into_raw(Box::new(XdnaInstance { devices })).cast() };
    SUCCESS
}

unsafe extern "C" fn instance_close(instance: *mut c_void) {
    if !instance.is_null() {
        unsafe { drop(Box::from_raw(instance.cast::<XdnaInstance>())) };
    }
}

unsafe extern "C" fn device_count(instance: *mut c_void, out: *mut u32) -> i32 {
    if instance.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = (*instance.cast::<XdnaInstance>()).devices.len() as u32 };
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
    let Some(device) = (unsafe { &*instance.cast::<XdnaInstance>() })
        .devices
        .get(index as usize)
    else {
        return INVALID_ARGUMENT;
    };
    let out = unsafe { &mut *out };
    if out.abi_version != PLUGIN_ABI_VERSION
        || out.struct_size as usize != std::mem::size_of::<PluginDeviceInfo>()
    {
        return INVALID_ARGUMENT;
    }
    out.backend = BACKEND_XDNA;
    out.api_version = 10;
    out.vendor_id = 0x1022;
    out.device_id = device.device_id;
    out.device_type = 5;
    out.driver_id = device.revision;
    out.strict_profile_supported = 1;
    out.queue_family_count = 1;
    out.memory_heap_count = 1;
    out.cooperative_matrix_count = 0;
    out.features = 0;
    out.external_host_alignment = 0;
    out.cooperative_matrix_revision = 0;
    out.shader_bfloat16_revision = 0;
    out.shader_enqueue_revision = 0;
    out.cooperative_matrix_stages = 0;
    out.max_grid = [1, 1, 1];
    out.peer_memory_fd = 1;
    out.peer_timeline_fd = 0;
    out.timestamp_period_ns = 0.0;
    out.name = device.name;
    out.device_uuid = [0; 16];
    out.driver_uuid = [0; 16];
    out.driver_name = [0; 256];
    for (dst, src) in out.driver_name.iter_mut().zip(b"amdxdna DRM\0") {
        *dst = *src as c_char;
    }
    SUCCESS
}

unsafe extern "C" fn queue_family_info(
    instance: *mut c_void,
    index: u32,
    family: u32,
    out: *mut PluginQueueFamilyInfo,
) -> i32 {
    if instance.is_null()
        || out.is_null()
        || family != 0
        || (index as usize) >= unsafe { &*instance.cast::<XdnaInstance>() }.devices.len()
    {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = PluginQueueFamilyInfo {
            flags: 6,
            count: 1,
            timestamp_valid_bits: 0,
        }
    };
    SUCCESS
}

unsafe extern "C" fn memory_heap_info(
    instance: *mut c_void,
    index: u32,
    heap: u32,
    out: *mut PluginMemoryHeapInfo,
) -> i32 {
    if instance.is_null() || out.is_null() || heap != 0 {
        return INVALID_ARGUMENT;
    }
    let Some(device) = (unsafe { &*instance.cast::<XdnaInstance>() })
        .devices
        .get(index as usize)
    else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *out = PluginMemoryHeapInfo {
            bytes: device.memory_bytes,
            flags: 1,
            reserved: 0,
        }
    };
    SUCCESS
}

unsafe extern "C" fn cooperative_matrix(
    _: *mut c_void,
    _: u32,
    _: u32,
    _: *mut CooperativeMatrixTuple,
) -> i32 {
    UNSUPPORTED
}

struct XdnaDevice {
    file: File,
    sender: mpsc::Sender<XdnaWork>,
    worker: Option<JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    heap: Mutex<Option<XdnaAllocation>>,
    allocations: Arc<Mutex<BTreeMap<u64, AllocationSpan>>>,
    imported: Mutex<BTreeMap<u32, Weak<ImportedBacking>>>,
}

struct ImportedBacking {
    file: File,
    handle: u32,
    address: u64,
    map: usize,
    map_bytes: usize,
}

impl Drop for ImportedBacking {
    fn drop(&mut self) {
        unsafe {
            munmap(self.map as *mut c_void, self.map_bytes);
            close_bo(self.file.as_raw_fd(), self.handle);
        }
    }
}

#[derive(Clone, Copy)]
struct AllocationSpan {
    bytes: u64,
    handle: u32,
    host: usize,
}

struct XdnaPipeline {
    file: File,
    context: u32,
    syncobj: u32,
    pdi_bo: u32,
    argument_size: u32,
    argument_fields: Vec<package::ArgumentField>,
}

impl Drop for XdnaPipeline {
    fn drop(&mut self) {
        let fd = self.file.as_raw_fd();
        let mut context = DestroyHwctx {
            handle: self.context,
            pad: 0,
        };
        unsafe {
            ioctl(fd, DRM_IOCTL_AMDXDNA_DESTROY_HWCTX, &mut context);
            close_bo(fd, self.pdi_bo);
            let mut syncobj = SyncobjDestroy {
                handle: self.syncobj,
                pad: 0,
            };
            ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut syncobj);
        }
    }
}

struct TimelineState {
    value: u64,
    failure: i32,
}

type TimelineShared = Arc<(Mutex<TimelineState>, Condvar)>;

struct XdnaTimeline {
    shared: TimelineShared,
}

#[derive(Clone)]
enum XdnaOp {
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
    Barrier,
    Dispatch {
        pipeline: usize,
        grid: [u32; 3],
        arguments: u64,
    },
}

struct XdnaCommand {
    operations: Vec<XdnaOp>,
}

struct XdnaSegment {
    waits: Vec<(TimelineShared, u64)>,
    operations: Vec<XdnaOp>,
    signals: Vec<(TimelineShared, u64)>,
}

enum XdnaWork {
    Submit(Vec<XdnaSegment>),
    Synchronize(mpsc::Sender<()>),
    Shutdown,
}

fn signal(shared: &TimelineShared, value: u64) -> i32 {
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    if value <= state.value {
        return INVALID_ARGUMENT;
    }
    state.value = value;
    changed.notify_all();
    SUCCESS
}

fn fail(shared: &TimelineShared, status: i32) {
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    state.failure = status;
    changed.notify_all();
}

fn wait_value(shared: &TimelineShared, value: u64, timeout_ns: u64) -> Result<bool, i32> {
    let deadline = (timeout_ns != u64::MAX)
        .then(|| Instant::now().checked_add(Duration::from_nanos(timeout_ns)))
        .flatten();
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if state.failure != 0 {
            return Err(state.failure);
        }
        if state.value >= value {
            return Ok(true);
        }
        if deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Ok(false);
        }
        let duration = deadline
            .map(|limit| limit.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(1));
        state = changed
            .wait_timeout(state, duration)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

fn allocation_view(
    allocations: &BTreeMap<u64, AllocationSpan>,
    address: u64,
    bytes: u64,
) -> Option<(u32, *mut u8)> {
    let (&base, span) = allocations.range(..=address).next_back()?;
    let end = address.checked_add(bytes)?;
    (end <= base.checked_add(span.bytes)?).then(|| {
        (
            span.handle,
            (span.host + (address - base) as usize) as *mut u8,
        )
    })
}

fn register_span(
    allocations: &mut BTreeMap<u64, AllocationSpan>,
    address: u64,
    span: AllocationSpan,
) -> Result<(), i32> {
    let end = address.checked_add(span.bytes).ok_or(INVALID_ARGUMENT)?;
    if allocations
        .range(..end)
        .next_back()
        .is_some_and(|(&base, existing)| {
            base.checked_add(existing.bytes)
                .is_none_or(|last| last > address)
        })
    {
        return Err(INVALID_ARGUMENT);
    }
    allocations.insert(address, span);
    Ok(())
}

unsafe fn dispatch(
    file: &File,
    allocations: &Arc<Mutex<BTreeMap<u64, AllocationSpan>>>,
    pipeline: usize,
    grid: [u32; 3],
    arguments: u64,
) -> i32 {
    if pipeline == 0 || grid != [1, 1, 1] || arguments == 0 {
        return INVALID_ARGUMENT;
    }
    let pipeline = unsafe { &*(pipeline as *const XdnaPipeline) };
    let size = pipeline.argument_size as usize;
    if !(16..=4080).contains(&size) || !size.is_multiple_of(4) {
        return INCOMPATIBLE_BINARY;
    }
    let allocations = allocations.lock().unwrap_or_else(|e| e.into_inner());
    let Some((argument_handle, argument_host)) =
        allocation_view(&allocations, arguments, size as u64)
    else {
        return INVALID_ARGUMENT;
    };
    let block = unsafe { std::slice::from_raw_parts(argument_host, size) };
    let (Some(instruction), Some(instruction_bytes), Some(property_count)) =
        (u64_at(block, 0), u32_at(block, 8), u32_at(block, 12))
    else {
        return INVALID_ARGUMENT;
    };
    if instruction == 0 || instruction_bytes == 0 || property_count as usize > (size - 16) / 4 {
        return INVALID_ARGUMENT;
    }
    let Some((instruction_handle, _)) =
        allocation_view(&allocations, instruction, instruction_bytes as u64)
    else {
        return INVALID_ARGUMENT;
    };
    let mut handles = BTreeSet::from([argument_handle, instruction_handle]);
    for field in &pipeline.argument_fields {
        if field.kind != package::ARG_DEVICE_ADDRESS {
            continue;
        }
        let Some(address) = u64_at(block, field.offset as usize) else {
            return INVALID_ARGUMENT;
        };
        if address == 0 {
            continue;
        }
        let Some((handle, _)) = allocation_view(&allocations, address, 1) else {
            return INVALID_ARGUMENT;
        };
        handles.insert(handle);
    }
    let handles: Vec<u32> = handles.into_iter().collect();
    let fd = file.as_raw_fd();
    let mut command = CreateBo {
        flags: 0,
        vaddr: 0,
        size: 4096,
        kind: 4,
        handle: 0,
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_CREATE_BO, &mut command) } != 0 {
        return OUT_OF_MEMORY;
    }
    let mut info = BoInfo {
        ext: 0,
        ext_flags: 0,
        handle: command.handle,
        pad: 0,
        map_offset: 0,
        vaddr: 0,
        xdna_addr: 0,
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_GET_BO_INFO, &mut info) } != 0
        || info.map_offset == u64::MAX
    {
        unsafe { close_bo(fd, command.handle) };
        return DEVICE_LOST;
    }
    let map = unsafe {
        mmap(
            ptr::null_mut(),
            4096,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd,
            info.map_offset as i64,
        )
    };
    if map == usize::MAX as *mut c_void {
        unsafe { close_bo(fd, command.handle) };
        return OUT_OF_MEMORY;
    }
    let payload_words = (size - 16) / 4;
    let count = 1 + 4 + 2 + payload_words;
    if count > 2047 {
        unsafe {
            munmap(map, 4096);
            close_bo(fd, command.handle);
        }
        return INVALID_ARGUMENT;
    }
    unsafe {
        ptr::write_bytes(map, 0, 4096);
        ptr::write(
            map.cast::<u32>(),
            1 | ((count as u32) << 12) | (20 << 23) | (3 << 28),
        );
        ptr::write(map.cast::<u32>().add(1), 1);
        ptr::write_unaligned(map.cast::<u64>().byte_add(8), instruction);
        ptr::write(map.cast::<u32>().add(4), instruction_bytes);
        ptr::write(map.cast::<u32>().add(5), property_count);
        let property_bytes = property_count as usize * 4;
        ptr::copy_nonoverlapping(
            block.as_ptr().add(16),
            map.cast::<u8>().add(24),
            property_bytes,
        );
        ptr::write_unaligned(map.cast::<u8>().add(24 + property_bytes).cast::<u64>(), 3);
        ptr::copy_nonoverlapping(
            block.as_ptr().add(16 + property_bytes),
            map.cast::<u8>().add(32 + property_bytes),
            size - 16 - property_bytes,
        );
    }
    let mut exec = ExecCmd {
        hwctx: pipeline.context,
        cmd_handles: u64::from(command.handle),
        args: handles.as_ptr() as u64,
        cmd_count: 1,
        arg_count: handles.len() as u32,
        ..ExecCmd::default()
    };
    let status = if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_EXEC_CMD, &mut exec) } != 0 {
        SUBMISSION_FAILED
    } else {
        let mut wait = SyncobjTimelineWait {
            handles: (&raw const pipeline.syncobj) as u64,
            points: (&raw const exec.seq) as u64,
            timeout_nsec: i64::MAX,
            count_handles: 1,
            flags: 2,
            first_signaled: 0,
            pad: 0,
            deadline_nsec: 0,
        };
        if unsafe { ioctl(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut wait) } != 0 {
            SUBMISSION_FAILED
        } else if unsafe { ptr::read_volatile(map.cast::<u32>()) } & 0xf == 4 {
            SUCCESS
        } else {
            SUBMISSION_FAILED
        }
    };
    unsafe {
        munmap(map, 4096);
        close_bo(fd, command.handle);
    }
    status
}

fn queue_worker(
    receiver: mpsc::Receiver<XdnaWork>,
    file: File,
    allocations: Arc<Mutex<BTreeMap<u64, AllocationSpan>>>,
    cancelled: Arc<AtomicBool>,
) {
    while let Ok(work) = receiver.recv() {
        match work {
            XdnaWork::Submit(segments) => {
                let all_signals: Vec<_> = segments
                    .iter()
                    .flat_map(|segment| {
                        segment
                            .signals
                            .iter()
                            .map(|(timeline, _)| Arc::clone(timeline))
                    })
                    .collect();
                let mut status = SUCCESS;
                for segment in segments {
                    for (timeline, value) in segment.waits {
                        loop {
                            if cancelled.load(Ordering::Acquire) {
                                status = DEVICE_LOST;
                                break;
                            }
                            match wait_value(&timeline, value, 1_000_000) {
                                Ok(true) => break,
                                Ok(false) => continue,
                                Err(error) => {
                                    status = error;
                                    break;
                                }
                            }
                        }
                        if status != SUCCESS {
                            break;
                        }
                    }
                    if status != SUCCESS {
                        break;
                    }
                    for operation in segment.operations {
                        status = match operation {
                            XdnaOp::Copy { dst, src, bytes } => {
                                let mapped = allocations.lock().unwrap_or_else(|e| e.into_inner());
                                match (
                                    allocation_view(&mapped, dst, bytes),
                                    allocation_view(&mapped, src, bytes),
                                    usize::try_from(bytes),
                                ) {
                                    (Some((_, destination)), Some((_, source)), Ok(length)) => {
                                        unsafe { ptr::copy(source, destination, length) };
                                        SUCCESS
                                    }
                                    _ => INVALID_ARGUMENT,
                                }
                            }
                            XdnaOp::Fill {
                                dst,
                                bytes,
                                pattern,
                            } => {
                                let mapped = allocations.lock().unwrap_or_else(|e| e.into_inner());
                                match (
                                    allocation_view(&mapped, dst, bytes),
                                    usize::try_from(bytes / 4),
                                ) {
                                    (Some((_, destination)), Ok(count))
                                        if bytes.is_multiple_of(4) =>
                                    {
                                        for index in 0..count {
                                            unsafe {
                                                ptr::write_unaligned(
                                                    destination.cast::<u32>().add(index),
                                                    pattern,
                                                )
                                            };
                                        }
                                        SUCCESS
                                    }
                                    _ => INVALID_ARGUMENT,
                                }
                            }
                            XdnaOp::Barrier => SUCCESS,
                            XdnaOp::Dispatch {
                                pipeline,
                                grid,
                                arguments,
                            } => unsafe {
                                dispatch(&file, &allocations, pipeline, grid, arguments)
                            },
                        };
                        if status != SUCCESS {
                            break;
                        }
                    }
                    if status != SUCCESS {
                        break;
                    }
                    for (timeline, value) in segment.signals {
                        status = signal(&timeline, value);
                        if status != SUCCESS {
                            break;
                        }
                    }
                    if status != SUCCESS {
                        break;
                    }
                }
                if status != SUCCESS {
                    for timeline in all_signals {
                        fail(&timeline, status);
                    }
                }
            }
            XdnaWork::Synchronize(done) => {
                let _ = done.send(());
            }
            XdnaWork::Shutdown => break,
        }
    }
}

struct XdnaAllocation {
    handle: u32,
    address: u64,
    bytes: u64,
    map: *mut c_void,
    map_bytes: usize,
    view_offset: usize,
    owns_map: bool,
    imported: Option<Arc<ImportedBacking>>,
}

unsafe fn close_bo(fd: i32, handle: u32) {
    let mut arg = GemClose { handle, pad: 0 };
    unsafe { ioctl(fd, DRM_IOCTL_GEM_CLOSE, &mut arg) };
}

unsafe fn map_bo(fd: i32, handle: u32, bytes: u64) -> Result<XdnaAllocation, i32> {
    unsafe { map_bo_aligned(fd, handle, bytes, 4096) }
}

unsafe fn map_bo_aligned(
    fd: i32,
    handle: u32,
    bytes: u64,
    alignment: usize,
) -> Result<XdnaAllocation, i32> {
    let mut info = BoInfo {
        ext: 0,
        ext_flags: 0,
        handle,
        pad: 0,
        map_offset: 0,
        vaddr: 0,
        xdna_addr: 0,
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_GET_BO_INFO, &mut info) } != 0
        || info.map_offset == u64::MAX
    {
        return Err(DEVICE_LOST);
    }
    let map_bytes = usize::try_from(bytes.checked_add(4095).ok_or(OUT_OF_MEMORY)? & !4095)
        .map_err(|_| OUT_OF_MEMORY)?;
    let Ok(offset) = i64::try_from(info.map_offset) else {
        return Err(DEVICE_LOST);
    };
    let map = if alignment <= 4096 {
        unsafe {
            mmap(
                ptr::null_mut(),
                map_bytes,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                offset,
            )
        }
    } else {
        let reserve_bytes = map_bytes.checked_add(alignment).ok_or(OUT_OF_MEMORY)?;
        let reserved = unsafe {
            mmap(
                ptr::null_mut(),
                reserve_bytes,
                0,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if reserved == usize::MAX as *mut c_void {
            return Err(OUT_OF_MEMORY);
        }
        let aligned = (reserved as usize + alignment - 1) & !(alignment - 1);
        let map = unsafe {
            mmap(
                aligned as *mut c_void,
                map_bytes,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_FIXED,
                fd,
                offset,
            )
        };
        if map == usize::MAX as *mut c_void {
            unsafe { munmap(reserved, reserve_bytes) };
            return Err(OUT_OF_MEMORY);
        }
        let prefix = aligned - reserved as usize;
        let suffix = alignment - prefix;
        if prefix != 0 {
            unsafe { munmap(reserved, prefix) };
        }
        if suffix != 0 {
            unsafe { munmap((aligned + map_bytes) as *mut c_void, suffix) };
        }
        map
    };
    if map == usize::MAX as *mut c_void {
        return Err(UNSUPPORTED);
    }
    info.vaddr = 0;
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_GET_BO_INFO, &mut info) } != 0
        || info.xdna_addr == u64::MAX
        || info.xdna_addr == 0
    {
        unsafe { munmap(map, map_bytes) };
        return Err(DEVICE_LOST);
    }
    Ok(XdnaAllocation {
        handle,
        address: info.xdna_addr,
        bytes,
        map,
        map_bytes,
        view_offset: 0,
        owns_map: true,
        imported: None,
    })
}

fn ensure_heap(device: &XdnaDevice) -> Result<(), i32> {
    let mut heap = device.heap.lock().unwrap_or_else(|e| e.into_inner());
    if heap.is_some() {
        return Ok(());
    }
    let fd = device.file.as_raw_fd();
    let mut bo = CreateBo {
        flags: 0,
        vaddr: 0,
        size: HEAP_BYTES as u64,
        kind: 2,
        handle: 0,
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_CREATE_BO, &mut bo) } != 0 {
        return Err(OUT_OF_MEMORY);
    }
    match unsafe { map_bo_aligned(fd, bo.handle, bo.size, HEAP_BYTES) } {
        Ok(allocation) => {
            *heap = Some(allocation);
            Ok(())
        }
        Err(status) => {
            unsafe { close_bo(fd, bo.handle) };
            Err(status)
        }
    }
}

unsafe extern "C" fn device_open(
    instance: *mut c_void,
    index: u32,
    count: u32,
    requests: *const PluginQueueRequest,
    out: *mut *mut c_void,
) -> i32 {
    if instance.is_null() || out.is_null() || requests.is_null() || count != 1 {
        return INVALID_ARGUMENT;
    }
    let Some(info) = (unsafe { &*instance.cast::<XdnaInstance>() })
        .devices
        .get(index as usize)
    else {
        return INVALID_ARGUMENT;
    };
    let request = unsafe { &*requests };
    if request.family != 0 || request.count != 1 {
        return INVALID_ARGUMENT;
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&info.path) else {
        return DEVICE_LOST;
    };
    let Ok(worker_file) = file.try_clone() else {
        return DEVICE_LOST;
    };
    let allocations = Arc::new(Mutex::new(BTreeMap::new()));
    let worker_allocations = Arc::clone(&allocations);
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker = std::thread::spawn(move || {
        queue_worker(receiver, worker_file, worker_allocations, worker_cancelled)
    });
    unsafe {
        *out = Box::into_raw(Box::new(XdnaDevice {
            file,
            sender,
            worker: Some(worker),
            cancelled,
            heap: Mutex::new(None),
            allocations,
            imported: Mutex::new(BTreeMap::new()),
        }))
        .cast()
    };
    SUCCESS
}

unsafe extern "C" fn device_close(device: *mut c_void) {
    if device.is_null() {
        return;
    }
    let mut device = unsafe { Box::from_raw(device.cast::<XdnaDevice>()) };
    device.cancelled.store(true, Ordering::Release);
    let _ = device.sender.send(XdnaWork::Shutdown);
    if let Some(worker) = device.worker.take() {
        let _ = worker.join();
    }
    if let Some(heap) = device.heap.lock().unwrap_or_else(|e| e.into_inner()).take() {
        unsafe {
            munmap(heap.map, heap.map_bytes);
            close_bo(device.file.as_raw_fd(), heap.handle);
        }
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
    let request = unsafe { &*request };
    if request.bytes == 0
        || request.bytes > (usize::MAX - 32767) as u64
        || !request.alignment.is_power_of_two()
        || request.alignment > 4096
        || request.required_properties & !3 != 0
        || !matches!(request.required_heap, 0 | u32::MAX)
        || request.queue_family_count != 1
        || request.queue_families.is_null()
        || unsafe { *request.queue_families } != 0
    {
        return UNSUPPORTED;
    }
    let device = unsafe { &*device.cast::<XdnaDevice>() };
    let kind = if request.required_properties & 1 != 0 {
        3
    } else {
        1
    };
    if kind == 3
        && let Err(status) = ensure_heap(device)
    {
        return status;
    }
    let fd = device.file.as_raw_fd();
    let mut bo = CreateBo {
        flags: 0,
        vaddr: 0,
        size: request.bytes,
        kind,
        handle: 0,
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_CREATE_BO, &mut bo) } != 0 {
        return OUT_OF_MEMORY;
    }
    let allocation = if kind == 3 {
        let mut bo_info = BoInfo {
            ext: 0,
            ext_flags: 0,
            handle: bo.handle,
            pad: 0,
            map_offset: 0,
            vaddr: 0,
            xdna_addr: 0,
        };
        if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_GET_BO_INFO, &mut bo_info) } != 0
            || bo_info.vaddr == u64::MAX
            || bo_info.xdna_addr == u64::MAX
        {
            unsafe { close_bo(fd, bo.handle) };
            return DEVICE_LOST;
        }
        XdnaAllocation {
            handle: bo.handle,
            address: bo_info.xdna_addr,
            bytes: request.bytes,
            map: bo_info.vaddr as *mut c_void,
            map_bytes: (request.bytes as usize + 32767) & !32767,
            view_offset: 0,
            owns_map: false,
            imported: None,
        }
    } else {
        match unsafe { map_bo(fd, bo.handle, request.bytes) } {
            Ok(allocation) => allocation,
            Err(status) => {
                unsafe { close_bo(fd, bo.handle) };
                return status;
            }
        }
    };
    let mut allocations = device.allocations.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(status) = register_span(
        &mut allocations,
        allocation.address,
        AllocationSpan {
            bytes: allocation.bytes,
            handle: allocation.handle,
            host: allocation.map as usize + allocation.view_offset,
        },
    ) {
        unsafe {
            if allocation.owns_map {
                munmap(allocation.map, allocation.map_bytes);
            }
            close_bo(fd, allocation.handle);
        }
        return status;
    }
    unsafe {
        *info = PluginAllocationInfo {
            base: allocation.address,
            bytes: request.bytes,
            backing_bytes: allocation.map_bytes as u64,
            heap_bytes: memory_bytes(),
            memory_type_index: 0,
            heap_index: 0,
            properties: 3,
            exclusive: request.exclusive,
        };
        *out = Box::into_raw(Box::new(allocation)).cast();
    }
    SUCCESS
}

unsafe extern "C" fn allocation_destroy(device: *mut c_void, allocation: *mut c_void) {
    if device.is_null() || allocation.is_null() {
        return;
    }
    let allocation = unsafe { Box::from_raw(allocation.cast::<XdnaAllocation>()) };
    unsafe { &*device.cast::<XdnaDevice>() }
        .allocations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&allocation.address);
    if allocation.imported.is_none() {
        unsafe {
            if allocation.owns_map {
                munmap(allocation.map, allocation.map_bytes);
            }
            close_bo(
                (*device.cast::<XdnaDevice>()).file.as_raw_fd(),
                allocation.handle,
            );
        }
    }
}

unsafe extern "C" fn allocation_map(
    _: *mut c_void,
    allocation: *mut c_void,
    address: u64,
    out: *mut *mut c_void,
) -> i32 {
    if allocation.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let allocation = unsafe { &*allocation.cast::<XdnaAllocation>() };
    let Some(offset) = address.checked_sub(allocation.address) else {
        return INVALID_ARGUMENT;
    };
    if offset >= allocation.bytes {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = allocation
            .map
            .byte_add(allocation.view_offset + offset as usize)
    };
    SUCCESS
}
unsafe extern "C" fn allocation_unmap(_: *mut c_void, _: *mut c_void) {}
unsafe extern "C" fn allocation_visibility(
    device: *mut c_void,
    allocation: *mut c_void,
    flush: u32,
) -> i32 {
    if device.is_null() || allocation.is_null() || flush > 1 {
        return INVALID_ARGUMENT;
    }
    let allocation = unsafe { &*allocation.cast::<XdnaAllocation>() };
    let mut arg = SyncBo {
        // Both directions flush CPU cache lines for unassigned shared BOs.
        // The 0.10 KMD rejects FROM_DEVICE on such BOs.
        handle: allocation.handle,
        direction: 0,
        offset: allocation.view_offset as u64,
        size: allocation.bytes,
    };
    if unsafe {
        ioctl(
            (*device.cast::<XdnaDevice>()).file.as_raw_fd(),
            DRM_IOCTL_AMDXDNA_SYNC_BO,
            &mut arg,
        )
    } == 0
    {
        SUCCESS
    } else {
        DEVICE_LOST
    }
}
unsafe extern "C" fn allocation_register_host(
    _: *mut c_void,
    _: *mut c_void,
    _: u64,
    _: *mut *mut c_void,
    _: *mut PluginAllocationInfo,
) -> i32 {
    UNSUPPORTED
}
unsafe extern "C" fn allocation_export_fd(
    device: *mut c_void,
    allocation: *mut c_void,
    out: *mut i32,
    out_offset: *mut u64,
) -> i32 {
    if device.is_null() || allocation.is_null() || out.is_null() || out_offset.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = -1;
        *out_offset = 0;
    };
    let allocation = unsafe { &*allocation.cast::<XdnaAllocation>() };
    if !allocation.owns_map {
        return UNSUPPORTED;
    }
    let mut arg = PrimeHandle {
        handle: allocation.handle,
        flags: 0x80002,
        fd: -1,
    };
    if unsafe {
        ioctl(
            (*device.cast::<XdnaDevice>()).file.as_raw_fd(),
            DRM_IOCTL_PRIME_HANDLE_TO_FD,
            &mut arg,
        )
    } != 0
    {
        return UNSUPPORTED;
    }
    unsafe { *out = arg.fd };
    SUCCESS
}

unsafe extern "C" fn allocation_import_fd(
    device: *mut c_void,
    _source: *mut c_void,
    offset: u64,
    bytes: u64,
    fd: i32,
    out: *mut *mut c_void,
    info: *mut PluginAllocationInfo,
) -> i32 {
    if fd < 0 {
        return INVALID_ARGUMENT;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if device.is_null() || out.is_null() || info.is_null() || bytes == 0 {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let length = unsafe { lseek(fd.as_raw_fd(), 0, 2) };
    if length <= 0
        || offset
            .checked_add(bytes)
            .is_none_or(|end| end > length as u64)
    {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<XdnaDevice>() };
    let device_fd = device.file.as_raw_fd();
    let mut arg = PrimeHandle {
        handle: 0,
        flags: 0,
        fd: fd.as_raw_fd(),
    };
    if unsafe { ioctl(device_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut arg) } != 0 {
        return UNSUPPORTED;
    }
    let mut imported = device.imported.lock().unwrap_or_else(|e| e.into_inner());
    let backing = if let Some(backing) = imported.get(&arg.handle).and_then(Weak::upgrade) {
        backing
    } else {
        let mapped = match unsafe { map_bo(device_fd, arg.handle, length as u64) } {
            Ok(allocation) => allocation,
            Err(status) => {
                unsafe { close_bo(device_fd, arg.handle) };
                return status;
            }
        };
        let Ok(file) = device.file.try_clone() else {
            unsafe {
                munmap(mapped.map, mapped.map_bytes);
                close_bo(device_fd, arg.handle);
            }
            return DEVICE_LOST;
        };
        let backing = Arc::new(ImportedBacking {
            file,
            handle: arg.handle,
            address: mapped.address,
            map: mapped.map as usize,
            map_bytes: mapped.map_bytes,
        });
        imported.insert(arg.handle, Arc::downgrade(&backing));
        backing
    };
    drop(imported);
    let Some(address) = backing.address.checked_add(offset) else {
        return INVALID_ARGUMENT;
    };
    let allocation = XdnaAllocation {
        handle: backing.handle,
        address,
        bytes,
        map: backing.map as *mut c_void,
        map_bytes: backing.map_bytes,
        view_offset: offset as usize,
        owns_map: false,
        imported: Some(backing),
    };
    let mut allocations = device.allocations.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(status) = register_span(
        &mut allocations,
        allocation.address,
        AllocationSpan {
            bytes: allocation.bytes,
            handle: allocation.handle,
            host: allocation.map as usize + allocation.view_offset,
        },
    ) {
        return status;
    }
    unsafe {
        *info = PluginAllocationInfo {
            base: allocation.address,
            bytes,
            backing_bytes: length as u64,
            heap_bytes: memory_bytes(),
            memory_type_index: 0,
            heap_index: 0,
            properties: 3,
            exclusive: 0,
        };
        *out = Box::into_raw(Box::new(allocation)).cast();
    }
    SUCCESS
}
unsafe extern "C" fn device_wait_idle(device: *mut c_void, _: *mut i32) -> i32 {
    if device.is_null() {
        return INVALID_ARGUMENT;
    }
    let (sender, receiver) = mpsc::channel();
    if unsafe { &*device.cast::<XdnaDevice>() }
        .sender
        .send(XdnaWork::Synchronize(sender))
        .is_err()
    {
        return DEVICE_LOST;
    }
    receiver.recv().map_or(DEVICE_LOST, |()| SUCCESS)
}

unsafe extern "C" fn timeline_create(
    device: *mut c_void,
    exportable: u32,
    out: *mut *mut c_void,
    _: *mut i32,
) -> i32 {
    if device.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    if exportable != 0 {
        return UNSUPPORTED;
    }
    let shared = Arc::new((
        Mutex::new(TimelineState {
            value: 0,
            failure: 0,
        }),
        Condvar::new(),
    ));
    unsafe { *out = Box::into_raw(Box::new(XdnaTimeline { shared })).cast() };
    SUCCESS
}

unsafe extern "C" fn timeline_import_fd(
    _: *mut c_void,
    fd: i32,
    _: *mut *mut c_void,
    _: *mut i32,
) -> i32 {
    if fd >= 0 {
        unsafe { drop(OwnedFd::from_raw_fd(fd)) };
    }
    UNSUPPORTED
}
unsafe extern "C" fn timeline_destroy(_: *mut c_void, timeline: *mut c_void) {
    if !timeline.is_null() {
        unsafe { drop(Box::from_raw(timeline.cast::<XdnaTimeline>())) };
    }
}
unsafe extern "C" fn timeline_query(
    timeline: *mut c_void,
    out: *mut u64,
    backend: *mut i32,
) -> i32 {
    if timeline.is_null() || out.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    let timeline = unsafe { &*timeline.cast::<XdnaTimeline>() };
    let state = timeline.shared.0.lock().unwrap_or_else(|e| e.into_inner());
    if state.failure != 0 {
        unsafe { *backend = state.failure };
        return SUBMISSION_FAILED;
    }
    unsafe { *out = state.value };
    SUCCESS
}
unsafe extern "C" fn timeline_wait(
    timeline: *mut c_void,
    value: u64,
    timeout_ns: u64,
    completed: *mut u32,
    backend: *mut i32,
) -> i32 {
    if timeline.is_null() || completed.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    let timeline = unsafe { &*timeline.cast::<XdnaTimeline>() };
    match wait_value(&timeline.shared, value, timeout_ns) {
        Ok(done) => {
            unsafe { *completed = u32::from(done) };
            SUCCESS
        }
        Err(error) => {
            unsafe { *backend = error };
            SUBMISSION_FAILED
        }
    }
}
unsafe extern "C" fn timeline_signal(timeline: *mut c_void, value: u64, _: *mut i32) -> i32 {
    if timeline.is_null() {
        return INVALID_ARGUMENT;
    }
    signal(&unsafe { &*timeline.cast::<XdnaTimeline>() }.shared, value)
}
unsafe extern "C" fn timeline_export_fd(_: *mut c_void, _: *mut i32, _: *mut i32) -> i32 {
    UNSUPPORTED
}
unsafe extern "C" fn pipeline_import(
    device: *mut c_void,
    image: *const u8,
    bytes: u64,
    out: *mut *mut c_void,
    backend: *mut i32,
) -> i32 {
    if device.is_null()
        || image.is_null()
        || out.is_null()
        || backend.is_null()
        || bytes == 0
        || bytes > 128 * 1024 * 1024
    {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let image = unsafe { std::slice::from_raw_parts(image, bytes as usize) };
    let Ok(package) = package::Package::parse(image) else {
        return INCOMPATIBLE_BINARY;
    };
    let expected = [
        (package::ARG_DEVICE_ADDRESS, 0, 8),
        (package::ARG_VALUE, 8, 4),
        (package::ARG_VALUE, 12, 4),
    ];
    let valid_prefix = package.argument_fields.len() >= expected.len()
        && package
            .argument_fields
            .iter()
            .take(expected.len())
            .zip(expected)
            .all(|(field, expected)| (field.kind, field.offset, field.width) == expected);
    if package.format != package::XDNA_XCLBIN_FORMAT
        || package.binaries.len() != 1
        || package.required_features != 0
        || !package.required_matrices.is_empty()
        || package.workgroup != [1, 1, 1]
        || package.argument_size < 16
        || package.argument_size > 4080
        || !package.argument_size.is_multiple_of(4)
        || !valid_prefix
    {
        return UNSUPPORTED;
    }
    let Some(native) = parse_xclbin(&package.binaries[0].data, &package.entry) else {
        return INCOMPATIBLE_BINARY;
    };
    if native.pdi.is_empty() {
        return INCOMPATIBLE_BINARY;
    }
    let device = unsafe { &*device.cast::<XdnaDevice>() };
    let fd = device.file.as_raw_fd();
    let Ok(file) = device.file.try_clone() else {
        return DEVICE_LOST;
    };
    let mut metadata = [0u8; 64];
    let mut query = GetInfo {
        param: 1,
        buffer_size: metadata.len() as u32,
        buffer: metadata.as_mut_ptr() as u64,
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_GET_INFO, &mut query) } != 0 {
        return DEVICE_LOST;
    }
    let (Some(total_columns), Some(core_rows)) = (u16_at(&metadata, 4), u16_at(&metadata, 16))
    else {
        return DEVICE_LOST;
    };
    if native.columns > u32::from(total_columns) || core_rows == 0 {
        return INCOMPATIBLE_BINARY;
    }
    if let Err(status) = ensure_heap(device) {
        return status;
    }
    let qos = Qos {
        priority: 0x200,
        ..Qos::default()
    };
    let mut context = CreateHwctx {
        qos_p: (&raw const qos) as u64,
        max_opc: native.operations_per_cycle,
        num_tiles: native.columns * u32::from(core_rows),
        ..CreateHwctx::default()
    };
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_CREATE_HWCTX, &mut context) } != 0 {
        unsafe { *backend = std::io::Error::last_os_error().raw_os_error().unwrap_or(0) };
        return INCOMPATIBLE_BINARY;
    }
    let mut pdi_bo = CreateBo {
        flags: 0,
        vaddr: 0,
        size: native.pdi.len() as u64,
        kind: 3,
        handle: 0,
    };
    let mut status = SUCCESS;
    if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_CREATE_BO, &mut pdi_bo) } != 0 {
        status = OUT_OF_MEMORY;
    } else {
        let mut info = BoInfo {
            ext: 0,
            ext_flags: 0,
            handle: pdi_bo.handle,
            pad: 0,
            map_offset: 0,
            vaddr: 0,
            xdna_addr: 0,
        };
        if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_GET_BO_INFO, &mut info) } != 0
            || info.vaddr == u64::MAX
        {
            status = DEVICE_LOST;
        } else {
            unsafe {
                ptr::copy_nonoverlapping(
                    native.pdi.as_ptr(),
                    info.vaddr as *mut u8,
                    native.pdi.len(),
                )
            };
            let mut sync = SyncBo {
                handle: pdi_bo.handle,
                direction: 0,
                offset: 0,
                size: pdi_bo.size,
            };
            if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_SYNC_BO, &mut sync) } != 0 {
                status = DEVICE_LOST;
            } else {
                let config = CuConfig {
                    num_cus: 1,
                    pad: [0; 3],
                    cu_bo: pdi_bo.handle,
                    cu_func: native.cu_func,
                    reserved: [0; 3],
                };
                let mut request = ConfigHwctx {
                    handle: context.handle,
                    param_type: 0,
                    param_val: (&raw const config) as u64,
                    param_val_size: std::mem::size_of::<CuConfig>() as u32,
                    pad: 0,
                };
                if unsafe { ioctl(fd, DRM_IOCTL_AMDXDNA_CONFIG_HWCTX, &mut request) } != 0 {
                    status = INCOMPATIBLE_BINARY;
                }
            }
        }
    }
    if status != SUCCESS {
        unsafe {
            *backend = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if pdi_bo.handle != 0 {
                close_bo(fd, pdi_bo.handle);
            }
            let mut destroy = DestroyHwctx {
                handle: context.handle,
                pad: 0,
            };
            ioctl(fd, DRM_IOCTL_AMDXDNA_DESTROY_HWCTX, &mut destroy);
            let mut syncobj = SyncobjDestroy {
                handle: context.syncobj_handle,
                pad: 0,
            };
            ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut syncobj);
        }
        return status;
    }
    unsafe {
        *out = Box::into_raw(Box::new(XdnaPipeline {
            file,
            context: context.handle,
            syncobj: context.syncobj_handle,
            pdi_bo: pdi_bo.handle,
            argument_size: package.argument_size,
            argument_fields: package.argument_fields,
        }))
        .cast();
    }
    SUCCESS
}

unsafe extern "C" fn pipeline_destroy(_: *mut c_void, pipeline: *mut c_void) {
    if !pipeline.is_null() {
        unsafe { drop(Box::from_raw(pipeline.cast::<XdnaPipeline>())) };
    }
}
unsafe extern "C" fn query_create(_: *mut c_void, _: *mut *mut c_void, _: *mut i32) -> i32 {
    UNSUPPORTED
}
unsafe extern "C" fn query_destroy(_: *mut c_void, _: *mut c_void) {}
unsafe extern "C" fn command_create(
    device: *mut c_void,
    family: u32,
    out: *mut *mut c_void,
    _: *mut i32,
) -> i32 {
    if device.is_null() || out.is_null() || family != 0 {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = Box::into_raw(Box::new(XdnaCommand {
            operations: Vec::new(),
        }))
        .cast()
    };
    SUCCESS
}
unsafe extern "C" fn command_destroy(_: *mut c_void, command: *mut c_void) {
    if !command.is_null() {
        unsafe { drop(Box::from_raw(command.cast::<XdnaCommand>())) };
    }
}
unsafe extern "C" fn command_record(
    command: *mut c_void,
    count: u32,
    ops: *const PluginCommandOp,
    _: *mut i32,
) -> i32 {
    if command.is_null() || (count != 0 && ops.is_null()) {
        return INVALID_ARGUMENT;
    }
    let mut decoded = Vec::new();
    let ops = if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ops, count as usize) }
    };
    for op in ops {
        let a = op.args;
        let value = match op.kind {
            1 => XdnaOp::Copy {
                dst: a[0],
                src: a[1],
                bytes: a[2],
            },
            2 => XdnaOp::Fill {
                dst: a[0],
                bytes: a[1],
                pattern: a[2] as u32,
            },
            3..=5 => XdnaOp::Barrier,
            6 => XdnaOp::Dispatch {
                pipeline: a[0] as usize,
                grid: [a[1] as u32, a[2] as u32, a[3] as u32],
                arguments: a[4],
            },
            _ => return UNSUPPORTED,
        };
        decoded.push(value);
    }
    unsafe { (*command.cast::<XdnaCommand>()).operations = decoded };
    SUCCESS
}

unsafe extern "C" fn queue_submit(
    device: *mut c_void,
    queue: u32,
    count: u32,
    input: *const PluginSubmitSegment,
    _: *mut i32,
) -> i32 {
    if device.is_null() || queue != 0 || (count != 0 && input.is_null()) {
        return INVALID_ARGUMENT;
    }
    let mut segments = Vec::new();
    let input = if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(input, count as usize) }
    };
    for input in input {
        if input.wait_count != 0 && input.waits.is_null()
            || input.signal_count != 0 && input.signals.is_null()
        {
            return INVALID_ARGUMENT;
        }
        let mut waits = Vec::new();
        let wait_points = if input.wait_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(input.waits, input.wait_count as usize) }
        };
        for point in wait_points {
            if point.timeline.is_null() {
                return INVALID_ARGUMENT;
            }
            waits.push((
                Arc::clone(&unsafe { &*point.timeline.cast::<XdnaTimeline>() }.shared),
                point.value,
            ));
        }
        let mut signals = Vec::new();
        let signal_points = if input.signal_count == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(input.signals, input.signal_count as usize) }
        };
        for point in signal_points {
            if point.timeline.is_null() {
                return INVALID_ARGUMENT;
            }
            signals.push((
                Arc::clone(&unsafe { &*point.timeline.cast::<XdnaTimeline>() }.shared),
                point.value,
            ));
        }
        let operations = if input.command.is_null() {
            Vec::new()
        } else {
            unsafe { &*input.command.cast::<XdnaCommand>() }
                .operations
                .clone()
        };
        segments.push(XdnaSegment {
            waits,
            operations,
            signals,
        });
    }
    if unsafe { &*device.cast::<XdnaDevice>() }
        .sender
        .send(XdnaWork::Submit(segments))
        .is_err()
    {
        DEVICE_LOST
    } else {
        SUCCESS
    }
}
unsafe extern "C" fn instance_native_handles(
    _: *mut c_void,
    _: u32,
    _: *mut *mut c_void,
    _: *mut *mut c_void,
) -> i32 {
    UNSUPPORTED
}
unsafe extern "C" fn device_native_handles(
    _: *mut c_void,
    _: u32,
    _: *mut *mut c_void,
    _: *mut *mut c_void,
) -> i32 {
    UNSUPPORTED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_close_cancels_an_unsatisfied_queue_wait() {
        let file = File::open("/dev/null").unwrap();
        let worker_file = file.try_clone().unwrap();
        let allocations = Arc::new(Mutex::new(BTreeMap::new()));
        let worker_allocations = Arc::clone(&allocations);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = mpsc::channel();
        let shared = Arc::new((
            Mutex::new(TimelineState {
                value: 0,
                failure: 0,
            }),
            Condvar::new(),
        ));
        sender
            .send(XdnaWork::Submit(vec![XdnaSegment {
                waits: vec![(Arc::clone(&shared), 1)],
                operations: Vec::new(),
                signals: vec![(Arc::clone(&shared), 2)],
            }]))
            .unwrap();
        let worker = std::thread::spawn(move || {
            queue_worker(receiver, worker_file, worker_allocations, worker_cancelled)
        });
        let device = Box::into_raw(Box::new(XdnaDevice {
            file,
            sender,
            worker: Some(worker),
            cancelled,
            heap: Mutex::new(None),
            allocations,
            imported: Mutex::new(BTreeMap::new()),
        })) as usize;
        let (done, finished) = mpsc::channel();
        let closer = std::thread::spawn(move || {
            unsafe { device_close(device as *mut c_void) };
            done.send(()).unwrap();
        });
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        closer.join().unwrap();
        assert_eq!(wait_value(&shared, 2, 0), Err(DEVICE_LOST));
    }

    #[test]
    fn overlapping_import_does_not_replace_a_live_mapping() {
        let mut allocations = BTreeMap::new();
        let mut storage = [0u8; 32];
        register_span(
            &mut allocations,
            0x1000,
            AllocationSpan {
                bytes: 32,
                handle: 7,
                host: storage.as_mut_ptr() as usize,
            },
        )
        .unwrap();
        for address in [0x1000, 0x1008, 0xff8] {
            assert_eq!(
                register_span(
                    &mut allocations,
                    address,
                    AllocationSpan {
                        bytes: 16,
                        handle: 9,
                        host: 0,
                    }
                ),
                Err(INVALID_ARGUMENT)
            );
            let (handle, pointer) = allocation_view(&allocations, 0x1008, 16).unwrap();
            assert_eq!(handle, 7);
            unsafe { ptr::write_bytes(pointer, 42, 16) };
            assert_eq!(&storage[8..24], &[42; 16]);
        }
        assert!(
            register_span(
                &mut allocations,
                0x1020,
                AllocationSpan {
                    bytes: 16,
                    handle: 9,
                    host: 0,
                }
            )
            .is_ok()
        );
    }
}
