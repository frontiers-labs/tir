use super::{Device, GetDeviceProcAddr, VK_SUCCESS, VkDevice, VkResult, load_device};
use crate::package::{BinaryKey, Package, PipelineBinary};
use std::ffi::{CString, c_char, c_void};
use std::ptr;

type VkShaderModule = u64;
type VkPipelineLayout = u64;
type VkPipeline = u64;
type VkPipelineBinary = u64;

const VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO: i32 = 16;
const VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO: i32 = 18;
const VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO: i32 = 29;
const VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO: i32 = 30;
const VK_STRUCTURE_TYPE_PIPELINE_CREATE_FLAGS_2_CREATE_INFO: i32 = 1_000_470_005;
const VK_STRUCTURE_TYPE_PIPELINE_BINARY_CREATE_INFO_KHR: i32 = 1_000_483_001;
const VK_STRUCTURE_TYPE_PIPELINE_BINARY_INFO_KHR: i32 = 1_000_483_002;
const VK_STRUCTURE_TYPE_PIPELINE_BINARY_KEY_KHR: i32 = 1_000_483_003;
const VK_STRUCTURE_TYPE_RELEASE_CAPTURED_PIPELINE_DATA_INFO_KHR: i32 = 1_000_483_005;
const VK_STRUCTURE_TYPE_PIPELINE_BINARY_DATA_INFO_KHR: i32 = 1_000_483_006;
const VK_STRUCTURE_TYPE_PIPELINE_BINARY_HANDLES_INFO_KHR: i32 = 1_000_483_009;
const VK_PIPELINE_CREATE_2_CAPTURE_DATA_BIT_KHR: u64 = 0x8000_0000;
const VK_SHADER_STAGE_COMPUTE_BIT: u32 = 0x20;

type CreateShaderModule = unsafe extern "system" fn(
    VkDevice,
    *const ShaderModuleCreateInfo,
    *const c_void,
    *mut VkShaderModule,
) -> VkResult;
type DestroyShaderModule = unsafe extern "system" fn(VkDevice, VkShaderModule, *const c_void);
type CreatePipelineLayout = unsafe extern "system" fn(
    VkDevice,
    *const PipelineLayoutCreateInfo,
    *const c_void,
    *mut VkPipelineLayout,
) -> VkResult;
type DestroyPipelineLayout = unsafe extern "system" fn(VkDevice, VkPipelineLayout, *const c_void);
type CreateComputePipelines = unsafe extern "system" fn(
    VkDevice,
    u64,
    u32,
    *const ComputePipelineCreateInfo,
    *const c_void,
    *mut VkPipeline,
) -> VkResult;
type DestroyPipeline = unsafe extern "system" fn(VkDevice, VkPipeline, *const c_void);
type GetPipelineKey =
    unsafe extern "system" fn(VkDevice, *const c_void, *mut PipelineBinaryKey) -> VkResult;
type CreatePipelineBinaries = unsafe extern "system" fn(
    VkDevice,
    *const PipelineBinaryCreateInfo,
    *const c_void,
    *mut PipelineBinaryHandlesInfo,
) -> VkResult;
type DestroyPipelineBinary = unsafe extern "system" fn(VkDevice, VkPipelineBinary, *const c_void);
type GetPipelineBinaryData = unsafe extern "system" fn(
    VkDevice,
    *const PipelineBinaryDataInfo,
    *mut PipelineBinaryKey,
    *mut usize,
    *mut c_void,
) -> VkResult;
type ReleaseCapturedPipelineData = unsafe extern "system" fn(
    VkDevice,
    *const ReleaseCapturedPipelineDataInfo,
    *const c_void,
) -> VkResult;

#[repr(C)]
struct ShaderModuleCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    code_size: usize,
    code: *const u32,
}

#[repr(C)]
struct PushConstantRange {
    stage_flags: u32,
    offset: u32,
    size: u32,
}

#[repr(C)]
struct PipelineLayoutCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    set_layout_count: u32,
    set_layouts: *const u64,
    push_constant_range_count: u32,
    push_constant_ranges: *const PushConstantRange,
}

