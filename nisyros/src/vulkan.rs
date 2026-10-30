//! Private Vulkan declarations used by the first backend. No Vulkan loader or
//! SDK is needed at link time; every entry point comes from libvulkan.so.1.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::transmute_copy;
use std::ptr::{self, NonNull};

mod memory;
pub use memory::{AllocError, AllocRequest, Allocation};
mod pipeline;
pub use pipeline::{Pipeline, PipelineError};
mod command;
pub use command::{Command, CommandOp, SubmitSegment, Timeline};

type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkDevice = *mut c_void;
type VkQueue = *mut c_void;
type VkResult = i32;

const VK_SUCCESS: VkResult = 0;
const VK_INCOMPLETE: VkResult = 5;
const VK_STRUCTURE_TYPE_APPLICATION_INFO: i32 = 0;
const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: i32 = 3;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2: i32 = 1_000_059_000;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2: i32 = 1_000_059_001;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES: i32 = 1_000_071_004;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES: i32 = 1_000_196_000;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_MEMORY_HOST_PROPERTIES_EXT: i32 = 1_000_178_002;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES: i32 = 1_000_207_000;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES: i32 = 1_000_257_000;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES: i32 = 1_000_314_007;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DEVICE_ADDRESS_COMMANDS_FEATURES_KHR: i32 = 1_000_318_006;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_5_FEATURES: i32 = 1_000_470_000;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_FLAGS_FEATURES_KHR: i32 = 1_000_668_004;
const VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_BINARY_FEATURES_KHR: i32 = 1_000_483_000;
const VULKAN_1_3: u32 = (1 << 22) | (3 << 12);
const VULKAN_1_4: u32 = (1 << 22) | (4 << 12);

#[link(name = "dl")]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

type GetInstanceProcAddr = unsafe extern "system" fn(VkInstance, *const c_char) -> *const c_void;
type CreateInstance = unsafe extern "system" fn(
    *const InstanceCreateInfo,
    *const c_void,
    *mut VkInstance,
) -> VkResult;
type DestroyInstance = unsafe extern "system" fn(VkInstance, *const c_void);
type EnumeratePhysicalDevices =
    unsafe extern "system" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type EnumerateDeviceExtensionProperties = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const c_char,
    *mut u32,
    *mut ExtensionProperties,
) -> VkResult;
type GetPhysicalDeviceProperties =
    unsafe extern "system" fn(VkPhysicalDevice, *mut PhysicalDeviceProperties);
type GetPhysicalDeviceQueueFamilyProperties =
    unsafe extern "system" fn(VkPhysicalDevice, *mut u32, *mut QueueFamilyProperties);
type GetPhysicalDeviceFeatures2 =
    unsafe extern "system" fn(VkPhysicalDevice, *mut PhysicalDeviceFeatures2);
type GetPhysicalDeviceProperties2 =
    unsafe extern "system" fn(VkPhysicalDevice, *mut PhysicalDeviceProperties2);
type GetPhysicalDeviceMemoryProperties =
    unsafe extern "system" fn(VkPhysicalDevice, *mut memory::MemoryProperties);
type CreateDevice = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const DeviceCreateInfo,
    *const c_void,
    *mut VkDevice,
) -> VkResult;
type GetDeviceProcAddr = unsafe extern "system" fn(VkDevice, *const c_char) -> *const c_void;
type DestroyDevice = unsafe extern "system" fn(VkDevice, *const c_void);
type GetDeviceQueue = unsafe extern "system" fn(VkDevice, u32, u32, *mut VkQueue);

#[repr(C)]
struct ApplicationInfo {
    s_type: i32,
    p_next: *const c_void,
    application_name: *const c_char,
    application_version: u32,
    engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct InstanceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    application_info: *const ApplicationInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
}

#[repr(C)]
struct DeviceQueueCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_family_index: u32,
    queue_count: u32,
    queue_priorities: *const f32,
}

#[repr(C)]
struct DeviceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_create_info_count: u32,
    queue_create_infos: *const DeviceQueueCreateInfo,
    enabled_layer_count: u32,
    enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    enabled_extension_names: *const *const c_char,
    enabled_features: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ExtensionProperties {
    name: [c_char; 256],
    spec_version: u32,
}

impl Default for ExtensionProperties {
    fn default() -> Self {
        Self {
            name: [0; 256],
            spec_version: 0,
        }
    }
}

