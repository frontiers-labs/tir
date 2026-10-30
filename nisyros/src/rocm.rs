//! Built-in ROCm execution provider loaded through the HIP runtime at run time.

use super::*;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

const HIP_SUCCESS: i32 = 0;
const HIP_ERROR_OUT_OF_MEMORY: i32 = 2;
const HIP_MEMCPY_DEFAULT: i32 = 4;
const HIP_MEMCPY_DEVICE_TO_DEVICE: i32 = 3;
const HIP_MEMCPY_HOST_TO_DEVICE: i32 = 1;
const HIP_MEMCPY_DEVICE_TO_HOST: i32 = 2;
const HIP_STREAM_NON_BLOCKING: u32 = 1;
const HIP_HOST_REGISTER_MAPPED: u32 = 2;
const HIP_MALLOC_SIGNAL_MEMORY: u32 = 2;
const HSA_SUCCESS: i32 = 0;
const HSA_OUT_OF_RESOURCES: i32 = 0x1008;
const HSA_AGENT_INFO_DEVICE: i32 = 17;
const HSA_DEVICE_TYPE_GPU: u32 = 1;
const NO_EXPORT_OFFSET: u64 = u64::MAX;
const TIMELINE_PAGE_BYTES: usize = 4096;
const MFD_CLOEXEC: u32 = 1;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const F_DUPFD_CLOEXEC: i32 = 1030;

unsafe extern "C" {
    fn memfd_create(name: *const c_char, flags: u32) -> i32;
    fn ftruncate(fd: i32, length: i64) -> i32;
    fn mmap(
        address: *mut c_void,
        length: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, length: usize) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
}

type HipDevice = i32;
type HipStream = *mut c_void;
type HipModule = *mut c_void;
type HipFunction = *mut c_void;
type HipEvent = *mut c_void;

macro_rules! hip_functions {
    ($($field:ident: $ty:ty = $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        struct HipFns { $($field: $ty),+ }
        impl HipFns {
            unsafe fn load(handle: *mut c_void) -> Option<Self> {
                unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Option<T> {
                    let raw = unsafe { dlsym(handle, name.as_ptr()) };
                    if raw.is_null() { None } else {
                        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&raw) })
                    }
                }
                Some(Self { $($field: unsafe {
                    symbol(handle, CStr::from_bytes_with_nul_unchecked(concat!($name, "\0").as_bytes()))?
                }),+ })
            }
        }
    };
}

hip_functions! {
    init: unsafe extern "C" fn(u32) -> i32 = "hipInit",
    get_device_count: unsafe extern "C" fn(*mut i32) -> i32 = "hipGetDeviceCount",
    device_get: unsafe extern "C" fn(*mut HipDevice, i32) -> i32 = "hipDeviceGet",
    device_get_name: unsafe extern "C" fn(*mut c_char, i32, HipDevice) -> i32 = "hipDeviceGetName",
    device_get_uuid: unsafe extern "C" fn(*mut u8, HipDevice) -> i32 = "hipDeviceGetUuid",
    device_get_attribute: unsafe extern "C" fn(*mut i32, i32, i32) -> i32 = "hipDeviceGetAttribute",
    set_device: unsafe extern "C" fn(i32) -> i32 = "hipSetDevice",
    mem_get_info: unsafe extern "C" fn(*mut usize, *mut usize) -> i32 = "hipMemGetInfo",
    malloc: unsafe extern "C" fn(*mut *mut c_void, usize) -> i32 = "hipMalloc",
    ext_malloc: unsafe extern "C" fn(*mut *mut c_void, usize, u32) -> i32 = "hipExtMallocWithFlags",
    free: unsafe extern "C" fn(*mut c_void) -> i32 = "hipFree",
    host_malloc: unsafe extern "C" fn(*mut *mut c_void, usize, u32) -> i32 = "hipHostMalloc",
    host_free: unsafe extern "C" fn(*mut c_void) -> i32 = "hipHostFree",
    host_register: unsafe extern "C" fn(*mut c_void, usize, u32) -> i32 = "hipHostRegister",
    host_unregister: unsafe extern "C" fn(*mut c_void) -> i32 = "hipHostUnregister",
    host_get_device_pointer: unsafe extern "C" fn(*mut *mut c_void, *mut c_void, u32) -> i32 = "hipHostGetDevicePointer",
    stream_create: unsafe extern "C" fn(*mut HipStream, u32) -> i32 = "hipStreamCreateWithFlags",
    stream_destroy: unsafe extern "C" fn(HipStream) -> i32 = "hipStreamDestroy",
    stream_synchronize: unsafe extern "C" fn(HipStream) -> i32 = "hipStreamSynchronize",
    stream_write_value64: unsafe extern "C" fn(HipStream, *mut c_void, u64, u32) -> i32 = "hipStreamWriteValue64",
    memcpy_async: unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32, HipStream) -> i32 = "hipMemcpyAsync",
    memcpy: unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32) -> i32 = "hipMemcpy",
    memset_d32_async: unsafe extern "C" fn(*mut c_void, i32, usize, HipStream) -> i32 = "hipMemsetD32Async",
    module_load_data: unsafe extern "C" fn(*mut HipModule, *const c_void) -> i32 = "hipModuleLoadData",
    module_unload: unsafe extern "C" fn(HipModule) -> i32 = "hipModuleUnload",
    module_get_function: unsafe extern "C" fn(*mut HipFunction, HipModule, *const c_char) -> i32 = "hipModuleGetFunction",
    module_launch_kernel: unsafe extern "C" fn(HipFunction, u32, u32, u32, u32, u32, u32, u32, HipStream, *mut *mut c_void, *mut *mut c_void) -> i32 = "hipModuleLaunchKernel",
    event_create: unsafe extern "C" fn(*mut HipEvent) -> i32 = "hipEventCreate",
    event_destroy: unsafe extern "C" fn(HipEvent) -> i32 = "hipEventDestroy",
    event_record: unsafe extern "C" fn(HipEvent, HipStream) -> i32 = "hipEventRecord",
    event_synchronize: unsafe extern "C" fn(HipEvent) -> i32 = "hipEventSynchronize",
    event_elapsed_time: unsafe extern "C" fn(*mut f32, HipEvent, HipEvent) -> i32 = "hipEventElapsedTime",
}

#[derive(Clone, Copy)]
#[repr(C)]
struct HsaAgent {
    handle: u64,
}

#[derive(Clone, Copy)]
struct HsaFns {
    shutdown: unsafe extern "C" fn() -> i32,
    iterate_agents: unsafe extern "C" fn(
        unsafe extern "C" fn(HsaAgent, *mut c_void) -> i32,
        *mut c_void,
    ) -> i32,
    agent_get_info: unsafe extern "C" fn(HsaAgent, i32, *mut c_void) -> i32,
    export_dmabuf: unsafe extern "C" fn(*const c_void, usize, *mut i32, *mut u64) -> i32,
    map_buffer: unsafe extern "C" fn(
        u32,
        *const HsaAgent,
        i32,
        u32,
        *mut usize,
        *mut *mut c_void,
        *mut usize,
        *mut *const c_void,
    ) -> i32,
    unmap_buffer: unsafe extern "C" fn(*mut c_void) -> i32,
}

