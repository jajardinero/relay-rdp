//! The Graphics Pipeline (MS-RDPEGFX): H.264 and the lossless/progressive codecs.
//!
//! `ironrdp-egfx` handles the channel protocol (capabilities, ZGFX, frame acknowledgements,
//! AVC420 decoding through our decoder). Everything visual happens here: surfaces, solid fills,
//! surface-to-surface copies, the bitmap cache, the remaining codecs, and composition of the
//! mapped surfaces onto the shared frame at the end of every server frame.

mod h264;
pub(crate) mod surface;
pub(crate) mod vaapi;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ironrdp_egfx::client::{
    BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler, Surface as GfxSurface,
};
use ironrdp_egfx::decode::H264Decoder;
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, CapabilitiesV8Flags, CapabilitiesV81Flags, CapabilitiesV107Flags,
    CapabilitySet, Codec1Type, Codec2Type, DeleteEncodingContextPdu, EvictCacheEntryPdu, GfxPdu,
    SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu, WireToSurface2Pdu,
};
use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::progressive::ProgressiveDecoder;
use ironrdp_graphics::rdp6::BitmapStreamDecoder;
use ironrdp_pdu::geometry::{ExclusiveRectangle, InclusiveRectangle};
use ironrdp_session::image::DecodedImage;
use parking_lot::Mutex;

use self::surface::{SrcFormat, Surface};
use super::framebuffer::Rect;
use super::{EventSink, SessionEvent, SharedFrame};
use crate::gpu::{GpuContext, GpuImage};
use crate::store::profiles::GraphicsMode;

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
    failure: Mutex<Option<String>>,
}

struct HardwarePipeline {
    context: Arc<GpuContext>,
    error: Arc<Mutex<Option<String>>>,
    output: Option<Arc<GpuImage>>,
    decoders: HashMap<u16, vaapi::Decoder>,
    spare: Option<vaapi::Decoder>,
    annex_b: Vec<u8>,
    announced: bool,
}