// The fixed prefix is followed by VkPhysicalDeviceLimits and sparse properties.
// The tail is deliberately larger than the Vulkan 1.3 structure and aligned to
// its maximum member alignment; only the defined prefix is read.
#[repr(C)]
struct PhysicalDeviceProperties {
    api_version: u32,
    driver_version: u32,
    vendor_id: u32,
    device_id: u32,
    device_type: u32,
    name: [c_char; 256],
    pipeline_cache_uuid: [u8; 16],
    _alignment_padding: u32,
    _limits_before_grid: [u8; 220],
    max_compute_work_group_count: [u32; 3],
    _tail: [u64; 64],
}

#[repr(C)]
struct PhysicalDeviceProperties2 {
    s_type: i32,
    p_next: *mut c_void,
    properties: PhysicalDeviceProperties,
}

#[repr(C)]
struct ExternalMemoryHostProperties {
    s_type: i32,
    p_next: *mut c_void,
    min_alignment: u64,
}

#[repr(C)]
struct PhysicalDeviceIdProperties {
    s_type: i32,
    p_next: *mut c_void,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
    device_luid: [u8; 8],
    device_node_mask: u32,
    device_luid_valid: u32,
}

#[repr(C)]
struct PhysicalDeviceDriverProperties {
    s_type: i32,
    p_next: *mut c_void,
    driver_id: i32,
    driver_name: [c_char; 256],
    driver_info: [c_char; 256],
    conformance_version: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct QueueFamilyProperties {
    pub flags: u32,
    pub count: u32,
    timestamp_valid_bits: u32,
    min_image_transfer_granularity: [u32; 3],
}

#[repr(C)]
struct PhysicalDeviceFeatures2 {
    s_type: i32,
    p_next: *mut c_void,
    features: [u32; 55],
}

#[repr(C)]
struct BoolFeature {
    s_type: i32,
    p_next: *mut c_void,
    enabled: u32,
}

#[repr(C)]
struct BufferAddressFeatures {
    s_type: i32,
    p_next: *mut c_void,
    enabled: u32,
    capture_replay: u32,
    multi_device: u32,
}

pub struct DeviceRecord {
    pub physical: VkPhysicalDevice,
    pub api_version: u32,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: u32,
    pub name: [c_char; 256],
    pub device_uuid: [u8; 16],
    pub driver_uuid: [u8; 16],
    pub driver_id: u32,
    pub driver_name: [c_char; 256],
    pub queue_families: Vec<QueueFamilyProperties>,
    pub strict_profile_supported: bool,
    pub max_grid: [u32; 3],
    memory_properties: memory::MemoryProperties,
    flags_extension: Option<FlagsExtension>,
    pub external_host_alignment: Option<u64>,
}

#[derive(Clone, Copy)]
enum FlagsExtension {
    Core,
    Maintenance5,
    ExtendedFlags,
}

pub struct QueueRequest {
    pub family: u32,
    pub count: u32,
    pub priority: f32,
}

pub struct Queue {
    pub family: u32,
    pub index: u32,
    pub _handle: VkQueue,
}

pub struct Device {
    handle: VkDevice,
    destroy: DestroyDevice,
    pub queues: Vec<Queue>,
    memory: memory::MemoryFns,
    memory_properties: memory::MemoryProperties,
    command: command::CommandFns,
    pipeline: pipeline::PipelineFns,
    pub max_grid: [u32; 3],
    pub external_host_alignment: Option<u64>,
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: The caller closes all device children before dropping Device.
        unsafe { (self.destroy)(self.handle, ptr::null()) };
    }
}

pub struct Instance {
    library: NonNull<c_void>,
    handle: VkInstance,
    get_proc: GetInstanceProcAddr,
    destroy_instance: DestroyInstance,
    enumerate_physical_devices: EnumeratePhysicalDevices,
    enumerate_extensions: EnumerateDeviceExtensionProperties,
    get_properties: GetPhysicalDeviceProperties,
    get_queue_families: GetPhysicalDeviceQueueFamilyProperties,
    get_features2: GetPhysicalDeviceFeatures2,
    get_properties2: GetPhysicalDeviceProperties2,
    get_memory_properties: GetPhysicalDeviceMemoryProperties,
}