impl HsaFns {
    unsafe fn load(handle: *mut c_void) -> Option<(Self, unsafe extern "C" fn() -> i32)> {
        unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &CStr) -> Option<T> {
            let raw = unsafe { dlsym(handle, name.as_ptr()) };
            (!raw.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&raw) })
        }
        Some((
            Self {
                shutdown: unsafe { symbol(handle, c"hsa_shut_down")? },
                iterate_agents: unsafe { symbol(handle, c"hsa_iterate_agents")? },
                agent_get_info: unsafe { symbol(handle, c"hsa_agent_get_info")? },
                export_dmabuf: unsafe { symbol(handle, c"hsa_amd_portable_export_dmabuf")? },
                map_buffer: unsafe { symbol(handle, c"hsa_amd_interop_map_buffer")? },
                unmap_buffer: unsafe { symbol(handle, c"hsa_amd_interop_unmap_buffer")? },
            },
            unsafe { symbol(handle, c"hsa_init")? },
        ))
    }
}

struct HsaRuntime {
    fns: HsaFns,
    _library: DynamicLibrary,
}

impl Drop for HsaRuntime {
    fn drop(&mut self) {
        unsafe {
            (self.fns.shutdown)();
        }
    }
}

struct RocmInstance {
    _library: DynamicLibrary,
    _hsa: Option<HsaRuntime>,
    fns: HipFns,
    hsa_fns: Option<HsaFns>,
    hsa_agent: Option<HsaAgent>,
    devices: Vec<RocmDeviceInfo>,
}

struct RocmDeviceInfo {
    ordinal: i32,
    name: [c_char; 256],
    uuid: [u8; 16],
    device_id: u32,
    strict: bool,
    total_memory: u64,
    max_grid: [u32; 3],
    peer_timeline_fd: bool,
}

struct RocmDevice {
    fns: HipFns,
    hsa_fns: Option<HsaFns>,
    hsa_agent: Option<HsaAgent>,
    ordinal: i32,
    total_memory: u64,
    timestamp_origin: HipEvent,
    queues: Vec<RocmQueue>,
}

