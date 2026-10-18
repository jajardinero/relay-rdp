//! Shared GPU compositor. The session thread submits offscreen work; egui samples
//! the completed desktop on the same queue. Video pixels never enter CPU memory.
mod import;

use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, ensure};
use ash::vk;
use egui_wgpu::{RenderState, WgpuSetup, WgpuSetupExisting, wgpu};
use parking_lot::{Mutex, RwLock};
use wgpu::util::DeviceExt;

use crate::session::framebuffer::Rect;
use crate::session::gfx::avc444::Layout;
use crate::session::gfx::vaapi::HardwareFrame;

pub struct GpuImage {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    /// Binds the image for quads that copy from it; made on first use.
    source: OnceLock<wgpu::BindGroup>,
}

/// Staging memory is allocated in chunks of this size (a 1080p frame of tiles is about 8 MiB).
const STAGING_CHUNK: u64 = 8 << 20;
/// A batch that has staged this much upload data is submitted without waiting for the frame end.
const MAX_BATCH_BYTES: u64 = 64 << 20;
/// Uploads this large are copied into the image; smaller ones are drawn as quads.
const DIRECT_UPLOAD_BYTES: usize = 1 << 20;
/// A batch's quad data is staged in pieces of at least this size.
const STREAM_PIECE: u64 = 1 << 20;
/// The quad data starts with the batch's header: the progressive tile table's first word.
const STREAM_HEADER: u64 = 32;
/// The smallest buffer for quad data; it doubles when a batch's data doesn't fit.
const MIN_STORAGE: u64 = 4 << 20;
/// Workgroup memory for reconstructing progressive tiles: a component's samples and one
/// level's rows, 4096 i32 each (`progressive.wgsl`).
const PROGRESSIVE_WORKGROUP_BYTES: u32 = 2 * 4096 * 4;

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    /// The VA-API render node for this adapter; none when decoded frames can't be imported.
    pub render_node: Option<String>,
    pending: Mutex<VecDeque<wgpu::SubmissionIndex>>,
    batch: Mutex<Batch>,
    /// Staging area for copies within one image, which wgpu can't do directly.
    scratch: Mutex<Option<Arc<GpuImage>>>,
    conversion: wgpu::RenderPipeline,
    quads: wgpu::RenderPipeline,
    /// Bound as the source while quads copy nothing, since the target can't be.
    blank: wgpu::BindGroup,
    /// None when the device has too little workgroup memory to reconstruct progressive tiles.
    progressive: Option<ProgressivePipelines>,
    avc444: Avc444Pipelines,
    failure: Arc<Mutex<Option<String>>>,
    /// Held shared around each submission and exclusively by [`exclusive`](Self::exclusive).
    submit_lock: RwLock<()>,
}

/// Commands recorded since the last submission. Surface operations append here; the session
/// submits once per RDP frame, and any other submission sends the batch first to keep order.
struct Batch {
    encoder: Option<wgpu::CommandEncoder>,
    belt: wgpu::util::StagingBelt,
    /// Upload bytes staged in this batch.
    staged: u64,
    /// One converted row on its way to staging memory, which is write-only.
    line: Vec<u8>,
    /// What the batch's quads read.
    stream: Stream,
    pending: Quads,
    /// The buffer quads and progressive tiles read; replaced only between batches.
    storage: Option<QuadStorage>,
}

/// Fills, small uploads, copies from other images and progressive tiles waiting to be drawn
/// into one image, as instanced quads in a single render pass. Unlike copies into the same
/// image, draws need no barrier between them.
#[derive(Default)]
struct Quads {
    target: Option<Arc<GpuImage>>,
    /// x, y, width, height, kind, then a packed RGBA color (fills), the first word of the
    /// quad's pixels (uploads), the source x and y (copies), or the tile's first word and the
    /// quad's position in it (progressive tiles); see `quads.wgsl`. The last word, the
    /// target's size, is filled in when they are drawn.
    items: Vec<[u32; 8]>,
    /// The images copies read: each from its first item until the next. Items before the
    /// first read none.
    sources: Vec<(u32, wgpu::BindGroup)>,
}

/// What a batch's quads read: uploaded pixels, progressive tiles' coefficients, and the
/// quads themselves. It goes straight into staging memory as it arrives, and into the storage
/// buffer in one copy ahead of the batch's commands. Each render pass reads its own part, so
/// passes never wait on one another's data.
#[derive(Default)]
struct Stream {
    /// Staging memory, in storage buffer order.
    pieces: Vec<Piece>,
    /// The storage buffer bytes in use.
    len: u64,
    /// Progressive tiles' first words: standard, then reduce-extrapolate DWT.
    tiles: [Vec<u32>; 2],
}

/// Staging memory for a run of the storage buffer.
struct Piece {
    buffer: wgpu::Buffer,
    offset: u64,
    /// Where it goes in the storage buffer.
    at: u64,
    size: u64,
    /// The bytes written.
    used: u64,
}

impl Stream {
    /// Reserves `size` bytes (a multiple of 4) at a multiple of `align` (at least 8) in the
    /// storage buffer: that place, and the staging memory to write.
    fn reserve(
        &mut self,
        belt: &mut wgpu::util::StagingBelt,
        size: u64,
        align: u64,
    ) -> (u64, wgpu::BufferSlice<'_>) {
        if self.pieces.is_empty() {
            self.len = STREAM_HEADER;
            self.add_piece(belt, 0, STREAM_HEADER + size);
        }
        let at = self.len.next_multiple_of(align);
        let last = self.pieces.last().expect("the first piece");
        if at + size > last.at + last.size {
            self.add_piece(belt, at, size);
        }
        let piece = self.pieces.last_mut().expect("the first piece");
        piece.used = at + size - piece.at;
        self.len = at + size;
        let start = piece.offset + (at - piece.at);
        (at, piece.buffer.slice(start..start + size))
    }

    fn add_piece(&mut self, belt: &mut wgpu::util::StagingBelt, at: u64, size: u64) {
        let size = size.max(STREAM_PIECE).next_multiple_of(8);
        let slice = belt.allocate(
            NonZeroU64::new(size).expect("at least a piece"),
            NonZeroU64::new(8).expect("non-zero"),
        );
        self.pieces.push(Piece {
            buffer: slice.buffer().clone(),
            offset: slice.offset(),
            at,
            size,
            used: 0,
        });
    }