#[repr(C)]
struct PipelineShaderStageCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    stage: u32,
    module: VkShaderModule,
    name: *const c_char,
    specialization: *const c_void,
}

#[repr(C)]
struct ComputePipelineCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    stage: PipelineShaderStageCreateInfo,
    layout: VkPipelineLayout,
    base_pipeline: VkPipeline,
    base_pipeline_index: i32,
}

#[repr(C)]
struct PipelineCreateFlags2CreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u64,
}

#[repr(C)]
struct PipelineBinaryKey {
    s_type: i32,
    p_next: *mut c_void,
    key_size: u32,
    key: [u8; 32],
}

impl PipelineBinaryKey {
    fn empty() -> Self {
        Self {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_KEY_KHR,
            p_next: ptr::null_mut(),
            key_size: 0,
            key: [0; 32],
        }
    }

    fn from_package(key: BinaryKey) -> Self {
        Self {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_KEY_KHR,
            p_next: ptr::null_mut(),
            key_size: key.size,
            key: key.bytes,
        }
    }

    fn into_package(self) -> BinaryKey {
        BinaryKey {
            size: self.key_size,
            bytes: self.key,
        }
    }
}

#[repr(C)]
struct PipelineBinaryData {
    data_size: usize,
    data: *mut c_void,
}

#[repr(C)]
struct PipelineBinaryKeysAndData {
    binary_count: u32,
    keys: *const PipelineBinaryKey,
    data: *const PipelineBinaryData,
}

#[repr(C)]
struct PipelineBinaryCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    keys_and_data: *const PipelineBinaryKeysAndData,
    pipeline: VkPipeline,
    pipeline_create_info: *const c_void,
}

#[repr(C)]
struct PipelineBinaryInfo {
    s_type: i32,
    p_next: *const c_void,
    binary_count: u32,
    binaries: *const VkPipelineBinary,
}

#[repr(C)]
struct ReleaseCapturedPipelineDataInfo {
    s_type: i32,
    p_next: *const c_void,
    pipeline: VkPipeline,
}

#[repr(C)]
struct PipelineBinaryDataInfo {
    s_type: i32,
    p_next: *const c_void,
    binary: VkPipelineBinary,
}

#[repr(C)]
struct PipelineBinaryHandlesInfo {
    s_type: i32,
    p_next: *const c_void,
    count: u32,
    binaries: *mut VkPipelineBinary,
}

#[derive(Clone, Copy)]
pub(super) struct PipelineFns {
    create_shader_module: CreateShaderModule,
    destroy_shader_module: DestroyShaderModule,
    create_layout: CreatePipelineLayout,
    destroy_layout: DestroyPipelineLayout,
    create_compute_pipelines: CreateComputePipelines,
    destroy_pipeline: DestroyPipeline,
    get_pipeline_key: GetPipelineKey,
    create_binaries: CreatePipelineBinaries,
    destroy_binary: DestroyPipelineBinary,
    get_binary_data: GetPipelineBinaryData,
    release_captured: ReleaseCapturedPipelineData,
}

