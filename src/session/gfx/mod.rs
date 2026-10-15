//! The Graphics Pipeline (MS-RDPEGFX): H.264 and the lossless/progressive codecs.
//!
//! `ironrdp-egfx` handles the channel protocol (capabilities, ZGFX, frame acknowledgements,
//! AVC420 decoding through our decoder). Everything visual happens here: surfaces, solid fills,
//! surface-to-surface copies, the bitmap cache, the remaining codecs, and composition of the
//! mapped surfaces onto the shared frame at the end of every server frame.

pub(crate) mod avc444;
mod h264;
pub(crate) mod surface;
pub(crate) mod vaapi;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use ironrdp_core::Decode as _;
use ironrdp_egfx::client::{
    BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler, Surface as GfxSurface,
};
use ironrdp_egfx::decode::H264Decoder;
use ironrdp_egfx::pdu::{
    Avc420BitmapStream, Avc444BitmapStream, CacheToSurfacePdu, CapabilitiesV8Flags,
    CapabilitiesV81Flags, CapabilitiesV107Flags, CapabilitySet, Codec1Type, Codec2Type,
    DeleteEncodingContextPdu, Encoding, EvictCacheEntryPdu, GfxPdu, SolidFillPdu,
    SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu, WireToSurface2Pdu,
};
use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::progressive::{DecodedTile, ProgressiveDecodeError, ProgressiveDecoder};
use ironrdp_graphics::rdp6::BitmapStreamDecoder;
use ironrdp_pdu::geometry::{ExclusiveRectangle, InclusiveRectangle};
use ironrdp_session::image::DecodedImage;
use parking_lot::Mutex;

use self::avc444::Layout;
use self::h264::{SoftVideo, View};
use self::surface::{SrcFormat, Surface};
use super::framebuffer::Rect;
use super::{Codec, EventSink, Rendering, SessionEvent, SharedFrame};
use crate::gpu::{GpuContext, GpuImage, Store444};
use crate::store::profiles::GraphicsMode;

/// Why the graphics pipeline stopped.
#[derive(Debug, Clone)]
pub struct GfxFailure {
    pub reason: String,
    /// The GPU path (VA-API decoding, import, or GPU surfaces) failed, so the session should
    /// reconnect with software decoding rather than give up on the pipeline.
    pub hardware: bool,
}

/// Encoding context IDs are scoped to an RDP surface, not the connection.
#[derive(Default)]
struct ProgressiveContexts {
    surfaces: HashMap<u16, ProgressiveDecoder>,
}

impl ProgressiveContexts {
    fn decode(
        &mut self,
        surface_id: u16,
        context_id: u32,
        width: u16,
        height: u16,
        data: &[u8],
    ) -> Result<Vec<DecodedTile>, ProgressiveDecodeError> {
        self.surfaces
            .entry(surface_id)
            .or_default()
            .decode_bitmap(context_id, width, height, data)
    }

    fn delete_context(&mut self, surface_id: u16, context_id: u32) {
        if let Some(decoder) = self.surfaces.get_mut(&surface_id) {
            decoder.delete_context(context_id);
        }
    }

    fn remove_surface(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
    }

    fn reset(&mut self) {
        self.surfaces.clear();
    }
}

fn graphics_capabilities(mode: GraphicsMode) -> Vec<CapabilitySet> {
    // Preserve the working Windows advertisement order: preferred video
    // versions first, followed by the V8 compatibility fallback.
    let mut caps = Vec::new();
    match mode {
        GraphicsMode::Auto => {}
        GraphicsMode::FullColor => caps.push(CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::empty(),
        }),
        GraphicsMode::SharpText | GraphicsMode::Compatible => caps.push(CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::AVC_DISABLED,
        }),
    }
    if mode.uses_h264() {
        caps.push(CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED,
        });
    }
    caps.push(CapabilitySet::V8 {
        flags: CapabilitiesV8Flags::empty(),
    });
    caps
}

/// State shared between the pipeline handler (called from inside the protocol stack) and the
/// session loop.
pub struct GfxShared {
    state: Mutex<GfxState>,
    frame: Arc<SharedFrame>,
    events: EventSink,
    mode: GraphicsMode,
    /// Frames presented to the shared frame buffer so far.
    presented: AtomicU64,
    /// Set when the server sent something we can't decode; the session then falls back to
    /// legacy graphics.
    failure: Mutex<Option<GfxFailure>>,
    /// The latest H.264 flavour decoded (0: none yet, 1: 4:2:0, 2: 4:4:4).
    video: AtomicU8,
    /// Why hardware decoding or H.264 isn't available, for the session bar.
    fallback: Mutex<Option<String>>,
}

struct HardwarePipeline {
    context: Arc<GpuContext>,
    error: Arc<Mutex<Option<String>>>,
    output: Option<Arc<GpuImage>>,
    decoders: HashMap<u16, vaapi::Decoder>,
    spare: Option<vaapi::Decoder>,
    /// Full color mode: each surface's 4:4:4 picture.
    stores: HashMap<u16, Store444>,
    annex_b: Vec<u8>,
    announced: bool,
}

struct GfxState {
    gpu: Option<HardwarePipeline>,
    /// Full color mode without hardware decoding.
    soft: Option<SoftVideo>,
    output: (u32, u32),
    surfaces: BTreeMap<u16, Surface>,
    cache: HashMap<u16, Surface>,
    clear: ClearCodecDecoder,
    planar: BitmapStreamDecoder,
    progressive: ProgressiveContexts,
    rfx: ironrdp_session::rfx::DecodingContext,
    scratch: Vec<u8>,
    warned_alpha: bool,
}

impl GfxShared {
    pub(super) fn new(
        frame: Arc<SharedFrame>,
        events: EventSink,
        mode: GraphicsMode,
        hardware: bool,
        context: Option<Arc<GpuContext>>,
        previous_reason: Option<String>,
    ) -> Arc<Self> {
        let mut reason = previous_reason;
        let gpu = if hardware && mode.uses_h264() {
            match context
                .ok_or_else(|| anyhow::anyhow!("Vulkan/VA-API interop unavailable"))
                .and_then(|context| {
                    vaapi::Decoder::new(&context.render_node).map(|decoder| (context, decoder))
                }) {
                Ok((context, decoder)) => Some(HardwarePipeline {
                    context,
                    error: Arc::new(Mutex::new(None)),
                    output: None,
                    decoders: HashMap::new(),
                    spare: Some(decoder),
                    stores: HashMap::new(),
                    annex_b: Vec::new(),
                    announced: false,
                }),
                Err(error) => {
                    reason = Some(error.to_string());
                    None
                }
            }
        } else {
            None
        };
        let name = if gpu.is_some() {
            "VA-API (waiting for H.264)"
        } else if mode.uses_h264() {
            "Software (OpenH264)"
        } else {
            "Software (RDP graphics)"
        };
        tracing::info!(decoder = name, ?reason, "video decoder selected");
        Arc::new(Self {
            state: Mutex::new(GfxState {
                gpu,
                soft: None,
                output: (0, 0),
                surfaces: BTreeMap::new(),
                cache: HashMap::new(),
                clear: ClearCodecDecoder::new(),
                planar: BitmapStreamDecoder::default(),
                progressive: ProgressiveContexts::default(),
                rfx: ironrdp_session::rfx::DecodingContext::new(),
                scratch: Vec::new(),
                warned_alpha: false,
            }),
            frame,
            events,
            mode,
            presented: AtomicU64::new(0),
            failure: Mutex::new(None),
            video: AtomicU8::new(0),
            fallback: Mutex::new(reason),
        })
    }

    /// The DVC processor to register on the dynamic channel.
    pub(super) fn client(self: &Arc<Self>) -> GraphicsPipelineClient {
        let decoder: Option<Box<dyn H264Decoder>> =
            if self.mode == GraphicsMode::Auto && !self.hardware_active() {
                match h264::SystemH264::load(Arc::clone(self)) {
                    Ok(decoder) => Some(Box::new(decoder)),
                    Err(e) => {
                        tracing::warn!(error = %e, "H.264 unavailable; using lossless codecs only");
                        *self.fallback.lock() = Some(e.to_string());
                        None
                    }
                }
            } else {
                None
            };
        if self.mode == GraphicsMode::FullColor && !self.hardware_active() {
            match SoftVideo::new() {
                Ok(soft) => self.state.lock().soft = Some(soft),
                Err(e) => {
                    tracing::warn!(error = %e, "H.264 unavailable; using lossless codecs only");
                    *self.fallback.lock() = Some(e.to_string());
                }
            }
        }
        GraphicsPipelineClient::new(Box::new(Handler(Arc::clone(self))), decoder)
    }

    pub fn presented(&self) -> u64 {
        self.presented.load(Ordering::Acquire)
    }

    pub fn hardware_active(&self) -> bool {
        self.state.lock().gpu.is_some()
    }

