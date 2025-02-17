// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! A pipeline that performs general-purpose operations.
//!
//! A compute pipeline takes buffers and/or images as both inputs and outputs. It operates
//! "standalone", with no additional infrastructure such as render passes or vertex input. Compute
//! pipelines can be used by themselves for performing work on the Vulkan device, but they can also
//! assist graphics operations by precalculating or postprocessing the operations from another kind
//! of pipeline. While it theoretically possible to perform graphics operations entirely in a
//! compute pipeline, a graphics pipeline is better suited to that task.
//!
//! A compute pipeline is relatively simple to create, requiring only a pipeline layout and a single
//! shader, the *compute shader*. The compute shader is the actual program that performs the work.
//! Once created, you can execute a compute pipeline by *binding* it in a command buffer, binding
//! any descriptor sets and/or push constants that the pipeline needs, and then issuing a `dispatch`
//! command on the command buffer.

use super::PipelineCreateFlags;
use crate::{
    descriptor_set::layout::DescriptorSetLayoutCreationError,
    device::{Device, DeviceOwned},
    macros::impl_id_counter,
    pipeline::{
        cache::PipelineCache,
        layout::{PipelineLayout, PipelineLayoutCreationError, PipelineLayoutSupersetError},
        Pipeline, PipelineBindPoint,
    },
    shader::{
        DescriptorBindingRequirements, PipelineShaderStageCreateInfo, ShaderExecution, ShaderStage,
        SpecializationConstant,
    },
    OomError, RequirementNotMet, RequiresOneOf, VulkanError, VulkanObject,
};
use ahash::HashMap;
use std::{
    error::Error,
    ffi::CString,
    fmt::{Debug, Display, Error as FmtError, Formatter},
    mem::MaybeUninit,
    num::NonZeroU64,
    ptr,
    sync::Arc,
};

/// A pipeline object that describes to the Vulkan implementation how it should perform compute
/// operations.
///
/// The template parameter contains the descriptor set to use with this pipeline.
///
/// Pass an optional `Arc` to a `PipelineCache` to enable pipeline caching. The vulkan
/// implementation will handle the `PipelineCache` and check if it is available.
/// Check the documentation of the `PipelineCache` for more information.
pub struct ComputePipeline {
    handle: ash::vk::Pipeline,
    device: Arc<Device>,
    id: NonZeroU64,
    layout: Arc<PipelineLayout>,
    descriptor_binding_requirements: HashMap<(u32, u32), DescriptorBindingRequirements>,
    num_used_descriptor_sets: u32,
}

impl ComputePipeline {
    /// Creates a new `ComputePipeline`.
    #[inline]
    pub fn new(
        device: Arc<Device>,
        cache: Option<Arc<PipelineCache>>,
        create_info: ComputePipelineCreateInfo,
    ) -> Result<Arc<ComputePipeline>, ComputePipelineCreationError> {
        Self::validate_new(&device, cache.as_ref().map(AsRef::as_ref), &create_info)?;

        unsafe { Ok(Self::new_unchecked(device, cache, create_info)?) }
    }