impl PipelineFns {
    pub(super) unsafe fn load(get_proc: GetDeviceProcAddr, device: VkDevice) -> Option<Self> {
        // SAFETY: Each signature matches Vulkan 1.3 or VK_KHR_pipeline_binary.
        unsafe {
            Some(Self {
                create_shader_module: load_device(get_proc, device, c"vkCreateShaderModule")?,
                destroy_shader_module: load_device(get_proc, device, c"vkDestroyShaderModule")?,
                create_layout: load_device(get_proc, device, c"vkCreatePipelineLayout")?,
                destroy_layout: load_device(get_proc, device, c"vkDestroyPipelineLayout")?,
                create_compute_pipelines: load_device(
                    get_proc,
                    device,
                    c"vkCreateComputePipelines",
                )?,
                destroy_pipeline: load_device(get_proc, device, c"vkDestroyPipeline")?,
                get_pipeline_key: load_device(get_proc, device, c"vkGetPipelineKeyKHR")?,
                create_binaries: load_device(get_proc, device, c"vkCreatePipelineBinariesKHR")?,
                destroy_binary: load_device(get_proc, device, c"vkDestroyPipelineBinaryKHR")?,
                get_binary_data: load_device(get_proc, device, c"vkGetPipelineBinaryDataKHR")?,
                release_captured: load_device(
                    get_proc,
                    device,
                    c"vkReleaseCapturedPipelineDataKHR",
                )?,
            })
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PipelineError {
    Invalid,
    Incompatible,
    Backend(VkResult),
}

pub struct Pipeline {
    device: VkDevice,
    functions: PipelineFns,
    pub handle: VkPipeline,
    pub layout: VkPipelineLayout,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: The device outlives every executable it owns.
        unsafe {
            (self.functions.destroy_pipeline)(self.device, self.handle, ptr::null());
            (self.functions.destroy_layout)(self.device, self.layout, ptr::null());
        }
    }
}

struct Temporary<'a> {
    device: &'a Device,
    module: VkShaderModule,
    layout: VkPipelineLayout,
    pipeline: VkPipeline,
    captured: bool,
    binaries: Vec<VkPipelineBinary>,
}

impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        // SAFETY: All handles were created on this device and remain live.
        unsafe {
            if self.captured && self.pipeline != 0 {
                let info = ReleaseCapturedPipelineDataInfo {
                    s_type: VK_STRUCTURE_TYPE_RELEASE_CAPTURED_PIPELINE_DATA_INFO_KHR,
                    p_next: ptr::null(),
                    pipeline: self.pipeline,
                };
                (self.device.pipeline.release_captured)(self.device.handle, &info, ptr::null());
            }
            for binary in self.binaries.drain(..).filter(|&binary| binary != 0) {
                (self.device.pipeline.destroy_binary)(self.device.handle, binary, ptr::null());
            }
            if self.pipeline != 0 {
                (self.device.pipeline.destroy_pipeline)(
                    self.device.handle,
                    self.pipeline,
                    ptr::null(),
                );
            }
            if self.module != 0 {
                (self.device.pipeline.destroy_shader_module)(
                    self.device.handle,
                    self.module,
                    ptr::null(),
                );
            }
            if self.layout != 0 {
                (self.device.pipeline.destroy_layout)(self.device.handle, self.layout, ptr::null());
            }
        }
    }
}

impl Device {
    fn global_key(&self) -> Result<BinaryKey, PipelineError> {
        let mut key = PipelineBinaryKey::empty();
        // SAFETY: A null pipeline-create info requests the implementation key.
        let result =
            unsafe { (self.pipeline.get_pipeline_key)(self.handle, ptr::null(), &mut key) };
        if result == VK_SUCCESS && (1..=32).contains(&key.key_size) {
            Ok(key.into_package())
        } else {
            Err(PipelineError::Backend(result))
        }
    }