struct RocmQueue {
    stream: HipStream,
    sender: mpsc::Sender<RocmQueueWork>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

enum RocmQueueWork {
    Submit(Vec<RocmSubmitSegment>),
    Synchronize(mpsc::Sender<i32>),
    Shutdown,
}

struct RocmSubmitSegment {
    waits: Vec<(*mut RocmTimeline, u64)>,
    operations: Vec<RocmOp>,
    signals: Vec<(*mut RocmTimeline, u64)>,
}

enum AllocationKind {
    Device,
    Host(*mut c_void),
    Shadow(*mut c_void),
    Registered(*mut c_void),
    Imported { base: *mut c_void, hsa: HsaFns },
}
struct RocmAllocation {
    fns: HipFns,
    pointer: *mut c_void,
    bytes: u64,
    kind: AllocationKind,
    export_offset: AtomicU64,
}

impl Drop for RocmAllocation {
    fn drop(&mut self) {
        unsafe {
            match self.kind {
                AllocationKind::Device => {
                    (self.fns.free)(self.pointer);
                }
                AllocationKind::Host(host) => {
                    (self.fns.host_free)(host);
                }
                AllocationKind::Shadow(host) => {
                    (self.fns.free)(self.pointer);
                    (self.fns.host_free)(host);
                }
                AllocationKind::Registered(host) => {
                    (self.fns.host_unregister)(host);
                }
                AllocationKind::Imported { base, hsa } => {
                    (hsa.unmap_buffer)(base);
                }
            }
        }
    }
}

struct RocmPipeline {
    fns: HipFns,
    module: HipModule,
    function: HipFunction,
    workgroup: [u32; 3],
}
impl Drop for RocmPipeline {
    fn drop(&mut self) {
        unsafe {
            (self.fns.module_unload)(self.module);
        }
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum RocmOp {
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
        query: *mut c_void,
        destination: u64,
    },
}
unsafe impl Send for RocmOp {}
unsafe impl Send for RocmSubmitSegment {}
struct RocmCommand {
    operations: Mutex<Vec<RocmOp>>,
}

struct RocmTimeline {
    fns: HipFns,
    state: *mut AtomicU64,
    device_state: *mut c_void,
    backing: TimelineBacking,
    lock: Mutex<()>,
    changed: Condvar,
    failure: *mut AtomicI32,
}
enum TimelineBacking {
    Device { _failure: Box<AtomicI32> },
    Shared { _fd: OwnedFd, mapping: *mut c_void },
}
impl Drop for RocmTimeline {
    fn drop(&mut self) {
        unsafe {
            match &self.backing {
                TimelineBacking::Device { .. } => {
                    (self.fns.free)(self.device_state);
                }
                TimelineBacking::Shared { mapping, .. } => {
                    (self.fns.host_unregister)(*mapping);
                    munmap(*mapping, TIMELINE_PAGE_BYTES);
                }
            }
        }
    }
}
struct RocmQuery {
    origin: HipEvent,
}

fn hip_status(status: i32, backend: *mut i32) -> i32 {
    if !backend.is_null() {
        unsafe { *backend = status }
    }
    match status {
        HIP_SUCCESS => SUCCESS,
        HIP_ERROR_OUT_OF_MEMORY => OUT_OF_MEMORY,
        _ => DEVICE_LOST,
    }
}

unsafe fn set_device(device: &RocmDevice) -> i32 {
    unsafe { (device.fns.set_device)(device.ordinal) }
}

fn create_timeline_memfd() -> Option<OwnedFd> {
    let fd = unsafe { memfd_create(c"nisyros-rocm-timeline".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    (unsafe { ftruncate(fd.as_raw_fd(), TIMELINE_PAGE_BYTES as i64) } == 0).then_some(fd)
}

unsafe fn register_shared_timeline(
    fns: HipFns,
    fd: OwnedFd,
    initialize: bool,
    backend: *mut i32,
) -> Result<RocmTimeline, i32> {
    let file = std::fs::File::from(fd);
    if file
        .metadata()
        .map_or(true, |metadata| metadata.len() < TIMELINE_PAGE_BYTES as u64)
    {
        return Err(INVALID_ARGUMENT);
    }
    let fd = OwnedFd::from(file);
    let mapping = unsafe {
        mmap(
            ptr::null_mut(),
            TIMELINE_PAGE_BYTES,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if mapping as isize == -1 {
        return Err(UNSUPPORTED);
    }
    let status =
        unsafe { (fns.host_register)(mapping, TIMELINE_PAGE_BYTES, HIP_HOST_REGISTER_MAPPED) };
    if status != HIP_SUCCESS {
        unsafe { munmap(mapping, TIMELINE_PAGE_BYTES) };
        return Err(hip_status(status, backend));
    }
    let mut device_state = ptr::null_mut();
    let status = unsafe { (fns.host_get_device_pointer)(&mut device_state, mapping, 0) };
    if status != HIP_SUCCESS || device_state.is_null() {
        unsafe {
            (fns.host_unregister)(mapping);
            munmap(mapping, TIMELINE_PAGE_BYTES);
        }
        return Err(if status == HIP_SUCCESS {
            DEVICE_LOST
        } else {
            hip_status(status, backend)
        });
    }
    if initialize {
        unsafe {
            ptr::write(mapping.cast::<AtomicU64>(), AtomicU64::new(0));
            ptr::write(mapping.byte_add(8).cast(), AtomicI32::new(HIP_SUCCESS));
        }
    }
    Ok(RocmTimeline {
        fns,
        state: mapping.cast(),
        device_state,
        backing: TimelineBacking::Shared { _fd: fd, mapping },
        lock: Mutex::new(()),
        changed: Condvar::new(),
        failure: unsafe { mapping.byte_add(8).cast() },
    })
}

unsafe fn probe_shared_timeline(fns: HipFns) -> bool {
    let Some(fd) = create_timeline_memfd() else {
        return false;
    };
    let duplicate = unsafe { fcntl(fd.as_raw_fd(), F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return false;
    }
    let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let mut backend = 0;
    let Ok(source) = (unsafe { register_shared_timeline(fns, fd, true, &mut backend) }) else {
        return false;
    };
    let Ok(target) = (unsafe { register_shared_timeline(fns, duplicate, false, &mut backend) })
    else {
        return false;
    };
    let mut stream = ptr::null_mut();
    if unsafe { (fns.stream_create)(&mut stream, HIP_STREAM_NON_BLOCKING) } != HIP_SUCCESS {
        return false;
    }
    let write = unsafe { (fns.stream_write_value64)(stream, source.device_state, 1, 0) };
    let sync = if write == HIP_SUCCESS {
        unsafe { (fns.stream_synchronize)(stream) }
    } else {
        write
    };
    unsafe { (fns.stream_destroy)(stream) };
    sync == HIP_SUCCESS && unsafe { (*target.state).load(Ordering::Acquire) } == 1
}

unsafe extern "C" fn collect_hsa_gpu(agent: HsaAgent, data: *mut c_void) -> i32 {
    let (fns, agents) = unsafe { &mut *data.cast::<(HsaFns, Vec<HsaAgent>)>() };
    let mut device_type = 0u32;
    if unsafe {
        (fns.agent_get_info)(
            agent,
            HSA_AGENT_INFO_DEVICE,
            (&mut device_type as *mut u32).cast(),
        )
    } == HSA_SUCCESS
        && device_type == HSA_DEVICE_TYPE_GPU
    {
        agents.push(agent);
    }
    HSA_SUCCESS
}

unsafe fn open_hsa() -> Option<(HsaRuntime, HsaAgent)> {
    let handle = unsafe { dlopen(c"libhsa-runtime64.so.1".as_ptr(), 2) };
    if handle.is_null() {
        return None;
    }
    let library = DynamicLibrary(handle);
    let (fns, init) = unsafe { HsaFns::load(handle) }?;
    if unsafe { init() } != HSA_SUCCESS {
        return None;
    }
    let runtime = HsaRuntime {
        fns,
        _library: library,
    };
    let mut context = (fns, Vec::new());
    if unsafe {
        (fns.iterate_agents)(
            collect_hsa_gpu,
            (&mut context as *mut (HsaFns, Vec<HsaAgent>)).cast(),
        )
    } != HSA_SUCCESS
        || context.1.len() != 1
    {
        return None;
    }
    Some((runtime, context.1[0]))
}

/// Negotiates the built-in ROCm provider table.
pub(super) unsafe extern "C" fn rocm_plugin_get_api(
    abi_version: u32,
    services: *const PluginServices,
    table_size: u32,
    table: *mut c_void,
) -> i32 {
    if table.is_null() || table_size < 8 {
        return INVALID_ARGUMENT;
    }
    let size = std::mem::size_of::<PluginApi>();
    unsafe {
        ptr::write_bytes(table.cast::<u8>(), 0, (table_size as usize).min(size));
    }
    if abi_version != PLUGIN_ABI_VERSION {
        return UNSUPPORTED;
    }
    if !valid_services(services) {
        return INVALID_ARGUMENT;
    }
    unsafe {
        ptr::write(table.cast::<u32>(), size as u32);
        ptr::write(table.cast::<u32>().add(1), PLUGIN_ABI_VERSION);
    }
    if (table_size as usize) < size {
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

unsafe extern "C" fn instance_open(services: *const PluginServices, out: *mut *mut c_void) -> i32 {
    if !valid_services(services) || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    let handle = unsafe { dlopen(c"libamdhip64.so".as_ptr(), 2) };
    if handle.is_null() {
        report(services, NO_DEVICE, c"ROCm HIP runtime unavailable");
        return NO_DEVICE;
    }
    let library = DynamicLibrary(handle);
    let Some(fns) = (unsafe { HipFns::load(handle) }) else {
        report(
            services,
            NO_DEVICE,
            c"ROCm HIP runtime is missing required entry points",
        );
        return NO_DEVICE;
    };
    if unsafe { (fns.init)(0) } != HIP_SUCCESS {
        return NO_DEVICE;
    }
    let mut count = 0;
    if unsafe { (fns.get_device_count)(&mut count) } != HIP_SUCCESS || count <= 0 {
        return NO_DEVICE;
    }
    let mut devices = Vec::with_capacity(count as usize);
    for ordinal in 0..count {
        let mut device = 0;
        let mut name = [0; 256];
        let mut uuid = [0; 16];
        if unsafe { (fns.device_get)(&mut device, ordinal) } != HIP_SUCCESS
            || unsafe { (fns.device_get_name)(name.as_mut_ptr(), 256, device) } != HIP_SUCCESS
        {
            continue;
        }
        let _ = unsafe { (fns.device_get_uuid)(uuid.as_mut_ptr(), device) };
        if uuid != [0; 16]
            && devices
                .iter()
                .any(|record: &RocmDeviceInfo| record.uuid == uuid)
        {
            continue;
        }
        if unsafe { (fns.set_device)(ordinal) } != HIP_SUCCESS {
            continue;
        }
        let (mut free, mut total) = (0, 0);
        let memory_ok =
            unsafe { (fns.mem_get_info)(&mut free, &mut total) } == HIP_SUCCESS && total != 0;
        let mut dimensions = [u32::MAX; 3];
        for (i, attribute) in [29, 30, 31].into_iter().enumerate() {
            let mut value = 0;
            if unsafe { (fns.device_get_attribute)(&mut value, attribute, ordinal) } == HIP_SUCCESS
                && value > 0
            {
                dimensions[i] = value as u32;
            }
        }
        let mut allocation = ptr::null_mut();
        let allocation_ok = unsafe { (fns.malloc)(&mut allocation, 8) } == HIP_SUCCESS;
        if allocation_ok {
            unsafe {
                (fns.free)(allocation);
            }
        }
        let mut signal = ptr::null_mut();
        let signal_ok =
            unsafe { (fns.ext_malloc)(&mut signal, 8, HIP_MALLOC_SIGNAL_MEMORY) } == HIP_SUCCESS;
        if signal_ok {
            unsafe {
                (fns.free)(signal);
            }
        }
        let mut stream = ptr::null_mut();
        let stream_ok =
            unsafe { (fns.stream_create)(&mut stream, HIP_STREAM_NON_BLOCKING) } == HIP_SUCCESS;
        if stream_ok {
            unsafe {
                (fns.stream_destroy)(stream);
            }
        }
        let peer_timeline_fd = unsafe { probe_shared_timeline(fns) };
        let mut chip_id = 0;
        let _ = unsafe { (fns.device_get_attribute)(&mut chip_id, 10020, ordinal) };
        devices.push(RocmDeviceInfo {
            ordinal,
            name,
            uuid,
            device_id: chip_id as u32,
            strict: memory_ok
                && allocation_ok
                && signal_ok
                && stream_ok
                && !dimensions.contains(&u32::MAX),
            total_memory: total as u64,
            max_grid: dimensions,
            peer_timeline_fd,
        });
    }
    if devices.is_empty() {
        return NO_DEVICE;
    }
    let hsa = if devices.len() == 1 {
        unsafe { open_hsa() }
    } else {
        None
    };
    let (hsa_runtime, hsa_fns, hsa_agent) = match hsa {
        Some((runtime, agent)) => {
            let runtime_fns = runtime.fns;
            (Some(runtime), Some(runtime_fns), Some(agent))
        }
        None => (None, None, None),
    };
    unsafe {
        *out = Box::into_raw(Box::new(RocmInstance {
            _library: library,
            _hsa: hsa_runtime,
            fns,
            hsa_fns,
            hsa_agent,
            devices,
        }))
        .cast();
    }
    SUCCESS
}

unsafe extern "C" fn instance_close(value: *mut c_void) {
    if !value.is_null() {
        unsafe {
            drop(Box::from_raw(value.cast::<RocmInstance>()));
        }
    }
}
unsafe extern "C" fn device_count(value: *mut c_void, out: *mut u32) -> i32 {
    if value.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = (*value.cast::<RocmInstance>()).devices.len() as u32;
    }
    SUCCESS
}

unsafe extern "C" fn device_info(
    value: *mut c_void,
    index: u32,
    out: *mut PluginDeviceInfo,
) -> i32 {
    if value.is_null() || out.is_null() {
        return INVALID_ARGUMENT;
    }
    let out = unsafe { &mut *out };
    if (out.struct_size as usize) < std::mem::size_of::<PluginDeviceInfo>()
        || out.abi_version != PLUGIN_ABI_VERSION
    {
        return INVALID_ARGUMENT;
    }
    let Some(info) = (unsafe { &*value.cast::<RocmInstance>() })
        .devices
        .get(index as usize)
    else {
        return INVALID_ARGUMENT;
    };
    out.backend = BACKEND_ROCM;
    out.api_version = 0;
    out.vendor_id = 0x1002;
    out.device_id = info.device_id;
    out.device_type = 2;
    out.driver_id = 0;
    out.strict_profile_supported = u32::from(info.strict);
    out.queue_family_count = 1;
    out.memory_heap_count = 1;
    out.cooperative_matrix_count = 0;
    out.features = 0;
    out.external_host_alignment = 4096;
    out.cooperative_matrix_revision = 0;
    out.shader_bfloat16_revision = 0;
    out.shader_enqueue_revision = 0;
    out.cooperative_matrix_stages = 0;
    out.max_grid = info.max_grid;
    out.peer_memory_fd = u32::from(
        (unsafe { &*value.cast::<RocmInstance>() })
            .hsa_agent
            .is_some(),
    );
    out.peer_timeline_fd = u32::from(info.peer_timeline_fd);
    out.timestamp_period_ns = 1.0;
    out.name = info.name;
    out.device_uuid = info.uuid;
    out.driver_uuid = [0; 16];
    out.driver_name = [0; 256];
    let driver = b"ROCm HIP\0";
    for (dst, src) in out.driver_name.iter_mut().zip(driver.iter()) {
        *dst = *src as c_char;
    }
    SUCCESS
}

unsafe extern "C" fn queue_family_info(
    instance: *mut c_void,
    device: u32,
    family: u32,
    out: *mut PluginQueueFamilyInfo,
) -> i32 {
    if instance.is_null()
        || out.is_null()
        || family != 0
        || (device as usize) >= unsafe { &*instance.cast::<RocmInstance>() }.devices.len()
    {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = PluginQueueFamilyInfo {
            flags: 3,
            count: 32,
            timestamp_valid_bits: 64,
        };
    }
    SUCCESS
}
unsafe extern "C" fn memory_heap_info(
    instance: *mut c_void,
    device: u32,
    heap: u32,
    out: *mut PluginMemoryHeapInfo,
) -> i32 {
    if instance.is_null() || out.is_null() || heap != 0 {
        return INVALID_ARGUMENT;
    }
    let Some(info) = (unsafe { &*instance.cast::<RocmInstance>() })
        .devices
        .get(device as usize)
    else {
        return INVALID_ARGUMENT;
    };
    unsafe {
        *out = PluginMemoryHeapInfo {
            bytes: info.total_memory,
            flags: 1,
            reserved: 0,
        };
    }
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
    unsafe {
        *out = ptr::null_mut();
    }
    let owner = unsafe { &*instance.cast::<RocmInstance>() };
    let Some(info) = owner.devices.get(index as usize) else {
        return INVALID_ARGUMENT;
    };
    let request = unsafe { &*requests };
    if request.family != 0
        || request.count == 0
        || request.count > 32
        || !request.priority.is_finite()
    {
        return INVALID_ARGUMENT;
    }
    if unsafe { (owner.fns.set_device)(info.ordinal) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    let mut timestamp_origin = ptr::null_mut();
    let status = unsafe { (owner.fns.event_create)(&mut timestamp_origin) };
    if status != HIP_SUCCESS {
        return hip_status(status, ptr::null_mut());
    }
    let status = unsafe { (owner.fns.event_record)(timestamp_origin, ptr::null_mut()) };
    if status == HIP_SUCCESS {
        let status = unsafe { (owner.fns.event_synchronize)(timestamp_origin) };
        if status != HIP_SUCCESS {
            unsafe { (owner.fns.event_destroy)(timestamp_origin) };
            return hip_status(status, ptr::null_mut());
        }
    } else {
        unsafe { (owner.fns.event_destroy)(timestamp_origin) };
        return hip_status(status, ptr::null_mut());
    }
    let mut queues: Vec<RocmQueue> = Vec::with_capacity(request.count as usize);
    for _ in 0..request.count {
        let mut stream = ptr::null_mut();
        let status = unsafe { (owner.fns.stream_create)(&mut stream, HIP_STREAM_NON_BLOCKING) };
        if status != HIP_SUCCESS {
            for mut queue in queues {
                let _ = queue.sender.send(RocmQueueWork::Shutdown);
                if let Some(worker) = queue.worker.take() {
                    let _ = worker.join();
                }
                unsafe { (owner.fns.stream_destroy)(queue.stream) };
            }
            unsafe { (owner.fns.event_destroy)(timestamp_origin) };
            return hip_status(status, ptr::null_mut());
        }
        let (sender, receiver) = mpsc::channel();
        let fns = owner.fns;
        let ordinal = info.ordinal;
        let stream_address = stream as usize;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::spawn(move || {
            rocm_queue_worker(
                fns,
                ordinal,
                stream_address as HipStream,
                receiver,
                worker_cancelled,
            )
        });
        queues.push(RocmQueue {
            stream,
            sender,
            cancelled,
            worker: Some(worker),
        });
    }
    unsafe {
        *out = Box::into_raw(Box::new(RocmDevice {
            fns: owner.fns,
            hsa_fns: owner.hsa_fns,
            hsa_agent: owner.hsa_agent,
            ordinal: info.ordinal,
            total_memory: info.total_memory,
            timestamp_origin,
            queues,
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn device_close(value: *mut c_void) {
    if !value.is_null() {
        let device = unsafe { Box::from_raw(value.cast::<RocmDevice>()) };
        let _ = unsafe { set_device(&device) };
        for queue in &device.queues {
            queue.cancelled.store(true, Ordering::Release);
            let _ = queue.sender.send(RocmQueueWork::Shutdown);
        }
        for mut queue in device.queues {
            if let Some(worker) = queue.worker.take() {
                let _ = worker.join();
            }
            unsafe { (device.fns.stream_destroy)(queue.stream) };
        }
        unsafe { (device.fns.event_destroy)(device.timestamp_origin) };
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
    let device = unsafe { &*device.cast::<RocmDevice>() };
    let request = unsafe { &*request };
    if request.bytes == 0
        || request.bytes > usize::MAX as u64
        || !request.alignment.is_power_of_two()
        || !matches!(request.required_heap, 0 | u32::MAX)
        || !matches!(request.preferred_heap, 0 | u32::MAX)
        || request.queue_family_count > 1
    {
        return INVALID_ARGUMENT;
    }
    if request.queue_family_count == 1
        && (request.queue_families.is_null() || unsafe { *request.queue_families } != 0)
    {
        return INVALID_ARGUMENT;
    }
    if unsafe { set_device(device) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    let host_visible = request.required_properties & (2 | 4) != 0;
    let coherent = request.required_properties & 4 != 0;
    let mut host = ptr::null_mut();
    let mut pointer = ptr::null_mut();
    let status = if host_visible {
        let status = unsafe {
            (device.fns.host_malloc)(&mut host, request.bytes as usize, HIP_HOST_REGISTER_MAPPED)
        };
        if status == HIP_SUCCESS {
            if coherent {
                unsafe { (device.fns.host_get_device_pointer)(&mut pointer, host, 0) }
            } else {
                unsafe { (device.fns.malloc)(&mut pointer, request.bytes as usize) }
            }
        } else {
            status
        }
    } else {
        unsafe { (device.fns.malloc)(&mut pointer, request.bytes as usize) }
    };
    if status != HIP_SUCCESS {
        if !host.is_null() {
            unsafe {
                (device.fns.host_free)(host);
            }
        }
        return hip_status(status, ptr::null_mut());
    }
    if pointer.is_null()
        || (host_visible && host.is_null())
        || !(pointer as u64).is_multiple_of(request.alignment)
        || (host_visible && !(host as u64).is_multiple_of(request.alignment))
    {
        unsafe {
            if coherent {
                (device.fns.host_free)(host);
            } else {
                (device.fns.free)(pointer);
                if host_visible {
                    (device.fns.host_free)(host);
                }
            }
        }
        return UNSUPPORTED;
    }
    unsafe {
        *info = PluginAllocationInfo {
            base: pointer as u64,
            bytes: request.bytes,
            backing_bytes: request.bytes,
            heap_bytes: device.total_memory,
            memory_type_index: u32::from(host_visible),
            heap_index: 0,
            properties: if coherent {
                7
            } else if host_visible {
                3
            } else {
                1
            },
            exclusive: request.exclusive,
        };
        *out = Box::into_raw(Box::new(RocmAllocation {
            fns: device.fns,
            pointer,
            bytes: request.bytes,
            kind: if coherent {
                AllocationKind::Host(host)
            } else if host_visible {
                AllocationKind::Shadow(host)
            } else {
                AllocationKind::Device
            },
            export_offset: AtomicU64::new(NO_EXPORT_OFFSET),
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn allocation_destroy(_: *mut c_void, value: *mut c_void) {
    if !value.is_null() {
        unsafe {
            drop(Box::from_raw(value.cast::<RocmAllocation>()));
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
    unsafe { *out = ptr::null_mut() };
    let allocation = unsafe { &*allocation.cast::<RocmAllocation>() };
    let Some(offset) = address.checked_sub(allocation.pointer as u64) else {
        return INVALID_ARGUMENT;
    };
    match allocation.kind {
        AllocationKind::Registered(host)
        | AllocationKind::Host(host)
        | AllocationKind::Shadow(host)
            if offset < allocation.bytes =>
        {
            unsafe {
                *out = host.byte_add(offset as usize);
            }
            SUCCESS
        }
        _ => UNSUPPORTED,
    }
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
    let device = unsafe { &*device.cast::<RocmDevice>() };
    if unsafe { set_device(device) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    let allocation = unsafe { &*allocation.cast::<RocmAllocation>() };
    let AllocationKind::Shadow(host) = allocation.kind else {
        return SUCCESS;
    };
    let status = if flush != 0 {
        unsafe {
            (device.fns.memcpy)(
                allocation.pointer,
                host,
                allocation.bytes as usize,
                HIP_MEMCPY_HOST_TO_DEVICE,
            )
        }
    } else {
        unsafe {
            (device.fns.memcpy)(
                host,
                allocation.pointer,
                allocation.bytes as usize,
                HIP_MEMCPY_DEVICE_TO_HOST,
            )
        }
    };
    hip_status(status, ptr::null_mut())
}
unsafe extern "C" fn allocation_register_host(
    device: *mut c_void,
    host: *mut c_void,
    bytes: u64,
    out: *mut *mut c_void,
    info: *mut PluginAllocationInfo,
) -> i32 {
    if device.is_null()
        || host.is_null()
        || bytes == 0
        || bytes > usize::MAX as u64
        || out.is_null()
        || info.is_null()
    {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<RocmDevice>() };
    if unsafe { set_device(device) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    let status =
        unsafe { (device.fns.host_register)(host, bytes as usize, HIP_HOST_REGISTER_MAPPED) };
    if status != HIP_SUCCESS {
        return hip_status(status, ptr::null_mut());
    }
    let mut pointer = ptr::null_mut();
    let status = unsafe { (device.fns.host_get_device_pointer)(&mut pointer, host, 0) };
    if status != HIP_SUCCESS {
        unsafe {
            (device.fns.host_unregister)(host);
        }
        return hip_status(status, ptr::null_mut());
    }
    unsafe {
        *info = PluginAllocationInfo {
            base: pointer as u64,
            bytes,
            backing_bytes: bytes,
            heap_bytes: device.total_memory,
            memory_type_index: 1,
            heap_index: 0,
            properties: 7,
            exclusive: 0,
        };
        *out = Box::into_raw(Box::new(RocmAllocation {
            fns: device.fns,
            pointer,
            bytes,
            kind: AllocationKind::Registered(host),
            export_offset: AtomicU64::new(NO_EXPORT_OFFSET),
        }))
        .cast();
    }
    SUCCESS
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
    let device = unsafe { &*device.cast::<RocmDevice>() };
    let allocation = unsafe { &*allocation.cast::<RocmAllocation>() };
    let Some(hsa) = device.hsa_fns else {
        return UNSUPPORTED;
    };
    if unsafe { set_device(device) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    let mut fd = -1;
    let mut offset = 0;
    let status = unsafe {
        (hsa.export_dmabuf)(
            allocation.pointer,
            allocation.bytes as usize,
            &mut fd,
            &mut offset,
        )
    };
    if status != HSA_SUCCESS {
        return if status == HSA_OUT_OF_RESOURCES {
            OUT_OF_MEMORY
        } else {
            UNSUPPORTED
        };
    }
    if fd < 0 {
        return DEVICE_LOST;
    }
    allocation.export_offset.store(offset, Ordering::Release);
    unsafe {
        *out = fd;
        *out_offset = offset;
    };
    SUCCESS
}
unsafe extern "C" fn allocation_import_fd(
    device: *mut c_void,
    source_allocation: *mut c_void,
    backing_offset: u64,
    bytes: u64,
    fd: i32,
    out: *mut *mut c_void,
    info: *mut PluginAllocationInfo,
) -> i32 {
    if fd < 0 {
        return INVALID_ARGUMENT;
    }
    // The plugin ABI transfers ownership of the descriptor on every path.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if device.is_null()
        || bytes == 0
        || bytes > usize::MAX as u64
        || out.is_null()
        || info.is_null()
    {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<RocmDevice>() };
    let source = (!source_allocation.is_null())
        .then(|| unsafe { &*source_allocation.cast::<RocmAllocation>() });
    let (Some(hsa), Some(agent)) = (device.hsa_fns, device.hsa_agent) else {
        return UNSUPPORTED;
    };
    let export_offset = source.map_or(0, |source| source.export_offset.load(Ordering::Acquire));
    if export_offset == NO_EXPORT_OFFSET {
        return INVALID_ARGUMENT;
    }
    let Some(logical_offset) = backing_offset.checked_sub(export_offset) else {
        return INVALID_ARGUMENT;
    };
    if source.is_some_and(|source| {
        logical_offset
            .checked_add(bytes)
            .is_none_or(|end| end > source.bytes)
    }) {
        return INVALID_ARGUMENT;
    }
    let mut mapped_bytes = 0usize;
    let mut mapped = ptr::null_mut();
    let status = unsafe {
        (hsa.map_buffer)(
            1,
            &agent,
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            0,
            &mut mapped_bytes,
            &mut mapped,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if status != HSA_SUCCESS {
        if !mapped.is_null() {
            unsafe { (hsa.unmap_buffer)(mapped) };
        }
        return if status == HSA_OUT_OF_RESOURCES {
            OUT_OF_MEMORY
        } else {
            UNSUPPORTED
        };
    }
    if mapped.is_null() {
        return DEVICE_LOST;
    }
    if backing_offset
        .checked_add(bytes)
        .is_none_or(|end| end > mapped_bytes as u64)
    {
        unsafe {
            (hsa.unmap_buffer)(mapped);
        }
        return INVALID_ARGUMENT;
    }
    let pointer = unsafe { mapped.byte_add(backing_offset as usize) };
    unsafe {
        *info = PluginAllocationInfo {
            base: pointer as u64,
            bytes,
            backing_bytes: mapped_bytes as u64,
            heap_bytes: device.total_memory,
            memory_type_index: 0,
            heap_index: 0,
            properties: 1,
            exclusive: 0,
        };
        *out = Box::into_raw(Box::new(RocmAllocation {
            fns: device.fns,
            pointer,
            bytes,
            kind: AllocationKind::Imported { base: mapped, hsa },
            export_offset: AtomicU64::new(NO_EXPORT_OFFSET),
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn device_wait_idle(device: *mut c_void, backend: *mut i32) -> i32 {
    if device.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<RocmDevice>() };
    for queue in &device.queues {
        let (sender, receiver) = mpsc::channel();
        if queue
            .sender
            .send(RocmQueueWork::Synchronize(sender))
            .is_err()
        {
            return DEVICE_LOST;
        }
        let Ok(status) = receiver.recv() else {
            return DEVICE_LOST;
        };
        if status != HIP_SUCCESS {
            return hip_status(status, backend);
        }
    }
    SUCCESS
}

unsafe extern "C" fn timeline_create(
    device: *mut c_void,
    exportable: u32,
    out: *mut *mut c_void,
    backend: *mut i32,
) -> i32 {
    if device.is_null() || out.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<RocmDevice>() };
    let status = unsafe { set_device(device) };
    if status != HIP_SUCCESS {
        return hip_status(status, backend);
    }
    if exportable != 0 {
        let Some(fd) = create_timeline_memfd() else {
            return UNSUPPORTED;
        };
        let timeline = match unsafe { register_shared_timeline(device.fns, fd, true, backend) } {
            Ok(timeline) => timeline,
            Err(status) => return status,
        };
        unsafe { *out = Box::into_raw(Box::new(timeline)).cast() };
        return SUCCESS;
    }
    let mut state = ptr::null_mut();
    let status = unsafe { (device.fns.ext_malloc)(&mut state, 8, HIP_MALLOC_SIGNAL_MEMORY) };
    if status != HIP_SUCCESS {
        return hip_status(status, backend);
    }
    unsafe {
        ptr::write(state.cast::<AtomicU64>(), AtomicU64::new(0));
    }
    let failure = Box::new(AtomicI32::new(HIP_SUCCESS));
    let failure_pointer = (&raw const *failure).cast_mut();
    unsafe {
        *out = Box::into_raw(Box::new(RocmTimeline {
            fns: device.fns,
            state: state.cast(),
            device_state: state,
            backing: TimelineBacking::Device { _failure: failure },
            lock: Mutex::new(()),
            changed: Condvar::new(),
            failure: failure_pointer,
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn timeline_import_fd(
    device: *mut c_void,
    fd: i32,
    out: *mut *mut c_void,
    backend: *mut i32,
) -> i32 {
    if fd < 0 {
        return INVALID_ARGUMENT;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if device.is_null() || out.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = ptr::null_mut() };
    let device = unsafe { &*device.cast::<RocmDevice>() };
    let status = unsafe { set_device(device) };
    if status != HIP_SUCCESS {
        return hip_status(status, backend);
    }
    let timeline = match unsafe { register_shared_timeline(device.fns, fd, false, backend) } {
        Ok(timeline) => timeline,
        Err(status) => return status,
    };
    unsafe { *out = Box::into_raw(Box::new(timeline)).cast() };
    SUCCESS
}
unsafe extern "C" fn timeline_destroy(_: *mut c_void, timeline: *mut c_void) {
    if !timeline.is_null() {
        unsafe {
            drop(Box::from_raw(timeline.cast::<RocmTimeline>()));
        }
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
    let timeline = unsafe { &*timeline.cast::<RocmTimeline>() };
    let failure = unsafe { (*timeline.failure).load(Ordering::Acquire) };
    if failure != HIP_SUCCESS {
        unsafe { *backend = failure };
        return SUBMISSION_FAILED;
    }
    unsafe {
        *out = (*timeline.state).load(Ordering::Acquire);
    }
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
    let timeline = unsafe { &*timeline.cast::<RocmTimeline>() };
    let deadline = std::time::Instant::now().checked_add(Duration::from_nanos(timeout_ns));
    let mut guard = timeline.lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let failure = unsafe { (*timeline.failure).load(Ordering::Acquire) };
        if failure != HIP_SUCCESS {
            unsafe { *backend = failure };
            return SUBMISSION_FAILED;
        }
        if unsafe { (*timeline.state).load(Ordering::Acquire) } >= value {
            unsafe {
                *completed = 1;
            }
            return SUCCESS;
        }
        if deadline.is_some_and(|limit| std::time::Instant::now() >= limit) {
            unsafe {
                *completed = 0;
            }
            return SUCCESS;
        }
        let wait = deadline
            .map(|limit| limit.saturating_duration_since(std::time::Instant::now()))
            .unwrap_or(Duration::from_millis(1))
            .min(Duration::from_millis(1));
        guard = timeline
            .changed
            .wait_timeout(guard, wait)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}
unsafe extern "C" fn timeline_signal(timeline: *mut c_void, value: u64, _: *mut i32) -> i32 {
    if timeline.is_null() {
        return INVALID_ARGUMENT;
    }
    let timeline = unsafe { &*timeline.cast::<RocmTimeline>() };
    let _guard = timeline.lock.lock().unwrap_or_else(|e| e.into_inner());
    if value <= unsafe { (*timeline.state).load(Ordering::Acquire) } {
        return INVALID_ARGUMENT;
    }
    unsafe {
        (*timeline.state).store(value, Ordering::Release);
    }
    timeline.changed.notify_all();
    SUCCESS
}
unsafe extern "C" fn timeline_export_fd(
    timeline: *mut c_void,
    out: *mut i32,
    backend: *mut i32,
) -> i32 {
    if timeline.is_null() || out.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *out = -1 };
    let timeline = unsafe { &*timeline.cast::<RocmTimeline>() };
    let TimelineBacking::Shared { _fd: fd, .. } = &timeline.backing else {
        return UNSUPPORTED;
    };
    let duplicate = unsafe { fcntl(fd.as_raw_fd(), F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return OUT_OF_MEMORY;
    }
    unsafe { *out = duplicate };
    SUCCESS
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
        || bytes == 0
        || bytes > 128 * 1024 * 1024
        || out.is_null()
        || backend.is_null()
    {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = ptr::null_mut();
    }
    let package =
        match package::Package::parse(unsafe { std::slice::from_raw_parts(image, bytes as usize) })
        {
            Ok(v) => v,
            Err(package::PackageError::UnsupportedFeature) => return UNSUPPORTED,
            Err(_) => return INCOMPATIBLE_BINARY,
        };
    if package.binaries.len() != 1
        || package.format != package::ROCM_CODE_OBJECT_FORMAT
        || package.required_features != 0
        || !package.required_matrices.is_empty()
    {
        return UNSUPPORTED;
    }
    let device = unsafe { &*device.cast::<RocmDevice>() };
    if unsafe { set_device(device) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    let mut module = ptr::null_mut();
    let status = unsafe {
        (device.fns.module_load_data)(&mut module, package.binaries[0].data.as_ptr().cast())
    };
    if status != HIP_SUCCESS {
        return hip_status(status, backend);
    }
    let entry = match CString::new(package.entry) {
        Ok(v) => v,
        Err(_) => {
            unsafe {
                (device.fns.module_unload)(module);
            }
            return INCOMPATIBLE_BINARY;
        }
    };
    let mut function = ptr::null_mut();
    let status = unsafe { (device.fns.module_get_function)(&mut function, module, entry.as_ptr()) };
    if status != HIP_SUCCESS {
        unsafe {
            (device.fns.module_unload)(module);
        }
        return hip_status(status, backend);
    }
    unsafe {
        *out = Box::into_raw(Box::new(RocmPipeline {
            fns: device.fns,
            module,
            function,
            workgroup: package.workgroup,
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn pipeline_destroy(_: *mut c_void, pipeline: *mut c_void) {
    if !pipeline.is_null() {
        unsafe {
            drop(Box::from_raw(pipeline.cast::<RocmPipeline>()));
        }
    }
}
unsafe extern "C" fn query_create(
    device: *mut c_void,
    out: *mut *mut c_void,
    backend: *mut i32,
) -> i32 {
    if device.is_null() || out.is_null() || backend.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<RocmDevice>() };
    if unsafe { set_device(device) } != HIP_SUCCESS {
        return DEVICE_LOST;
    }
    unsafe {
        *out = Box::into_raw(Box::new(RocmQuery {
            origin: device.timestamp_origin,
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn query_destroy(_: *mut c_void, query: *mut c_void) {
    if !query.is_null() {
        unsafe {
            drop(Box::from_raw(query.cast::<RocmQuery>()));
        }
    }
}
unsafe extern "C" fn command_create(
    device: *mut c_void,
    family: u32,
    out: *mut *mut c_void,
    _: *mut i32,
) -> i32 {
    if device.is_null() || family != 0 || out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = Box::into_raw(Box::new(RocmCommand {
            operations: Mutex::new(Vec::new()),
        }))
        .cast();
    }
    SUCCESS
}
unsafe extern "C" fn command_destroy(_: *mut c_void, command: *mut c_void) {
    if !command.is_null() {
        unsafe {
            drop(Box::from_raw(command.cast::<RocmCommand>()));
        }
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
    let source = if count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(ops, count as usize) }
    };
    let mut decoded = Vec::with_capacity(source.len());
    for op in source {
        if op.reserved != 0 {
            return INVALID_ARGUMENT;
        }
        let a = op.args;
        let value = match op.kind {
            1 => RocmOp::Copy {
                dst: a[0],
                src: a[1],
                bytes: a[2],
            },
            2 => RocmOp::Fill {
                dst: a[0],
                bytes: a[1],
                pattern: match u32::try_from(a[2]) {
                    Ok(v) => v,
                    Err(_) => return INVALID_ARGUMENT,
                },
            },
            3..=5 => RocmOp::Barrier,
            6 => RocmOp::Dispatch {
                pipeline: a[0] as *mut c_void,
                grid: [a[1] as u32, a[2] as u32, a[3] as u32],
                arguments: a[4],
            },
            7 => RocmOp::DispatchIndirect {
                pipeline: a[0] as *mut c_void,
                grid_address: a[1],
                arguments: a[2],
            },
            8 => RocmOp::Timestamp {
                query: a[0] as *mut c_void,
                destination: a[1],
            },
            _ => return INVALID_ARGUMENT,
        };
        decoded.push(value);
    }
    *unsafe { &*command.cast::<RocmCommand>() }
        .operations
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = decoded;
    SUCCESS
}

unsafe fn replay(
    device: &RocmDevice,
    stream: HipStream,
    operations: &[RocmOp],
    backend: *mut i32,
) -> i32 {
    for op in operations {
        let status = match *op {
            RocmOp::Copy { dst, src, bytes } => {
                let Ok(bytes) = usize::try_from(bytes) else {
                    return INVALID_ARGUMENT;
                };
                unsafe {
                    (device.fns.memcpy_async)(
                        dst as *mut c_void,
                        src as *const c_void,
                        bytes,
                        HIP_MEMCPY_DEVICE_TO_DEVICE,
                        stream,
                    )
                }
            }
            RocmOp::Fill {
                dst,
                bytes,
                pattern,
            } => {
                if bytes % 4 != 0 {
                    return INVALID_ARGUMENT;
                }
                unsafe {
                    (device.fns.memset_d32_async)(
                        dst as *mut c_void,
                        pattern as i32,
                        (bytes / 4) as usize,
                        stream,
                    )
                }
            }
            RocmOp::Barrier => HIP_SUCCESS,
            RocmOp::Dispatch {
                pipeline,
                grid,
                arguments,
            } => {
                if pipeline.is_null() {
                    return INVALID_ARGUMENT;
                }
                let pipeline = unsafe { &*pipeline.cast::<RocmPipeline>() };
                let mut argument = arguments;
                let mut params = [(&mut argument as *mut u64).cast::<c_void>()];
                unsafe {
                    (device.fns.module_launch_kernel)(
                        pipeline.function,
                        grid[0],
                        grid[1],
                        grid[2],
                        pipeline.workgroup[0],
                        pipeline.workgroup[1],
                        pipeline.workgroup[2],
                        0,
                        stream,
                        params.as_mut_ptr(),
                        ptr::null_mut(),
                    )
                }
            }
            RocmOp::DispatchIndirect {
                pipeline,
                grid_address,
                arguments,
            } => {
                if pipeline.is_null() {
                    return INVALID_ARGUMENT;
                }
                let mut grid = [0u32; 3];
                let status = unsafe {
                    (device.fns.memcpy_async)(
                        grid.as_mut_ptr().cast(),
                        grid_address as *const c_void,
                        12,
                        HIP_MEMCPY_DEFAULT,
                        stream,
                    )
                };
                if status != HIP_SUCCESS {
                    status
                } else {
                    let status = unsafe { (device.fns.stream_synchronize)(stream) };
                    if status != HIP_SUCCESS {
                        status
                    } else {
                        let pipeline = unsafe { &*pipeline.cast::<RocmPipeline>() };
                        let mut argument = arguments;
                        let mut params = [(&mut argument as *mut u64).cast::<c_void>()];
                        unsafe {
                            (device.fns.module_launch_kernel)(
                                pipeline.function,
                                grid[0],
                                grid[1],
                                grid[2],
                                pipeline.workgroup[0],
                                pipeline.workgroup[1],
                                pipeline.workgroup[2],
                                0,
                                stream,
                                params.as_mut_ptr(),
                                ptr::null_mut(),
                            )
                        }
                    }
                }
            }
            RocmOp::Timestamp { query, destination } => {
                if query.is_null() {
                    return INVALID_ARGUMENT;
                }
                let query = unsafe { &*query.cast::<RocmQuery>() };
                let mut event = ptr::null_mut();
                let status = unsafe { (device.fns.event_create)(&mut event) };
                if status != HIP_SUCCESS {
                    status
                } else {
                    let status = unsafe { (device.fns.event_record)(event, stream) };
                    if status != HIP_SUCCESS {
                        unsafe {
                            (device.fns.event_destroy)(event);
                        }
                        status
                    } else {
                        let status = unsafe { (device.fns.event_synchronize)(event) };
                        let mut ms = 0.0;
                        let elapsed = if status == HIP_SUCCESS {
                            unsafe { (device.fns.event_elapsed_time)(&mut ms, query.origin, event) }
                        } else {
                            status
                        };
                        unsafe {
                            (device.fns.event_destroy)(event);
                        }
                        if elapsed != HIP_SUCCESS {
                            elapsed
                        } else {
                            let ns = (f64::from(ms) * 1_000_000.0) as u64;
                            unsafe {
                                (device.fns.memcpy)(
                                    destination as *mut c_void,
                                    (&raw const ns).cast(),
                                    8,
                                    HIP_MEMCPY_DEFAULT,
                                )
                            }
                        }
                    }
                }
            }
        };
        if status != HIP_SUCCESS {
            return hip_status(status, backend);
        }
    }
    SUCCESS
}

fn rocm_queue_worker(
    fns: HipFns,
    ordinal: i32,
    stream: HipStream,
    receiver: mpsc::Receiver<RocmQueueWork>,
    cancelled: Arc<AtomicBool>,
) {
    if unsafe { (fns.set_device)(ordinal) } != HIP_SUCCESS {
        return;
    }
    let device = RocmDevice {
        fns,
        hsa_fns: None,
        hsa_agent: None,
        ordinal,
        total_memory: 0,
        timestamp_origin: ptr::null_mut(),
        queues: Vec::new(),
    };
    while let Ok(work) = receiver.recv() {
        match work {
            RocmQueueWork::Submit(segments) => {
                let failure_targets: Vec<_> = segments
                    .iter()
                    .flat_map(|segment| segment.signals.iter().map(|(timeline, _)| *timeline))
                    .collect();
                let mut failed = false;
                let mut failure_status = -1;
                for segment in segments {
                    for (timeline, value) in segment.waits {
                        loop {
                            if cancelled.load(Ordering::Acquire) {
                                failed = true;
                                break;
                            }
                            let timeline = unsafe { &*timeline };
                            let dependency_failure =
                                unsafe { (*timeline.failure).load(Ordering::Acquire) };
                            if dependency_failure != HIP_SUCCESS {
                                failed = true;
                                failure_status = dependency_failure;
                                break;
                            }
                            if unsafe { (*timeline.state).load(Ordering::Acquire) } >= value {
                                break;
                            }
                            let guard = timeline.lock.lock().unwrap_or_else(|e| e.into_inner());
                            drop(
                                timeline
                                    .changed
                                    .wait_timeout(guard, Duration::from_millis(1))
                                    .unwrap_or_else(|e| e.into_inner()),
                            );
                        }
                        if failed {
                            break;
                        }
                    }
                    let mut backend = HIP_SUCCESS;
                    if failed {
                        break;
                    }
                    if unsafe { replay(&device, stream, &segment.operations, &mut backend) }
                        != SUCCESS
                    {
                        failed = true;
                        failure_status = if backend == HIP_SUCCESS { -1 } else { backend };
                        break;
                    }
                    for (timeline, value) in segment.signals {
                        let state = unsafe { (*timeline).device_state };
                        let status = unsafe { (fns.stream_write_value64)(stream, state, value, 0) };
                        if status != HIP_SUCCESS {
                            failed = true;
                            failure_status = status;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                }
                if !failed {
                    let status = unsafe { (fns.stream_synchronize)(stream) };
                    if status != HIP_SUCCESS {
                        failed = true;
                        failure_status = status;
                    }
                }
                if failed {
                    for timeline in failure_targets {
                        unsafe { (*(*timeline).failure).store(failure_status, Ordering::Release) };
                    }
                }
            }
            RocmQueueWork::Synchronize(sender) => {
                let _ = sender.send(unsafe { (fns.stream_synchronize)(stream) });
            }
            RocmQueueWork::Shutdown => break,
        }
    }
}

unsafe extern "C" fn queue_submit(
    device: *mut c_void,
    index: u32,
    count: u32,
    segments: *const PluginSubmitSegment,
    backend: *mut i32,
) -> i32 {
    if device.is_null() || backend.is_null() || (count != 0 && segments.is_null()) {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<RocmDevice>() };
    let Some(queue) = device.queues.get(index as usize) else {
        return INVALID_ARGUMENT;
    };
    let segments = if count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(segments, count as usize) }
    };
    let mut submission = Vec::with_capacity(segments.len());
    for segment in segments {
        if (segment.wait_count != 0 && segment.waits.is_null())
            || (segment.signal_count != 0 && segment.signals.is_null())
        {
            return INVALID_ARGUMENT;
        }
        let waits = if segment.wait_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(segment.waits, segment.wait_count as usize) }
        };
        let mut wait_points = Vec::with_capacity(waits.len());
        for point in waits {
            if point.timeline.is_null() {
                return INVALID_ARGUMENT;
            }
            wait_points.push((point.timeline.cast::<RocmTimeline>(), point.value));
        }
        let operations = if segment.command.is_null() {
            Vec::new()
        } else {
            let command = unsafe { &*segment.command.cast::<RocmCommand>() };
            command
                .operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let signals = if segment.signal_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(segment.signals, segment.signal_count as usize) }
        };
        let mut signal_points = Vec::with_capacity(signals.len());
        for point in signals {
            if point.timeline.is_null() {
                return INVALID_ARGUMENT;
            }
            signal_points.push((point.timeline.cast::<RocmTimeline>(), point.value));
        }
        submission.push(RocmSubmitSegment {
            waits: wait_points,
            operations,
            signals: signal_points,
        });
    }
    match queue.sender.send(RocmQueueWork::Submit(submission)) {
        Ok(()) => SUCCESS,
        Err(_) => DEVICE_LOST,
    }
}
unsafe extern "C" fn instance_native_handles(
    _: *mut c_void,
    _: u32,
    out1: *mut *mut c_void,
    out2: *mut *mut c_void,
) -> i32 {
    if !out1.is_null() {
        unsafe {
            *out1 = ptr::null_mut();
        }
    }
    if !out2.is_null() {
        unsafe {
            *out2 = ptr::null_mut();
        }
    }
    UNSUPPORTED
}
unsafe extern "C" fn device_native_handles(
    device: *mut c_void,
    _: u32,
    out1: *mut *mut c_void,
    out2: *mut *mut c_void,
) -> i32 {
    if device.is_null() || out1.is_null() || out2.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device.cast::<RocmDevice>() };
    unsafe {
        *out1 = device.ordinal as usize as *mut c_void;
        *out2 = device
            .queues
            .first()
            .map_or(ptr::null_mut(), |queue| queue.stream);
    }
    SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_timeline_rejects_a_truncated_descriptor() {
        let handle = unsafe { dlopen(c"libamdhip64.so".as_ptr(), 2) };
        if handle.is_null() {
            return;
        }
        let _library = DynamicLibrary(handle);
        let Some(fns) = (unsafe { HipFns::load(handle) }) else {
            return;
        };
        let fd = create_timeline_memfd().unwrap();
        assert_eq!(unsafe { ftruncate(fd.as_raw_fd(), 8) }, 0);
        let result = unsafe { register_shared_timeline(fns, fd, false, ptr::null_mut()) };
        assert!(matches!(result, Err(INVALID_ARGUMENT)));
    }
}
