//! Vulkan import of VA-API's separate NV12 layers. Each layer may have several
//! modifier memory planes (e.g. auxiliary compression metadata); all layouts are
//! supplied, rather than assuming one linear plane per DMA-BUF.
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd};

use anyhow::{Context, ensure};
use ash::vk;
use egui_wgpu::wgpu;

use super::GpuContext;
use crate::session::gfx::vaapi::{HardwareFrame, Layer};

pub struct Imported {
    pub planes: Vec<wgpu::Texture>,
    device: ash::Device,
    family: u32,
    images: Vec<vk::Image>,
}
impl Imported {
    pub fn acquire(&self, encoder: &mut wgpu::CommandEncoder) {
        self.barrier(encoder, false);
    }
    pub fn release(&self, encoder: &mut wgpu::CommandEncoder) {
        self.barrier(encoder, true);
    }
    fn barrier(&self, encoder: &mut wgpu::CommandEncoder, release: bool) {
        let barriers: Vec<_> = self
            .images
            .iter()
            .map(|&image| {
                vk::ImageMemoryBarrier::default()
                    .image(image)
                    .old_layout(if release {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    } else {
                        vk::ImageLayout::GENERAL
                    })
                    .new_layout(if release {
                        vk::ImageLayout::GENERAL
                    } else {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    })
                    .src_queue_family_index(if release {
                        self.family
                    } else {
                        vk::QUEUE_FAMILY_FOREIGN_EXT
                    })
                    .dst_queue_family_index(if release {
                        vk::QUEUE_FAMILY_FOREIGN_EXT
                    } else {
                        self.family
                    })
                    .src_access_mask(if release {
                        vk::AccessFlags::SHADER_READ
                    } else {
                        vk::AccessFlags::empty()
                    })
                    .dst_access_mask(if release {
                        vk::AccessFlags::empty()
                    } else {
                        vk::AccessFlags::SHADER_READ
                    })
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
            })
            .collect();
        // SAFETY: images are held through submission completion. Native barriers
        // are recorded inside wgpu's encoder, never submitted to a raw queue.
        // The initial wgpu state matches the acquired shader-read layout; after
        // release these imports are retired and never sampled again.
        unsafe {
            encoder.as_hal_mut::<wgpu_hal::api::Vulkan, _, _>(|hal| {
                if let Some(hal) = hal {
                    self.device.cmd_pipeline_barrier(
                        hal.raw_handle(),
                        if release {
                            vk::PipelineStageFlags::FRAGMENT_SHADER
                        } else {
                            vk::PipelineStageFlags::TOP_OF_PIPE
                        },
                        if release {
                            vk::PipelineStageFlags::BOTTOM_OF_PIPE
                        } else {
                            vk::PipelineStageFlags::FRAGMENT_SHADER
                        },
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &barriers,
                    );
                }
            });
        }
    }
}

