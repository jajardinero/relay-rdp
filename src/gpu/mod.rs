//! Shared GPU compositor. The session thread submits offscreen work; egui samples
//! the completed desktop on the same queue. Video pixels never enter CPU memory.
mod import;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use ash::vk;
use egui_wgpu::{RenderState, WgpuSetup, WgpuSetupExisting, wgpu};
use parking_lot::Mutex;
use wgpu::util::DeviceExt;

use crate::session::framebuffer::Rect;
use crate::session::gfx::avc444::Layout;
use crate::session::gfx::vaapi::HardwareFrame;

pub struct GpuImage {
    pub texture: wgpu::Texture,
    pub width: u32,
    pub height: u32,
}

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub render_node: String,
    pending: Mutex<VecDeque<wgpu::SubmissionIndex>>,
    conversion: wgpu::RenderPipeline,
    avc444: Avc444Pipelines,
    failure: Arc<Mutex<Option<String>>>,
}

struct Avc444Pipelines {
    layout: wgpu::BindGroupLayout,
    main: wgpu::RenderPipeline,
    auxiliary: wgpu::RenderPipeline,
    convert: wgpu::RenderPipeline,
}

/// A surface's 4:4:4 picture on the GPU (r = Y, g = U, b = V), plus the color matrix of the
/// latest main view.
pub struct Store444 {
    pub image: Arc<GpuImage>,
    color: [[f32; 4]; 4],
}

/// Enable foreign queue ownership transfers in addition to wgpu's DMA-BUF
/// features. Fall back to ordinary device creation if the driver lacks interop.
pub async fn configure(
    setup: WgpuSetup,
    window: Arc<winit::window::Window>,
) -> anyhow::Result<WgpuSetup> {
    let instance = setup.new_instance().await;
    let surface = instance.create_surface(window)?;
    let preference = wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: preference,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        })
        .await?;
    let (device, queue) = open_device(&adapter).await?;
    Ok(WgpuSetupExisting {
        instance,
        adapter,
        device,
        queue,
    }
    .into())
}