impl Instance {
    pub fn open() -> Result<Self, ()> {
        // SAFETY: The library handle remains owned by Instance until after
        // vkDestroyInstance. Function signatures match vulkan_core.h.
        unsafe {
            let library = NonNull::new(dlopen(c"libvulkan.so.1".as_ptr(), 2)).ok_or(())?;
            let get_proc = dlsym(library.as_ptr(), c"vkGetInstanceProcAddr".as_ptr());
            if get_proc.is_null() {
                dlclose(library.as_ptr());
                return Err(());
            }
            let get_proc: GetInstanceProcAddr = transmute_copy(&get_proc);
            let create: CreateInstance = match load(get_proc, ptr::null_mut(), c"vkCreateInstance")
            {
                Some(f) => f,
                None => {
                    dlclose(library.as_ptr());
                    return Err(());
                }
            };
            let app = ApplicationInfo {
                s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
                p_next: ptr::null(),
                application_name: c"nisyros".as_ptr(),
                application_version: 1,
                engine_name: c"nisyros".as_ptr(),
                engine_version: 1,
                api_version: VULKAN_1_3,
            };
            let create_info = InstanceCreateInfo {
                s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                p_next: ptr::null(),
                flags: 0,
                application_info: &app,
                enabled_layer_count: 0,
                enabled_layer_names: ptr::null(),
                enabled_extension_count: 0,
                enabled_extension_names: ptr::null(),
            };
            let mut handle = ptr::null_mut();
            if create(&create_info, ptr::null(), &mut handle) != VK_SUCCESS || handle.is_null() {
                dlclose(library.as_ptr());
                return Err(());
            }
            let functions = (|| {
                Some(Self {
                    library,
                    handle,
                    get_proc,
                    destroy_instance: load(get_proc, handle, c"vkDestroyInstance")?,
                    enumerate_physical_devices: load(
                        get_proc,
                        handle,
                        c"vkEnumeratePhysicalDevices",
                    )?,
                    enumerate_extensions: load(
                        get_proc,
                        handle,
                        c"vkEnumerateDeviceExtensionProperties",
                    )?,
                    get_properties: load(get_proc, handle, c"vkGetPhysicalDeviceProperties")?,
                    get_queue_families: load(
                        get_proc,
                        handle,
                        c"vkGetPhysicalDeviceQueueFamilyProperties",
                    )?,
                    get_features2: load(get_proc, handle, c"vkGetPhysicalDeviceFeatures2")?,
                    get_properties2: load(get_proc, handle, c"vkGetPhysicalDeviceProperties2")?,
                    get_memory_properties: load(
                        get_proc,
                        handle,
                        c"vkGetPhysicalDeviceMemoryProperties",
                    )?,
                })
            })();
            if functions.is_none() {
                if let Some(destroy) =
                    load::<DestroyInstance>(get_proc, handle, c"vkDestroyInstance")
                {
                    destroy(handle, ptr::null());
                }
                dlclose(library.as_ptr());
            }
            functions.ok_or(())
        }
    }

    pub fn devices(&self) -> Result<Vec<DeviceRecord>, ()> {
        // SAFETY: The instance owns all function pointers and every output
        // allocation has the layout and capacity required by Vulkan.
        unsafe {
            let mut count = 0;
            if (self.enumerate_physical_devices)(self.handle, &mut count, ptr::null_mut())
                != VK_SUCCESS
            {
                return Err(());
            }
            let mut handles = vec![ptr::null_mut(); count as usize];
            let result =
                (self.enumerate_physical_devices)(self.handle, &mut count, handles.as_mut_ptr());
            if result != VK_SUCCESS && result != VK_INCOMPLETE {
                return Err(());
            }
            handles.truncate(count as usize);
            handles
                .into_iter()
                .map(|physical| self.device(physical))
                .collect()
        }
    }