pub fn import(context: &GpuContext, frame: &HardwareFrame) -> anyhow::Result<Imported> {
    let desc = &frame.desc;
    ensure!(
        desc.allocation_width >= desc.width
            && desc.allocation_height >= desc.height
            && desc.allocation_width <= 8192
            && desc.allocation_height <= 8192,
        "invalid VA-API allocation dimensions"
    );
    ensure!(
        (1..=4).contains(&desc.num_objects) && desc.num_layers == 2,
        "unsupported NV12 DMA-BUF descriptor"
    );
    ensure!(
        desc.layers[0].format == u32::from_le_bytes(*b"R8  ")
            && desc.layers[1].format == u32::from_le_bytes(*b"GR88"),
        "unsupported NV12 layer formats"
    );
    // SAFETY: device handles are borrowed; imported textures keep the device
    // alive. No raw queue operations bypass wgpu's synchronization.
    let hal = unsafe { context.device.as_hal::<wgpu_hal::api::Vulkan>() }
        .context("Vulkan device lost")?;
    let mut result = Imported {
        planes: Vec::new(),
        device: hal.raw_device().clone(),
        family: hal.queue_family_index(),
        images: Vec::new(),
    };
    for (index, layer) in desc.layers.iter().take(2).enumerate() {
        let width = if index == 0 {
            desc.allocation_width
        } else {
            desc.allocation_width.div_ceil(2)
        };
        let height = if index == 0 {
            desc.allocation_height
        } else {
            desc.allocation_height.div_ceil(2)
        };
        let format = if index == 0 {
            wgpu::TextureFormat::R8Unorm
        } else {
            wgpu::TextureFormat::Rg8Unorm
        };
        let vk_format = if index == 0 {
            vk::Format::R8_UNORM
        } else {
            vk::Format::R8G8_UNORM
        };
        validate_layer(layer, desc.num_objects)?;
        let object = &desc.objects[layer.object_index[0] as usize];
        ensure!(
            object.fd >= 0 && object.size > 0 && object.modifier != u64::MAX,
            "invalid DMA-BUF object"
        );
        for plane in 0..layer.num_planes as usize {
            ensure!(
                layer.object_index[plane] == layer.object_index[0],
                "disjoint modifier objects are not supported"
            );
            ensure!(
                layer.pitch[plane] > 0 && layer.offset[plane] < object.size,
                "invalid DMA-BUF plane layout"
            );
        }
        // Query modifier memory-plane count and sampling support before import.
        let instance = hal.shared_instance().raw_instance();
        let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut modifier_list);
        unsafe {
            instance.get_physical_device_format_properties2(
                hal.raw_physical_device(),
                vk_format,
                &mut props,
            );
        }
        let mut modifiers = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            modifier_list.drm_format_modifier_count as usize
        ];
        modifier_list.p_drm_format_modifier_properties = modifiers.as_mut_ptr();
        let mut props = vk::FormatProperties2::default().push_next(&mut modifier_list);
        unsafe {
            instance.get_physical_device_format_properties2(
                hal.raw_physical_device(),
                vk_format,
                &mut props,
            );
        }
        ensure!(
            modifiers
                .iter()
                .any(|m| m.drm_format_modifier == object.modifier
                    && m.drm_format_modifier_plane_count == layer.num_planes
                    && m.drm_format_modifier_tiling_features.contains(
                        vk::FormatFeatureFlags::SAMPLED_IMAGE
                            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                    )),
            "driver cannot sample exported DMA-BUF modifier"
        );
        let layouts: Vec<_> = (0..layer.num_planes as usize)
            .map(|i| vk::SubresourceLayout {
                offset: u64::from(layer.offset[i]),
                row_pitch: u64::from(layer.pitch[i]),
                ..Default::default()
            })
            .collect();
        let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(object.modifier)
            .plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let create = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut modifier)
            .push_next(&mut external);
        // Every error before ownership transfer is cleaned by RawImport.
        let mut raw = RawImport {
            device: hal.raw_device().clone(),
            image: unsafe { hal.raw_device().create_image(&create, None)? },
            memory: None,
        };
        let requirements = unsafe { raw.device.get_image_memory_requirements(raw.image) };
        // SAFETY: fd belongs to HardwareFrame, which outlives this call. Vulkan
        // receives a duplicate, leaving the frame's own fd untouched.
        let fd = unsafe { BorrowedFd::borrow_raw(object.fd) }.try_clone_to_owned()?;
        let external_fd = ash::khr::external_memory_fd::Device::new(instance, &raw.device);
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        unsafe {
            external_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )?;
        }
        let bits = fd_props.memory_type_bits & requirements.memory_type_bits;
        ensure!(bits != 0, "no compatible DMA-BUF memory type");
        ensure!(
            requirements.size <= u64::from(object.size),
            "exported DMA-BUF object too small"
        );
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(raw.image);
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(u64::from(object.size))
            .memory_type_index(bits.trailing_zeros())
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let memory = unsafe { raw.device.allocate_memory(&allocate, None)? };
        let _ = fd.into_raw_fd(); // Vulkan consumes the duplicate only on successful allocation.
        raw.memory = Some(memory);
        unsafe {
            raw.device.bind_image_memory(raw.image, memory, 0)?;
        }
        let hal_desc = wgpu_hal::TextureDescriptor {
            label: Some("VA-API imported plane"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu_hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let hal_texture = unsafe {
            hal.texture_from_raw(
                raw.image,
                &hal_desc,
                None,
                wgpu_hal::vulkan::TextureMemory::Dedicated(memory),
            )
        };
        result.images.push(raw.image);
        raw.image = vk::Image::null();
        raw.memory = None;
        let texture = unsafe {
            context
                .device
                .create_texture_from_hal::<wgpu_hal::api::Vulkan>(
                    hal_texture,
                    &wgpu::TextureDescriptor {
                        label: Some("VA-API plane"),
                        size: hal_desc.size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::RESOURCE,
                )
        };
        result.planes.push(texture);
    }
    Ok(result)
}
fn validate_layer(layer: &Layer, objects: u32) -> anyhow::Result<()> {
    ensure!(
        (1..=4).contains(&layer.num_planes),
        "invalid DMA-BUF plane count"
    );
    ensure!(
        layer
            .object_index
            .iter()
            .take(layer.num_planes as usize)
            .all(|&i| i < objects),
        "invalid DMA-BUF object index"
    );
    Ok(())
}
struct RawImport {
    device: ash::Device,
    image: vk::Image,
    memory: Option<vk::DeviceMemory>,
}
impl Drop for RawImport {
    fn drop(&mut self) {
        // SAFETY: not submitted or transferred to wgpu; exclusively owned.
        unsafe {
            if self.image != vk::Image::null() {
                self.device.destroy_image(self.image, None);
            }
            if let Some(memory) = self.memory {
                self.device.free_memory(memory, None);
            }
        }
    }
}
