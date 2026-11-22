//! Remote desktop framebuffers and remote bitmap cursors converted to GPUI `RenderImage`s.
//!
//! GPUI's `RenderImage` expects tightly packed BGRA8 frames. Each active session keeps a
//! persistent BGRA buffer so incremental CPU dirty rects only swizzle changed pixels, and
//! GPU-composited frames (e.g. VA-API H.264) are read back on demand. Replaced images are
//! queued and dropped from GPUI's sprite atlas on the next paint to prevent VRAM leaks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{RenderImage, Window};
use smallvec::SmallVec;

use crate::gpu::GpuContext;
use crate::session::{SessionId, SharedFrame};

struct RemoteSurface {
    width: u32,
    height: u32,
    /// Persistent BGRA8 pixel buffer matching `width * height * 4`.
    bgra: Vec<u8>,
    image: Option<Arc<RenderImage>>,
    /// Tracks the GPU image pointer we last read back (still re-reads when wake was signaled).
    last_gpu_ptr: usize,
}

#[derive(Clone)]
pub struct FrameImage {
    pub image: Arc<RenderImage>,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
pub struct RemoteTextures {
    surfaces: HashMap<SessionId, RemoteSurface>,
    stale_images: Vec<Arc<RenderImage>>,
}

impl RemoteTextures {
    /// Drops any replaced or closed `RenderImage`s from GPUI's sprite atlas.
    pub fn flush_dropped(&mut self, window: &mut Window) {
        for image in self.stale_images.drain(..) {
            let _ = window.drop_image(image);
        }
    }

    /// Synchronizes all active session frames with GPUI `RenderImage`s.
    pub fn sync(
        &mut self,
        gpu: Option<&Arc<GpuContext>>,
        sessions: &[(SessionId, Arc<SharedFrame>)],
    ) {
        let mut stale = Vec::new();
        self.surfaces.retain(|id, surface| {
            let alive = sessions.iter().any(|(sid, _)| sid == id);
            if !alive && let Some(img) = surface.image.take() {
                stale.push(img);
            }
            alive
        });
        self.stale_images.extend(stale);

        for (id, frame) in sessions {
            let woke = frame.take_wake();
            let mut data = frame.lock();
            let (width, height) = (data.width(), data.height());
            if width == 0 || height == 0 {
                continue;
            }

            let surface = self.surfaces.entry(*id).or_insert_with(|| RemoteSurface {
                width: 0,
                height: 0,
                bgra: Vec::new(),
                image: None,
                last_gpu_ptr: 0,
            });

            if let Some(gpu_image) = &data.gpu {
                let ptr = Arc::as_ptr(gpu_image) as usize;
                let needs_read = woke
                    || surface.image.is_none()
                    || surface.width != width
                    || surface.height != height
                    || surface.last_gpu_ptr != ptr;
                if !needs_read {
                    continue;
                }
                let Some(gpu) = gpu else { continue };
                let (buffer, stride) = gpu.read_image(gpu_image);
                drop(data);

                let slice = buffer.slice(..);
                let (tx, rx) = std::sync::mpsc::channel();
                slice.map_async(wgpu::MapMode::Read, move |res| {
                    let _ = tx.send(res);
                });
                let _ = gpu.device.poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(Duration::from_secs(2)),
                });
                if rx.recv().ok().and_then(Result::ok).is_none() {
                    continue;
                }
                let Ok(mapped) = slice.get_mapped_range() else {
                    continue;
                };

                surface.width = width;
                surface.height = height;
                surface.last_gpu_ptr = ptr;
                surface.bgra.resize((width as usize) * (height as usize) * 4, 0);
                rgba_to_bgra_strided(&mapped, stride as usize, &mut surface.bgra, width, height);
                drop(mapped);
                buffer.unmap();

                if let Some(old) = surface.image.replace(make_render_image(
                    width,
                    height,
                    surface.bgra.clone(),
                )) {
                    self.stale_images.push(old);
                }
                continue;
            }

            let resized = surface.width != width || surface.height != height || surface.image.is_none();
            if resized {
                surface.width = width;
                surface.height = height;
                surface.last_gpu_ptr = 0;
                surface.bgra.resize((width as usize) * (height as usize) * 4, 0);
                data.mark_all_dirty();
            }

            let dirty = data.take_dirty();
            if dirty.is_empty() && !resized {
                continue;
            }

            let src = data.data();
            let stride = data.stride();
            let dst_stride = width as usize * 4;
            for rect in dirty {
                let row_pixels = rect.w as usize;
                for y in rect.y..(rect.y + rect.h).min(height) {
                    let src_row = &src[y as usize * stride + rect.x as usize * 4..];
                    let dst_row = &mut surface.bgra[y as usize * dst_stride + rect.x as usize * 4..];
                    let bytes = row_pixels * 4;
                    if src_row.len() < bytes || dst_row.len() < bytes {
                        break;
                    }
                    rgba_slice_to_bgra(&src_row[..bytes], &mut dst_row[..bytes]);
                }
            }
            drop(data);

            if let Some(old) = surface.image.replace(make_render_image(
                width,
                height,
                surface.bgra.clone(),
            )) {
                self.stale_images.push(old);
            }
        }
    }

    pub fn get(&self, id: SessionId) -> Option<FrameImage> {
        let surface = self.surfaces.get(&id)?;
        let image = surface.image.as_ref()?;
        Some(FrameImage {
            image: Arc::clone(image),
            width: surface.width,
            height: surface.height,
        })
    }
}

fn make_render_image(width: u32, height: u32, bgra: Vec<u8>) -> Arc<RenderImage> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)
        .expect("BGRA buffer size matches width * height * 4");
    let frame = image::Frame::new(buffer);
    let mut frames = SmallVec::new();
    frames.push(frame);
    Arc::new(RenderImage::new(frames))
}

#[inline]
fn rgba_slice_to_bgra(src: &[u8], dst: &mut [u8]) {
    let (src_chunks, _) = src.as_chunks::<4>();
    let (dst_chunks, _) = dst.as_chunks_mut::<4>();
    for (s, d) in src_chunks.iter().zip(dst_chunks.iter_mut()) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

fn rgba_to_bgra_strided(src: &[u8], src_stride: usize, dst: &mut [u8], width: u32, height: u32) {
    let row_bytes = width as usize * 4;
    for y in 0..height as usize {
        let s = &src[y * src_stride..y * src_stride + row_bytes];
        let d = &mut dst[y * row_bytes..(y + 1) * row_bytes];
        rgba_slice_to_bgra(s, d);
    }
}
