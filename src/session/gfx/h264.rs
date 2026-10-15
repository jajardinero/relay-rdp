//! H.264 (AVC420) decoding through the system's OpenH264 library, loaded at runtime.
//!
//! Loading the distribution's library avoids compiling OpenH264 (and needing NASM) and keeps
//! the codec patched by the system's package manager. The Rust bindings target OpenH264 2.6
//! (soname `libopenh264.so.8`); only that ABI is loaded.
//!
//! Auto mode uses [`SystemH264`] through IronRDP's decoder hook (4:2:0 only). Full color mode
//! uses [`SoftVideo`], which keeps per-surface decoders and 4:4:4 pictures.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource as _;

use super::avc444::{Layout, Planes, Yuv444};
use crate::session::framebuffer::Rect;

/// The ABI the bindings were generated for.
const SONAME: &str = "libopenh264.so.8";

const SEARCH_DIRS: &[&str] = &[
    "/usr/lib",
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/local/lib",
];

pub struct SystemH264 {
    library: PathBuf,
    decoder: Decoder,
    annex_b: Vec<u8>,
    /// A broken H.264 stream can't recover until the next keyframe, which the server only sends
    /// when asked. Failures are reported here so the session reconnects (getting a keyframe)
    /// instead of ending.
    health: Arc<super::GfxShared>,
}

impl SystemH264 {
    pub fn load(health: Arc<super::GfxShared>) -> anyhow::Result<Self> {
        let library = locate()?;
        let decoder = open(&library)?;
        tracing::info!(library = %library.display(), "H.264 decoder ready");
        Ok(Self {
            library,
            decoder,
            annex_b: Vec::new(),
            health,
        })
    }
}

/// Finds OpenH264. `RELAY_OPENH264` may point at a Cisco binary instead, which is verified
/// against its published hash before it is used.
pub fn locate() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("RELAY_OPENH264") {
        let path = PathBuf::from(path);
        OpenH264API::from_blob_path(&path)?;
        return Ok(path);
    }
    SEARCH_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(SONAME))
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("{SONAME} not found (install the openh264 package)"))
}

pub fn open(library: &Path) -> anyhow::Result<Decoder> {
    // SAFETY: only `libopenh264.so.8` (or a hash-verified Cisco build of it) is loaded, which is
    // the OpenH264 2.6 ABI these bindings were generated against.
    let api = unsafe { OpenH264API::from_blob_path_unchecked(library)? };
    Ok(Decoder::with_api_config(api, DecoderConfig::new())?)
}

/// Which part of a 4:4:4 picture an H.264 view carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The main view (also every plain AVC420 picture).
    Luma,
    Chroma(Layout),
}

/// Software decoding for Full color mode. Every H.264 picture of a surface, 4:2:0 or 4:4:4,
/// goes through that surface's one decoder, as the server encodes them as one stream.
pub struct SoftVideo {
    library: PathBuf,
    surfaces: HashMap<u16, SoftSurface>,
    annex_b: Vec<u8>,
    rgba: Vec<u8>,
}

struct SoftSurface {
    decoder: Decoder,
    store: Yuv444,
}

impl SoftVideo {
    pub fn new() -> anyhow::Result<Self> {
        let library = locate()?;
        open(&library)?;
        tracing::info!(library = %library.display(), "H.264 4:4:4 software decoding ready");
        Ok(Self {
            library,
            surfaces: HashMap::new(),
            annex_b: Vec::new(),
            rgba: Vec::new(),
        })
    }

    pub fn remove(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
    }

    pub fn clear(&mut self) {
        self.surfaces.clear();
    }

    /// Decodes one view into the surface's 4:4:4 picture of `size`, updating `rects`.
    pub fn decode(
        &mut self,
        surface_id: u16,
        size: (usize, usize),
        data: &[u8],
        rects: &[Rect],
        view: View,
    ) -> anyhow::Result<()> {
        let surface = match self.surfaces.entry(surface_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(SoftSurface {
                decoder: open(&self.library)?,
                store: Yuv444::new(size.0, size.1),
            }),
        };
        if surface.store.size() != size {
            surface.store = Yuv444::new(size.0, size.1);
        }
        super::vaapi::annex_b(data, &mut self.annex_b)?;
        let yuv = surface
            .decoder
            .decode(&self.annex_b)?
            .ok_or_else(|| anyhow::anyhow!("H.264 decoder produced no picture"))?;
        let (y_stride, uv_stride, _) = yuv.strides();
        let planes = Planes {
            y: yuv.y(),
            u: yuv.u(),
            v: yuv.v(),
            y_stride,
            uv_stride,
        };
        for &rect in rects {
            match view {
                View::Luma => surface.store.apply_luma(&planes, rect),
                View::Chroma(layout) => surface.store.apply_chroma(&planes, rect, layout),
            }
        }
        Ok(())
    }

    /// Converts `rect` of the surface's picture to RGBA; returns the clipped rect and pixels.
    pub fn render(&mut self, surface_id: u16, rect: Rect) -> Option<(Rect, &[u8])> {
        let surface = self.surfaces.get(&surface_id)?;
        let rect = surface.store.to_rgba(rect, &mut self.rgba)?;
        Some((rect, &self.rgba))
    }
}

impl H264Decoder for SystemH264 {
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
        if let Err(error) = super::vaapi::annex_b(data, &mut self.annex_b) {
            self.health.fail(error.to_string());
            return Err(DecoderError::msg(error.to_string()));
        }
        let yuv = match self.decoder.decode(&self.annex_b) {
            Ok(Some(yuv)) => yuv,
            Ok(None) => {
                self.health
                    .fail("H.264 decoder produced no picture".to_owned());
                return Err(DecoderError::msg("H.264 decoder produced no picture"));
            }
            Err(e) => {
                self.health.fail(format!("H.264 decode: {e}"));
                return Err(DecoderError::new("H.264 decode", e));
            }
        };

        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        self.health.note_h264(false);
        Ok(DecodedFrame::new(rgba, width as u32, height as u32))
    }

    fn reset(&mut self) {
        match open(&self.library) {
            Ok(decoder) => self.decoder = decoder,
            Err(e) => tracing::warn!(error = %e, "couldn't reset the H.264 decoder; reusing it"),
        }
    }
}