    fn validate_new(
        device: &Device,
        cache: Option<&PipelineCache>,
        create_info: &ComputePipelineCreateInfo,
    ) -> Result<(), ComputePipelineCreationError> {
        // VUID-vkCreateComputePipelines-pipelineCache-parent
        if let Some(cache) = &cache {
            assert_eq!(device, cache.device().as_ref());
        }

        let &ComputePipelineCreateInfo {
            flags: _,
            ref stage,
            ref layout,
            _ne: _,
        } = create_info;

        {
            let &PipelineShaderStageCreateInfo {
                flags,
                ref entry_point,
                ref specialization_info,
                _ne: _,
            } = &stage;

            // VUID-VkPipelineShaderStageCreateInfo-flags-parameter
            flags.validate_device(device)?;

            let entry_point_info = entry_point.info();

            // VUID-VkComputePipelineCreateInfo-stage-00701
            // VUID-VkPipelineShaderStageCreateInfo-stage-parameter
            if !matches!(entry_point_info.execution, ShaderExecution::Compute) {
                return Err(ComputePipelineCreationError::ShaderStageInvalid {
                    stage: ShaderStage::from(&entry_point_info.execution),
                });
            }

            for (&constant_id, provided_value) in specialization_info {
                // Per `VkSpecializationMapEntry` spec:
                // "If a constantID value is not a specialization constant ID used in the shader,
                // that map entry does not affect the behavior of the pipeline."
                // We *may* want to be stricter than this for the sake of catching user errors?
                if let Some(default_value) =
                    entry_point_info.specialization_constants.get(&constant_id)
                {
                    // VUID-VkSpecializationMapEntry-constantID-00776
                    // Check for equal types rather than only equal size.
                    if !provided_value.eq_type(default_value) {
                        return Err(
                        ComputePipelineCreationError::ShaderSpecializationConstantTypeMismatch {
                            constant_id,
                            default_value: *default_value,
                            provided_value: *provided_value,
                        },
                    );
                    }
                }
            }

            // VUID-VkComputePipelineCreateInfo-layout-07987
            // VUID-VkComputePipelineCreateInfo-layout-07988
            // VUID-VkComputePipelineCreateInfo-layout-07990
            // VUID-VkComputePipelineCreateInfo-layout-07991
            // TODO: Make sure that all of these are indeed checked.
            layout.ensure_compatible_with_shader(
                entry_point_info
                    .descriptor_binding_requirements
                    .iter()
                    .map(|(k, v)| (*k, v)),
                entry_point_info.push_constant_requirements.as_ref(),
            )?;

            // VUID-VkComputePipelineCreateInfo-stage-00702
            // VUID-VkComputePipelineCreateInfo-layout-01687
            // TODO:
        }

        Ok(())
    }

    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    pub unsafe fn new_unchecked(
        device: Arc<Device>,
        cache: Option<Arc<PipelineCache>>,
        create_info: ComputePipelineCreateInfo,
    ) -> Result<Arc<ComputePipeline>, VulkanError> {
        let &ComputePipelineCreateInfo {
            flags,
            ref stage,
            ref layout,
            _ne: _,
        } = &create_info;

        let stage_vk;
        let name_vk;
        let specialization_info_vk;
        let specialization_map_entries_vk: Vec<_>;
        let mut specialization_data_vk: Vec<u8>;

        {
            let &PipelineShaderStageCreateInfo {
                flags,
                ref entry_point,
                ref specialization_info,
                _ne: _,
            } = stage;

            let entry_point_info = entry_point.info();
            name_vk = CString::new(entry_point_info.name.as_str()).unwrap();

            specialization_data_vk = Vec::new();
            specialization_map_entries_vk = specialization_info
                .iter()
                .map(|(&constant_id, value)| {
                    let data = value.as_bytes();
                    let offset = specialization_data_vk.len() as u32;
                    let size = data.len();
                    specialization_data_vk.extend(data);

                    ash::vk::SpecializationMapEntry {
                        constant_id,
                        offset,
                        size,
                    }
                })
                .collect();

            specialization_info_vk = ash::vk::SpecializationInfo {
                map_entry_count: specialization_map_entries_vk.len() as u32,
                p_map_entries: specialization_map_entries_vk.as_ptr(),
                data_size: specialization_data_vk.len(),
                p_data: specialization_data_vk.as_ptr() as *const _,
            };
            stage_vk = ash::vk::PipelineShaderStageCreateInfo {
                flags: flags.into(),
                stage: ShaderStage::from(&entry_point_info.execution).into(),
                module: entry_point.module().handle(),
                p_name: name_vk.as_ptr(),
                p_specialization_info: if specialization_info_vk.data_size == 0 {
                    ptr::null()
                } else {
                    &specialization_info_vk
                },
                ..Default::default()
            };
        }

        let create_infos_vk = ash::vk::ComputePipelineCreateInfo {
            flags: flags.into(),
            stage: stage_vk,
            layout: layout.handle(),
            base_pipeline_handle: ash::vk::Pipeline::null(),
            base_pipeline_index: 0,
            ..Default::default()
        };

        let handle = {
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();
            (fns.v1_0.create_compute_pipelines)(
                device.handle(),
                cache.as_ref().map_or(Default::default(), |c| c.handle()),
                1,
                &create_infos_vk,
                ptr::null(),
                output.as_mut_ptr(),
            )
            .result()
            .map_err(VulkanError::from)?;
            output.assume_init()
        };

        Ok(Self::from_handle(device, handle, create_info))
    }

    /// Creates a new `ComputePipeline` from a raw object handle.
    ///
    /// # Safety
    ///
    /// - `handle` must be a valid Vulkan object handle created from `device`.
    /// - `create_info` must match the info used to create the object.
    #[inline]
    pub unsafe fn from_handle(
        device: Arc<Device>,
        handle: ash::vk::Pipeline,
        create_info: ComputePipelineCreateInfo,
    ) -> Arc<ComputePipeline> {
        let ComputePipelineCreateInfo {
            flags: _,
            stage,
            layout,
            _ne: _,
        } = create_info;

        let descriptor_binding_requirements: HashMap<_, _> = stage
            .entry_point
            .info()
            .descriptor_binding_requirements
            .iter()
            .map(|(&loc, reqs)| (loc, reqs.clone()))
            .collect();
        let num_used_descriptor_sets = descriptor_binding_requirements
            .keys()
            .map(|loc| loc.0)
            .max()
            .map(|x| x + 1)
            .unwrap_or(0);

        Arc::new(ComputePipeline {
            handle,
            device,
            id: Self::next_id(),
            layout,
            descriptor_binding_requirements,
            num_used_descriptor_sets,
        })
    }

