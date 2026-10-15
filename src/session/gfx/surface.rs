//! Graphics-pipeline surfaces: off-screen RGBA buffers the server draws into, some of which are
//! mapped onto the output (the desktop we display).

use crate::gpu::{GpuContext, GpuImage};
use parking_lot::Mutex;
use std::sync::Arc;

use crate::session::framebuffer::{DirtyRegion, Rect};

/// Pixel layout of incoming data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrcFormat {
    /// RGBA with a meaningful (opaque) alpha.
    Rgba,
    /// RGBA whose alpha byte is undefined.
    Rgbx,
    Bgra,
    Rgb,
}

impl SrcFormat {
    fn bytes_per_pixel(self) -> usize {
        match self {
            SrcFormat::Rgb => 3,
            _ => 4,
        }
    }
}

pub struct GpuSurface {
    pub context: Arc<GpuContext>,
    pub image: Arc<GpuImage>,
    pub error: Arc<Mutex<Option<String>>>,
}

impl GpuSurface {
    fn record(&self, result: anyhow::Result<()>) {
        if let Err(e) = result {
            self.error.lock().get_or_insert_with(|| e.to_string());
        }
    }
}

pub struct Surface {
    pub gpu: Option<GpuSurface>,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Where the surface sits on the output, if mapped.
    pub origin: Option<(u32, u32)>,
    pub damage: DirtyRegion,
}

impl Surface {
    pub fn new(width: u32, height: u32) -> Self {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for alpha in data.iter_mut().skip(3).step_by(4) {
            *alpha = 0xFF;
        }
        Self {
            gpu: None,
            width,
            height,
            data,
            origin: None,
            damage: DirtyRegion::default(),
        }
    }

    pub fn new_gpu(
        width: u32,
        height: u32,
        context: Arc<GpuContext>,
        error: Arc<Mutex<Option<String>>>,
    ) -> Self {
        let image = context.image(width, height);
        let gpu = GpuSurface {
            context,
            image,
            error,
        };
        gpu.record(gpu.context.clear(&gpu.image, [0, 0, 0, 255]));
        Self {
            gpu: Some(gpu),
            width,
            height,
            data: Vec::new(),
            origin: None,
            damage: DirtyRegion::default(),
        }
    }

    /// Snapshot before any destination writes, including overlapping self-copies.
    pub fn snapshot(&self, rect: Rect) -> Option<Surface> {
        let rect = rect.clip(self.width, self.height)?;
        if let Some(gpu) = &self.gpu {
            let mut result = Self::new_gpu(
                rect.w,
                rect.h,
                Arc::clone(&gpu.context),
                Arc::clone(&gpu.error),
            );
            result.copy_gpu(&gpu.image, rect, 0, 0);
            Some(result)
        } else {
            let (_, data) = self.read(rect)?;
            let mut result = Self::new(rect.w, rect.h);
            result.write(
                Rect::new(0, 0, rect.w, rect.h),
                &data,
                rect.w as usize * 4,
                SrcFormat::Rgba,
            );
            Some(result)
        }
    }

    pub fn copy_from(&mut self, src: &Surface, x: u32, y: u32) {
        if let Some(gpu) = &src.gpu {
            self.copy_gpu(&gpu.image, Rect::new(0, 0, src.width, src.height), x, y);
        } else {
            self.write(
                Rect::new(x, y, src.width, src.height),
                &src.data,
                src.stride(),
                SrcFormat::Rgba,
            );
        }
    }

