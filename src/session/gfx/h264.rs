//! H.264 (AVC420) decoding through the system's OpenH264 library, loaded at runtime.
//!
//! Loading the distribution's library avoids compiling OpenH264 (and needing NASM) and keeps
//! the codec patched by the system's package manager. The Rust bindings target OpenH264 2.6
//! (soname `libopenh264.so.8`); only that ABI is loaded.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource as _;

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
    /// Finds and loads OpenH264. `RELAY_OPENH264` may point at a Cisco binary instead, which
    /// is verified against its published hash before loading.
    pub fn load(health: Arc<super::GfxShared>) -> anyhow::Result<Self> {
        if let Some(path) = std::env::var_os("RELAY_OPENH264") {
            let path = PathBuf::from(path);
            let api = OpenH264API::from_blob_path(&path)?;
            let decoder = Decoder::with_api_config(api, DecoderConfig::new())?;
            return Ok(Self {
                library: path,
                decoder,
                annex_b: Vec::new(),
                health,
            });
        }

        let library = SEARCH_DIRS
            .iter()
            .map(|dir| Path::new(dir).join(SONAME))
            .find(|p| p.exists())
            .ok_or_else(|| anyhow::anyhow!("{SONAME} not found (install the openh264 package)"))?;
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

fn open(library: &Path) -> anyhow::Result<Decoder> {
    // SAFETY: only `libopenh264.so.8` is loaded, which is the OpenH264 2.6 ABI these bindings
    // were generated against.
    let api = unsafe { OpenH264API::from_blob_path_unchecked(library)? };
    Ok(Decoder::with_api_config(api, DecoderConfig::new())?)
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
        Ok(DecodedFrame::new(rgba, width as u32, height as u32))
    }

    fn reset(&mut self) {
        match open(&self.library) {
            Ok(decoder) => self.decoder = decoder,
            Err(e) => tracing::warn!(error = %e, "couldn't reset the H.264 decoder; reusing it"),
        }
    }
}