    /// Returns the `Device` this compute pipeline was created with.
    #[inline]
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl Pipeline for ComputePipeline {
    #[inline]
    fn bind_point(&self) -> PipelineBindPoint {
        PipelineBindPoint::Compute
    }

    #[inline]
    fn layout(&self) -> &Arc<PipelineLayout> {
        &self.layout
    }

    #[inline]
    fn num_used_descriptor_sets(&self) -> u32 {
        self.num_used_descriptor_sets
    }

    #[inline]
    fn descriptor_binding_requirements(
        &self,
    ) -> &HashMap<(u32, u32), DescriptorBindingRequirements> {
        &self.descriptor_binding_requirements
    }
}

impl Debug for ComputePipeline {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        write!(f, "<Vulkan compute pipeline {:?}>", self.handle)
    }
}

impl_id_counter!(ComputePipeline);

unsafe impl VulkanObject for ComputePipeline {
    type Handle = ash::vk::Pipeline;

    #[inline]
    fn handle(&self) -> Self::Handle {
        self.handle
    }
}

unsafe impl DeviceOwned for ComputePipeline {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        self.device()
    }
}

impl Drop for ComputePipeline {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let fns = self.device.fns();
            (fns.v1_0.destroy_pipeline)(self.device.handle(), self.handle, ptr::null());
        }
    }
}

/// Parameters to create a new `ComputePipeline`.
#[derive(Clone, Debug)]
pub struct ComputePipelineCreateInfo {
    /// Specifies how to create the pipeline.
    ///
    /// The default value is empty.
    pub flags: PipelineCreateFlags,

    /// The compute shader stage to use.
    ///
    /// There is no default value.
    pub stage: PipelineShaderStageCreateInfo,

    /// The pipeline layout to use.
    ///
    /// There is no default value.
    pub layout: Arc<PipelineLayout>,

    pub _ne: crate::NonExhaustive,
}

impl ComputePipelineCreateInfo {
    /// Returns a `ComputePipelineCreateInfo` with the specified `stage` and `layout`.
    #[inline]
    pub fn stage_layout(stage: PipelineShaderStageCreateInfo, layout: Arc<PipelineLayout>) -> Self {
        Self {
            flags: PipelineCreateFlags::empty(),
            stage,
            layout,
            _ne: crate::NonExhaustive(()),
        }
    }
}

/// Error that can happen when creating a compute pipeline.
#[derive(Clone, Debug, PartialEq)]
pub enum ComputePipelineCreationError {
    /// Not enough memory.
    OomError(OomError),

    RequirementNotMet {
        required_for: &'static str,
        requires_one_of: RequiresOneOf,
    },

    /// Error while creating a descriptor set layout object.
    DescriptorSetLayoutCreationError(DescriptorSetLayoutCreationError),

    /// Error while creating the pipeline layout object.
    PipelineLayoutCreationError(PipelineLayoutCreationError),

    /// The pipeline layout is not compatible with what the shader expects.
    IncompatiblePipelineLayout(PipelineLayoutSupersetError),

    /// The value provided for a shader specialization constant has a
    /// different type than the constant's default value.
    ShaderSpecializationConstantTypeMismatch {
        constant_id: u32,
        default_value: SpecializationConstant,
        provided_value: SpecializationConstant,
    },

    /// The provided shader stage is not a compute shader.
    ShaderStageInvalid { stage: ShaderStage },
}

impl Error for ComputePipelineCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OomError(err) => Some(err),
            Self::DescriptorSetLayoutCreationError(err) => Some(err),
            Self::PipelineLayoutCreationError(err) => Some(err),
            Self::IncompatiblePipelineLayout(err) => Some(err),
            _ => None,
        }
    }
}