struct GfxState {
    gpu: Option<HardwarePipeline>,
    output: (u32, u32),
    surfaces: BTreeMap<u16, Surface>,
    cache: HashMap<u16, Surface>,
    clear: ClearCodecDecoder,
    planar: BitmapStreamDecoder,
    progressive: ProgressiveDecoder,
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
        let gpu = if hardware && mode == GraphicsMode::Auto {
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
        } else if mode == GraphicsMode::Auto {
            "Software (OpenH264)"
        } else {
            "Software (RDP graphics)"
        };
        tracing::info!(decoder = name, ?reason, "video decoder selected");
        events.send(SessionEvent::DecoderStatus {
            name: name.to_owned(),
            reason,
        });
        Arc::new(Self {
            state: Mutex::new(GfxState {
                gpu,
                output: (0, 0),
                surfaces: BTreeMap::new(),
                cache: HashMap::new(),
                clear: ClearCodecDecoder::new(),
                planar: BitmapStreamDecoder::default(),
                progressive: ProgressiveDecoder::new(),
                rfx: ironrdp_session::rfx::DecodingContext::new(),
                scratch: Vec::new(),
                warned_alpha: false,
            }),
            frame,
            events,
            mode,
            presented: AtomicU64::new(0),
            failure: Mutex::new(None),
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
                        self.events.send(SessionEvent::DecoderStatus {
                            name: "Software (lossless RDP codecs)".to_owned(),
                            reason: Some(e.to_string()),
                        });
                        None
                    }
                }
            } else {
                None
            };
        GraphicsPipelineClient::new(Box::new(Handler(Arc::clone(self))), decoder)
    }

    pub fn presented(&self) -> u64 {
        self.presented.load(Ordering::Acquire)
    }

    pub fn hardware_active(&self) -> bool {
        self.state.lock().gpu.is_some()
    }

    pub fn take_failure(&self) -> Option<String> {
        self.failure.lock().take().or_else(|| {
            self.state
                .lock()
                .gpu
                .as_ref()
                .and_then(|gpu| gpu.error.lock().take())
        })
    }

    pub fn output_size(&self) -> Option<(u32, u32)> {
        let output = self.state.lock().output;
        (output.0 > 0 && output.1 > 0).then_some(output)
    }

    fn fail(&self, reason: String) {
        tracing::warn!(%reason, "graphics pipeline failure");
        self.failure.lock().get_or_insert(reason);
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
        self.0.hardware_active()
    }

    fn on_avc420(
        &mut self,
        surface_id: u16,
        dest: &ExclusiveRectangle,
        stream: &ironrdp_egfx::pdu::Avc420BitmapStream<'_>,
    ) -> ironrdp_pdu::PduResult<()> {
        // Recording the failure lets the outer session loop reconnect with
        // software. Never attempt a midstream decoder swap without a keyframe.
        if let Err(error) = self.hardware_frame(surface_id, dest, stream) {
            self.0.fail(error.to_string());
        }
        Ok(())
    }

    fn capabilities(&self) -> Vec<CapabilitySet> {
        match self.0.mode {
            // AVC444 isn't decoded, so H.264 is only offered through 8.1 (which implies AVC420
            // alone); 10.x would let the server pick AVC444.
            GraphicsMode::Auto => vec![
                CapabilitySet::V8_1 {
                    flags: CapabilitiesV81Flags::AVC420_ENABLED,
                },
                CapabilitySet::V8 {
                    flags: CapabilitiesV8Flags::empty(),
                },
            ],
            GraphicsMode::SharpText | GraphicsMode::Compatible => vec![
                CapabilitySet::V10_7 {
                    flags: CapabilitiesV107Flags::AVC_DISABLED,
                },
                CapabilitySet::V8 {
                    flags: CapabilitiesV8Flags::empty(),
                },
            ],
        }
    }

    fn on_capabilities_confirmed(&mut self, caps: &CapabilitySet) {
        tracing::info!(?caps, "graphics pipeline active");
    }

    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        let mut state = self.0.state.lock();
        state.output = (width, height);
        state.surfaces.clear();
        state.cache.clear();
        if let Some(gpu) = &mut state.gpu {
            gpu.decoders.clear();
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
        }
        state.surfaces.insert(surface.id, image);
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        let mut state = self.0.state.lock();
        state.surfaces.remove(&surface_id);
        if let Some(gpu) = &mut state.gpu {
            gpu.decoders.remove(&surface_id);
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
        match progressive.decode_bitmap(pdu.codec_context_id, w, h, &pdu.bitmap_data) {
            Ok(tiles) => {
                for tile in tiles {
                    let r = Rect::new(
                        u32::from(tile.x_idx) * 64,
                        u32::from(tile.y_idx) * 64,
                        64,
                        64,
                    );
                    surface.write(r, &tile.pixels, 64 * 4, SrcFormat::Rgba);
                }
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
            .delete_context(pdu.codec_context_id);
    }

    fn on_unhandled_pdu(&mut self, pdu: &GfxPdu) {
        match pdu {
            GfxPdu::WireToSurface1(wire) => {
                if let Err(reason) = self.wire_to_surface1(wire) {
                    self.0.fail(reason);
                }
            }
            other => tracing::debug!(pdu = ?std::mem::discriminant(other), "ignored graphics PDU"),
        }
    }
}

impl Handler {
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
            self.0.events.send(SessionEvent::DecoderStatus {
                name: "Hardware (VA-API → Vulkan)".to_owned(),
                reason: None,
            });
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
                let (Ok(w16), Ok(h16)) = (u16::try_from(w), u16::try_from(h)) else {
                    return Ok(());
                };
                let bgra = clear
                    .decode(&pdu.bitmap_data, w16, h16)
                    .map_err(|e| format!("ClearCodec: {e}"))?;
                surface.write(dest, &bgra, w as usize * 4, SrcFormat::Bgra);
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