    pub fn open_device(
        &self,
        record: &DeviceRecord,
        requests: &[QueueRequest],
    ) -> Result<Device, ()> {
        if !record.strict_profile_supported || requests.is_empty() {
            return Err(());
        }
        let priorities: Vec<Vec<f32>> = requests
            .iter()
            .map(|request| vec![request.priority; request.count as usize])
            .collect();
        let queue_infos: Vec<_> = requests
            .iter()
            .zip(&priorities)
            .map(|(request, priorities)| DeviceQueueCreateInfo {
                s_type: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                p_next: ptr::null(),
                flags: 0,
                queue_family_index: request.family,
                queue_count: request.count,
                queue_priorities: priorities.as_ptr(),
            })
            .collect();
        let mut names = vec![
            c"VK_KHR_pipeline_binary".as_ptr(),
            c"VK_KHR_device_address_commands".as_ptr(),
        ];
        match record.flags_extension.ok_or(())? {
            FlagsExtension::Core => {}
            FlagsExtension::Maintenance5 => names.push(c"VK_KHR_maintenance5".as_ptr()),
            FlagsExtension::ExtendedFlags => names.push(c"VK_KHR_extended_flags".as_ptr()),
        }
        if record.external_host_alignment.is_some() {
            names.push(c"VK_EXT_external_memory_host".as_ptr());
        }
        let mut binary = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_BINARY_FEATURES_KHR,
            p_next: ptr::null_mut(),
            enabled: 1,
        };
        let mut address_commands = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DEVICE_ADDRESS_COMMANDS_FEATURES_KHR,
            p_next: (&mut binary as *mut BoolFeature).cast(),
            enabled: 1,
        };
        let mut maintenance5 = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_5_FEATURES,
            p_next: (&mut address_commands as *mut BoolFeature).cast(),
            enabled: u32::from(matches!(
                record.flags_extension,
                Some(FlagsExtension::Core | FlagsExtension::Maintenance5)
            )),
        };
        let mut extended_flags = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_FLAGS_FEATURES_KHR,
            p_next: (&mut address_commands as *mut BoolFeature).cast(),
            enabled: u32::from(matches!(
                record.flags_extension,
                Some(FlagsExtension::ExtendedFlags)
            )),
        };
        let flags_head: *mut c_void = match record.flags_extension {
            Some(FlagsExtension::Core | FlagsExtension::Maintenance5) => {
                (&mut maintenance5 as *mut BoolFeature).cast()
            }
            Some(FlagsExtension::ExtendedFlags) => (&mut extended_flags as *mut BoolFeature).cast(),
            None => return Err(()),
        };
        let mut sync2 = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
            p_next: flags_head,
            enabled: 1,
        };
        let mut buffer_address = BufferAddressFeatures {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
            p_next: (&mut sync2 as *mut BoolFeature).cast(),
            enabled: 1,
            capture_replay: 0,
            multi_device: 0,
        };
        let mut timeline = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
            p_next: (&mut buffer_address as *mut BufferAddressFeatures).cast(),
            enabled: 1,
        };
        let features = PhysicalDeviceFeatures2 {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            p_next: (&mut timeline as *mut BoolFeature).cast(),
            features: [0; 55],
        };
        let info = DeviceCreateInfo {
            s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
            p_next: (&raw const features).cast(),
            flags: 0,
            queue_create_info_count: queue_infos.len() as u32,
            queue_create_infos: queue_infos.as_ptr(),
            enabled_layer_count: 0,
            enabled_layer_names: ptr::null(),
            enabled_extension_count: names.len() as u32,
            enabled_extension_names: names.as_ptr(),
            enabled_features: ptr::null(),
        };
        // SAFETY: The feature chain, queue arrays, and names remain live until
        // vkCreateDevice returns. Every function is loaded from this instance.
        unsafe {
            let create: CreateDevice =
                load(self.get_proc, self.handle, c"vkCreateDevice").ok_or(())?;
            let mut handle = ptr::null_mut();
            if create(record.physical, &info, ptr::null(), &mut handle) != VK_SUCCESS
                || handle.is_null()
            {
                return Err(());
            }
            let Some(get_device_proc): Option<GetDeviceProcAddr> =
                load(self.get_proc, self.handle, c"vkGetDeviceProcAddr")
            else {
                if let Some(destroy) =
                    load::<DestroyDevice>(self.get_proc, self.handle, c"vkDestroyDevice")
                {
                    destroy(handle, ptr::null());
                }
                return Err(());
            };
            let Some(destroy): Option<DestroyDevice> =
                load_device(get_device_proc, handle, c"vkDestroyDevice")
            else {
                return Err(());
            };
            let Some(get_queue): Option<GetDeviceQueue> =
                load_device(get_device_proc, handle, c"vkGetDeviceQueue")
            else {
                destroy(handle, ptr::null());
                return Err(());
            };
            let Some(memory) = memory::MemoryFns::load(get_device_proc, handle) else {
                destroy(handle, ptr::null());
                return Err(());
            };
            let Some(command) = command::CommandFns::load(get_device_proc, handle) else {
                destroy(handle, ptr::null());
                return Err(());
            };
            let Some(pipeline) = pipeline::PipelineFns::load(get_device_proc, handle) else {
                destroy(handle, ptr::null());
                return Err(());
            };
            let mut queues = Vec::new();
            for request in requests {
                for index in 0..request.count {
                    let mut queue = ptr::null_mut();
                    get_queue(handle, request.family, index, &mut queue);
                    if queue.is_null() {
                        destroy(handle, ptr::null());
                        return Err(());
                    }
                    queues.push(Queue {
                        family: request.family,
                        index,
                        _handle: queue,
                    });
                }
            }
            Ok(Device {
                handle,
                destroy,
                queues,
                memory,
                memory_properties: record.memory_properties,
                command,
                pipeline,
                max_grid: record.max_grid,
                external_host_alignment: record.external_host_alignment,
            })
        }
    }

    unsafe fn device(&self, physical: VkPhysicalDevice) -> Result<DeviceRecord, ()> {
        let mut props: PhysicalDeviceProperties = unsafe { std::mem::zeroed() };
        unsafe { (self.get_properties)(physical, &mut props) };

        let mut extension_count = 0;
        if unsafe {
            (self.enumerate_extensions)(
                physical,
                ptr::null(),
                &mut extension_count,
                ptr::null_mut(),
            )
        } != VK_SUCCESS
        {
            return Err(());
        }
        let mut extensions = vec![ExtensionProperties::default(); extension_count as usize];
        let result = unsafe {
            (self.enumerate_extensions)(
                physical,
                ptr::null(),
                &mut extension_count,
                extensions.as_mut_ptr(),
            )
        };
        if result != VK_SUCCESS && result != VK_INCOMPLETE {
            return Err(());
        }
        extensions.truncate(extension_count as usize);
        let has = |name: &CStr| {
            extensions.iter().any(|ext| {
                // Vulkan requires a terminated extension name.
                unsafe { CStr::from_ptr(ext.name.as_ptr()) == name }
            })
        };

        let mut family_count = 0;
        unsafe { (self.get_queue_families)(physical, &mut family_count, ptr::null_mut()) };
        let mut queue_families = vec![QueueFamilyProperties::default(); family_count as usize];
        unsafe {
            (self.get_queue_families)(physical, &mut family_count, queue_families.as_mut_ptr())
        };
        queue_families.truncate(family_count as usize);

        let mut binary = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PIPELINE_BINARY_FEATURES_KHR,
            p_next: ptr::null_mut(),
            enabled: 0,
        };
        let mut address_commands = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DEVICE_ADDRESS_COMMANDS_FEATURES_KHR,
            p_next: (&mut binary as *mut BoolFeature).cast(),
            enabled: 0,
        };
        let mut maintenance5 = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MAINTENANCE_5_FEATURES,
            p_next: (&mut address_commands as *mut BoolFeature).cast(),
            enabled: 0,
        };
        let mut extended_flags = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTENDED_FLAGS_FEATURES_KHR,
            p_next: (&mut address_commands as *mut BoolFeature).cast(),
            enabled: 0,
        };
        let maintenance_available = has(c"VK_KHR_maintenance5") || props.api_version >= VULKAN_1_4;
        let extended_available = has(c"VK_KHR_extended_flags");
        if maintenance_available && extended_available {
            maintenance5.p_next = (&mut extended_flags as *mut BoolFeature).cast();
        }
        let flags_head: *mut c_void = if maintenance_available {
            (&mut maintenance5 as *mut BoolFeature).cast()
        } else if extended_available {
            (&mut extended_flags as *mut BoolFeature).cast()
        } else {
            (&mut address_commands as *mut BoolFeature).cast()
        };
        let mut sync2 = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
            p_next: flags_head,
            enabled: 0,
        };
        let mut buffer_address = BufferAddressFeatures {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
            p_next: (&mut sync2 as *mut BoolFeature).cast(),
            enabled: 0,
            capture_replay: 0,
            multi_device: 0,
        };
        let mut timeline = BoolFeature {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
            p_next: (&mut buffer_address as *mut BufferAddressFeatures).cast(),
            enabled: 0,
        };
        let mut features = PhysicalDeviceFeatures2 {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            p_next: (&mut timeline as *mut BoolFeature).cast(),
            features: [0; 55],
        };
        unsafe { (self.get_features2)(physical, &mut features) };

        let mut memory_properties = memory::MemoryProperties::default();
        unsafe { (self.get_memory_properties)(physical, &mut memory_properties) };
        let mut host = ExternalMemoryHostProperties {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_MEMORY_HOST_PROPERTIES_EXT,
            p_next: ptr::null_mut(),
            min_alignment: 0,
        };
        let mut driver = PhysicalDeviceDriverProperties {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES,
            p_next: if has(c"VK_EXT_external_memory_host") {
                (&mut host as *mut ExternalMemoryHostProperties).cast()
            } else {
                ptr::null_mut()
            },
            driver_id: 0,
            driver_name: [0; 256],
            driver_info: [0; 256],
            conformance_version: [0; 4],
        };
        let mut identity = PhysicalDeviceIdProperties {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ID_PROPERTIES,
            p_next: (&mut driver as *mut PhysicalDeviceDriverProperties).cast(),
            device_uuid: [0; 16],
            driver_uuid: [0; 16],
            device_luid: [0; 8],
            device_node_mask: 0,
            device_luid_valid: 0,
        };
        let mut properties = PhysicalDeviceProperties2 {
            s_type: VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
            p_next: (&mut identity as *mut PhysicalDeviceIdProperties).cast(),
            properties: unsafe { std::mem::zeroed() },
        };
        unsafe { (self.get_properties2)(physical, &mut properties) };
        let external_host_alignment = (host.min_alignment != 0).then_some(host.min_alignment);

        let flags_extension = if maintenance_available && maintenance5.enabled != 0 {
            if has(c"VK_KHR_maintenance5") {
                Some(FlagsExtension::Maintenance5)
            } else {
                Some(FlagsExtension::Core)
            }
        } else if extended_available && extended_flags.enabled != 0 {
            Some(FlagsExtension::ExtendedFlags)
        } else {
            None
        };
        let strict_profile_supported = props.api_version >= VULKAN_1_3
            && has(c"VK_KHR_pipeline_binary")
            && has(c"VK_KHR_device_address_commands")
            && flags_extension.is_some()
            && binary.enabled != 0
            && address_commands.enabled != 0
            && sync2.enabled != 0
            && buffer_address.enabled != 0
            && timeline.enabled != 0;
        Ok(DeviceRecord {
            physical,
            api_version: props.api_version,
            vendor_id: props.vendor_id,
            device_id: props.device_id,
            device_type: props.device_type,
            name: props.name,
            device_uuid: identity.device_uuid,
            driver_uuid: identity.driver_uuid,
            driver_id: driver.driver_id as u32,
            driver_name: driver.driver_name,
            queue_families,
            strict_profile_supported,
            max_grid: props.max_compute_work_group_count,
            memory_properties,
            flags_extension,
            external_host_alignment,
        })
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: Every Vulkan child is released before its Instance owner.
        unsafe {
            (self.destroy_instance)(self.handle, ptr::null());
            dlclose(self.library.as_ptr());
        }
    }
}