impl Display for ComputePipelineCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::OomError(_) => write!(f, "not enough memory available"),
            Self::RequirementNotMet {
                required_for,
                requires_one_of,
            } => write!(
                f,
                "a requirement was not met for: {}; requires one of: {}",
                required_for, requires_one_of,
            ),
            Self::DescriptorSetLayoutCreationError(_) => {
                write!(f, "error while creating a descriptor set layout object",)
            }
            Self::PipelineLayoutCreationError(_) => {
                write!(f, "error while creating the pipeline layout object",)
            }
            Self::IncompatiblePipelineLayout(_) => write!(
                f,
                "the pipeline layout is not compatible with what the shader expects",
            ),
            Self::ShaderSpecializationConstantTypeMismatch {
                constant_id,
                default_value,
                provided_value,
            } => write!(
                f,
                "the value provided for shader specialization constant id {} ({:?}) has a \
                different type than the constant's default value ({:?})",
                constant_id, provided_value, default_value,
            ),
            Self::ShaderStageInvalid { stage } => write!(
                f,
                "the provided shader stage ({:?}) is not a compute shader",
                stage,
            ),
        }
    }
}

impl From<OomError> for ComputePipelineCreationError {
    fn from(err: OomError) -> ComputePipelineCreationError {
        Self::OomError(err)
    }
}

impl From<RequirementNotMet> for ComputePipelineCreationError {
    fn from(err: RequirementNotMet) -> Self {
        Self::RequirementNotMet {
            required_for: err.required_for,
            requires_one_of: err.requires_one_of,
        }
    }
}

impl From<DescriptorSetLayoutCreationError> for ComputePipelineCreationError {
    fn from(err: DescriptorSetLayoutCreationError) -> Self {
        Self::DescriptorSetLayoutCreationError(err)
    }
}

impl From<PipelineLayoutCreationError> for ComputePipelineCreationError {
    fn from(err: PipelineLayoutCreationError) -> Self {
        Self::PipelineLayoutCreationError(err)
    }
}

impl From<PipelineLayoutSupersetError> for ComputePipelineCreationError {
    fn from(err: PipelineLayoutSupersetError) -> Self {
        Self::IncompatiblePipelineLayout(err)
    }
}