    /// Writes `bytes` (a multiple of 4) at a multiple of `align`; returns their first word.
    fn write(
        &mut self,
        belt: &mut wgpu::util::StagingBelt,
        bytes: &[u8],
        align: u64,
    ) -> anyhow::Result<u32> {
        let (at, slice) = self.reserve(belt, bytes.len() as u64, align);
        slice.get_mapped_range_mut()?.copy_from_slice(bytes);
        Ok((at / 4) as u32)
    }
}

struct QuadStorage {
    buffer: wgpu::Buffer,
    /// For the quads, which read it.
    draw: wgpu::BindGroup,
    /// For progressive tiles' reconstruction, which rewrites their coefficients as samples.
    reconstruct: Option<wgpu::BindGroup>,
}

/// The compute pipelines that reconstruct progressive tiles: standard, then
/// reduce-extrapolate DWT.
struct ProgressivePipelines {
    layout: wgpu::BindGroupLayout,
    pipelines: [wgpu::ComputePipeline; 2],
}

const FILL: u32 = 0;
const PIXELS: u32 = 1;
const COPY: u32 = 2;
const TILE: u32 = 3;

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
            // Progressive tiles reconstruct on the GPU only with this much (see `progressive`).
            max_compute_workgroup_storage_size: adapter
                .limits()
                .max_compute_workgroup_storage_size
                .clamp(
                    wgpu::Limits::default().max_compute_workgroup_storage_size,
                    PROGRESSIVE_WORKGROUP_BYTES,
                ),
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

/// The VA-API render node matching the device's adapter, when VA-API frames can be imported.
fn interop_node(device: &wgpu::Device) -> anyhow::Result<String> {
    ensure!(
        device
            .features()
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
        "Vulkan DMA-BUF import is unavailable on this adapter"
    );
    // SAFETY: only querying physical-device properties and extension names.
    unsafe {
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
        Ok(std::fs::read_dir("/dev/dri")
            .context("No DRM render devices available")?
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_name().to_string_lossy().starts_with("renderD")
                    && entry.metadata().is_ok_and(|m| m.rdev() == expected)
            })
            .context("No matching VA-API render node for the Vulkan adapter")?
            .path()
            .to_string_lossy()
            .into_owned())
    }
}

/// The size of a copy of `rect` from `src` to (x, y) in `dst`, clipped to both images.
fn copy_size(src: &GpuImage, rect: Rect, dst: &GpuImage, x: u32, y: u32) -> Option<(u32, u32)> {
    let w = rect
        .w
        .min(src.width.saturating_sub(rect.x))
        .min(dst.width.saturating_sub(x));
    let h = rect
        .h
        .min(src.height.saturating_sub(rect.y))
        .min(dst.height.saturating_sub(y));
    (w > 0 && h > 0).then_some((w, h))
}

/// Binds `view` as the image the quad pipeline copies from.
fn source_group(
    device: &wgpu::Device,
    quads: &wgpu::RenderPipeline,
    view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("RDP quads source"),
        layout: &quads.get_bind_group_layout(1),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(view),
        }],
    })
}

impl GpuContext {
    pub fn new(state: &RenderState) -> Option<Arc<Self>> {
        let storage = wgpu::DownlevelFlags::VERTEX_STORAGE
            | wgpu::DownlevelFlags::FRAGMENT_STORAGE
            | wgpu::DownlevelFlags::COMPUTE_SHADERS;
        if !state
            .adapter
            .get_downlevel_capabilities()
            .flags
            .contains(storage)
        {
            tracing::info!("GPU drawing unavailable: no compute shaders or storage buffers");
            return None;
        }
        Some(Self::from_device(state.device.clone(), state.queue.clone()))
    }

    /// Drawing needs only the device; VA-API decoding also needs DMA-BUF interop.
    fn from_device(device: wgpu::Device, queue: wgpu::Queue) -> Arc<Self> {
        let render_node = interop_node(&device)
            .inspect_err(|error| {
                tracing::info!(%error, "hardware decoding unavailable; the GPU still draws")
            })
            .ok();
        Arc::new(Self::with_device(device, queue, render_node))
    }

    /// The render node for VA-API decoding, if decoded frames can be shared with this device.
    pub fn render_node(&self) -> anyhow::Result<&str> {
        self.render_node
            .as_deref()
            .context("Vulkan/VA-API interop unavailable")
    }