    fn create_root_layout(&self) -> Result<VkPipelineLayout, PipelineError> {
        let range = PushConstantRange {
            stage_flags: VK_SHADER_STAGE_COMPUTE_BIT,
            offset: 0,
            size: 8,
        };
        let info = PipelineLayoutCreateInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            set_layout_count: 0,
            set_layouts: ptr::null(),
            push_constant_range_count: 1,
            push_constant_ranges: &range,
        };
        let mut layout = 0;
        // SAFETY: The layout has one valid eight-byte compute push constant.
        let result =
            unsafe { (self.pipeline.create_layout)(self.handle, &info, ptr::null(), &mut layout) };
        if result == VK_SUCCESS && layout != 0 {
            Ok(layout)
        } else {
            Err(PipelineError::Backend(result))
        }
    }

    pub fn capture_pipeline(
        &self,
        words: &[u32],
        mut package: Package,
    ) -> Result<Package, PipelineError> {
        if words.len() < 5 || words[0] != 0x0723_0203 || !package.binaries.is_empty() {
            return Err(PipelineError::Invalid);
        }
        let entry = CString::new(package.entry.as_str()).map_err(|_| PipelineError::Invalid)?;
        let mut temporary = Temporary {
            device: self,
            module: 0,
            layout: 0,
            pipeline: 0,
            captured: false,
            binaries: Vec::new(),
        };
        let module_info = ShaderModuleCreateInfo {
            s_type: VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            code_size: words.len() * 4,
            code: words.as_ptr(),
        };
        // SAFETY: The word buffer contains caller-supplied SPIR-V and remains live.
        let result = unsafe {
            (self.pipeline.create_shader_module)(
                self.handle,
                &module_info,
                ptr::null(),
                &mut temporary.module,
            )
        };
        if result != VK_SUCCESS {
            return Err(PipelineError::Backend(result));
        }
        temporary.layout = self.create_root_layout()?;
        let stage = PipelineShaderStageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            stage: VK_SHADER_STAGE_COMPUTE_BIT,
            module: temporary.module,
            name: entry.as_ptr(),
            specialization: ptr::null(),
        };
        let capture = PipelineCreateFlags2CreateInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_CREATE_FLAGS_2_CREATE_INFO,
            p_next: ptr::null(),
            flags: VK_PIPELINE_CREATE_2_CAPTURE_DATA_BIT_KHR,
        };
        let create_info = ComputePipelineCreateInfo {
            s_type: VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
            p_next: (&raw const capture).cast(),
            flags: 0,
            stage,
            layout: temporary.layout,
            base_pipeline: 0,
            base_pipeline_index: -1,
        };
        // SAFETY: The pipeline-create chain and module/layout remain live.
        let result = unsafe {
            (self.pipeline.create_compute_pipelines)(
                self.handle,
                0,
                1,
                &create_info,
                ptr::null(),
                &mut temporary.pipeline,
            )
        };
        if result != VK_SUCCESS {
            return Err(PipelineError::Backend(result));
        }
        temporary.captured = true;
        let binary_create = PipelineBinaryCreateInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_CREATE_INFO_KHR,
            p_next: ptr::null(),
            keys_and_data: ptr::null(),
            pipeline: temporary.pipeline,
            pipeline_create_info: ptr::null(),
        };
        let mut handles = PipelineBinaryHandlesInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_HANDLES_INFO_KHR,
            p_next: ptr::null(),
            count: 0,
            binaries: ptr::null_mut(),
        };
        // SAFETY: The captured pipeline remains valid until Temporary drops.
        let result = unsafe {
            (self.pipeline.create_binaries)(self.handle, &binary_create, ptr::null(), &mut handles)
        };
        if result != VK_SUCCESS {
            return Err(PipelineError::Backend(result));
        }
        if handles.count == 0 || handles.count > 16 {
            return Err(PipelineError::Invalid);
        }
        temporary.binaries = vec![0; handles.count as usize];
        handles.binaries = temporary.binaries.as_mut_ptr();
        let result = unsafe {
            (self.pipeline.create_binaries)(self.handle, &binary_create, ptr::null(), &mut handles)
        };
        if result != VK_SUCCESS {
            return Err(PipelineError::Backend(result));
        }
        package.global_key = self.global_key()?;
        for &binary in &temporary.binaries {
            let info = PipelineBinaryDataInfo {
                s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_DATA_INFO_KHR,
                p_next: ptr::null(),
                binary,
            };
            let mut key = PipelineBinaryKey::empty();
            let mut size = 0;
            let result = unsafe {
                (self.pipeline.get_binary_data)(
                    self.handle,
                    &info,
                    &mut key,
                    &mut size,
                    ptr::null_mut(),
                )
            };
            if result != VK_SUCCESS || size == 0 || size > 64 * 1024 * 1024 {
                return Err(PipelineError::Backend(result));
            }
            let mut data = vec![0; size];
            let result = unsafe {
                (self.pipeline.get_binary_data)(
                    self.handle,
                    &info,
                    &mut key,
                    &mut size,
                    data.as_mut_ptr().cast(),
                )
            };
            if result != VK_SUCCESS {
                return Err(PipelineError::Backend(result));
            }
            data.truncate(size);
            package.binaries.push(PipelineBinary {
                key: key.into_package(),
                data,
            });
        }
        Ok(package)
    }

    pub fn import_pipeline(&self, package: &Package) -> Result<Pipeline, PipelineError> {
        if self.global_key()? != package.global_key {
            return Err(PipelineError::Incompatible);
        }
        let entry = CString::new(package.entry.as_str()).map_err(|_| PipelineError::Invalid)?;
        let keys: Vec<_> = package
            .binaries
            .iter()
            .map(|binary| PipelineBinaryKey::from_package(binary.key))
            .collect();
        let data: Vec<_> = package
            .binaries
            .iter()
            .map(|binary| PipelineBinaryData {
                data_size: binary.data.len(),
                data: binary.data.as_ptr().cast_mut().cast(),
            })
            .collect();
        let keys_and_data = PipelineBinaryKeysAndData {
            binary_count: keys.len() as u32,
            keys: keys.as_ptr(),
            data: data.as_ptr(),
        };
        let binary_create = PipelineBinaryCreateInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_CREATE_INFO_KHR,
            p_next: ptr::null(),
            keys_and_data: &keys_and_data,
            pipeline: 0,
            pipeline_create_info: ptr::null(),
        };
        let mut temporary = Temporary {
            device: self,
            module: 0,
            layout: 0,
            pipeline: 0,
            captured: false,
            binaries: vec![0; package.binaries.len()],
        };
        let mut handles = PipelineBinaryHandlesInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_HANDLES_INFO_KHR,
            p_next: ptr::null(),
            count: temporary.binaries.len() as u32,
            binaries: temporary.binaries.as_mut_ptr(),
        };
        // SAFETY: Every key and data buffer remains live until import returns.
        let result = unsafe {
            (self.pipeline.create_binaries)(self.handle, &binary_create, ptr::null(), &mut handles)
        };
        if result != VK_SUCCESS {
            return Err(PipelineError::Backend(result));
        }
        if handles.count as usize != package.binaries.len() {
            return Err(PipelineError::Incompatible);
        }
        temporary.layout = self.create_root_layout()?;
        let binary_info = PipelineBinaryInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_BINARY_INFO_KHR,
            p_next: ptr::null(),
            binary_count: handles.count,
            binaries: temporary.binaries.as_ptr(),
        };
        let stage = PipelineShaderStageCreateInfo {
            s_type: VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            stage: VK_SHADER_STAGE_COMPUTE_BIT,
            module: 0,
            name: entry.as_ptr(),
            specialization: ptr::null(),
        };
        let create_info = ComputePipelineCreateInfo {
            s_type: VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
            p_next: (&raw const binary_info).cast(),
            flags: 0,
            stage,
            layout: temporary.layout,
            base_pipeline: 0,
            base_pipeline_index: -1,
        };
        // SAFETY: The complete ordered binary set and matching layout/state
        // remain live. No shader module, SPIR-V, or pipeline cache is supplied.
        let result = unsafe {
            (self.pipeline.create_compute_pipelines)(
                self.handle,
                0,
                1,
                &create_info,
                ptr::null(),
                &mut temporary.pipeline,
            )
        };
        if result != VK_SUCCESS {
            return Err(PipelineError::Backend(result));
        }
        let pipeline = Pipeline {
            device: self.handle,
            functions: self.pipeline,
            handle: temporary.pipeline,
            layout: temporary.layout,
        };
        temporary.pipeline = 0;
        temporary.layout = 0;
        Ok(pipeline)
    }
}

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn pipeline_layout_matches_c_header() {
        assert_eq!(
            (
                size_of::<ShaderModuleCreateInfo>(),
                size_of::<PipelineLayoutCreateInfo>()
            ),
            (40, 48)
        );
        assert_eq!(
            (
                size_of::<PipelineShaderStageCreateInfo>(),
                size_of::<ComputePipelineCreateInfo>()
            ),
            (48, 96)
        );
        assert_eq!(size_of::<PipelineCreateFlags2CreateInfo>(), 24);
        assert_eq!(
            (
                size_of::<PipelineBinaryKey>(),
                size_of::<PipelineBinaryData>()
            ),
            (56, 16)
        );
        assert_eq!(
            (
                size_of::<PipelineBinaryCreateInfo>(),
                size_of::<PipelineBinaryInfo>(),
                size_of::<PipelineBinaryHandlesInfo>()
            ),
            (40, 32, 32)
        );
    }
}
