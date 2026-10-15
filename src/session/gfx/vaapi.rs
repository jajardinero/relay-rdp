//! Safe ownership of the small native VA-API decoder bridge.
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;

use anyhow::{Context, bail, ensure};

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Object {
    pub fd: c_int,
    pub size: u32,
    pub modifier: u64,
}
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Layer {
    pub format: u32,
    pub num_planes: u32,
    pub object_index: [u32; 4],
    pub offset: [u32; 4],
    pub pitch: [u32; 4],
}
#[repr(C)]
#[derive(Debug, Default)]
pub struct Description {
    pub width: u32,
    pub height: u32,
    pub allocation_width: u32,
    pub allocation_height: u32,
    pub colorspace: c_int,
    pub full_range: c_int,
    pub chroma_location: c_int,
    pub num_objects: u32,
    pub num_layers: u32,
    pub objects: [Object; 4],
    pub layers: [Layer; 4],
}
unsafe extern "C" {
    fn relay_decoder_new(node: *const c_char, error: *mut c_char, size: c_int) -> *mut c_void;
    fn relay_decoder_free(decoder: *mut c_void);
    fn relay_frame_free(frame: *mut c_void);
    fn relay_decode(
        decoder: *mut c_void,
        data: *const u8,
        len: c_int,
        frame: *mut *mut c_void,
        desc: *mut Description,
        error: *mut c_char,
        size: c_int,
    ) -> c_int;
}

pub struct Decoder(NonNull<c_void>);
// SAFETY: exclusively owned AVCodecContext, used only through &mut self.
unsafe impl Send for Decoder {}
pub struct HardwareFrame {
    raw: NonNull<c_void>,
    pub desc: Description,
}
// SAFETY: an owned immutable AVFrame reference and owned export handles. FFmpeg
// reference counting and VA-API surface release support cross-thread destruction.
unsafe impl Send for HardwareFrame {}
// SAFETY: the frame and descriptor are immutable after export. Native access
// happens only in Drop, after all shared owners and GPU submissions retire.
unsafe impl Sync for HardwareFrame {}

impl Decoder {
    pub fn new(node: &str) -> anyhow::Result<Self> {
        let node = CString::new(node)?;
        let mut error = [0 as c_char; 512];
        // SAFETY: valid NUL-terminated path, writable error buffer of stated size.
        let raw =
            unsafe { relay_decoder_new(node.as_ptr(), error.as_mut_ptr(), error.len() as c_int) };
        Ok(Self(NonNull::new(raw).with_context(|| error_text(&error))?))
    }
    pub fn decode(&mut self, annex_b: &[u8]) -> anyhow::Result<Option<HardwareFrame>> {
        let len = c_int::try_from(annex_b.len()).context("H.264 packet too large")?;
        ensure!(len > 0, "empty H.264 packet");
        let mut raw = std::ptr::null_mut();
        let mut desc = Description::default();
        let mut error = [0 as c_char; 512];
        // SAFETY: the decoder is exclusive, all buffers are valid for the call;
        // the bridge copies the packet into padded FFmpeg-owned storage.
        let result = unsafe {
            relay_decode(
                self.0.as_ptr(),
                annex_b.as_ptr(),
                len,
                &mut raw,
                &mut desc,
                error.as_mut_ptr(),
                error.len() as c_int,
            )
        };
        match result {
            0 => {
                let frame = HardwareFrame {
                    raw: NonNull::new(raw).context("missing hardware picture")?,
                    desc,
                };
                ensure!(
                    frame.desc.width > 0
                        && frame.desc.width <= 8192
                        && frame.desc.height > 0
                        && frame.desc.height <= 8192,
                    "invalid hardware picture dimensions"
                );
                Ok(Some(frame))
            }
            1 => Ok(None),
            _ => bail!("{}", error_text(&error)),
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: uniquely owned context, freed exactly once.
        unsafe { relay_decoder_free(self.0.as_ptr()) };
    }
}
impl Drop for HardwareFrame {
    fn drop(&mut self) {
        // SAFETY: uniquely owned frame and export fds, freed after GPU completion.
        unsafe { relay_frame_free(self.raw.as_ptr()) };
    }
}
fn error_text(error: &[c_char]) -> String {
    // SAFETY: every bridge error is written with snprintf into a zeroed buffer.
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// Strict conversion shared with the software decoder. Truncation must cause
/// recovery rather than feeding a partially valid reference frame to a decoder.
pub fn annex_b(data: &[u8], output: &mut Vec<u8>) -> anyhow::Result<()> {
    output.clear();
    let mut rest = data;
    while !rest.is_empty() {
        ensure!(rest.len() >= 4, "truncated H.264 NAL length");
        let len = u32::from_be_bytes(rest[..4].try_into().expect("checked prefix")) as usize;
        rest = &rest[4..];
        ensure!(len > 0 && len <= rest.len(), "invalid H.264 NAL length");
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&rest[..len]);
        rest = &rest[len..];
    }
    ensure!(!output.is_empty(), "empty H.264 stream");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_nal_boundaries() {
        let mut output = Vec::new();
        annex_b(&[0, 0, 0, 2, 0x67, 0x12, 0, 0, 0, 1, 0x68], &mut output).unwrap();
        assert_eq!(output, [0, 0, 0, 1, 0x67, 0x12, 0, 0, 0, 1, 0x68]);
        for data in [
            &[][..],
            &[0, 0, 0],
            &[0, 0, 0, 0],
            &[0, 0, 0, 2, 0x67],
            &[0, 0, 0, 1, 0x67, 0],
        ] {
            assert!(annex_b(data, &mut output).is_err());
        }
    }
    #[test]
    fn unavailable_device_is_a_recoverable_error() {
        assert!(Decoder::new("/dev/dri/relay-nonexistent-device").is_err());
    }
}