async fn open_device(adapter: &wgpu::Adapter) -> anyhow::Result<(wgpu::Device, wgpu::Queue)> {
    let features = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
    let mut desc = wgpu::DeviceDescriptor {
        label: Some("Relay GPU"),
        required_limits: wgpu::Limits {
            max_texture_dimension_2d: 8192,
            ..Default::default()
        },
        ..Default::default()
    };
    let custom = if adapter.features().contains(features) {
        // SAFETY: HAL handles are borrowed only, and the added extension is
        // enumerated on this physical device before requesting it.
        unsafe {
            if let Some(hal) = adapter.as_hal::<wgpu_hal::api::Vulkan>() {
                let extensions = hal
                    .shared_instance()
                    .raw_instance()
                    .enumerate_device_extension_properties(hal.raw_physical_device())?;
                let foreign = extensions.iter().any(|e| {
                    std::ffi::CStr::from_ptr(e.extension_name.as_ptr())
                        == ash::ext::queue_family_foreign::NAME
                });
                if foreign {
                    desc.required_features = features;
                    match hal.open_with_callback(
                        features,
                        &desc.required_limits,
                        &desc.memory_hints,
                        Some(Box::new(|args| {
                            args.extensions.push(ash::ext::queue_family_foreign::NAME)
                        })),
                    ) {
                        Ok(device) => Some(device),
                        Err(error) => {
                            tracing::warn!(%error, "interop device unavailable; using standard GPU device");
                            desc.required_features = wgpu::Features::empty();
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }
    } else {
        None
    };
    let (device, queue) = match custom {
        // SAFETY: the OpenDevice came from this adapter using precisely desc.
        Some(hal) => unsafe {
            adapter.create_device_from_hal::<wgpu_hal::api::Vulkan>(hal, &desc)?
        },
        None => adapter.request_device(&desc).await?,
    };
    Ok((device, queue))
}

impl GpuContext {
    pub fn new(state: &RenderState) -> anyhow::Result<Arc<Self>> {
        Self::from_device(state.device.clone(), state.queue.clone())
    }

    fn from_device(device: wgpu::Device, queue: wgpu::Queue) -> anyhow::Result<Arc<Self>> {
        ensure!(
            device
                .features()
                .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
            "Vulkan DMA-BUF import is unavailable on this adapter"
        );
        // SAFETY: only querying physical-device properties and extension names.
        let node = unsafe {
            let hal = device
                .as_hal::<wgpu_hal::api::Vulkan>()
                .context("Vulkan renderer required")?;
            ensure!(
                hal.enabled_device_extensions()
                    .contains(&ash::ext::queue_family_foreign::NAME),
                "Vulkan foreign queue ownership is unavailable"
            );
            let instance = hal.shared_instance().raw_instance();
            let extensions =
                instance.enumerate_device_extension_properties(hal.raw_physical_device())?;
            ensure!(
                extensions
                    .iter()
                    .any(|e| std::ffi::CStr::from_ptr(e.extension_name.as_ptr())
                        == ash::ext::physical_device_drm::NAME),
                "Vulkan driver cannot identify its DRM device"
            );
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(hal.raw_physical_device(), &mut props);
            ensure!(drm.has_render != 0, "Vulkan adapter has no DRM render node");
            let expected = libc::makedev(drm.render_major as u32, drm.render_minor as u32);
            use std::os::unix::fs::MetadataExt;
            std::fs::read_dir("/dev/dri")
                .context("No DRM render devices available")?
                .filter_map(Result::ok)
                .find(|entry| {
                    entry.file_name().to_string_lossy().starts_with("renderD")
                        && entry.metadata().is_ok_and(|m| m.rdev() == expected)
                })
                .context("No matching VA-API render node for the Vulkan adapter")?
                .path()
                .to_string_lossy()
                .into_owned()
        };
        Ok(Arc::new(Self::with_device(device, queue, node)))
    }

    fn with_device(device: wgpu::Device, queue: wgpu::Queue, render_node: String) -> Self {
        let failure = Arc::new(Mutex::new(None));
        let error_sink = Arc::clone(&failure);
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            tracing::error!(%error, "GPU error");
            error_sink.lock().get_or_insert_with(|| error.to_string());
        }));
        let lost_sink = Arc::clone(&failure);
        device.set_device_lost_callback(move |reason, message| {
            if reason != wgpu::DeviceLostReason::Destroyed {
                lost_sink
                    .lock()
                    .get_or_insert_with(|| format!("GPU device lost: {message}"));
            }
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("NV12 conversion"),
            source: wgpu::ShaderSource::Wgsl(include_str!("nv12.wgsl").into()),
        });
        let conversion = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("NV12 to RGBA"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let avc444 = Avc444Pipelines::new(&device);
        Self {
            device,
            queue,
            render_node,
            pending: Mutex::new(VecDeque::new()),
            conversion,
            avc444,
            failure,
        }
    }

    pub fn image(&self, width: u32, height: u32) -> Arc<GpuImage> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("RDP GPU surface"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        Arc::new(GpuImage {
            texture,
            width,
            height,
        })
    }

    pub fn encoder(&self) -> wgpu::CommandEncoder {
        self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("RDP composition"),
            })
    }

    /// Keep at most three compositor submissions in flight. Polling here also
    /// retires video surfaces while the tab is hidden or the UI is idle.
    pub fn submit(
        &self,
        encoder: wgpu::CommandEncoder,
        hold: impl Send + 'static,
    ) -> anyhow::Result<()> {
        if let Some(reason) = self.failure.lock().clone() {
            anyhow::bail!("{reason}");
        }
        let mut pending = self.pending.lock();
        if pending.len() >= 3 {
            self.device.poll(wgpu::PollType::Wait {
                submission_index: pending.pop_front(),
                timeout: Some(Duration::from_secs(5)),
            })?;
        }
        let index = self.queue.submit([encoder.finish()]);
        self.queue.on_submitted_work_done(move || drop(hold));
        pending.push_back(index);
        self.device.poll(wgpu::PollType::Poll)?;
        if let Some(reason) = self.failure.lock().clone() {
            anyhow::bail!("{reason}");
        }
        Ok(())
    }

    pub fn copy(
        encoder: &mut wgpu::CommandEncoder,
        src: &GpuImage,
        rect: Rect,
        dst: &GpuImage,
        x: u32,
        y: u32,
    ) {
        let w = rect
            .w
            .min(src.width.saturating_sub(rect.x))
            .min(dst.width.saturating_sub(x));
        let h = rect
            .h
            .min(src.height.saturating_sub(rect.y))
            .min(dst.height.saturating_sub(y));
        if w == 0 || h == 0 {
            return;
        }
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &src.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &dst.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn clear(&self, image: &GpuImage, rgba: [u8; 4]) -> anyhow::Result<()> {
        let view = image.texture.create_view(&Default::default());
        let mut encoder = self.encoder();
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(rgba[0]) / 255.0,
                            g: f64::from(rgba[1]) / 255.0,
                            b: f64::from(rgba[2]) / 255.0,
                            a: f64::from(rgba[3]) / 255.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        self.submit(encoder, ())
    }

    pub fn fill(&self, image: &GpuImage, rect: Rect, rgba: [u8; 4]) -> anyhow::Result<()> {
        let Some(rect) = rect.clip(image.width, image.height) else {
            return Ok(());
        };
        if rect.x == 0 && rect.y == 0 && rect.w == image.width && rect.h == image.height {
            return self.clear(image, rgba);
        }
        // A 1x1 clear texture can be rendered only with a scissor. A render pass
        // with a small attachment view cannot clear a subrect, so use a tiny
        // uploaded solid tile and repeated GPU copies.
        let tile_w = rect.w.min(64);
        let tile_h = rect.h.min(64);
        let tile = self.image(tile_w, tile_h);
        self.upload(
            &tile,
            Rect::new(0, 0, tile_w, tile_h),
            &rgba.repeat((tile_w * tile_h) as usize),
            tile_w * 4,
        )?;
        let mut encoder = self.encoder();
        for y in (0..rect.h).step_by(tile_h as usize) {
            for x in (0..rect.w).step_by(tile_w as usize) {
                Self::copy(
                    &mut encoder,
                    &tile,
                    Rect::new(0, 0, tile_w.min(rect.w - x), tile_h.min(rect.h - y)),
                    image,
                    rect.x + x,
                    rect.y + y,
                );
            }
        }
        self.submit(encoder, tile)
    }

    pub fn upload(
        &self,
        image: &GpuImage,
        rect: Rect,
        bytes: &[u8],
        stride: u32,
    ) -> anyhow::Result<()> {
        let Some(rect) = rect.clip(image.width, image.height) else {
            return Ok(());
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &image.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: rect.w,
                height: rect.h,
                depth_or_array_layers: 1,
            },
        );
        self.submit(self.encoder(), ())
    }

    pub fn convert(&self, frame: HardwareFrame) -> anyhow::Result<Arc<GpuImage>> {
        ensure!(
            matches!(frame.desc.colorspace, 1 | 2 | 4 | 5 | 6 | 7 | 9),
            "unsupported H.264 color matrix"
        );
        let imported = import::import(self, &frame)?;
        let desc = &frame.desc;
        let output = self.image(desc.width, desc.height);
        let mut params =
            color_parameters(desc.colorspace, desc.full_range != 0, desc.chroma_location);
        // Clamp chroma filtering to the visible picture. Decoder allocations
        // include macroblock padding whose neutral chroma must not bleed in.
        params[3][2] = desc.width as f32;
        params[3][3] = desc.height as f32;
        let bytes: Vec<u8> = params
            .iter()
            .flatten()
            .flat_map(|f| f.to_ne_bytes())
            .collect();
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("H.264 color metadata"),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let views = [
            imported.planes[0].create_view(&Default::default()),
            imported.planes[1].create_view(&Default::default()),
        ];
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.conversion.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let view = output.texture.create_view(&Default::default());
        let resources = Arc::new((frame, imported));
        // wgpu 30 forbids mixing raw and safe commands in one encoder. Keep
        // acquire, conversion, and release in separate ordered submissions.
        let mut acquire = self.encoder();
        resources.1.acquire(&mut acquire);
        self.submit(acquire, Arc::clone(&resources))?;
        let mut encoder = self.encoder();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("H.264 GPU color conversion"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.conversion);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        self.submit(encoder, Arc::clone(&resources))?;
        let mut release = self.encoder();
        resources.1.release(&mut release);
        self.submit(release, resources)?;
        Ok(output)
    }
}