    /// Records that an H.264 picture was decoded (`full`: 4:4:4).
    pub fn note_h264(&self, full: bool) {
        self.video
            .store(if full { 2 } else { 1 }, Ordering::Release);
    }

    pub fn rendering(&self) -> Rendering {
        let video = match self.video.load(Ordering::Acquire) {
            0 => None,
            1 => Some(Codec::H264),
            _ => Some(Codec::H264Full),
        };
        Rendering::for_pipeline(self.hardware_active(), video, self.fallback.lock().clone())
    }

    pub fn take_failure(&self) -> Option<GfxFailure> {
        self.failure.lock().take().or_else(|| {
            let state = self.state.lock();
            let reason = state.gpu.as_ref()?.error.lock().take()?;
            Some(GfxFailure {
                reason,
                hardware: true,
            })
        })
    }

    pub fn output_size(&self) -> Option<(u32, u32)> {
        let output = self.state.lock().output;
        (output.0 > 0 && output.1 > 0).then_some(output)
    }

    /// A failure that has nothing to do with the GPU path (a codec or protocol error).
    fn fail(&self, reason: String) {
        tracing::warn!(%reason, "graphics pipeline failure");
        self.failure.lock().get_or_insert(GfxFailure {
            reason,
            hardware: false,
        });
    }

    /// An H.264 failure: it counts against hardware decoding when that's what was decoding.
    fn fail_video(&self, reason: String) {
        let hardware = self.hardware_active();
        tracing::warn!(%reason, hardware, "video decoding failure");
        self.failure
            .lock()
            .get_or_insert(GfxFailure { reason, hardware });
    }

    fn present(&self, state: &mut GfxState) {
        if let Some(gpu) = &mut state.gpu {
            let Some(output) = &gpu.output else { return };
            let mut encoder = gpu.context.encoder();
            let mut any = false;
            for surface in state.surfaces.values_mut() {
                let rects = surface.damage.take();
                let (Some((ox, oy)), Some(source)) = (surface.origin, &surface.gpu) else {
                    continue;
                };
                for rect in rects {
                    GpuContext::copy(
                        &mut encoder,
                        &source.image,
                        rect,
                        output,
                        ox + rect.x,
                        oy + rect.y,
                    );
                    any = true;
                }
            }
            if any {
                match gpu.context.submit(encoder, ()) {
                    Ok(()) => {
                        self.frame.lock().publish_gpu(Arc::clone(output));
                        self.presented.fetch_add(1, Ordering::AcqRel);
                        if self.frame.request_wake() {
                            self.events.wake();
                        }
                    }
                    Err(error) => {
                        gpu.error.lock().get_or_insert_with(|| error.to_string());
                    }
                }
            }
            return;
        }
        let mut frame = self.frame.lock();
        let mut any = false;
        for surface in state.surfaces.values_mut() {
            let rects = surface.damage.take();
            let Some((ox, oy)) = surface.origin else {
                continue;
            };
            for rect in rects {
                let start = rect.y as usize * surface.stride() + rect.x as usize * 4;
                frame.blit_rgba_at(
                    &surface.data[start..],
                    surface.stride(),
                    rect.w,
                    rect.h,
                    ox + rect.x,
                    oy + rect.y,
                );
                any = true;
            }
        }
        drop(frame);
        if any {
            self.presented.fetch_add(1, Ordering::AcqRel);
            if self.frame.request_wake() {
                self.events.wake();
            }
        }
    }
}

fn rect(r: &ExclusiveRectangle) -> Option<Rect> {
    (r.right > r.left && r.bottom > r.top).then(|| {
        Rect::new(
            u32::from(r.left),
            u32::from(r.top),
            u32::from(r.right - r.left),
            u32::from(r.bottom - r.top),
        )
    })
}

struct Handler(Arc<GfxShared>);

impl GraphicsPipelineHandler for Handler {
    fn handles_avc420(&self) -> bool {
        let state = self.0.state.lock();
        state.gpu.is_some() || state.soft.is_some()
    }

    fn on_avc420(
        &mut self,
        surface_id: u16,
        dest: &ExclusiveRectangle,
        stream: &ironrdp_egfx::pdu::Avc420BitmapStream<'_>,
    ) -> ironrdp_pdu::PduResult<()> {
        // Recording the failure lets the outer session loop reconnect with
        // software. Never attempt a midstream decoder swap without a keyframe.
        let result = if self.0.mode == GraphicsMode::FullColor {
            // Plain 4:2:0 pictures update the 4:4:4 picture as main views, so the
            // surface's single H.264 stream stays consistent.
            self.full_color_frame(surface_id, dest, &[(stream, View::Luma)], false)
        } else {
            self.hardware_frame(surface_id, dest, stream)
        };
        if let Err(error) = result {
            self.0.fail_video(error.to_string());
        }
        Ok(())
    }

    fn capabilities(&self) -> Vec<CapabilitySet> {
        let caps = graphics_capabilities(self.0.mode);
        tracing::info!(mode = ?self.0.mode, ?caps, "advertising graphics capabilities");
        caps
    }

    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        tracing::info!(?caps, "graphics pipeline active");
        if self.0.mode.uses_h264() {
            let video = match caps {
                CapabilitySet::V8 { .. } => false,
                CapabilitySet::V8_1 { flags } => {
                    flags.contains(CapabilitiesV81Flags::AVC420_ENABLED)
                }
                CapabilitySet::V10_7 { flags } => {
                    !flags.contains(CapabilitiesV107Flags::AVC_DISABLED)
                }
                _ => true,
            };
            if !video {
                let reason = "The server selected graphics without H.264; its bitmap codecs decode on the CPU";
                tracing::warn!(?caps, reason, "server did not enable H.264");
                self.0
                    .fallback
                    .lock()
                    .get_or_insert_with(|| reason.to_owned());
            }
        }
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        let mut state = self.0.state.lock();
        state.output = (width, height);
        state.surfaces.clear();
        state.cache.clear();
        if let Some(soft) = &mut state.soft {
            soft.clear();
        }
        if let Some(gpu) = &mut state.gpu {
            gpu.decoders.clear();
            gpu.stores.clear();
            gpu.annex_b.clear();
            let output = gpu.context.image(width, height);
            if let Err(error) = gpu.context.clear(&output, [0, 0, 0, 255]) {
                gpu.error.lock().get_or_insert_with(|| error.to_string());
            }
            gpu.output = Some(output);
        }
        state.progressive.reset();
        drop(state);
        if !self.0.hardware_active() {
            self.0.frame.lock().resize(width, height);
        }
        if let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) {
            self.0
                .events
                .send(SessionEvent::DesktopResized { width, height });
        }
    }

    fn on_surface_created(&mut self, surface: &GfxSurface) {
        let mut state = self.0.state.lock();
        let (width, height) = (u32::from(surface.width), u32::from(surface.height));
        let image = if let Some(gpu) = &state.gpu {
            Surface::new_gpu(
                width,
                height,
                Arc::clone(&gpu.context),
                Arc::clone(&gpu.error),
            )
        } else {
            Surface::new(width, height)
        };
        if let Some(gpu) = &mut state.gpu {
            gpu.decoders.remove(&surface.id);
            gpu.stores.remove(&surface.id);
        }
        if let Some(soft) = &mut state.soft {
            soft.remove(surface.id);
        }
        state.progressive.remove_surface(surface.id);
        state.surfaces.insert(surface.id, image);
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        let mut state = self.0.state.lock();
        state.surfaces.remove(&surface_id);
        state.progressive.remove_surface(surface_id);
        if let Some(gpu) = &mut state.gpu {
            gpu.decoders.remove(&surface_id);
            gpu.stores.remove(&surface_id);
        }
        if let Some(soft) = &mut state.soft {
            soft.remove(surface_id);
        }
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        if let Some(surface) = self.0.state.lock().surfaces.get_mut(&surface_id) {
            surface.origin = Some((origin_x, origin_y));
            surface.mark_all_damaged();
        }
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        if update.data.is_empty() {
            return;
        }
        let Some(dest) = rect(&update.destination_rectangle) else {
            return;
        };
        let format = if update.codec_id == Codec1Type::Uncompressed {
            SrcFormat::Rgbx
        } else {
            SrcFormat::Rgba
        };
        let mut state = self.0.state.lock();
        if let Some(surface) = state.surfaces.get_mut(&update.surface_id) {
            let dest = Rect::new(
                dest.x,
                dest.y,
                dest.w.min(u32::from(update.width)),
                dest.h.min(u32::from(update.height)),
            );
            surface.write(dest, &update.data, usize::from(update.width) * 4, format);
        }
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        let mut state = self.0.state.lock();
        self.0.present(&mut state);
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        let c = &pdu.fill_pixel;
        let rgba = [c.r, c.g, c.b, 0xFF];
        let mut state = self.0.state.lock();
        if let Some(surface) = state.surfaces.get_mut(&pdu.surface_id) {
            for r in pdu.rectangles.iter().filter_map(rect) {
                surface.fill(r, rgba);
            }
        }
    }

    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        let Some(src_rect) = rect(&pdu.source_rectangle) else {
            return;
        };
        let mut state = self.0.state.lock();
        let Some(snapshot) = state
            .surfaces
            .get(&pdu.source_surface_id)
            .and_then(|s| s.snapshot(src_rect))
        else {
            return;
        };
        if let Some(dest) = state.surfaces.get_mut(&pdu.destination_surface_id) {
            for point in &pdu.destination_points {
                dest.copy_from(&snapshot, u32::from(point.x), u32::from(point.y));
            }
        }
    }

    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        let Some(src_rect) = rect(&pdu.source_rectangle) else {
            return;
        };
        let mut state = self.0.state.lock();
        if let Some(snapshot) = state
            .surfaces
            .get(&pdu.surface_id)
            .and_then(|s| s.snapshot(src_rect))
        {
            state.cache.insert(pdu.cache_slot, snapshot);
        }
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let mut state = self.0.state.lock();
        let GfxState {
            cache, surfaces, ..
        } = &mut *state;
        let (Some(entry), Some(surface)) = (
            cache.get(&pdu.cache_slot),
            surfaces.get_mut(&pdu.surface_id),
        ) else {
            return;
        };
        for point in &pdu.destination_points {
            surface.copy_from(entry, u32::from(point.x), u32::from(point.y));
        }
    }

    fn on_evict_cache_entry(&mut self, pdu: &EvictCacheEntryPdu) {
        self.0.state.lock().cache.remove(&pdu.cache_slot);
    }

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        if pdu.codec_id != Codec2Type::RemoteFxProgressive {
            return self.0.fail(format!("unsupported codec {:?}", pdu.codec_id));
        }
        let mut state = self.0.state.lock();
        let GfxState {
            surfaces,
            progressive,
            ..
        } = &mut *state;
        let Some(surface) = surfaces.get_mut(&pdu.surface_id) else {
            return;
        };
        let (Ok(w), Ok(h)) = (u16::try_from(surface.width), u16::try_from(surface.height)) else {
            return;
        };
        match progressive.decode(pdu.surface_id, pdu.codec_context_id, w, h, &pdu.bitmap_data) {
            Ok(tiles) => {
                apply_progressive_tiles(surface, tiles);
            }
            Err(e) => {
                drop(state);
                self.0.fail(format!("progressive decode: {e}"));
            }
        }
    }

    fn on_delete_encoding_context(&mut self, pdu: &DeleteEncodingContextPdu) {
        self.0
            .state
            .lock()
            .progressive
            .delete_context(pdu.surface_id, pdu.codec_context_id);
    }

    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::WireToSurface1(wire)
                if matches!(wire.codec_id, Codec1Type::Avc444 | Codec1Type::Avc444v2)
                    && self.0.mode == GraphicsMode::FullColor =>
            {
                if let Err(error) = self.avc444(wire) {
                    self.0.fail_video(error.to_string());
                }
            }
            GfxPdu::WireToSurface1(wire) => {
                if let Err(reason) = self.wire_to_surface1(wire) {
                    self.0.fail(reason);
                }
            }
            other => tracing::debug!(pdu = ?std::mem::discriminant(other), "ignored graphics PDU"),
        }
    }
}