unsafe fn load<T: Copy>(
    get_proc: GetInstanceProcAddr,
    instance: VkInstance,
    name: &CStr,
) -> Option<T> {
    // SAFETY: The caller chooses T from the matching Vulkan C declaration.
    let function = unsafe { get_proc(instance, name.as_ptr()) };
    (!function.is_null()).then(|| unsafe { transmute_copy(&function) })
}

unsafe fn load_device<T: Copy>(
    get_proc: GetDeviceProcAddr,
    device: VkDevice,
    name: &CStr,
) -> Option<T> {
    // SAFETY: The caller chooses T from the matching Vulkan C declaration.
    let function = unsafe { get_proc(device, name.as_ptr()) };
    (!function.is_null()).then(|| unsafe { transmute_copy(&function) })
}

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn vulkan_base_layout_matches_c_header() {
        assert_eq!(
            (size_of::<ApplicationInfo>(), align_of::<ApplicationInfo>()),
            (48, 8)
        );
        assert_eq!(
            (
                size_of::<InstanceCreateInfo>(),
                align_of::<InstanceCreateInfo>()
            ),
            (64, 8)
        );
        assert_eq!(
            (
                size_of::<PhysicalDeviceFeatures2>(),
                align_of::<PhysicalDeviceFeatures2>()
            ),
            (240, 8)
        );
        assert_eq!(size_of::<QueueFamilyProperties>(), 24);
        assert_eq!(
            (
                size_of::<DeviceQueueCreateInfo>(),
                size_of::<DeviceCreateInfo>()
            ),
            (40, 72)
        );
        assert_eq!(offset_of!(PhysicalDeviceProperties, name), 20);
        assert_eq!(
            offset_of!(PhysicalDeviceProperties, max_compute_work_group_count),
            516
        );
        assert!(size_of::<PhysicalDeviceProperties>() >= 824);
    }
}