impl Avc444Pipelines {
    fn new(device: &wgpu::Device) -> Self {
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("AVC444"),
            entries: &[
                texture(0),
                texture(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("AVC444"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("AVC444"),
            source: wgpu::ShaderSource::Wgsl(include_str!("yuv444.wgsl").into()),
        });
        let pipeline = |entry: &str, write_mask| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Self {
            main: pipeline("main_view", wgpu::ColorWrites::ALL),
            // The auxiliary view never touches luma.
            auxiliary: pipeline(
                "auxiliary_view",
                wgpu::ColorWrites::GREEN | wgpu::ColorWrites::BLUE,
            ),
            convert: pipeline("convert", wgpu::ColorWrites::ALL),
            layout,
        }
    }
}

impl GpuContext {
    /// A new 4:4:4 store, black with neutral chroma.
    pub fn store_444(&self, width: u32, height: u32) -> anyhow::Result<Store444> {
        let image = self.image(width, height);
        self.clear(&image, [0, 128, 128, 255])?;
        Ok(Store444 {
            image,
            color: color_parameters(1, true, 0),
        })
    }

    fn uniform_444(&self, layout: [f32; 4], color: &[[f32; 4]; 4]) -> wgpu::Buffer {
        let rows = [layout, color[0], color[1], color[2]];
        let bytes: Vec<u8> = rows
            .iter()
            .flatten()
            .flat_map(|f| f.to_ne_bytes())
            .collect();
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("AVC444 parameters"),
                contents: &bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            })
    }

    /// Writes one decoded view into `store` within `rects` (picture coordinates). `auxiliary`
    /// selects the chroma view and its layout; `None` is the main (luma) view.
    pub fn apply_view(
        &self,
        frame: HardwareFrame,
        store: &mut Store444,
        rects: &[Rect],
        auxiliary: Option<Layout>,
    ) -> anyhow::Result<()> {
        ensure!(
            matches!(frame.desc.colorspace, 1 | 2 | 4 | 5 | 6 | 7 | 9),
            "unsupported H.264 color matrix"
        );
        if auxiliary.is_none() {
            store.color = color_parameters(frame.desc.colorspace, frame.desc.full_range != 0, 0);
        }
        let imported = import::import(self, &frame)?;
        let v2 = if auxiliary == Some(Layout::V2) {
            1.0
        } else {
            0.0
        };
        let uniform = self.uniform_444([0.0, 0.0, store.image.width as f32, v2], &store.color);
        let views = [
            imported.planes[0].create_view(&Default::default()),
            imported.planes[1].create_view(&Default::default()),
        ];
        let bind = self.bind_444(&views[0], &views[1], &uniform);
        let target = store.image.texture.create_view(&Default::default());
        let pipeline = if auxiliary.is_some() {
            &self.avc444.auxiliary
        } else {
            &self.avc444.main
        };
        let bounds = (
            frame.desc.width.min(store.image.width),
            frame.desc.height.min(store.image.height),
        );
        let resources = Arc::new((frame, imported));
        let mut acquire = self.encoder();
        resources.1.acquire(&mut acquire);
        self.submit(acquire, Arc::clone(&resources))?;
        let mut encoder = self.encoder();
        Self::draw_rects(
            &mut encoder,
            &target,
            pipeline,
            &bind,
            rects,
            (0, 0),
            bounds,
        );
        self.submit(encoder, Arc::clone(&resources))?;
        let mut release = self.encoder();
        resources.1.release(&mut release);
        self.submit(release, resources)
    }

    /// Converts `rects` of the store to RGB into `target`, with the picture origin at (x, y).
    pub fn render_444(
        &self,
        store: &Store444,
        rects: &[Rect],
        target: &GpuImage,
        x: u32,
        y: u32,
    ) -> anyhow::Result<()> {
        let uniform = self.uniform_444([x as f32, y as f32, 0.0, 0.0], &store.color);
        let source = store.image.texture.create_view(&Default::default());
        let bind = self.bind_444(&source, &source, &uniform);
        let view = target.texture.create_view(&Default::default());
        let bounds = (
            target.width.min(x + store.image.width),
            target.height.min(y + store.image.height),
        );
        let mut encoder = self.encoder();
        Self::draw_rects(
            &mut encoder,
            &view,
            &self.avc444.convert,
            &bind,
            rects,
            (x, y),
            bounds,
        );
        self.submit(encoder, (uniform, Arc::clone(&store.image)))
    }

    fn bind_444(
        &self,
        first: &wgpu::TextureView,
        second: &wgpu::TextureView,
        uniform: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("AVC444"),
            layout: &self.avc444.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(first),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(second),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        })
    }

    /// One full-target triangle per rect, scissored to the rect (offset, clipped to `bounds`).
    fn draw_rects(
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        pipeline: &wgpu::RenderPipeline,
        bind: &wgpu::BindGroup,
        rects: &[Rect],
        offset: (u32, u32),
        bounds: (u32, u32),
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("AVC444"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind, &[]);
        for rect in rects {
            let moved = Rect::new(rect.x + offset.0, rect.y + offset.1, rect.w, rect.h);
            if let Some(r) = moved.clip(bounds.0, bounds.1) {
                pass.set_scissor_rect(r.x, r.y, r.w, r.h);
                pass.draw(0..3, 0..1);
            }
        }
    }
}