impl From<VulkanError> for ComputePipelineCreationError {
    fn from(err: VulkanError) -> ComputePipelineCreationError {
        match err {
            err @ VulkanError::OutOfHostMemory => Self::OomError(OomError::from(err)),
            err @ VulkanError::OutOfDeviceMemory => Self::OomError(OomError::from(err)),
            _ => panic!("unexpected error: {:?}", err),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        buffer::{Buffer, BufferCreateInfo, BufferUsage},
        command_buffer::{
            allocator::StandardCommandBufferAllocator, AutoCommandBufferBuilder, CommandBufferUsage,
        },
        descriptor_set::{
            allocator::StandardDescriptorSetAllocator, PersistentDescriptorSet, WriteDescriptorSet,
        },
        memory::allocator::{AllocationCreateInfo, MemoryUsage, StandardMemoryAllocator},
        pipeline::{
            compute::ComputePipelineCreateInfo, layout::PipelineDescriptorSetLayoutCreateInfo,
            ComputePipeline, Pipeline, PipelineBindPoint, PipelineLayout,
        },
        shader::{PipelineShaderStageCreateInfo, ShaderModule},
        sync::{now, GpuFuture},
    };

    // TODO: test for basic creation
    // TODO: test for pipeline layout error

    #[test]
    fn specialization_constants() {
        // This test checks whether specialization constants work.
        // It executes a single compute shader (one invocation) that writes the value of a spec.
        // constant to a buffer. The buffer content is then checked for the right value.

        let (device, queue) = gfx_dev_and_queue!();

        let cs = unsafe {
            /*
            #version 450

            layout(local_size_x = 1, local_size_y = 1, local_size_z = 1) in;

            layout(constant_id = 83) const int VALUE = 0xdeadbeef;

            layout(set = 0, binding = 0) buffer Output {
                int write;
            } write;

            void main() {
                write.write = VALUE;
            }
            */
            const MODULE: [u8; 480] = [
                3, 2, 35, 7, 0, 0, 1, 0, 1, 0, 8, 0, 14, 0, 0, 0, 0, 0, 0, 0, 17, 0, 2, 0, 1, 0, 0,
                0, 11, 0, 6, 0, 1, 0, 0, 0, 71, 76, 83, 76, 46, 115, 116, 100, 46, 52, 53, 48, 0,
                0, 0, 0, 14, 0, 3, 0, 0, 0, 0, 0, 1, 0, 0, 0, 15, 0, 5, 0, 5, 0, 0, 0, 4, 0, 0, 0,
                109, 97, 105, 110, 0, 0, 0, 0, 16, 0, 6, 0, 4, 0, 0, 0, 17, 0, 0, 0, 1, 0, 0, 0, 1,
                0, 0, 0, 1, 0, 0, 0, 3, 0, 3, 0, 2, 0, 0, 0, 194, 1, 0, 0, 5, 0, 4, 0, 4, 0, 0, 0,
                109, 97, 105, 110, 0, 0, 0, 0, 5, 0, 4, 0, 7, 0, 0, 0, 79, 117, 116, 112, 117, 116,
                0, 0, 6, 0, 5, 0, 7, 0, 0, 0, 0, 0, 0, 0, 119, 114, 105, 116, 101, 0, 0, 0, 5, 0,
                4, 0, 9, 0, 0, 0, 119, 114, 105, 116, 101, 0, 0, 0, 5, 0, 4, 0, 11, 0, 0, 0, 86,
                65, 76, 85, 69, 0, 0, 0, 72, 0, 5, 0, 7, 0, 0, 0, 0, 0, 0, 0, 35, 0, 0, 0, 0, 0, 0,
                0, 71, 0, 3, 0, 7, 0, 0, 0, 3, 0, 0, 0, 71, 0, 4, 0, 9, 0, 0, 0, 34, 0, 0, 0, 0, 0,
                0, 0, 71, 0, 4, 0, 9, 0, 0, 0, 33, 0, 0, 0, 0, 0, 0, 0, 71, 0, 4, 0, 11, 0, 0, 0,
                1, 0, 0, 0, 83, 0, 0, 0, 19, 0, 2, 0, 2, 0, 0, 0, 33, 0, 3, 0, 3, 0, 0, 0, 2, 0, 0,
                0, 21, 0, 4, 0, 6, 0, 0, 0, 32, 0, 0, 0, 1, 0, 0, 0, 30, 0, 3, 0, 7, 0, 0, 0, 6, 0,
                0, 0, 32, 0, 4, 0, 8, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 59, 0, 4, 0, 8, 0, 0, 0, 9,
                0, 0, 0, 2, 0, 0, 0, 43, 0, 4, 0, 6, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 50, 0, 4, 0,
                6, 0, 0, 0, 11, 0, 0, 0, 239, 190, 173, 222, 32, 0, 4, 0, 12, 0, 0, 0, 2, 0, 0, 0,
                6, 0, 0, 0, 54, 0, 5, 0, 2, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 248, 0, 2,
                0, 5, 0, 0, 0, 65, 0, 5, 0, 12, 0, 0, 0, 13, 0, 0, 0, 9, 0, 0, 0, 10, 0, 0, 0, 62,
                0, 3, 0, 13, 0, 0, 0, 11, 0, 0, 0, 253, 0, 1, 0, 56, 0, 1, 0,
            ];
            let module = ShaderModule::from_bytes(device.clone(), &MODULE).unwrap();
            module.entry_point("main").unwrap()
        };

        let pipeline = {
            let stage = PipelineShaderStageCreateInfo {
                specialization_info: [(83, 0x12345678i32.into())].into_iter().collect(),
                ..PipelineShaderStageCreateInfo::entry_point(cs)
            };
            let layout = PipelineLayout::new(
                device.clone(),
                PipelineDescriptorSetLayoutCreateInfo::from_stages([&stage])
                    .into_pipeline_layout_create_info(device.clone())
                    .unwrap(),
            )
            .unwrap();
            ComputePipeline::new(
                device.clone(),
                None,
                ComputePipelineCreateInfo::stage_layout(stage, layout),
            )
            .unwrap()
        };

        let memory_allocator = StandardMemoryAllocator::new_default(device.clone());
        let data_buffer = Buffer::from_data(
            &memory_allocator,
            BufferCreateInfo {
                usage: BufferUsage::STORAGE_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                usage: MemoryUsage::Upload,
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let ds_allocator = StandardDescriptorSetAllocator::new(device.clone());
        let set = PersistentDescriptorSet::new(
            &ds_allocator,
            pipeline.layout().set_layouts().get(0).unwrap().clone(),
            [WriteDescriptorSet::buffer(0, data_buffer.clone())],
        )
        .unwrap();

        let cb_allocator = StandardCommandBufferAllocator::new(device.clone(), Default::default());
        let mut cbb = AutoCommandBufferBuilder::primary(
            &cb_allocator,
            queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();
        cbb.bind_pipeline_compute(pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Compute,
                pipeline.layout().clone(),
                0,
                set,
            )
            .dispatch([1, 1, 1])
            .unwrap();
        let cb = cbb.build().unwrap();

        let future = now(device)
            .then_execute(queue, cb)
            .unwrap()
            .then_signal_fence_and_flush()
            .unwrap();
        future.wait(None).unwrap();

        let data_buffer_content = data_buffer.read().unwrap();
        assert_eq!(*data_buffer_content, 0x12345678);
    }
}