    pub fn copy_gpu(&mut self, src: &GpuImage, rect: Rect, x: u32, y: u32) {
        let Some(gpu) = &self.gpu else { return };
        let mut encoder = gpu.context.encoder();
        GpuContext::copy(&mut encoder, src, rect, &gpu.image, x, y);
        gpu.record(gpu.context.submit(encoder, ()));
        if let Some(rect) = Rect::new(x, y, rect.w, rect.h).clip(self.width, self.height) {
            self.damage.add(rect);
        }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn fill(&mut self, rect: Rect, rgba: [u8; 4]) {
        let Some(rect) = rect.clip(self.width, self.height) else {
            return;
        };
        if let Some(gpu) = &self.gpu {
            gpu.record(gpu.context.fill(&gpu.image, rect, rgba));
            self.damage.add(rect);
            return;
        }
        let stride = self.stride();
        for y in rect.y..rect.y + rect.h {
            let start = y as usize * stride + rect.x as usize * 4;
            for px in self.data[start..start + rect.w as usize * 4].chunks_exact_mut(4) {
                px.copy_from_slice(&rgba);
            }
        }
        self.damage.add(rect);
    }

    /// Writes a `rect.w` x `rect.h` image (rows `src_stride` bytes apart) at `rect`.
    pub fn write(&mut self, rect: Rect, src: &[u8], src_stride: usize, format: SrcFormat) {
        let Some(clipped) = rect.clip(self.width, self.height) else {
            return;
        };
        if let Some(gpu) = &self.gpu {
            let mut normalized = Self::new(clipped.w, clipped.h);
            normalized.write(
                Rect::new(0, 0, clipped.w, clipped.h),
                src,
                src_stride,
                format,
            );
            gpu.record(
                gpu.context
                    .upload(&gpu.image, clipped, &normalized.data, clipped.w * 4),
            );
            self.damage.add(clipped);
            return;
        }
        let stride = self.stride();
        let bpp = format.bytes_per_pixel();
        let width = clipped.w as usize;

        for row in 0..clipped.h as usize {
            let src_start = row * src_stride;
            let Some(src_row) = src.get(src_start..src_start + width * bpp) else {
                break;
            };
            let dst_start = (clipped.y as usize + row) * stride + clipped.x as usize * 4;
            let dst_row = &mut self.data[dst_start..dst_start + width * 4];
            match format {
                SrcFormat::Rgba => dst_row.copy_from_slice(src_row),
                SrcFormat::Rgbx => {
                    dst_row.copy_from_slice(src_row);
                    for alpha in dst_row.iter_mut().skip(3).step_by(4) {
                        *alpha = 0xFF;
                    }
                }
                SrcFormat::Bgra => {
                    for (d, s) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
                        d.copy_from_slice(&[s[2], s[1], s[0], 0xFF]);
                    }
                }
                SrcFormat::Rgb => {
                    for (d, s) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(3)) {
                        d.copy_from_slice(&[s[0], s[1], s[2], 0xFF]);
                    }
                }
            }
        }
        self.damage.add(clipped);
    }

    /// Copies `rect` out as tightly packed RGBA.
    pub fn read(&self, rect: Rect) -> Option<(Rect, Vec<u8>)> {
        let rect = rect.clip(self.width, self.height)?;
        let stride = self.stride();
        let row_bytes = rect.w as usize * 4;
        let mut out = Vec::with_capacity(row_bytes * rect.h as usize);
        for y in rect.y..rect.y + rect.h {
            let start = y as usize * stride + rect.x as usize * 4;
            out.extend_from_slice(&self.data[start..start + row_bytes]);
        }
        Some((rect, out))
    }

    pub fn mark_all_damaged(&mut self) {
        self.damage.add(Rect::new(0, 0, self.width, self.height));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(s: &Surface, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * s.width as usize + x as usize) * 4;
        s.data[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn fill_clips_and_damages() {
        let mut s = Surface::new(8, 8);
        s.fill(Rect::new(6, 6, 10, 10), [1, 2, 3, 255]);
        assert_eq!(pixel(&s, 7, 7), [1, 2, 3, 255]);
        assert_eq!(pixel(&s, 5, 5), [0, 0, 0, 255]);
        assert_eq!(s.damage.take(), vec![Rect::new(6, 6, 2, 2)]);
    }

    #[test]
    fn write_converts_formats() {
        let mut s = Surface::new(4, 1);
        s.write(Rect::new(0, 0, 1, 1), &[10, 20, 30, 0], 4, SrcFormat::Bgra);
        s.write(Rect::new(1, 0, 1, 1), &[10, 20, 30], 3, SrcFormat::Rgb);
        s.write(Rect::new(2, 0, 1, 1), &[10, 20, 30, 0], 4, SrcFormat::Rgbx);
        assert_eq!(pixel(&s, 0, 0), [30, 20, 10, 255]);
        assert_eq!(pixel(&s, 1, 0), [10, 20, 30, 255]);
        assert_eq!(pixel(&s, 2, 0), [10, 20, 30, 255]);
    }

    #[test]
    fn read_round_trips() {
        let mut s = Surface::new(4, 4);
        s.fill(Rect::new(1, 1, 2, 2), [9, 9, 9, 255]);
        let (rect, data) = s.read(Rect::new(1, 1, 2, 2)).unwrap();
        let mut t = Surface::new(4, 4);
        t.write(
            Rect::new(0, 0, rect.w, rect.h),
            &data,
            rect.w as usize * 4,
            SrcFormat::Rgba,
        );
        assert_eq!(pixel(&t, 1, 1), [9, 9, 9, 255]);
        assert_eq!(pixel(&t, 2, 2), [0, 0, 0, 255]);
    }
}