/// AVC420 region rectangles (inclusive) as picture rects; none means the whole picture.
fn picture_rects(regions: &[InclusiveRectangle], width: u32, height: u32) -> Vec<Rect> {
    let rects: Vec<Rect> = regions
        .iter()
        .filter_map(|r| {
            Rect::new(
                u32::from(r.left),
                u32::from(r.top),
                u32::from(r.right.saturating_sub(r.left)) + 1,
                u32::from(r.bottom.saturating_sub(r.top)) + 1,
            )
            .clip(width, height)
        })
        .collect();
    if rects.is_empty() {
        vec![Rect::new(0, 0, width, height)]
    } else {
        rects
    }
}

fn apply_progressive_tiles(surface: &mut Surface, tiles: Vec<DecodedTile>) {
    for tile in tiles {
        let area = Rect::new(
            u32::from(tile.x_idx) * 64,
            u32::from(tile.y_idx) * 64,
            64,
            64,
        );
        // Only the parts inside the region's rectangles change (a tile can overlap
        // content the server didn't update).
        let region = tile.rects.iter().map(|r| {
            Rect::new(
                u32::from(r.x),
                u32::from(r.y),
                u32::from(r.width),
                u32::from(r.height),
            )
        });
        let parts: Vec<Rect> = if tile.rects.is_empty() {
            vec![area]
        } else {
            region.filter_map(|r| r.intersect(area)).collect()
        };
        for part in parts {
            let offset = ((part.y - area.y) as usize * 64 + (part.x - area.x) as usize) * 4;
            surface.write(part, &tile.pixels[offset..], 64 * 4, SrcFormat::Rgba);
        }
    }
}

fn decode_clearcodec(
    decoder: &mut ClearCodecDecoder,
    surface: &mut Surface,
    dest: Rect,
    data: &[u8],
) -> Result<(), String> {
    let (Ok(w), Ok(h)) = (u16::try_from(dest.w), u16::try_from(dest.h)) else {
        return Ok(());
    };
    let bgra = decoder
        .decode(data, w, h)
        .map_err(|e| format!("ClearCodec: {e}"))?;
    // Only the drawn areas change; streams without a residual layer can leave gaps.
    let stride = dest.w as usize * 4;
    for area in decoder.coverage() {
        let (x, y) = (u32::from(area.x), u32::from(area.y));
        let offset = y as usize * stride + x as usize * 4;
        let rect = Rect::new(
            dest.x + x,
            dest.y + y,
            u32::from(area.width),
            u32::from(area.height),
        );
        surface.write(rect, &bgra[offset..], stride, SrcFormat::Bgra);
    }
    Ok(())
}

impl Handler {
    fn avc444(&self, pdu: &WireToSurface1Pdu) -> anyhow::Result<()> {
        let mut cursor = ironrdp_core::ReadCursor::new(&pdu.bitmap_data);
        let stream = Avc444BitmapStream::decode(&mut cursor)
            .map_err(|e| anyhow::anyhow!("AVC444 stream: {e}"))?;
        let layout = if pdu.codec_id == Codec1Type::Avc444 {
            Layout::V1
        } else {
            Layout::V2
        };
        let chroma = View::Chroma(layout);
        let views: Vec<(&Avc420BitmapStream<'_>, View)> = if stream.encoding == Encoding::LUMA {
            vec![(&stream.stream1, View::Luma)]
        } else if stream.encoding == Encoding::CHROMA {
            // A chroma-only update carries the auxiliary view in the first stream.
            vec![(&stream.stream1, chroma)]
        } else {
            let mut views = vec![(&stream.stream1, View::Luma)];
            if let Some(aux) = &stream.stream2 {
                views.push((aux, chroma));
            }
            views
        };
        self.full_color_frame(pdu.surface_id, &pdu.destination_rectangle, &views, true)
    }