    fn with_device(device: wgpu::Device, queue: wgpu::Queue, render_node: Option<String>) -> Self {
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
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("RDP quads"),
            source: wgpu::ShaderSource::Wgsl(include_str!("quads.wgsl").into()),
        });
        let quads = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("RDP quads"),
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
        let blank = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("RDP quads blank source"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let blank = source_group(&device, &quads, &blank.create_view(&Default::default()));
        let progressive = (device.limits().max_compute_workgroup_storage_size
            >= PROGRESSIVE_WORKGROUP_BYTES)
            .then(|| ProgressivePipelines::new(&device));
        if progressive.is_none() {
            tracing::info!("progressive tiles reconstruct on the CPU: too little workgroup memory");
        }
        let avc444 = Avc444Pipelines::new(&device);
        let belt = wgpu::util::StagingBelt::new(device.clone(), STAGING_CHUNK);
        Self {
            device,
            queue,
            render_node,
            pending: Mutex::new(VecDeque::new()),
            batch: Mutex::new(Batch {
                encoder: None,
                belt,
                staged: 0,
                line: Vec::new(),
                stream: Stream::default(),
                pending: Quads::default(),
                storage: None,
            }),
            scratch: Mutex::new(None),
            conversion,
            quads,
            blank,
            progressive,
            avc444,
            failure,
            submit_lock: RwLock::new(()),
        }
    }

    /// Runs `f` while no session submits GPU work. Reconfiguring the window's surface waits for
    /// the queue to drain, and wgpu fails it if another thread submits meanwhile: the window
    /// keeps its old size while egui draws at the new one, failing every frame.
    pub fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R {
        let _drained = self.submit_lock.write();
        f()
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
            view: texture.create_view(&Default::default()),
            texture,
            width,
            height,
            source: OnceLock::new(),
        })
    }

    /// The bind group quads copy `image` through.
    fn source(&self, image: &GpuImage) -> wgpu::BindGroup {
        image
            .source
            .get_or_init(|| source_group(&self.device, &self.quads, &image.view))
            .clone()
    }

    pub fn encoder(&self) -> wgpu::CommandEncoder {
        self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("RDP composition"),
            })
    }

    /// Submits `encoder` after everything recorded so far. Keeps at most three
    /// submissions in flight. Polling here also retires video surfaces while
    /// the tab is hidden or the UI is idle.
    pub fn submit(
        &self,
        encoder: wgpu::CommandEncoder,
        hold: impl Send + 'static,
    ) -> anyhow::Result<()> {
        let mut batch = self.batch.lock();
        let recorded = self.finish_batch(&mut batch);
        self.submit_buffers(recorded.into_iter().chain([encoder.finish()]), hold)
    }

    /// Submits everything recorded so far, if anything.
    pub fn flush(&self) -> anyhow::Result<()> {
        let mut batch = self.batch.lock();
        self.flush_batch(&mut batch)
    }

    fn flush_batch(&self, batch: &mut Batch) -> anyhow::Result<()> {
        let buffers = self.finish_batch(batch);
        if buffers.is_empty() {
            return Ok(());
        }
        self.submit_buffers(buffers, ())
    }

    /// Ends the batch: its quad data's upload, then its commands. The caller submits them
    /// while still holding the batch lock.
    fn finish_batch(&self, batch: &mut Batch) -> Vec<wgpu::CommandBuffer> {
        self.draw_quads(batch);
        let upload = self.upload_stream(batch);
        let encoder = batch.encoder.take();
        if let Some(last) = encoder.as_ref().or(upload.as_ref()) {
            batch.belt.finish_and_recall_on_submit(last);
        }
        batch.staged = 0;
        upload
            .into_iter()
            .chain(encoder)
            .map(wgpu::CommandEncoder::finish)
            .collect()
    }

    /// Records the copy of the batch's quad data into the storage buffer, then the
    /// reconstruction of its progressive tiles, to run before the batch's commands.
    fn upload_stream(&self, batch: &mut Batch) -> Option<wgpu::CommandEncoder> {
        let Batch {
            belt,
            stream,
            storage,
            ..
        } = batch;
        if stream.pieces.is_empty() {
            return None;
        }
        let storage = storage
            .as_ref()
            .expect("quad data has room in a storage buffer");
        let counts = stream.tiles.each_ref().map(|tiles| tiles.len() as u32);
        let mut header = [0u32; (STREAM_HEADER / 4) as usize];
        if counts != [0, 0] {
            // The tile table: the number of standard tiles, then every tile's first word.
            let [standard, extrapolate] = &stream.tiles;
            let table: Vec<u8> = [counts[0]]
                .iter()
                .chain(standard)
                .chain(extrapolate)
                .flat_map(|word| word.to_le_bytes())
                .collect();
            match stream.write(belt, &table, 8) {
                Ok(word) => header[0] = word,
                Err(error) => self.fail(error),
            }
        }
        let first = &stream.pieces[0];
        let header_bytes: Vec<u8> = header.iter().flat_map(|w| w.to_le_bytes()).collect();
        match first
            .buffer
            .slice(first.offset..first.offset + STREAM_HEADER)
            .get_mapped_range_mut()
        {
            Ok(mut view) => view.copy_from_slice(&header_bytes),
            Err(error) => self.fail(error.into()),
        }
        let mut encoder = self.encoder();
        for piece in &stream.pieces {
            encoder.copy_buffer_to_buffer(
                &piece.buffer,
                piece.offset,
                &storage.buffer,
                piece.at,
                piece.used,
            );
        }
        // Progressive tiles become samples, one workgroup per tile and component. Only a
        // context with the pipelines takes tiles.
        if let (true, Some(progressive), Some(bind)) =
            (counts != [0, 0], &self.progressive, &storage.reconstruct)
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("RemoteFX progressive tiles"),
                timestamp_writes: None,
            });
            pass.set_bind_group(0, bind, &[]);
            for (pipeline, count) in progressive.pipelines.iter().zip(counts) {
                if count > 0 {
                    pass.set_pipeline(pipeline);
                    pass.dispatch_workgroups(count, 3, 1);
                }
            }
        }
        *stream = Stream::default();
        Some(encoder)
    }

    fn fail(&self, error: anyhow::Error) {
        self.failure.lock().get_or_insert_with(|| error.to_string());
    }

    /// Makes room in the storage buffer for `bytes` more data and `quads` more quads of this
    /// batch, submitting the batch first when they wouldn't fit. The buffer is replaced, with
    /// a bigger one, only between batches, when no recorded pass reads it.
    fn make_room(&self, batch: &mut Batch, bytes: u64, quads: usize) -> anyhow::Result<()> {
        let need = |batch: &Batch| {
            let items = (batch.pending.items.len() + quads) as u64;
            let table = 2 + batch.stream.tiles.iter().map(Vec::len).sum::<usize>() as u64;
            // Each part may be padded to its alignment.
            batch.stream.len.max(STREAM_HEADER) + (bytes + 8) + (items * 32 + 32) + (table * 4 + 8)
        };
        let capacity = batch.storage.as_ref().map_or(0, |s| s.buffer.size());
        if need(batch) <= capacity {
            return Ok(());
        }
        let mut grow = capacity;
        let started = batch.encoder.is_some()
            || !batch.stream.pieces.is_empty()
            || batch.pending.target.is_some();
        if capacity > 0 && started {
            // This batch's passes read the current buffer. Later batches get one twice the size.
            self.draw_quads(batch);
            self.flush_batch(batch)?;
            grow = capacity * 2;
        }
        let limits = self.device.limits();
        let largest = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        let need = need(batch);
        let size = need
            .max(grow)
            .max(MIN_STORAGE)
            .next_power_of_two()
            .min(largest);
        ensure!(
            size >= need,
            "{need} bytes of quad data don't fit a storage buffer"
        );
        if size > capacity {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("RDP quads"),
                size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind = |label, layout: &wgpu::BindGroupLayout| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(label),
                    layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                })
            };
            batch.storage = Some(QuadStorage {
                draw: bind("RDP quads", &self.quads.get_bind_group_layout(0)),
                reconstruct: (self.progressive.as_ref())
                    .map(|progressive| bind("RemoteFX progressive tiles", &progressive.layout)),
                buffer,
            });
        }
        Ok(())
    }

    fn submit_buffers(
        &self,
        buffers: impl IntoIterator<Item = wgpu::CommandBuffer>,
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
        let index = {
            let _submitting = self.submit_lock.read();
            self.queue.submit(buffers)
        };
        self.queue.on_submitted_work_done(move || drop(hold));
        pending.push_back(index);
        self.device.poll(wgpu::PollType::Poll)?;
        if let Some(reason) = self.failure.lock().clone() {
            anyhow::bail!("{reason}");
        }
        Ok(())
    }

    /// Records commands into the current batch, after the pending quads.
    fn record<R>(&self, commands: impl FnOnce(&mut wgpu::CommandEncoder) -> R) -> R {
        let mut batch = self.batch.lock();
        self.draw_quads(&mut batch);
        commands(batch.encoder.get_or_insert_with(|| self.encoder()))
    }

    /// The pending quads, for drawing into `image`; quads pending for another image are
    /// drawn first.
    fn quads_for<'a>(&self, batch: &'a mut Batch, image: &Arc<GpuImage>) -> &'a mut Quads {
        if batch
            .pending
            .target
            .as_ref()
            .is_some_and(|target| !Arc::ptr_eq(target, image))
        {
            self.draw_quads(batch);
        }
        batch
            .pending
            .target
            .get_or_insert_with(|| Arc::clone(image));
        &mut batch.pending
    }

    /// Records the pending quads as one render pass, reading their own part of the batch's
    /// quad data.
    fn draw_quads(&self, batch: &mut Batch) {
        let Batch {
            encoder,
            belt,
            staged,
            stream,
            pending,
            storage,
            ..
        } = batch;
        let Some(target) = pending.target.take() else {
            return;
        };
        let size = target.width | (target.height << 16);
        let bytes: Vec<u8> = pending
            .items
            .iter()
            .flat_map(|item| item[..7].iter().chain([&size]))
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let first_word = match stream.write(belt, &bytes, 32) {
            Ok(word) => word,
            Err(error) => {
                self.fail(error);
                pending.items.clear();
                pending.sources.clear();
                return;
            }
        };
        *staged += bytes.len() as u64;
        let storage = storage.as_ref().expect("room was made for these quads");
        let first_quad = first_word / 8;
        let count = pending.items.len() as u32;
        // A first fill covering the whole image becomes the pass's clear.
        let clear = pending
            .items
            .first()
            .filter(|q| q[4] == FILL && q[..4] == [0, 0, target.width, target.height])
            .map(|q| {
                let [r, g, b, a] = q[5].to_le_bytes().map(|c| f64::from(c) / 255.0);
                wgpu::Color { r, g, b, a }
            });
        let encoder = encoder.get_or_insert_with(|| self.encoder());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("RDP quads"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: clear.map_or(wgpu::LoadOp::Load, wgpu::LoadOp::Clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.quads);
            pass.set_bind_group(0, &storage.draw, &[]);
            // One draw per source image, each from its first quad to the next source's.
            let mut first = u32::from(clear.is_some());
            let mut source = &self.blank;
            let ends = pending.sources.iter().map(|(start, s)| (*start, s));
            for (end, next) in ends.chain([(count, &self.blank)]) {
                if end > first {
                    pass.set_bind_group(1, source, &[]);
                    pass.draw(0..6, first_quad + first..first_quad + end);
                    first = end;
                }
                source = next;
            }
        }
        pending.items.clear();
        pending.sources.clear();
    }

    /// Copies `image` into a buffer the caller maps for reading (rows `stride` bytes apart),
    /// after the work submitted so far. For snapshots; frames themselves never leave the GPU.
    pub fn read_image(&self, image: &GpuImage) -> (wgpu::Buffer, u32) {
        let stride = (image.width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("RDP snapshot"),
            size: u64::from(stride) * u64::from(image.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.encoder();
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
        // Straight to the queue, not through the batch: the copy sees the last finished frame
        // and leaves a frame the session is drawing alone.
        let commands = encoder.finish();
        let _submitting = self.submit_lock.read();
        self.queue.submit([commands]);
        (buffer, stride)
    }

    /// Whether [`draw_tile`](Self::draw_tile) is available.
    pub fn reconstructs_tiles(&self) -> bool {
        self.progressive.is_some()
    }

    /// Records a RemoteFX progressive tile whose top-left corner is at (x, y) in `image`: its
    /// DWT `coefficients` (Y, Cb, Cr) become pixels on the GPU, as on the CPU, and the
    /// parts of the tile inside `parts` are drawn.
    pub fn draw_tile(
        &self,
        image: &Arc<GpuImage>,
        (x, y): (u32, u32),
        coefficients: &[[i16; 4096]; 3],
        reduce_extrapolate: bool,
        parts: &[Rect],
    ) -> anyhow::Result<()> {
        ensure!(
            self.reconstructs_tiles(),
            "progressive tiles need more workgroup memory"
        );
        let tile = Rect::new(x, y, 64, 64);
        let parts: Vec<Rect> = parts
            .iter()
            .filter_map(|part| part.intersect(tile)?.clip(image.width, image.height))
            .collect();
        if parts.is_empty() {
            return Ok(());
        }
        // The shaders read words of little-endian i16 pairs: the coefficients as they are.
        let values = coefficients.as_flattened();
        let swapped: Vec<u8>;
        let bytes: &[u8] = if cfg!(target_endian = "little") {
            bytemuck::cast_slice(values)
        } else {
            swapped = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            &swapped
        };
        let mut batch = self.batch.lock();
        self.make_room(&mut batch, bytes.len() as u64, parts.len())?;
        self.quads_for(&mut batch, image);
        let Batch {
            belt,
            stream,
            pending,
            staged,
            ..
        } = &mut *batch;
        let first = stream.write(belt, bytes, 8)?;
        *staged += bytes.len() as u64;
        stream.tiles[usize::from(reduce_extrapolate)].push(first);
        pending.items.extend(parts.iter().map(|part| {
            let at = (part.x - x) | ((part.y - y) << 16);
            [part.x, part.y, part.w, part.h, TILE, first, at, 0]
        }));
        if batch.staged >= MAX_BATCH_BYTES {
            self.flush_batch(&mut batch)?;
        }
        Ok(())
    }

    /// Records a copy of `rect` from `src` to (x, y) in `dst`, clipped to both images.
    pub fn copy(&self, src: &GpuImage, rect: Rect, dst: &Arc<GpuImage>, x: u32, y: u32) {
        if std::ptr::eq(src, &**dst) {
            return self.copy_within(dst, rect, &[(x, y)]);
        }
        let Some((w, h)) = copy_size(src, rect, dst, x, y) else {
            return;
        };
        let source = self.source(src);
        let mut batch = self.batch.lock();
        if let Err(error) = self.make_room(&mut batch, 0, 1) {
            return self.fail(error);
        }
        let quads = self.quads_for(&mut batch, dst);
        if quads.sources.last().is_none_or(|(_, last)| *last != source) {
            quads.sources.push((quads.items.len() as u32, source));
        }
        quads.items.push([x, y, w, h, COPY, rect.x, rect.y, 0]);
    }

    /// Records copies of `rect` to each of `points` in the same image, all reading the
    /// original pixels (overlapping copies included).
    pub fn copy_within(&self, image: &Arc<GpuImage>, rect: Rect, points: &[(u32, u32)]) {
        let Some(rect) = rect.clip(image.width, image.height) else {
            return;
        };
        let scratch = {
            let mut scratch = self.scratch.lock();
            match &*scratch {
                Some(s) if s.width >= rect.w && s.height >= rect.h => Arc::clone(s),
                old => {
                    let (w, h) = old.as_ref().map_or((rect.w, rect.h), |s| {
                        (s.width.max(rect.w), s.height.max(rect.h))
                    });
                    Arc::clone(scratch.insert(self.image(w, h)))
                }
            }
        };
        // Reading the image ends the pass drawing into it; the copies back start the next.
        self.record(|encoder| Self::encode_copy(encoder, image, rect, &scratch, 0, 0));
        for &(x, y) in points {
            self.copy(&scratch, Rect::new(0, 0, rect.w, rect.h), image, x, y);
        }
    }

    fn encode_copy(
        encoder: &mut wgpu::CommandEncoder,
        src: &GpuImage,
        rect: Rect,
        dst: &GpuImage,
        x: u32,
        y: u32,
    ) {
        let Some((w, h)) = copy_size(src, rect, dst, x, y) else {
            return;
        };
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

    pub fn clear(&self, image: &Arc<GpuImage>, rgba: [u8; 4]) {
        self.fill(image, &[Rect::new(0, 0, image.width, image.height)], rgba);
    }

    /// Records solid fills of `rects`.
    pub fn fill(&self, image: &Arc<GpuImage>, rects: &[Rect], rgba: [u8; 4]) {
        let rects: Vec<Rect> = rects
            .iter()
            .filter_map(|r| r.clip(image.width, image.height))
            .collect();
        if rects.is_empty() {
            return;
        }
        let color = u32::from_le_bytes(rgba);
        let mut batch = self.batch.lock();
        if let Err(error) = self.make_room(&mut batch, 0, rects.len()) {
            return self.fail(error);
        }
        let quads = self.quads_for(&mut batch, image);
        quads.items.extend(
            rects
                .iter()
                .map(|r| [r.x, r.y, r.w, r.h, FILL, color, 0, 0]),
        );
    }

    /// Records an upload of `rect` (clipped to the image). `row(i, pixels)` fills row `i` of
    /// the clipped rect as RGBA.
    pub fn upload(
        &self,
        image: &Arc<GpuImage>,
        rect: Rect,
        mut row: impl FnMut(usize, &mut [u8]),
    ) -> anyhow::Result<()> {
        let Some(rect) = rect.clip(image.width, image.height) else {
            return Ok(());
        };
        let row_bytes = rect.w as usize * 4;
        let bytes = row_bytes * rect.h as usize;
        let mut batch = self.batch.lock();
        if bytes >= DIRECT_UPLOAD_BYTES {
            self.draw_quads(&mut batch);
            self.copy_upload(&mut batch, image, rect, row)?;
        } else {
            self.make_room(&mut batch, bytes as u64, 1)?;
            self.quads_for(&mut batch, image);
            let Batch {
                belt,
                staged,
                line,
                stream,
                pending,
                ..
            } = &mut *batch;
            // Rows are converted in a small buffer, then go straight to staging memory.
            let (at, slice) = stream.reserve(belt, bytes as u64, 8);
            {
                let mut staging = slice.get_mapped_range_mut()?;
                line.resize(row_bytes, 0);
                for i in 0..rect.h as usize {
                    row(i, line);
                    let start = i * row_bytes;
                    staging
                        .slice(start..start + row_bytes)
                        .copy_from_slice(line);
                }
            }
            *staged += bytes as u64;
            let first = (at / 4) as u32;
            pending
                .items
                .push([rect.x, rect.y, rect.w, rect.h, PIXELS, first, 0, 0]);
        }
        if batch.staged >= MAX_BATCH_BYTES {
            self.flush_batch(&mut batch)?;
        }
        Ok(())
    }

    /// Records a copy of `rect`'s pixels into the image, through staging memory.
    fn copy_upload(
        &self,
        batch: &mut Batch,
        image: &GpuImage,
        rect: Rect,
        mut row: impl FnMut(usize, &mut [u8]),
    ) -> anyhow::Result<()> {
        let row_bytes = rect.w as usize * 4;
        let pitch = (rect.w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let size = u64::from(pitch) * u64::from(rect.h);
        let Batch {
            encoder,
            belt,
            staged,
            line,
            ..
        } = batch;
        let encoder = encoder.get_or_insert_with(|| self.encoder());
        let slice = belt.allocate(
            NonZeroU64::new(size).expect("clipped rects are not empty"),
            NonZeroU64::new(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT.into()).expect("non-zero"),
        );
        {
            let mut staging = slice.get_mapped_range_mut()?;
            line.resize(row_bytes, 0);
            for i in 0..rect.h as usize {
                row(i, line);
                let start = i * pitch as usize;
                staging
                    .slice(start..start + row_bytes)
                    .copy_from_slice(line);
            }
        }
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: slice.buffer(),
                layout: wgpu::TexelCopyBufferLayout {
                    offset: slice.offset(),
                    bytes_per_row: Some(pitch),
                    rows_per_image: None,
                },
            },
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
            wgpu::Extent3d {
                width: rect.w,
                height: rect.h,
                depth_or_array_layers: 1,
            },
        );
        *staged += size;
        Ok(())
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
                    view: &output.view,
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

impl ProgressivePipelines {
    fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("RemoteFX progressive tiles"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("RemoteFX progressive tiles"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("RemoteFX progressive tiles"),
            source: wgpu::ShaderSource::Wgsl(include_str!("progressive.wgsl").into()),
        });
        let pipelines = [false, true].map(|extrapolate| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("RemoteFX progressive tiles"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some("main"),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &[("REDUCE_EXTRAPOLATE", f64::from(u8::from(extrapolate)))],
                    ..Default::default()
                },
                cache: None,
            })
        });
        Self { layout, pipelines }
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
    pub fn store_444(&self, width: u32, height: u32) -> Store444 {
        let image = self.image(width, height);
        self.clear(&image, [0, 128, 128, 255]);
        Store444 {
            image,
            color: color_parameters(1, true, 0),
        }
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
            &store.image.view,
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

    /// Records the conversion of `rects` of the store to RGB into `target`, with the picture
    /// origin at (x, y).
    pub fn render_444(&self, store: &Store444, rects: &[Rect], target: &GpuImage, x: u32, y: u32) {
        let uniform = self.uniform_444([x as f32, y as f32, 0.0, 0.0], &store.color);
        let bind = self.bind_444(&store.image.view, &store.image.view, &uniform);
        let bounds = (
            target.width.min(x + store.image.width),
            target.height.min(y + store.image.height),
        );
        self.record(|encoder| {
            Self::draw_rects(
                encoder,
                &target.view,
                &self.avc444.convert,
                &bind,
                rects,
                (x, y),
                bounds,
            );
        });
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
        Ok(GpuContext::from_device(device, queue))
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
        let mut gpu_other = Surface::new_gpu(16, 12, context.clone(), error.clone());
        let mut cpu = Surface::new(32, 24);
        let mut cpu_other = Surface::new(16, 12);
        let gradient: Vec<u8> = (0..16 * 12 * 4).map(|i| i as u8).collect();
        // Every operation lands in one batch, so this also checks that they keep their order.
        for (surface, other) in [(&mut cpu, &mut cpu_other), (&mut gpu, &mut gpu_other)] {
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
            // Uploads after a snapshot must not reach it.
            surface.write(
                Rect::new(5, 6, 3, 3),
                &[1, 2, 3].repeat(9),
                9,
                SrcFormat::Rgb,
            );
            surface.copy_from(&cached, 22, 20); // Clipped cache restoration.
            let corners = [Rect::new(0, 0, 2, 2), Rect::new(30, 0, 5, 3)];
            surface.fill_rects(&corners, [7, 8, 9, 255]);
            // Overlapping copies within the surface all read the original pixels.
            surface.copy_within(Rect::new(4, 5, 9, 7), &[(6, 6), (2, 3), (25, 18)]);
            // A truncated source writes only its complete rows.
            let short = [90, 80, 70, 255].repeat(7);
            surface.write(Rect::new(1, 15, 3, 4), &short, 12, SrcFormat::Rgba);
            // Copies from another surface.
            other.write(Rect::new(0, 0, 16, 12), &gradient, 64, SrcFormat::Rgbx);
            let source = Rect::new(4, 3, 12, 9);
            match &other.gpu {
                Some(image) => surface.copy_gpu(&Arc::clone(&image.image), source, 20, 1),
                None => surface.copy_from(&other.snapshot(source).unwrap(), 20, 1),
            }
        }
        assert_eq!(read(&context, &gpu.gpu.as_ref().unwrap().image), cpu.data);
        assert!(error.lock().is_none(), "{:?}", error.lock());
        // Resize/output publication uses a fresh texture rather than a stale ID.
        let output = context.image(64, 48);
        context.clear(&output, [0, 0, 0, 255]);
        let image = &gpu.gpu.as_ref().unwrap().image;
        context.copy(image, Rect::new(0, 0, 32, 24), &output, 8, 8);
        let result = read(&context, &output);
        assert_eq!(&result[(8 * 64 + 8) * 4..(8 * 64 + 9) * 4], &cpu.data[..4]);
    }

    /// Large uploads are copied in, small ones and fills are drawn; both keep their order.
    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_large_uploads_keep_order_with_quads() {
        let context = context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let (w, h) = (640, 480);
        assert!(w as usize * h as usize * 4 >= DIRECT_UPLOAD_BYTES);
        let mut gpu = Surface::new_gpu(w, h, context.clone(), error.clone());
        let mut cpu = Surface::new(w, h);
        let picture: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        for surface in [&mut cpu, &mut gpu] {
            surface.fill(Rect::new(10, 10, 100, 100), [1, 2, 3, 255]);
            surface.write(
                Rect::new(0, 0, w, h),
                &picture,
                w as usize * 4,
                SrcFormat::Rgbx,
            );
            surface.write(Rect::new(5, 5, 4, 4), &[9; 64], 16, SrcFormat::Rgba);
            surface.fill(Rect::new(600, 400, 100, 100), [4, 5, 6, 255]);
            surface.write(
                Rect::new(0, 100, w, h - 100),
                &picture,
                w as usize * 4,
                SrcFormat::Bgra,
            );
            surface.fill(Rect::new(1, 1, 1, 1), [7, 8, 9, 255]);
        }
        assert_eq!(read(&context, &gpu.gpu.as_ref().unwrap().image), cpu.data);
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    /// Copies from other images join the quads; reads of the target itself (scrolls,
    /// snapshots) still see everything drawn before them.
    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_copies_keep_order_with_quads() {
        let context = context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let (w, h) = (96, 64);
        let picture: Vec<u8> = (0..w * h * 4).map(|i| (i % 249) as u8).collect();
        let mut entries = Vec::new();
        for color in [[200, 10, 20, 255], [30, 220, 40, 128]] {
            let mut cpu = Surface::new(16, 12);
            let mut gpu = Surface::new_gpu(16, 12, context.clone(), error.clone());
            for entry in [&mut cpu, &mut gpu] {
                entry.fill(Rect::new(0, 0, 16, 12), color);
                entry.write(Rect::new(3, 2, 4, 4), &[77; 64], 16, SrcFormat::Rgba);
            }
            entries.push((cpu, gpu));
        }
        let mut gpu = Surface::new_gpu(w, h, context.clone(), error.clone());
        let mut cpu = Surface::new(w, h);
        for (surface, gpu_side) in [(&mut cpu, false), (&mut gpu, true)] {
            let entry = |i: usize| {
                let (cpu, gpu) = &entries[i];
                if gpu_side { gpu } else { cpu }
            };
            surface.write(
                Rect::new(0, 0, w, h),
                &picture,
                w as usize * 4,
                SrcFormat::Rgbx,
            );
            surface.fill(Rect::new(4, 4, 30, 20), [1, 2, 3, 255]);
            surface.copy_from(entry(0), 10, 10);
            surface.write(Rect::new(12, 12, 4, 4), &[9; 64], 16, SrcFormat::Rgba);
            surface.copy_from(entry(1), 20, 8);
            surface.copy_from(entry(0), 88, 58);
            surface.copy_within(Rect::new(0, 4, w, 40), &[(0, 0), (5, 30)]);
            surface.fill(Rect::new(40, 2, 8, 8), [4, 5, 6, 255]);
            surface.copy_within(Rect::new(8, 8, 30, 30), &[(50, 20)]);
            let snapshot = surface.snapshot(Rect::new(7, 5, 32, 32)).unwrap();
            surface.copy_from(entry(1), 0, 0);
            surface.copy_from(&snapshot, 60, 0);
            // Part of another surface, as SurfaceToSurface copies it.
            let part = Rect::new(5, 3, 8, 6);
            match &entry(0).gpu {
                Some(source) => surface.copy_gpu(&source.image, part, 70, 40),
                None => surface.copy_from(&entry(0).snapshot(part).unwrap(), 70, 40),
            }
        }
        assert_eq!(read(&context, &gpu.gpu.as_ref().unwrap().image), cpu.data);
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    /// The cost of switching between target images: cache saves alternating with fills,
    /// each switch ending one render pass and starting another.
    #[test]
    #[ignore = "requires a Vulkan GPU; reports the cost of switching target images"]
    fn gpu_target_switch_throughput() {
        let context = context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let mut surface = Surface::new_gpu(1920, 1080, context.clone(), error.clone());
        let saves = 300;
        let mut cache: Vec<Surface> = (0..saves)
            .map(|_| Surface::new_gpu(64, 64, context.clone(), error.clone()))
            .collect();
        let image = Arc::clone(&surface.gpu.as_ref().unwrap().image);
        let mut frame = || {
            for (i, entry) in cache.iter_mut().enumerate() {
                let i = i as u32;
                entry.copy_gpu(
                    &image,
                    Rect::new((i * 7) % 1600, (i * 3) % 800, 64, 64),
                    0,
                    0,
                );
                surface.fill(Rect::new(i, i, 4, 4), [i as u8, 0, 0, 255]);
            }
            context.flush().unwrap();
            context
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .unwrap();
        };
        frame();
        let runs = 10;
        let start = std::time::Instant::now();
        for _ in 0..runs {
            frame();
        }
        let elapsed = start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs);
        eprintln!(
            "{saves} cache saves between fills: {elapsed:.2} ms, {:.1} us per switch",
            elapsed * 1000.0 / f64::from(2 * saves)
        );
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    /// Busy frames draw the same on the GPU as on the CPU: more quad data than the storage
    /// buffer first holds, and hundreds of switches between the surface and cache entries.
    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_busy_frames_match_cpu() {
        let context = context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let (w, h) = (1920, 1080);
        let mut gpu = Surface::new_gpu(w, h, context.clone(), error.clone());
        let mut cpu = Surface::new(w, h);
        let mut cpu_cache: Vec<Surface> = Vec::new();
        let mut gpu_cache: Vec<Surface> = Vec::new();
        for frame in 0..2u32 {
            for (surface, cache) in [(&mut cpu, &mut cpu_cache), (&mut gpu, &mut gpu_cache)] {
                for ty in 0..h.div_ceil(64) {
                    for tx in 0..w / 64 {
                        let tile: Vec<u8> = (0..64 * 64 * 4)
                            .map(|i| (i as u32 * 7 + tx * 31 + ty * 17 + frame) as u8)
                            .collect();
                        surface.write(
                            Rect::new(tx * 64, ty * 64, 64, 64),
                            &tile,
                            64 * 4,
                            SrcFormat::Rgbx,
                        );
                    }
                }
                for i in 0..300 {
                    let (x, y) = ((i * 37) % (w - 8), (i * 53) % (h - 16));
                    let glyph = [(i % 251) as u8; 8 * 16 * 4];
                    surface.write(Rect::new(x, y, 8, 16), &glyph, 8 * 4, SrcFormat::Bgra);
                    surface.fill(Rect::new(y % 500, x % 500, 9, 5), [i as u8, 3, 4, 255]);
                    if i % 2 == 0 {
                        let at = Rect::new((i * 61) % (w - 64), (i * 13) % (h - 64), 48, 40);
                        cache.push(surface.snapshot(at).unwrap());
                    }
                    if let Some(entry) = cache.get((i as usize * 7) % cache.len().max(1)) {
                        surface.copy_from(entry, (i * 97) % (w - 64), (i * 41) % (h - 64));
                    }
                }
                surface.copy_within(Rect::new(0, 40, w, 1000), &[(0, 0)]);
            }
            context.flush().unwrap();
            let actual = read(&context, &gpu.gpu.as_ref().unwrap().image);
            assert!(actual == cpu.data, "frame {frame} differs");
        }
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    /// Progressive tiles reconstructed on the GPU equal the CPU's, for both DWTs and any
    /// coefficients: full-range ones exercise every 16-bit truncation and the clamping.
    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_progressive_tiles_match_cpu() {
        use ironrdp_graphics::progressive::TileState;
        let context = context().unwrap();
        let (w, h) = (256, 192);
        let image = context.image(w, h);
        let background = [10, 20, 30, 255];
        context.clear(&image, background);
        let mut expected: Vec<u8> = background.repeat((w * h) as usize);
        let mut seed = 0x2545_f491_u32;
        let ranges = [
            32767, 32767, 8000, 8000, 1200, 1200, 40, 40, 32767, 600, 32767, 600,
        ];
        for (i, range) in ranges.into_iter().enumerate() {
            let mut state = TileState::new();
            state.use_reduce_extrapolate = i % 2 == 1;
            for value in state.coefficients.iter_mut().flatten() {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *value = ((seed % (2 * range + 1)) as i32 - range as i32) as i16;
            }
            let mut pixels = vec![0; 64 * 64 * 4];
            state.reconstruct_to_rgba(&mut pixels);
            let (x, y) = ((i as u32 % 4) * 64, (i as u32 / 4) * 64);
            // The last row of tiles draws only parts, as a region's rectangles clip them.
            let parts = if i < 8 {
                vec![Rect::new(x, y, 64, 64)]
            } else {
                vec![
                    Rect::new(x + 3, y, 20, 64),
                    Rect::new(x + 30, y + 40, 50, 10),
                ]
            };
            for part in &parts {
                let part = part.intersect(Rect::new(x, y, 64, 64)).unwrap();
                for row in part.y..part.y + part.h {
                    let from = (((row - y) * 64 + part.x - x) * 4) as usize;
                    let to = ((row * w + part.x) * 4) as usize;
                    let len = (part.w * 4) as usize;
                    expected[to..to + len].copy_from_slice(&pixels[from..from + len]);
                }
            }
            let extrapolate = state.use_reduce_extrapolate;
            context
                .draw_tile(&image, (x, y), &state.coefficients, extrapolate, &parts)
                .unwrap();
        }
        let actual = read(&context, &image);
        for (n, (a, e)) in actual.chunks(4).zip(expected.chunks(4)).enumerate() {
            let (x, y) = (n as u32 % w, n as u32 / w);
            assert_eq!(a, e, "pixel ({x}, {y}) of tile {}", (y / 64) * 4 + x / 64);
        }
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

    /// A busy 1080p lossless frame: a full progressive refresh, glyphs among cache hits, fills,
    /// a scroll and cache traffic, then composition onto the output.
    #[test]
    #[ignore = "requires a Vulkan GPU; reports lossless composition cost per frame"]
    fn gpu_lossless_frame_throughput() {
        let context = context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let (width, height) = (1920, 1080);
        let mut surface = Surface::new_gpu(width, height, context.clone(), error.clone());
        let output = context.image(width, height);
        let tile: Vec<u8> = (0..64 * 64 * 4).map(|i| (i * 7) as u8).collect();
        let glyph: Vec<u8> = (0..8 * 16 * 4).map(|i| (i * 13) as u8).collect();
        let cached: Vec<Surface> = (0..16)
            .map(|i| {
                let mut entry = Surface::new_gpu(16, 16, context.clone(), error.clone());
                entry.fill(Rect::new(0, 0, 16, 16), [i * 16, 80, 160, 255]);
                entry
            })
            .collect();
        let frame = |surface: &mut Surface| {
            for ty in 0..height.div_ceil(64) {
                for tx in 0..width / 64 {
                    surface.write(
                        Rect::new(tx * 64, ty * 64, 64, 64),
                        &tile,
                        64 * 4,
                        SrcFormat::Rgba,
                    );
                }
            }
            for i in 0..300 {
                let (x, y) = ((i * 37) % (width - 8), (i * 53) % (height - 16));
                surface.write(Rect::new(x, y, 8, 16), &glyph, 8 * 4, SrcFormat::Bgra);
                if i % 3 != 0 {
                    let (x, y) = ((i * 43) % (width - 16), (i * 31) % (height - 16));
                    surface.copy_from(&cached[i as usize % cached.len()], x, y);
                }
            }
            for i in 0..100 {
                let (x, y) = ((i * 71) % (width - 40), (i * 29) % (height - 20));
                surface.fill(Rect::new(x, y, 40, 20), [i as u8, 50, 90, 255]);
            }
            surface.copy_within(Rect::new(0, 40, width, 1000), &[(0, 0)]);
            for i in 0..30 {
                let cached = surface
                    .snapshot(Rect::new((i * 61) % (width - 64), 100, 64, 64))
                    .unwrap();
                surface.copy_from(&cached, (i * 97) % (width - 64), 700);
            }
            let image = &surface.gpu.as_ref().unwrap().image;
            for rect in surface.damage.take() {
                context.copy(image, rect, &output, rect.x, rect.y);
            }
            context.flush().unwrap();
        };
        let (frames, warmup) = (100, 5);
        let mut wall = Vec::new();
        let mut cpu = Vec::new();
        for n in 0..frames + warmup {
            let begin_cpu = thread_cpu();
            let begin = std::time::Instant::now();
            frame(&mut surface);
            context
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .unwrap();
            if n >= warmup {
                wall.push(begin.elapsed());
                cpu.push(thread_cpu() - begin_cpu);
            }
        }
        assert!(error.lock().is_none(), "{:?}", error.lock());
        let median = |times: &mut Vec<Duration>| {
            times.sort();
            times[times.len() / 2].as_secs_f64() * 1000.0
        };
        eprintln!(
            "1080p lossless frame (median): {:.3} ms elapsed, {:.3} ms session-thread CPU",
            median(&mut wall),
            median(&mut cpu),
        );
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
        let mut hardware = vaapi::Decoder::new(context.render_node().unwrap()).unwrap();
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
                                |row, line| {
                                    line.copy_from_slice(&rgba[row * width * 4..][..width * 4])
                                },
                            )
                            .unwrap();
                        context.flush().unwrap();
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
                let mut hardware = vaapi::Decoder::new(context.render_node().unwrap()).unwrap();
                let mut software = openh264::decoder::Decoder::with_api_config(
                    api(),
                    openh264::decoder::DecoderConfig::new(),
                )
                .unwrap();
                let views = split(w, h, &picture(w, h), layout);
                let (pw, ph) = (views.y_stride, views.main[0].len() / views.y_stride);
                let whole = Rect::new(0, 0, w as u32, h as u32);
                let mut store = context.store_444(w as u32, h as u32);
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
                context.render_444(&store, &[whole], &target, ox, oy);
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
        eprintln!("Testing GPU decoder on {}", context.render_node().unwrap());
        // SAFETY: matches the ABI used by the application's software decoder.
        let api =
            unsafe { openh264::OpenH264API::from_blob_path_unchecked("/usr/lib/libopenh264.so.8") }
                .unwrap();
        let mut encoder =
            Encoder::with_api_config(api, EncoderConfig::new().vui(VuiConfig::bt601())).unwrap();
        let initial_fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
        for (width, height) in [(64, 64), (98, 82), (64, 64)] {
            let mut decoder = vaapi::Decoder::new(context.render_node().unwrap()).unwrap();
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
