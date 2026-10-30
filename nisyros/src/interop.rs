//! Sized public backend extension tables and borrowed Vulkan handles.

use super::*;

const BACKEND_ABI_VERSION: u32 = 1;
const VULKAN_INTEROP_ABI_VERSION: u32 = 1;

#[repr(C)]
struct TableHeader {
    struct_size: u32,
    abi_version: u32,
}

type GetExtension = unsafe extern "C" fn(*mut Device, *const c_char, u32, u32, *mut c_void) -> i32;
type DeviceQuery = unsafe extern "C" fn(*mut Device, *mut VulkanDeviceInfo) -> i32;
type QueueQuery = unsafe extern "C" fn(*mut Queue, *mut VulkanQueueInfo) -> i32;

#[repr(C)]
struct BackendApi {
    header: TableHeader,
    get_extension: Option<GetExtension>,
}

#[repr(C)]
struct VulkanInteropApi {
    header: TableHeader,
    device_query: Option<DeviceQuery>,
    queue_query: Option<QueueQuery>,
}

#[repr(C)]
struct VulkanDeviceInfo {
    header: DescriptorHeader,
    instance: *mut c_void,
    physical_device: *mut c_void,
    device: *mut c_void,
}

#[repr(C)]
struct VulkanQueueInfo {
    header: DescriptorHeader,
    queue: *mut c_void,
    family: u32,
    index: u32,
}

fn prepare_table(
    table: *mut c_void,
    table_size: u32,
    provider_size: usize,
    version: u32,
    known: bool,
) -> i32 {
    if table.is_null() || table_size < std::mem::size_of::<TableHeader>() as u32 {
        return INVALID_ARGUMENT;
    }
    // SAFETY: The caller provides `table_size` writable bytes. Only the
    // provider-known prefix is touched; unknown trailing bytes remain intact.
    unsafe {
        ptr::write_bytes(
            table.cast::<u8>(),
            0,
            (table_size as usize).min(provider_size),
        )
    };
    if !known {
        return UNSUPPORTED;
    }
    unsafe {
        ptr::write(
            table.cast::<TableHeader>(),
            TableHeader {
                struct_size: provider_size as u32,
                abi_version: version,
            },
        )
    };
    if (table_size as usize) < provider_size {
        INCOMPLETE
    } else {
        SUCCESS
    }
}

/// Negotiates the public extension dispatch table for a compiled-in backend.
/// This table does not register or load backend plugins.
///
/// # Safety
/// `table` must hold `table_size` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_backend_get_api(
    backend: u32,
    abi_version: u32,
    table_size: u32,
    table: *mut c_void,
) -> i32 {
    let status = prepare_table(
        table,
        table_size,
        std::mem::size_of::<BackendApi>(),
        BACKEND_ABI_VERSION,
        backend == BACKEND_VULKAN && abi_version == BACKEND_ABI_VERSION,
    );
    if status != SUCCESS {
        return status;
    }
    unsafe { (*table.cast::<BackendApi>()).get_extension = Some(nisyros_backend_get_extension) };
    SUCCESS
}

unsafe extern "C" fn nisyros_backend_get_extension(
    device: *mut Device,
    name: *const c_char,
    version: u32,
    table_size: u32,
    table: *mut c_void,
) -> i32 {
    let known = !device.is_null()
        && !name.is_null()
        && unsafe { CStr::from_ptr(name) }.to_bytes() == b"nisyros.vulkan"
        && version == VULKAN_INTEROP_ABI_VERSION;
    let status = prepare_table(
        table,
        table_size,
        std::mem::size_of::<VulkanInteropApi>(),
        VULKAN_INTEROP_ABI_VERSION,
        known,
    );
    if status != SUCCESS {
        return status;
    }
    unsafe {
        let api = &mut *table.cast::<VulkanInteropApi>();
        api.device_query = Some(nisyros_vulkan_device_query);
        api.queue_query = Some(nisyros_vulkan_queue_query);
    }
    SUCCESS
}

unsafe extern "C" fn nisyros_vulkan_device_query(
    device: *mut Device,
    out: *mut VulkanDeviceInfo,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    let out = unsafe { &mut *out };
    let status = validate_header(&out.header, std::mem::size_of::<VulkanDeviceInfo>());
    if status != SUCCESS {
        return status;
    }
    out.instance = ptr::null_mut();
    out.physical_device = ptr::null_mut();
    out.device = ptr::null_mut();
    if device.is_null() {
        return INVALID_ARGUMENT;
    }
    let device = unsafe { &*device };
    let cluster = unsafe { &*device.cluster };
    let Some(local_index) = cluster
        .inventory
        .get(device.inventory_index as usize)
        .and_then(|entry| entry.local_index)
    else {
        return DEVICE_LOST;
    };
    out.instance = cluster.instance.native_handle();
    out.physical_device = cluster.devices[local_index].physical;
    out.device = device.backend.native_handle();
    SUCCESS
}

unsafe extern "C" fn nisyros_vulkan_queue_query(
    queue: *mut Queue,
    out: *mut VulkanQueueInfo,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    let out = unsafe { &mut *out };
    let status = validate_header(&out.header, std::mem::size_of::<VulkanQueueInfo>());
    if status != SUCCESS {
        return status;
    }
    out.queue = ptr::null_mut();
    out.family = 0;
    out.index = 0;
    if queue.is_null() {
        return INVALID_ARGUMENT;
    }
    let queue = unsafe { &*queue };
    let device = unsafe { &*queue.parent };
    let Some(native) = device.backend.queues.get(queue.backend_index) else {
        return DEVICE_LOST;
    };
    out.queue = native._handle;
    out.family = queue.family;
    out.index = queue.index;
    SUCCESS
}