    /// Full color mode: decode views into the surface's 4:4:4 picture, then convert the
    /// updated regions onto the surface (on the GPU when hardware decoding is active).
    fn full_color_frame(
        &self,
        surface_id: u16,
        dest: &ExclusiveRectangle,
        views: &[(&Avc420BitmapStream<'_>, View)],
        full: bool,
    ) -> anyhow::Result<()> {
        let Some(dest) = rect(dest) else {
            return Ok(());
        };
        if self.0.failure.lock().is_some() {
            return Ok(());
        }
        let mut state = self.0.state.lock();
        let GfxState {
            gpu,
            soft,
            surfaces,
            ..
        } = &mut *state;
        let Some(surface) = surfaces.get_mut(&surface_id) else {
            return Ok(());
        };
        let mut updated = Vec::new();

        if let Some(gpu) = gpu {
            let HardwarePipeline {
                context,
                decoders,
                spare,
                stores,
                annex_b,
                ..
            } = gpu;
            if let std::collections::hash_map::Entry::Vacant(entry) = decoders.entry(surface_id) {
                entry.insert(match spare.take() {
                    Some(decoder) => decoder,
                    None => vaapi::Decoder::new(&context.render_node)?,
                });
            }
            let decoder = decoders.get_mut(&surface_id).expect("just inserted");
            if stores
                .get(&surface_id)
                .is_none_or(|s| (s.image.width, s.image.height) != (dest.w, dest.h))
            {
                stores.insert(surface_id, context.store_444(dest.w, dest.h)?);
            }
            let store = stores.get_mut(&surface_id).expect("just inserted");
            for (stream, view) in views {
                let rects = picture_rects(&stream.rectangles, dest.w, dest.h);
                vaapi::annex_b(stream.data, annex_b)?;
                let frame = decoder
                    .decode(annex_b)?
                    .ok_or_else(|| anyhow::anyhow!("hardware decoder delayed an RDP picture"))?;
                anyhow::ensure!(
                    frame.desc.width >= dest.w && frame.desc.height >= dest.h,
                    "H.264 picture smaller than destination rectangle"
                );
                let auxiliary = match view {
                    View::Luma => None,
                    View::Chroma(layout) => Some(*layout),
                };
                context.apply_view(frame, store, &rects, auxiliary)?;
                updated.extend(rects);
            }
            let target = surface
                .gpu
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("surface has no GPU image"))?;
            context.render_444(store, &updated, &target.image, dest.x, dest.y)?;
            for r in &updated {
                let placed = Rect::new(dest.x + r.x, dest.y + r.y, r.w, r.h);
                if let Some(placed) = placed.clip(surface.width, surface.height) {
                    surface.damage.add(placed);
                }
            }
        } else if let Some(soft) = soft {
            let size = (dest.w as usize, dest.h as usize);
            for (stream, view) in views {
                let rects = picture_rects(&stream.rectangles, dest.w, dest.h);
                soft.decode(surface_id, size, stream.data, &rects, *view)?;
                updated.extend(rects);
            }
            for r in updated {
                if let Some((done, rgba)) = soft.render(surface_id, r) {
                    let placed = Rect::new(dest.x + done.x, dest.y + done.y, done.w, done.h);
                    surface.write(placed, rgba, done.w as usize * 4, SrcFormat::Rgba);
                }
            }
        } else {
            anyhow::bail!("H.264 arrived but no decoder is available");
        }
        drop(state);
        self.0.note_h264(full);
        Ok(())
    }

    fn hardware_frame(
        &self,
        surface_id: u16,
        dest: &ExclusiveRectangle,
        stream: &ironrdp_egfx::pdu::Avc420BitmapStream<'_>,
    ) -> anyhow::Result<()> {
        let Some(dest) = rect(dest) else {
            return Ok(());
        };
        if self.0.failure.lock().is_some() {
            return Ok(());
        }
        let mut state = self.0.state.lock();
        let GfxState { gpu, surfaces, .. } = &mut *state;
        let Some(gpu) = gpu else { return Ok(()) };
        let Some(surface) = surfaces.get_mut(&surface_id) else {
            return Ok(());
        };
        vaapi::annex_b(stream.data, &mut gpu.annex_b)?;
        if let std::collections::hash_map::Entry::Vacant(entry) = gpu.decoders.entry(surface_id) {
            entry.insert(match gpu.spare.take() {
                Some(decoder) => decoder,
                None => vaapi::Decoder::new(&gpu.context.render_node)?,
            });
        }
        let decoder = gpu.decoders.get_mut(&surface_id).expect("just inserted");
        // RDP associates each access unit with this destination immediately.
        // A delayed/reordered picture cannot safely be applied to a later PDU.
        let frame = decoder
            .decode(&gpu.annex_b)?
            .ok_or_else(|| anyhow::anyhow!("hardware decoder delayed an RDP picture"))?;
        anyhow::ensure!(
            frame.desc.width >= dest.w && frame.desc.height >= dest.h,
            "H.264 picture smaller than destination rectangle"
        );
        let image = gpu.context.convert(frame)?;
        // Match IronRDP's AVC420 destination behavior: decoded images may be
        // macroblock padded, and the wire destination selects the visible area.
        surface.copy_gpu(&image, Rect::new(0, 0, dest.w, dest.h), dest.x, dest.y);
        if !gpu.announced {
            gpu.announced = true;
            tracing::info!(node = %gpu.context.render_node, "VA-API H.264 decoding active; frames remain on GPU");
            self.0.note_h264(false);
        }
        Ok(())
    }

    fn wire_to_surface1(&self, pdu: &WireToSurface1Pdu) -> Result<(), String> {
        let Some(dest) = rect(&pdu.destination_rectangle) else {
            return Ok(());
        };
        let (w, h) = (dest.w, dest.h);
        let mut state = self.0.state.lock();
        let GfxState {
            surfaces,
            clear,
            planar,
            rfx,
            scratch,
            warned_alpha,
            ..
        } = &mut *state;
        let Some(surface) = surfaces.get_mut(&pdu.surface_id) else {
            return Ok(());
        };

        match pdu.codec_id {
            Codec1Type::ClearCodec => {
                decode_clearcodec(clear, surface, dest, &pdu.bitmap_data)?;
            }
            Codec1Type::Planar => {
                scratch.clear();
                planar
                    .decode_bitmap_stream_to_rgb24(
                        &pdu.bitmap_data,
                        scratch,
                        w as usize,
                        h as usize,
                    )
                    .map_err(|e| format!("planar: {e}"))?;
                surface.write(dest, scratch, w as usize * 3, SrcFormat::Rgb);
            }
            Codec1Type::RemoteFx => {
                let (Ok(w16), Ok(h16)) = (u16::try_from(w), u16::try_from(h)) else {
                    return Ok(());
                };
                let mut image = DecodedImage::new(PixelFormat::RgbA32, w16, h16);
                let target = InclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: w16 - 1,
                    bottom: h16 - 1,
                };
                let mut cursor = ironrdp_core::ReadCursor::new(&pdu.bitmap_data);
                let (_, updated) = rfx
                    .decode(&mut image, &target, &mut cursor)
                    .map_err(|e| format!("RemoteFX: {e}"))?;
                // Only the updated region is valid; the rest of the scratch image is blank.
                let region = Rect::new(
                    u32::from(updated.left),
                    u32::from(updated.top),
                    u32::from(updated.right.saturating_sub(updated.left)) + 1,
                    u32::from(updated.bottom.saturating_sub(updated.top)) + 1,
                );
                if let Some(region) = region.clip(w, h) {
                    let start = region.y as usize * image.stride() + region.x as usize * 4;
                    let target =
                        Rect::new(dest.x + region.x, dest.y + region.y, region.w, region.h);
                    surface.write(
                        target,
                        &image.data()[start..],
                        image.stride(),
                        SrcFormat::Rgbx,
                    );
                }
            }
            Codec1Type::Alpha => {
                // Alpha only matters for RemoteApp window shapes; the desktop is opaque.
                if !*warned_alpha {
                    *warned_alpha = true;
                    tracing::debug!("ignoring alpha codec updates");
                }
            }
            other => return Err(format!("unsupported codec {other:?}")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progressive_zero_difference() -> Vec<u8> {
        use ironrdp_pdu::codecs::rfx::EntropyAlgorithm;
        use ironrdp_pdu::codecs::rfx::progressive::{
            ProgressiveBlock, ProgressiveTile, decode_progressive_stream, encode_progressive_stream,
        };
        let mut zero = vec![0; 32768];
        let len =
            ironrdp_graphics::rlgr::encode(EntropyAlgorithm::Rlgr1, &[0; 4096], &mut zero).unwrap();
        zero.truncate(len);
        let mut blocks =
            decode_progressive_stream(include_bytes!("testdata/progressive-160x96.bin")).unwrap();
        for block in &mut blocks {
            match block {
                ProgressiveBlock::Context(context) => context.flags |= 1, // SUBBAND_DIFFING
                ProgressiveBlock::Region(region) => {
                    for tile in &mut region.tiles {
                        let ProgressiveTile::Simple(tile) = tile else {
                            panic!("expected simple tiles in FreeRDP fixture");
                        };
                        tile.flags = 1; // TILE_DIFFERENCE
                        tile.y_data = &zero;
                        tile.cb_data = &zero;
                        tile.cr_data = &zero;
                        tile.tail_data = &[];
                    }
                }
                _ => {}
            }
        }
        encode_progressive_stream(&blocks).unwrap()
    }

    #[test]
    fn progressive_difference_reference_survives_encoding_context_changes() {
        let first = include_bytes!("testdata/progressive-160x96.bin");
        let delta = progressive_zero_difference();
        let mut contexts = ProgressiveContexts::default();
        let expected = contexts.decode(1, 10, 160, 96, first).unwrap();
        // Windows may send the next difference under a different encoding context. Its
        // coefficient reference belongs to the surface, independently of either context.
        for context in [11, 12, 13] {
            let actual = contexts.decode(1, context, 160, 96, &delta).unwrap();
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(&expected) {
                assert_eq!(
                    actual.pixels, expected.pixels,
                    "reference lost in context {context}"
                );
            }
            contexts.delete_context(1, context);
            contexts.delete_context(1, 10);
        }
        // A separate or recreated surface must not inherit the old reference image.
        let other = contexts.decode(2, 10, 160, 96, &delta).unwrap();
        assert!(other.iter().all(|tile| {
            tile.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [128, 128, 128, 255])
        }));
        contexts.remove_surface(1);
        let recreated = contexts.decode(1, 10, 160, 96, &delta).unwrap();
        for (actual, expected) in recreated.iter().zip(&other) {
            assert_eq!(actual.pixels, expected.pixels);
        }
    }

    #[test]
    fn progressive_context_deletion_preserves_other_refinements() {
        let first = include_bytes!("testdata/progressive-rex.1.bin");
        let upgrade = include_bytes!("testdata/progressive-rex.2.bin");
        let mut reference = ProgressiveDecoder::new();
        reference.decode_bitmap(0, 128, 64, first).unwrap();
        let expected = reference.decode_bitmap(0, 128, 64, upgrade).unwrap();
        let mut contexts = ProgressiveContexts::default();
        contexts.decode(1, 21, 128, 64, first).unwrap();
        contexts.decode(1, 22, 128, 64, first).unwrap();
        contexts.delete_context(1, 21);
        let actual = contexts.decode(1, 22, 128, 64, upgrade).unwrap();
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(&expected) {
            assert_eq!(actual.pixels, expected.pixels);
        }
        contexts.delete_context(1, 22);
        assert!(contexts.decode(1, 23, 128, 64, upgrade).unwrap().is_empty());
    }

    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_lossless_progressive_reference_survives_context_changes() {
        let context = crate::gpu::tests::context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let mut surface = Surface::new_gpu(160, 96, Arc::clone(&context), Arc::clone(&error));
        let mut contexts = ProgressiveContexts::default();
        let first = include_bytes!("testdata/progressive-160x96.bin");
        let delta = progressive_zero_difference();
        let mut expected = Vec::new();
        for id in 0..4 {
            let data = if id == 0 { first.as_slice() } else { &delta };
            let tiles = contexts.decode(1, id, 160, 96, data).unwrap();
            apply_progressive_tiles(&mut surface, tiles);
            contexts.delete_context(1, id);
            let pixels = crate::gpu::tests::read(&context, &surface.gpu.as_ref().unwrap().image);
            if id == 0 {
                assert!(
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|p| *p != [128, 128, 128, 255])
                );
                expected = pixels;
            } else {
                assert_eq!(pixels, expected, "GPU tiles became gray in context {id}");
            }
        }
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    // MS-RDPEGFX 4.1.1.5: glyph pixels have no implied dimensions. Windows may reuse a
    // 2x8 glyph as 4x4 or 8x2, overwriting every pixel at the new destination.
    fn check_clearcodec_reshaped_glyphs(
        mut surface: Surface,
        mut read: impl FnMut(&Surface) -> Vec<u8>,
    ) {
        let mut decoder = ClearCodecDecoder::new();
        let mut stream = vec![0x01, 0, 4, 0]; // GLYPH_INDEX, sequence 0, slot 4
        for length in [16u32 * 4, 0, 0] {
            stream.extend_from_slice(&length.to_le_bytes());
        }
        let pixels: Vec<[u8; 4]> = (0..16u8).map(|i| [i, i + 20, i + 40, 255]).collect();
        for [r, g, b, _] in &pixels {
            stream.extend_from_slice(&[*b, *g, *r, 1]); // BGR, run of one
        }
        for (seq, (w, h)) in [(2, 8), (4, 4), (8, 2), (16, 1), (1, 16), (2, 8)]
            .into_iter()
            .enumerate()
        {
            let background = [90 + seq as u8, 100, 110, 255];
            surface.fill(Rect::new(0, 0, surface.width, surface.height), background);
            surface.damage.take();
            let dest = Rect::new(3, 2, w, h);
            let hit = [0x03, seq as u8, 4, 0]; // GLYPH_INDEX | GLYPH_HIT
            decode_clearcodec(
                &mut decoder,
                &mut surface,
                dest,
                if seq == 0 { &stream } else { &hit },
            )
            .unwrap();
            let mut expected = background.repeat((surface.width * surface.height) as usize);
            for (i, pixel) in pixels.iter().enumerate() {
                let x = dest.x + i as u32 % w;
                let y = dest.y + i as u32 / w;
                let offset = (y * surface.width + x) as usize * 4;
                expected[offset..offset + 4].copy_from_slice(pixel);
            }
            assert_eq!(read(&surface), expected, "stale pixels in {w}x{h} glyph");
            assert_eq!(surface.damage.take(), vec![dest]);
            // Invalid cache-hit sizes must leave the surface and cache intact. The next
            // valid hit above must still render all the original pixels.
            for (bad_w, bad_h) in [(4, 5), (3, 4)] {
                assert!(
                    decode_clearcodec(
                        &mut decoder,
                        &mut surface,
                        Rect::new(3, 2, bad_w, bad_h),
                        &hit,
                    )
                    .is_err()
                );
                assert!(decoder.coverage().is_empty());
                assert!(surface.damage.take().is_empty());
                assert_eq!(read(&surface), expected);
            }
        }
    }

    #[test]
    fn clearcodec_reshaped_glyphs_replace_stale_pixels() {
        check_clearcodec_reshaped_glyphs(Surface::new(24, 24), |surface| surface.data.clone());
    }

    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_lossless_reshaped_glyphs_replace_stale_pixels() {
        let context = crate::gpu::tests::context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let surface = Surface::new_gpu(24, 24, Arc::clone(&context), Arc::clone(&error));
        check_clearcodec_reshaped_glyphs(surface, |surface| {
            crate::gpu::tests::read(&context, &surface.gpu.as_ref().unwrap().image)
        });
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    fn check_clearcodec_nscodec(mut surface: Surface, read: impl FnOnce(&Surface) -> Vec<u8>) {
        // An 8x4 grayscale NSCodec picture: raw luma, compressed neutral chroma, omitted alpha.
        let mut nsc = Vec::new();
        for count in [32u32, 7, 7, 0] {
            nsc.extend_from_slice(&count.to_le_bytes());
        }
        nsc.extend_from_slice(&[1, 0, 0, 0]); // color loss level 1, no subsampling, reserved
        nsc.extend((0..32u8).map(|i| 60 + i * 4));
        for _ in 0..2 {
            nsc.extend_from_slice(&[0, 0, 26, 0, 0, 0, 0]); // run of 28 + four literal pixels
        }
        let mut subcodec = Vec::new();
        for value in [2u16, 3, 8, 4] {
            subcodec.extend_from_slice(&value.to_le_bytes());
        }
        subcodec.extend_from_slice(&(nsc.len() as u32).to_le_bytes());
        subcodec.push(1); // NSCodec
        subcodec.extend_from_slice(&nsc);
        let mut stream = vec![0, 0];
        for count in [0, 0, subcodec.len() as u32] {
            stream.extend_from_slice(&count.to_le_bytes());
        }
        stream.extend_from_slice(&subcodec);
        let background = [90, 100, 110, 255];
        surface.fill(Rect::new(0, 0, surface.width, surface.height), background);
        surface.damage.take();
        let mut decoder = ClearCodecDecoder::new();
        decode_clearcodec(&mut decoder, &mut surface, Rect::new(3, 2, 16, 12), &stream).unwrap();
        let mut expected = background.repeat((surface.width * surface.height) as usize);
        for i in 0..32u32 {
            let x = 3 + 2 + i % 8;
            let y = 2 + 3 + i / 8;
            let offset = (y * surface.width + x) as usize * 4;
            let v = 60 + i as u8 * 4;
            expected[offset..offset + 4].copy_from_slice(&[v, v, v, 255]);
        }
        assert_eq!(
            read(&surface),
            expected,
            "NSCodec picture became a black rectangle"
        );
        assert_eq!(surface.damage.take(), vec![Rect::new(5, 5, 8, 4)]);
        // Every truncated form must fail rather than silently upload blank pixels.
        for len in 0..nsc.len() {
            let mut truncated = stream[..stream.len() - nsc.len()].to_vec();
            // Update both layer and subcodec byte counts so the NSCodec parser sees the truncation.
            truncated[10..14].copy_from_slice(&(13 + len as u32).to_le_bytes());
            truncated[22..26].copy_from_slice(&(len as u32).to_le_bytes());
            truncated.extend_from_slice(&nsc[..len]);
            assert!(
                decoder.decode(&truncated, 16, 12).is_err(),
                "accepted NSCodec prefix {len}"
            );
        }
        for (offset, value) in [(0, 255), (16, 0), (16, 8), (17, 2), (54, 254)] {
            let mut malformed = stream.clone();
            malformed[27 + offset] = value;
            assert!(
                decoder.decode(&malformed, 16, 12).is_err(),
                "accepted invalid NSCodec field {offset}"
            );
        }
    }

    #[test]
    fn clearcodec_nscodec_replaces_black_rectangles() {
        check_clearcodec_nscodec(Surface::new(24, 24), |surface| surface.data.clone());
    }

    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_lossless_nscodec_replaces_black_rectangles() {
        let context = crate::gpu::tests::context().unwrap();
        let error = Arc::new(Mutex::new(None));
        let surface = Surface::new_gpu(24, 24, Arc::clone(&context), Arc::clone(&error));
        check_clearcodec_nscodec(surface, |surface| {
            crate::gpu::tests::read(&context, &surface.gpu.as_ref().unwrap().image)
        });
        assert!(error.lock().is_none(), "{:?}", error.lock());
    }

    #[test]
    fn avc420_advertisement_preserves_working_wire_order() {
        use ironrdp_egfx::pdu::CapabilitiesAdvertisePdu;

        let caps = graphics_capabilities(GraphicsMode::Auto);
        let wire = ironrdp_core::encode_vec(&CapabilitiesAdvertisePdu::from_typed(&caps)).unwrap();
        // Preserve V8.1 + AVC420_ENABLED followed by the V8 fallback.
        assert_eq!(
            wire,
            [
                2, 0, // capsSetCount
                5, 1, 8, 0, 4, 0, 0, 0, 16, 0, 0, 0, // V8.1 + AVC420
                4, 0, 8, 0, 4, 0, 0, 0, 0, 0, 0, 0, // V8
            ]
        );
    }

    #[test]
    fn graphics_modes_preserve_codec_choices() {
        let full = graphics_capabilities(GraphicsMode::FullColor);
        assert!(matches!(full[2], CapabilitySet::V8 { .. }));
        assert!(matches!(full[1], CapabilitySet::V8_1 { .. }));
        assert!(matches!(full[0], CapabilitySet::V10_7 { flags }
            if !flags.contains(CapabilitiesV107Flags::AVC_DISABLED)));
        for mode in [GraphicsMode::SharpText, GraphicsMode::Compatible] {
            let caps = graphics_capabilities(mode);
            assert_eq!(caps.len(), 2);
            assert!(matches!(caps[1], CapabilitySet::V8 { .. }));
            assert!(matches!(caps[0], CapabilitySet::V10_7 { flags }
                if flags.contains(CapabilitiesV107Flags::AVC_DISABLED)));
        }
    }

    #[test]
    fn progressive_contexts_are_scoped_and_released_per_surface() {
        let first = include_bytes!("testdata/progressive-std.1.bin");
        let upgrade = include_bytes!("testdata/progressive-std.2.bin");
        let other_first = include_bytes!("testdata/progressive-rex.1.bin");
        let other_upgrade = include_bytes!("testdata/progressive-rex.2.bin");
        let mut expected = ProgressiveDecoder::new();
        expected.decode_bitmap(0, 128, 64, first).unwrap();
        let expected = expected.decode_bitmap(0, 128, 64, upgrade).unwrap();
        let mut other_expected = ProgressiveDecoder::new();
        other_expected
            .decode_bitmap(0, 128, 64, other_first)
            .unwrap();
        let other_expected = other_expected
            .decode_bitmap(0, 128, 64, other_upgrade)
            .unwrap();

        let mut contexts = ProgressiveContexts::default();
        contexts.decode(1, 0, 128, 64, first).unwrap();
        // A different surface may reuse context 0 with different tile state.
        contexts.decode(2, 0, 128, 64, other_first).unwrap();
        let actual = contexts.decode(1, 0, 128, 64, upgrade).unwrap();
        assert!(!actual.is_empty());
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(&expected) {
            assert_eq!(actual.pixels, expected.pixels);
        }

        contexts.delete_context(1, 0);
        assert!(contexts.decode(1, 0, 128, 64, upgrade).unwrap().is_empty());
        let other_actual = contexts.decode(2, 0, 128, 64, other_upgrade).unwrap();
        assert_eq!(other_actual.len(), other_expected.len());
        for (actual, expected) in other_actual.iter().zip(&other_expected) {
            assert_eq!(actual.pixels, expected.pixels);
        }

        contexts.decode(1, 0, 128, 64, first).unwrap();
        contexts.remove_surface(1);
        assert!(contexts.decode(1, 0, 128, 64, upgrade).unwrap().is_empty());
        contexts.reset();
        assert!(
            contexts
                .decode(2, 0, 128, 64, other_upgrade)
                .unwrap()
                .is_empty()
        );
    }

    /// Windows sends one-colour RLEX tiles with a packed index byte before each run length;
    /// upstream IronRDP rejected them ("suite exceeds region pixel count").
    #[test]
    fn clearcodec_single_colour_rlex_tile() {
        let rlex = [1, 0x10, 0x20, 0x30, 0x00, 7]; // one BGR entry; 7 run + 1 suite pixel
        let mut subcodec = Vec::new();
        for value in [0u16, 0, 4, 2] {
            subcodec.extend_from_slice(&value.to_le_bytes()); // x, y, width, height
        }
        subcodec.extend_from_slice(&(rlex.len() as u32).to_le_bytes());
        subcodec.push(2); // RLEX
        subcodec.extend_from_slice(&rlex);
        let mut stream = vec![0, 0]; // flags, sequence number
        for count in [0, 0, subcodec.len() as u32] {
            stream.extend_from_slice(&count.to_le_bytes()); // residual, bands, subcodecs
        }
        stream.extend_from_slice(&subcodec);
        let pixels = ClearCodecDecoder::new().decode(&stream, 4, 2).unwrap();
        assert_eq!(pixels, [0x10, 0x20, 0x30, 0xFF].repeat(8));
    }

    /// Random ClearCodec tiles covering every layer and cache must decode exactly like FreeRDP
    /// (which interoperates with Windows), including which pixels they leave alone. See
    /// `write_clearcodec_streams`.
    #[test]
    fn clearcodec_matches_freerdp() {
        check_clearcodec_reference(
            include_bytes!("testdata/clearcodec.streams"),
            include_bytes!("testdata/clearcodec.reference"),
        );
    }

    #[test]
    fn clearcodec_nscodec_matches_freerdp() {
        // 392 independent encoder/decoder cases: every color loss level, both sampling
        // modes, odd and padded dimensions, flat colors, stripes, gradients, and noise.
        check_clearcodec_reference(
            include_bytes!("testdata/clearcodec-nsc.streams"),
            include_bytes!("testdata/clearcodec-nsc.reference"),
        );
    }

    fn check_clearcodec_reference(streams: &[u8], reference: &[u8]) {
        let mut decoder = ClearCodecDecoder::new();
        let mut pos = 0;
        for (n, expected) in reference.chunks_exact(8).enumerate() {
            let w = u16::from_le_bytes([streams[pos], streams[pos + 1]]);
            let h = u16::from_le_bytes([streams[pos + 2], streams[pos + 3]]);
            let len = u32::from_le_bytes(streams[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let data = &streams[pos + 8..pos + 8 + len];
            pos += 8 + len;
            let pixels = decoder
                .decode(data, w, h)
                .unwrap_or_else(|e| panic!("stream {n}: {e}"));
            // As FreeRDP sees it: drawn onto a tile filled with 0x5A.
            let mut tile = vec![0x5Au8; usize::from(w) * usize::from(h) * 4];
            for area in decoder.coverage() {
                for y in area.y..area.y + area.height {
                    let start = (usize::from(y) * usize::from(w) + usize::from(area.x)) * 4;
                    let end = start + usize::from(area.width) * 4;
                    tile[start..end].copy_from_slice(&pixels[start..end]);
                }
            }
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for (i, byte) in tile.iter().enumerate() {
                if i % 4 != 3 {
                    hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
                }
            }
            assert_eq!(
                hash.to_le_bytes(),
                expected,
                "stream {n} ({w}x{h}) differs from FreeRDP"
            );
        }
        assert_eq!(pos, streams.len());
    }

    /// Writes `clearcodec.streams`: a sequence of random but valid ClearCodec tiles exercising
    /// every layer (residual, all three V-bar kinds, uncompressed and RLEX subcodecs) and the
    /// glyph cache, decoded in order by one decoder. Each record is width (u16), height (u16),
    /// length (u32) and the stream. FreeRDP's decodes (`clearref` in `testdata/clearref.c`)
    /// give `clearcodec.reference`, one FNV-1a hash of the decoded BGRA tile per stream.
    ///
    ///     RELAY_FIXTURE_DIR=... cargo test --bin relay write_clearcodec_streams -- --ignored
    #[test]
    #[ignore = "generates test fixtures"]
    fn write_clearcodec_streams() {
        let dir = std::path::PathBuf::from(std::env::var_os("RELAY_FIXTURE_DIR").unwrap());
        let mut seed = 0x1234_5678_u32;
        let mut rand = move |n: u32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed % n.max(1)
        };
        fn run_length(out: &mut Vec<u8>, run: u32) {
            if run < 0xFF {
                out.push(run as u8);
            } else if run < 0xFFFF {
                out.push(0xFF);
                out.extend_from_slice(&(run as u16).to_le_bytes());
            } else {
                out.push(0xFF);
                out.extend_from_slice(&0xFFFFu16.to_le_bytes());
                out.extend_from_slice(&run.to_le_bytes());
            }
        }
        let palette: Vec<[u8; 3]> = (0..12)
            .map(|i| [(i * 37) as u8, (i * 91 + 20) as u8, (255 - i * 13) as u8])
            .collect();
        let mut short_bars: Vec<u32> = Vec::new(); // pixel count per short V-bar entry
        let mut full_bars: Vec<u32> = Vec::new(); // height per full V-bar entry
        let mut glyphs: Vec<(u16, u16, u16)> = Vec::new(); // (index, width, height)
        let mut records = Vec::new();
        for seq in 0..400u32 {
            let (w, h) = (1 + rand(64) as u16, 1 + rand(52) as u16);
            let mut stream = vec![0u8, (seq % 256) as u8];
            // Glyph hits replay a cached tile of the same size.
            if rand(6) == 0
                && let Some(&(index, gw, gh)) = glyphs.get(rand(glyphs.len() as u32) as usize)
            {
                stream[0] = 0x03; // GLYPH_INDEX | GLYPH_HIT
                stream.extend_from_slice(&index.to_le_bytes());
                records.push((gw, gh, stream));
                continue;
            }
            let glyph = u32::from(w) * u32::from(h) <= 1024 && rand(4) == 0;
            if glyph {
                let index = rand(4000) as u16;
                stream[0] = 0x01;
                stream.extend_from_slice(&index.to_le_bytes());
                glyphs.retain(|g| g.0 != index);
                glyphs.push((index, w, h));
            }
            if seq == 0 {
                stream[0] |= 0x04; // CACHE_RESET
            }
            // Residual: runs covering the whole tile (always for glyphs, so the cache holds
            // fully defined pixels).
            let mut residual = Vec::new();
            if glyph || rand(3) != 0 {
                let mut left = u32::from(w) * u32::from(h);
                while left > 0 {
                    let run = if rand(8) == 0 {
                        left
                    } else {
                        1 + rand(left.min(300))
                    };
                    let c = palette[rand(palette.len() as u32) as usize];
                    residual.extend_from_slice(&c);
                    run_length(&mut residual, run);
                    left -= run;
                }
            }
            // Bands of V-bars.
            let mut bands = Vec::new();
            for _ in 0..rand(3) {
                let x0 = rand(u32::from(w)) as u16;
                let x1 = x0 + rand(u32::from(w - x0)) as u16;
                let y0 = rand(u32::from(h)) as u16;
                let height = 1 + rand(u32::from(h - y0).min(52));
                let y1 = y0 + height as u16 - 1;
                for v in [x0, x1, y0, y1] {
                    bands.extend_from_slice(&v.to_le_bytes());
                }
                bands.extend_from_slice(&palette[rand(palette.len() as u32) as usize]);
                for _ in x0..=x1 {
                    let full_hits: Vec<usize> = full_bars
                        .iter()
                        .enumerate()
                        .filter(|(_, bar)| **bar == height)
                        .map(|(i, _)| i)
                        .collect();
                    let short_hits: Vec<usize> = short_bars
                        .iter()
                        .enumerate()
                        .filter(|(_, count)| **count <= height)
                        .map(|(i, _)| i)
                        .collect();
                    match rand(3) {
                        0 if !full_hits.is_empty() => {
                            let index = full_hits[rand(full_hits.len() as u32) as usize];
                            bands.extend_from_slice(&(0x8000 | index as u16).to_le_bytes());
                        }
                        1 if !short_hits.is_empty() => {
                            let index = short_hits[rand(short_hits.len() as u32) as usize];
                            let y_on = rand(height - short_bars[index] + 1);
                            bands.extend_from_slice(&(0x4000 | index as u16).to_le_bytes());
                            bands.push(y_on as u8);
                            full_bars.push(height);
                        }
                        _ => {
                            let y_on = rand(height + 1);
                            let y_off = y_on + rand(height - y_on + 1);
                            bands.extend_from_slice(&((y_off << 8 | y_on) as u16).to_le_bytes());
                            for _ in y_on..y_off {
                                bands.extend_from_slice(&[
                                    rand(256) as u8,
                                    rand(256) as u8,
                                    rand(256) as u8,
                                ]);
                            }
                            short_bars.push(y_off - y_on);
                            full_bars.push(height);
                        }
                    }
                }
            }
            // Subcodec regions: uncompressed or RLEX.
            let mut subcodecs = Vec::new();
            for _ in 0..rand(3) {
                let x = rand(u32::from(w)) as u16;
                let y = rand(u32::from(h)) as u16;
                let sw = 1 + rand(u32::from(w - x)) as u16;
                let sh = 1 + rand(u32::from(h - y)) as u16;
                let pixels = u32::from(sw) * u32::from(sh);
                let mut data = Vec::new();
                let codec = if rand(2) == 0 {
                    for _ in 0..pixels {
                        data.extend_from_slice(&[
                            rand(256) as u8,
                            rand(256) as u8,
                            rand(256) as u8,
                        ]);
                    }
                    0u8
                } else {
                    let count = 1 + rand(12);
                    data.push(count as u8);
                    for i in 0..count {
                        data.extend_from_slice(&palette[i as usize]);
                    }
                    let bits = if count == 1 {
                        1
                    } else {
                        32 - (count - 1).leading_zeros()
                    };
                    let mut left = pixels;
                    while left > 0 {
                        let stop = rand(count);
                        let depth = rand((stop + 1).min(1 << (8 - bits)).min(left));
                        let run = if left > depth + 1 && rand(2) == 0 {
                            rand(left - depth - 1).min(400)
                        } else {
                            0
                        };
                        let run = if rand(10) == 0 { left - depth - 1 } else { run };
                        data.push((stop | depth << bits) as u8);
                        run_length(&mut data, run);
                        left -= run + depth + 1;
                    }
                    2
                };
                for v in [x, y, sw, sh] {
                    subcodecs.extend_from_slice(&v.to_le_bytes());
                }
                subcodecs.extend_from_slice(&(data.len() as u32).to_le_bytes());
                subcodecs.push(codec);
                subcodecs.extend_from_slice(&data);
            }
            for layer in [&residual, &bands, &subcodecs] {
                stream.extend_from_slice(&(layer.len() as u32).to_le_bytes());
            }
            stream.extend_from_slice(&residual);
            stream.extend_from_slice(&bands);
            stream.extend_from_slice(&subcodecs);
            records.push((w, h, stream));
        }
        let mut file = Vec::new();
        for (w, h, stream) in &records {
            file.extend_from_slice(&w.to_le_bytes());
            file.extend_from_slice(&h.to_le_bytes());
            file.extend_from_slice(&(stream.len() as u32).to_le_bytes());
            file.extend_from_slice(stream);
        }
        std::fs::write(dir.join("clearcodec.streams"), file).unwrap();
    }

    /// Writes `progressive-{std,rex}.{1,2}.bin`: two 64x64 tiles as TILE_FIRST at a coarse
    /// quality, then TILE_UPGRADE to full quality, with the standard and the reduce-extrapolate
    /// (Windows' usual) DWT. The coefficients are synthetic but in the range of real pictures,
    /// so the streams test the decoding steps rather than an encoder. FreeRDP's decodes
    /// (`progref --decode`, in `testdata/progref.c`) are the reference.
    ///
    ///     RELAY_FIXTURE_DIR=... cargo test --bin relay write_progressive_passes -- --ignored
    #[test]
    #[ignore = "generates test fixtures"]
    fn write_progressive_passes() {
        use ironrdp_graphics::progressive::{
            SIGN_NEGATIVE, SIGN_POSITIVE, SIGN_ZERO, encode_upgrade_pass,
        };
        use ironrdp_pdu::codecs::rfx::progressive::{
            ComponentCodecQuant, ProgressiveBlock, ProgressiveCodecQuant, ProgressiveContextPdu,
            ProgressiveFrameBeginPdu, ProgressiveFrameEndPdu, ProgressiveRegion,
            ProgressiveSyncPdu, ProgressiveTile, TileFirst, TileUpgrade, encode_progressive_stream,
        };
        use ironrdp_pdu::codecs::rfx::{EntropyAlgorithm, RfxRectangle};
        let dir = std::path::PathBuf::from(std::env::var_os("RELAY_FIXTURE_DIR").unwrap());
        let mut seed = 0x9e37_79b9_u32;
        let mut random = move |range: i32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % (2 * range as u32 + 1)) as i32 - range
        };
        // Quant 6 (FreeRDP's minimum, as Windows uses): the decoder shifts each value by
        // quant - 1 + bitPos, so `precise` below is in units of 32.
        let base = ComponentCodecQuant {
            ll3: 6,
            hl3: 6,
            lh3: 6,
            hh3: 6,
            hl2: 6,
            lh2: 6,
            hh2: 6,
            hl1: 6,
            lh1: 6,
            hh1: 6,
        };
        let coarse_bits = 2;
        let coarse = ComponentCodecQuant {
            ll3: coarse_bits,
            hl3: coarse_bits,
            lh3: coarse_bits,
            hh3: coarse_bits,
            hl2: coarse_bits,
            lh2: coarse_bits,
            hh2: coarse_bits,
            hl1: coarse_bits,
            lh1: coarse_bits,
            hh1: coarse_bits,
        };
        for (name, extrapolate) in [("std", false), ("rex", true)] {
            let ll3 = if extrapolate { 4015 } else { 4032 };
            let mut first: Vec<[Vec<u8>; 3]> = Vec::new();
            let mut upgrade: Vec<[(Vec<u8>, Vec<u8>); 3]> = Vec::new();
            for _tile in 0..2 {
                let mut tile_first: [Vec<u8>; 3] = Default::default();
                let mut tile_upgrade: [(Vec<u8>, Vec<u8>); 3] = Default::default();
                for c in 0..3 {
                    // Low band around mid-grey with moderate chroma, sparse detail.
                    let mut precise = [0i16; 4096];
                    for (i, v) in precise.iter_mut().enumerate() {
                        *v = if i >= ll3 {
                            (if c == 0 { 8 } else { 0 } + random(40)) as i16
                        } else if random(3) == 0 {
                            random(6) as i16
                        } else {
                            0
                        };
                    }
                    // First pass: drop two bits (toward zero), delta-code LL3, RLGR1.
                    let mut wire: Vec<i16> = precise
                        .iter()
                        .map(|&v| v.signum() * (v.abs() >> coarse_bits))
                        .collect();
                    let previous: Vec<i16> = wire.iter().map(|v| v << coarse_bits).collect();
                    ironrdp_graphics::subband_reconstruction::encode(&mut wire[ll3..]);
                    let mut out = vec![0u8; 16384];
                    let n =
                        ironrdp_graphics::rlgr::encode(EntropyAlgorithm::Rlgr1, &wire, &mut out)
                            .unwrap();
                    tile_first[c] = out[..n].to_vec();
                    // FreeRDP's sign state: the first pass's RLGR values.
                    let sign: Vec<i8> = wire
                        .iter()
                        .map(|v| match v.signum() {
                            1 => SIGN_POSITIVE,
                            -1 => SIGN_NEGATIVE,
                            _ => SIGN_ZERO,
                        })
                        .collect();
                    tile_upgrade[c] = encode_upgrade_pass(
                        &precise,
                        &previous,
                        &coarse,
                        &ComponentCodecQuant::LOSSLESS,
                        &sign,
                        extrapolate,
                    );
                }
                first.push(tile_first);
                upgrade.push(tile_upgrade);
            }
            let region = |tiles| {
                ProgressiveBlock::Region(ProgressiveRegion {
                    tile_size: 64,
                    rects: vec![RfxRectangle {
                        x: 0,
                        y: 0,
                        width: 128,
                        height: 64,
                    }],
                    quant_vals: vec![base],
                    quant_prog_vals: vec![ProgressiveCodecQuant {
                        quality: 50,
                        y_quant: coarse,
                        cb_quant: coarse,
                        cr_quant: coarse,
                    }],
                    flags: u8::from(extrapolate), // RFX_DWT_REDUCE_EXTRAPOLATE
                    tiles,
                })
            };
            let first_tiles = first
                .iter()
                .enumerate()
                .map(|(i, [y, cb, cr])| {
                    ProgressiveTile::First(TileFirst {
                        quant_idx_y: 0,
                        quant_idx_cb: 0,
                        quant_idx_cr: 0,
                        x_idx: i as u16,
                        y_idx: 0,
                        flags: 0,
                        quality: 0,
                        y_data: y,
                        cb_data: cb,
                        cr_data: cr,
                        tail_data: &[],
                    })
                })
                .collect();
            let frame1 = encode_progressive_stream(&[
                ProgressiveBlock::Sync(ProgressiveSyncPdu),
                ProgressiveBlock::Context(ProgressiveContextPdu {
                    context_id: 0,
                    tile_size: 64,
                    flags: 0x01, // RFX_SUBBAND_DIFFING
                }),
                ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                    frame_index: 0,
                    region_count: 1,
                }),
                region(first_tiles),
                ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
            ])
            .unwrap();
            let upgrade_tiles = upgrade
                .iter()
                .enumerate()
                .map(|(i, [(ys, yr), (cbs, cbr), (crs, crr)])| {
                    ProgressiveTile::Upgrade(TileUpgrade {
                        quant_idx_y: 0,
                        quant_idx_cb: 0,
                        quant_idx_cr: 0,
                        x_idx: i as u16,
                        y_idx: 0,
                        quality: 0xFF,
                        y_srl_data: ys,
                        y_raw_data: yr,
                        cb_srl_data: cbs,
                        cb_raw_data: cbr,
                        cr_srl_data: crs,
                        cr_raw_data: crr,
                    })
                })
                .collect();
            let frame2 = encode_progressive_stream(&[
                ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
                    frame_index: 1,
                    region_count: 1,
                }),
                region(upgrade_tiles),
                ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
            ])
            .unwrap();
            std::fs::write(dir.join(format!("progressive-{name}.1.bin")), frame1).unwrap();
            std::fs::write(dir.join(format!("progressive-{name}.2.bin")), frame2).unwrap();
        }
    }

    /// A FreeRDP-encoded progressive picture must decode like FreeRDP decodes it. The fixtures
    /// come from `testdata/progref.c` (FreeRDP 3.31): `progref DIR 160 96`.
    #[test]
    fn progressive_matches_freerdp() {
        let (w, h) = (160usize, 96usize);
        let stream = include_bytes!("testdata/progressive-160x96.bin");
        let reference = include_bytes!("testdata/progressive-160x96.rgba");
        let tiles = ProgressiveDecoder::new()
            .decode_bitmap(1, w as u16, h as u16, stream)
            .unwrap();
        assert!(!tiles.is_empty());
        let mut worst = 0;
        for tile in &tiles {
            for ty in 0..64 {
                for tx in 0..64 {
                    let (x, y) = (
                        usize::from(tile.x_idx) * 64 + tx,
                        usize::from(tile.y_idx) * 64 + ty,
                    );
                    if x >= w || y >= h {
                        continue;
                    }
                    let a = &tile.pixels[(ty * 64 + tx) * 4..][..3];
                    let e = &reference[(y * w + x) * 4..][..3];
                    for c in 0..3 {
                        worst = worst.max((i32::from(a[c]) - i32::from(e[c])).abs());
                    }
                }
            }
        }
        assert!(
            worst <= 2,
            "largest channel difference from FreeRDP: {worst}"
        );
    }

    /// First and upgrade passes, standard and reduce-extrapolate DWT, must decode like FreeRDP
    /// (which interoperates with Windows). See `write_progressive_passes`.
    #[test]
    fn progressive_passes_match_freerdp() {
        let fixtures: [(&str, [&[u8]; 2], [&[u8]; 2]); 2] = [
            (
                "std",
                [
                    include_bytes!("testdata/progressive-std.1.bin"),
                    include_bytes!("testdata/progressive-std.2.bin"),
                ],
                [
                    include_bytes!("testdata/progressive-std.reference.1.rgba"),
                    include_bytes!("testdata/progressive-std.reference.2.rgba"),
                ],
            ),
            (
                "rex",
                [
                    include_bytes!("testdata/progressive-rex.1.bin"),
                    include_bytes!("testdata/progressive-rex.2.bin"),
                ],
                [
                    include_bytes!("testdata/progressive-rex.reference.1.rgba"),
                    include_bytes!("testdata/progressive-rex.reference.2.rgba"),
                ],
            ),
        ];
        let w = 128;
        for (name, frames, references) in fixtures {
            let mut decoder = ProgressiveDecoder::new();
            for (pass, (frame, reference)) in frames.iter().zip(references).enumerate() {
                let tiles = decoder.decode_bitmap(1, 128, 64, frame).unwrap();
                assert_eq!(tiles.len(), 2, "{name} pass {}", pass + 1);
                let mut worst = 0;
                for tile in &tiles {
                    for y in 0..64 {
                        for x in 0..64 {
                            let a = &tile.pixels[(y * 64 + x) * 4..][..3];
                            let e =
                                &reference[(y * w + usize::from(tile.x_idx) * 64 + x) * 4..][..3];
                            for c in 0..3 {
                                worst = worst.max((i32::from(a[c]) - i32::from(e[c])).abs());
                            }
                        }
                    }
                }
                assert!(
                    worst <= 1,
                    "{name} pass {}: differs from FreeRDP by {worst}",
                    pass + 1
                );
            }
        }
    }
}