/// FFmpeg's AVColorSpace and AVChromaLocation values. Unspecified RDP streams
/// use BT.709 full range; explicit stream metadata always takes precedence.
fn color_parameters(space: i32, full: bool, chroma: i32) -> [[f32; 4]; 4] {
    let (kr, kb) = match space {
        4 => (0.30, 0.11),
        5 | 6 => (0.299, 0.114),
        7 => (0.212, 0.087),
        9 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let ys = if full { 1.0 } else { 255.0 / 219.0 };
    let cs = if full { 1.0 } else { 255.0 / 224.0 };
    let yo = if full { 0.0 } else { 16.0 / 255.0 };
    let co = 128.0 / 255.0;
    let r = 2.0 * (1.0 - kr) * cs;
    let b = 2.0 * (1.0 - kb) * cs;
    let gu = -2.0 * kb * (1.0 - kb) / kg * cs;
    let gv = -2.0 * kr * (1.0 - kr) / kg * cs;
    let shift_x = if matches!(chroma, 1 | 3 | 5) {
        0.5
    } else {
        0.0
    };
    let shift_y = match chroma {
        3 | 4 => 0.5,
        5 | 6 => -0.5,
        _ => 0.0,
    };
    [
        [ys, 0.0, r, -ys * yo - r * co],
        [ys, gu, gv, -ys * yo - (gu + gv) * co],
        [ys, b, 0.0, -ys * yo - b * co],
        [shift_x, shift_y, 0.0, 0.0],
    ]
}

impl Drop for GpuContext {
    fn drop(&mut self) {
        // Retire the final few frames even when no further redraw occurs. This
        // also releases the AVFrame references owned by completion callbacks.
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(5)),
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::session::gfx::{
        surface::{SrcFormat, Surface},
        vaapi,
    };

    pub(crate) fn simulate_failure(context: &GpuContext) {
        context
            .failure
            .lock()
            .replace("injected hardware decoder failure".to_owned());
    }

    pub(crate) fn context() -> anyhow::Result<Arc<GpuContext>> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))?;
        let (device, queue) = pollster::block_on(open_device(&adapter))?;
        GpuContext::from_device(device, queue)
    }

    /// Test-only GPU readback. The production decoding path never maps pixels.
    pub(crate) fn read(context: &GpuContext, image: &GpuImage) -> Vec<u8> {
        let stride = (image.width * 4).div_ceil(256) * 256;
        let buffer = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(stride * image.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = context.encoder();
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &image.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );
        context.submit(encoder, ()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        context
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        rx.recv().unwrap().unwrap();
        let mapped = buffer.slice(..).get_mapped_range().unwrap();
        mapped
            .chunks(stride as usize)
            .flat_map(|row| row[..image.width as usize * 4].iter().copied())
            .collect()
    }

    #[test]
    fn conversion_respects_range_and_matrix() {
        for space in [1, 5, 9] {
            let params = color_parameters(space, false, 1);
            for (y, expected) in [(16.0, 0.0), (235.0, 1.0)] {
                for row in &params[..3] {
                    let rgb = row[0] * y / 255.0 + (row[1] + row[2]) * 128.0 / 255.0 + row[3];
                    assert!((rgb - expected).abs() < 0.00001);
                }
            }
        }
        assert_ne!(color_parameters(1, true, 2), color_parameters(5, true, 2));
    }

    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_composition_matches_cpu_reference() {
        let context = context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let mut gpu = Surface::new_gpu(32, 24, context.clone(), error.clone());
        let mut cpu = Surface::new(32, 24);
        for surface in [&mut cpu, &mut gpu] {
            surface.fill(Rect::new(0, 0, 32, 24), [30, 40, 50, 255]);
            surface.fill(Rect::new(3, 4, 10, 8), [200, 100, 50, 255]);
            surface.write(
                Rect::new(6, 7, 4, 2),
                &[10, 20, 30, 0].repeat(8),
                16,
                SrcFormat::Bgra,
            );
            let snapshot = surface.snapshot(Rect::new(3, 4, 10, 8)).unwrap();
            surface.copy_from(&snapshot, 5, 6); // Overlapping surface copy.
            let cached = surface.snapshot(Rect::new(5, 6, 10, 8)).unwrap();
            surface.fill(Rect::new(5, 6, 10, 8), [0, 0, 0, 255]);
            surface.copy_from(&cached, 22, 20); // Clipped cache restoration.
        }
        assert_eq!(read(&context, &gpu.gpu.as_ref().unwrap().image), cpu.data);
        assert!(error.lock().is_none(), "{:?}", error.lock());
        // Resize/output publication uses a fresh texture rather than a stale ID.
        let output = context.image(64, 48);
        context.clear(&output, [0, 0, 0, 255]).unwrap();
        let mut encoder = context.encoder();
        GpuContext::copy(
            &mut encoder,
            &gpu.gpu.as_ref().unwrap().image,
            Rect::new(0, 0, 32, 24),
            &output,
            8,
            8,
        );
        context.submit(encoder, ()).unwrap();
        let result = read(&context, &output);
        assert_eq!(&result[(8 * 64 + 8) * 4..(8 * 64 + 9) * 4], &cpu.data[..4]);
    }

    #[test]
    #[ignore = "requires a VA-API/Vulkan GPU; reports a synthetic 1080p comparison"]
    fn hardware_software_latency_comparison() {
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, VuiConfig};
        use openh264::formats::YUVBuffer;
        fn api() -> openh264::OpenH264API {
            // SAFETY: pinned system OpenH264 ABI.
            unsafe { openh264::OpenH264API::from_blob_path_unchecked("/usr/lib/libopenh264.so.8") }
                .unwrap()
        }
        fn thread_cpu() -> Duration {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: valid output pointer and a supported Linux clock.
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
                0
            );
            Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
        }
        let context = context().unwrap();
        let (width, height) = (1920, 1080);
        let mut encoder = Encoder::with_api_config(
            api(),
            EncoderConfig::new()
                .vui(VuiConfig::bt601())
                .bitrate(BitRate::from_bps(12_000_000)),
        )
        .unwrap();
        let mut packets = Vec::new();
        for step in 0..12 {
            let mut data = vec![128; width * height * 3 / 2];
            for y in 0..height {
                for x in 0..width {
                    data[y * width + x] = if x / 32 == step {
                        230
                    } else {
                        (32 + (x / 32 + y / 32) % 160) as u8
                    };
                }
            }
            packets.push(
                encoder
                    .encode(&YUVBuffer::from_vec(data, width, height))
                    .unwrap()
                    .to_vec(),
            );
        }
        let mut hardware = vaapi::Decoder::new(&context.render_node).unwrap();
        let mut software = openh264::decoder::Decoder::with_api_config(
            api(),
            openh264::decoder::DecoderConfig::new(),
        )
        .unwrap();
        let software_image = context.image(width as u32, height as u32);
        let mut rgba = vec![0; width * height * 4];
        for accelerated in [true, false] {
            let mut wall = Duration::ZERO;
            let mut cpu = Duration::ZERO;
            for round in 0..4 {
                for packet in &packets {
                    let begin_cpu = thread_cpu();
                    let begin = std::time::Instant::now();
                    if accelerated {
                        let frame = hardware.decode(packet).unwrap().expect("picture");
                        let _image = context.convert(frame).unwrap();
                    } else {
                        let frame = software.decode(packet).unwrap().expect("picture");
                        frame.write_rgba8(&mut rgba);
                        context
                            .upload(
                                &software_image,
                                Rect::new(0, 0, width as u32, height as u32),
                                &rgba,
                                width as u32 * 4,
                            )
                            .unwrap();
                    }
                    context
                        .device
                        .poll(wgpu::PollType::wait_indefinitely())
                        .unwrap();
                    if round > 0 {
                        wall += begin.elapsed();
                        cpu += thread_cpu() - begin_cpu;
                    }
                }
            }
            let count = (packets.len() * 3) as f64;
            eprintln!(
                "1080p {}: {:.3} ms/frame elapsed, {:.3} ms/frame decoding-thread CPU",
                if accelerated {
                    "VA-API + GPU conversion"
                } else {
                    "OpenH264 + RGBA upload"
                },
                wall.as_secs_f64() * 1000.0 / count,
                cpu.as_secs_f64() * 1000.0 / count
            );
        }
    }

    #[test]
    #[ignore = "requires VA-API H.264 decoding, Vulkan DMA-BUF import, and system OpenH264"]
    fn hardware_avc444_matches_software() {
        use crate::session::gfx::avc444::tests::{picture, split};
        use crate::session::gfx::avc444::{Planes, Yuv444};
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType};
        use openh264::formats::{YUVBuffer, YUVSource as _};
        let context = context().unwrap();
        // SAFETY: matches the ABI used by the application's software decoder.
        let api = || unsafe {
            openh264::OpenH264API::from_blob_path_unchecked("/usr/lib/libopenh264.so.8").unwrap()
        };
        for layout in [Layout::V1, Layout::V2] {
            for (w, h) in [(64usize, 64usize), (98, 82)] {
                // One stream carries both views, as in RDP.
                let config = EncoderConfig::new()
                    .usage_type(UsageType::ScreenContentRealTime)
                    .bitrate(BitRate::from_bps(24_000_000))
                    .max_frame_rate(FrameRate::from_hz(60.0))
                    .skip_frames(false);
                let mut encoder = Encoder::with_api_config(api(), config).unwrap();
                let mut hardware = vaapi::Decoder::new(&context.render_node).unwrap();
                let mut software = openh264::decoder::Decoder::with_api_config(
                    api(),
                    openh264::decoder::DecoderConfig::new(),
                )
                .unwrap();
                let views = split(w, h, &picture(w, h), layout);
                let (pw, ph) = (views.y_stride, views.main[0].len() / views.y_stride);
                let whole = Rect::new(0, 0, w as u32, h as u32);
                let mut store = context.store_444(w as u32, h as u32).unwrap();
                let mut reference = Yuv444::new(w, h);
                // A second round makes each view a predicted picture.
                for _ in 0..2 {
                    for (planes, auxiliary) in [(&views.main, None), (&views.aux, Some(layout))] {
                        let yuv = YUVBuffer::from_vec(planes.concat(), pw, ph);
                        let bytes = encoder.encode(&yuv).unwrap().to_vec();
                        let frame = hardware.decode(&bytes).unwrap().expect("low-delay picture");
                        let decoded = software.decode(&bytes).unwrap().expect("software picture");
                        let (y_stride, uv_stride, _) = decoded.strides();
                        let planes = Planes {
                            y: decoded.y(),
                            u: decoded.u(),
                            v: decoded.v(),
                            y_stride,
                            uv_stride,
                        };
                        match auxiliary {
                            None => reference.apply_luma(&planes, whole),
                            Some(layout) => reference.apply_chroma(&planes, whole, layout),
                        }
                        context
                            .apply_view(frame, &mut store, &[whole], auxiliary)
                            .unwrap();
                    }
                }
                // Draw at an offset, as onto a surface.
                let (ox, oy) = (6, 4);
                let target = context.image(w as u32 + ox, h as u32 + oy);
                context
                    .render_444(&store, &[whole], &target, ox, oy)
                    .unwrap();
                let actual = read(&context, &target);
                let mut expected = Vec::new();
                reference.to_rgba(whole, &mut expected).unwrap();
                let stride = w + ox as usize;
                for y in 0..h {
                    for x in 0..w {
                        let a = &actual[((y + oy as usize) * stride + x + ox as usize) * 4..][..4];
                        let e = &expected[(y * w + x) * 4..][..4];
                        for channel in 0..3 {
                            assert!(
                                (i32::from(a[channel]) - i32::from(e[channel])).abs() <= 2,
                                "{layout:?} {w}x{h} at ({x}, {y}): GPU {a:?} vs CPU {e:?}"
                            );
                        }
                        assert_eq!(a[3], 255);
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "requires VA-API H.264 decoding, Vulkan DMA-BUF import, and system OpenH264"]
    fn hardware_decode_import_and_reset() {
        use openh264::encoder::{Encoder, EncoderConfig, VuiConfig};
        use openh264::formats::YUVBuffer;
        let context = context().unwrap();
        eprintln!("Testing GPU decoder on {}", context.render_node);
        // SAFETY: matches the ABI used by the application's software decoder.
        let api =
            unsafe { openh264::OpenH264API::from_blob_path_unchecked("/usr/lib/libopenh264.so.8") }
                .unwrap();
        let mut encoder =
            Encoder::with_api_config(api, EncoderConfig::new().vui(VuiConfig::bt601())).unwrap();
        let initial_fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
        for (width, height) in [(64, 64), (98, 82), (64, 64)] {
            let mut decoder = vaapi::Decoder::new(&context.render_node).unwrap();
            // SAFETY: same known system ABI as the encoder above.
            let api = unsafe {
                openh264::OpenH264API::from_blob_path_unchecked("/usr/lib/libopenh264.so.8")
            }
            .unwrap();
            let mut software = openh264::decoder::Decoder::with_api_config(
                api,
                openh264::decoder::DecoderConfig::new(),
            )
            .unwrap();
            encoder.force_intra_frame();
            let mut pixels = vec![126u8; width * height * 3 / 2];
            pixels[width * height..width * height * 5 / 4].fill(90);
            pixels[width * height * 5 / 4..].fill(200);
            let yuv = YUVBuffer::from_vec(pixels, width, height);
            for _ in 0..8 {
                let bytes = encoder.encode(&yuv).unwrap().to_vec();
                let frame = decoder.decode(&bytes).unwrap().expect("low-delay picture");
                let reference = software
                    .decode(&bytes)
                    .unwrap()
                    .expect("software reference picture");
                let mut expected = vec![0; width * height * 4];
                reference.write_rgba8(&mut expected);
                assert_eq!(
                    (frame.desc.width, frame.desc.height),
                    (width as u32, height as u32)
                );
                let image = context.convert(frame).unwrap();
                let pixels = read(&context, &image);
                for (actual, expected) in pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(expected.as_chunks::<4>().0)
                {
                    for channel in 0..3 {
                        assert!(
                            (i32::from(actual[channel]) - i32::from(expected[channel])).abs() <= 8,
                            "GPU/reference pixel mismatch: {actual:?} vs {expected:?}"
                        );
                    }
                    assert_eq!(actual[3], 255);
                }
            }
        }
        context
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let final_fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
        assert!(
            final_fds <= initial_fds + 4,
            "descriptor leak: {initial_fds} -> {final_fds}"
        );
    }
}
