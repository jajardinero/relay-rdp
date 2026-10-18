//! H.264 (AVC420) decoding through OpenH264, loaded at runtime.
//!
//! Native packages use the distribution's library; AppImages bundle a matching build.
//! The Rust bindings target OpenH264 2.6 (soname `libopenh264.so.8`); only that ABI is loaded.
//!
//! Both H.264 modes decode in software through [`SoftVideo`], which keeps per-surface decoders
//! and 4:4:4 pictures (4:2:0 pictures fill them as main views), so only the regions the server
//! updated are drawn.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

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

/// Finds OpenH264. `RELAY_OPENH264` may point at a Cisco binary instead, which is verified
/// against its published hash before it is used.
pub fn locate() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("RELAY_OPENH264") {
        let path = PathBuf::from(path);
        OpenH264API::from_blob_path(&path)?;
        return Ok(path);
    }
    // AppImages keep the supported codec next to the executable's bin directory.
    // Prefer it over a host codec so the bundle remains independent of the distro ABI.
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| bundled_library(&exe));
    bundled
        .into_iter()
        .chain(SEARCH_DIRS.iter().map(|dir| Path::new(dir).join(SONAME)))
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("{SONAME} not found (install the openh264 package)"))
}

fn bundled_library(executable: &Path) -> Option<PathBuf> {
    let library = executable.parent()?.parent()?.join("lib").join(SONAME);
    library.is_file().then_some(library)
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
