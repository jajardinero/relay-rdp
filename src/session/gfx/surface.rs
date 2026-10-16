//! Graphics-pipeline surfaces: off-screen RGBA buffers the server draws into, some of which are
//! mapped onto the output (the desktop we display).

use crate::gpu::{GpuContext, GpuImage};
use ironrdp_graphics::progressive::TileCoefficients;
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

    /// Converts one row of pixels to RGBA with the alpha the surface expects.
    fn convert_row(self, dst: &mut [u8], src: &[u8]) {
        match self {
            SrcFormat::Rgba => dst.copy_from_slice(src),
            SrcFormat::Rgbx => {
                dst.copy_from_slice(src);
                for alpha in dst.iter_mut().skip(3).step_by(4) {
                    *alpha = 0xFF;
                }
            }
            SrcFormat::Bgra => {
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&[s[2], s[1], s[0], 0xFF]);
                }
            }
            SrcFormat::Rgb => {
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                    d.copy_from_slice(&[s[0], s[1], s[2], 0xFF]);
                }
            }
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
        context.clear(&image, [0, 0, 0, 255]);
        Self::with_image(image, context, error)
    }

    /// A GPU surface showing `image`, whose pixels are undefined until written.
    fn with_image(
        image: Arc<GpuImage>,
        context: Arc<GpuContext>,
        error: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            width: image.width,
            height: image.height,
            gpu: Some(GpuSurface {
                context,
                image,
                error,
            }),
            data: Vec::new(),
            origin: None,
            damage: DirtyRegion::default(),
        }
    }

    /// Snapshot before any destination writes, including overlapping self-copies.
    pub fn snapshot(&self, rect: Rect) -> Option<Surface> {
        let rect = rect.clip(self.width, self.height)?;
        if let Some(gpu) = &self.gpu {
            // The copy overwrites every pixel, so the image needs no clear.
            let mut result = Self::with_image(
                gpu.context.image(rect.w, rect.h),
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
        gpu.context.copy(src, rect, &gpu.image, x, y);
        if let Some(rect) = Rect::new(x, y, rect.w, rect.h).clip(self.width, self.height) {
            self.damage.add(rect);
        }
    }

    /// Copies `rect` to each of `points`, all from the pixels as they were before the call.
    pub fn copy_within(&mut self, rect: Rect, points: &[(u32, u32)]) {
        let Some(gpu) = &self.gpu else {
            if let Some(snapshot) = self.snapshot(rect) {
                for &(x, y) in points {
                    self.copy_from(&snapshot, x, y);
                }
            }
            return;
        };
        let Some(rect) = rect.clip(self.width, self.height) else {
            return;
        };
        gpu.context.copy_within(&gpu.image, rect, points);
        for &(x, y) in points {
            if let Some(placed) = Rect::new(x, y, rect.w, rect.h).clip(self.width, self.height) {
                self.damage.add(placed);
            }
        }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    #[cfg(test)]
    pub fn fill(&mut self, rect: Rect, rgba: [u8; 4]) {
        self.fill_rects(&[rect], rgba);
    }

    pub fn fill_rects(&mut self, rects: &[Rect], rgba: [u8; 4]) {
        let rects: Vec<Rect> = rects
            .iter()
            .filter_map(|r| r.clip(self.width, self.height))
            .collect();
        if let Some(gpu) = &self.gpu {
            gpu.context.fill(&gpu.image, &rects, rgba);
        } else {
            let stride = self.stride();
            for rect in &rects {
                for y in rect.y..rect.y + rect.h {
                    let start = y as usize * stride + rect.x as usize * 4;
                    for px in self.data[start..start + rect.w as usize * 4].chunks_exact_mut(4) {
                        px.copy_from_slice(&rgba);
                    }
                }
            }
        }
        for rect in rects {
            self.damage.add(rect);
        }
    }

    /// Draws the `parts` of a RemoteFX progressive tile at `area` from its coefficients, which
    /// only GPU surfaces take.
    pub fn draw_tile(&mut self, area: Rect, tile: &TileCoefficients, parts: &[Rect]) {
        let Some(gpu) = &self.gpu else { return };
        gpu.record(gpu.context.draw_tile(
            &gpu.image,
            (area.x, area.y),
            &tile.components,
            tile.reduce_extrapolate,
            parts,
        ));
        for part in parts {
            if let Some(part) = part
                .intersect(area)
                .and_then(|r| r.clip(self.width, self.height))
            {
                self.damage.add(part);
            }
        }
    }

    /// Writes a `rect.w` x `rect.h` image (rows `src_stride` bytes apart) at `rect`. Only
    /// complete rows of `src` are written.
    pub fn write(&mut self, rect: Rect, src: &[u8], src_stride: usize, format: SrcFormat) {
        let Some(clipped) = rect.clip(self.width, self.height) else {
            return;
        };
        let row_bytes = clipped.w as usize * format.bytes_per_pixel();
        let rows = (0..clipped.h as usize)
            .take_while(|row| row * src_stride + row_bytes <= src.len())
            .count();
        if rows == 0 {
            return;
        }
        let written = Rect::new(clipped.x, clipped.y, clipped.w, rows as u32);
        let source = |row: usize| &src[row * src_stride..][..row_bytes];
        if let Some(gpu) = &self.gpu {
            // Converted straight into the GPU's staging memory.
            gpu.record(gpu.context.upload(&gpu.image, written, |row, pixels| {
                format.convert_row(pixels, source(row))
            }));
        } else {
            let stride = self.stride();
            for row in 0..rows {
                let start = (written.y as usize + row) * stride + written.x as usize * 4;
                format.convert_row(
                    &mut self.data[start..][..written.w as usize * 4],
                    source(row),
                );
            }
        }
        self.damage.add(written);
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
    fn write_skips_incomplete_source_rows() {
        let mut s = Surface::new(4, 4);
        // Two complete 2-pixel rows and part of a third.
        s.write(Rect::new(1, 1, 2, 3), &[7; 20], 8, SrcFormat::Rgba);
        assert_eq!(pixel(&s, 2, 2), [7, 7, 7, 7]);
        assert_eq!(pixel(&s, 1, 3), [0, 0, 0, 255]);
        assert_eq!(s.damage.take(), vec![Rect::new(1, 1, 2, 2)]);
        s.write(Rect::new(0, 0, 2, 2), &[7; 7], 8, SrcFormat::Rgba);
        assert!(s.damage.take().is_empty());
    }

    #[test]
    fn copy_within_reads_pixels_before_the_copy() {
        let mut s = Surface::new(6, 1);
        for x in 0..3 {
            s.fill(Rect::new(x, 0, 1, 1), [x as u8 + 1, 0, 0, 255]);
        }
        s.damage.take();
        // The second copy overlaps the first one's destination, but still copies 1, 2, 3.
        s.copy_within(Rect::new(0, 0, 3, 1), &[(1, 0), (3, 0)]);
        let reds: Vec<u8> = (0..6).map(|x| pixel(&s, x, 0)[0]).collect();
        assert_eq!(reds, [1, 1, 2, 1, 2, 3]);
    }

    #[test]
    fn fill_rects_clips_each_rect() {
        let mut s = Surface::new(4, 4);
        s.fill_rects(
            &[
                Rect::new(0, 0, 1, 1),
                Rect::new(3, 3, 5, 5),
                Rect::new(9, 9, 1, 1),
            ],
            [5, 6, 7, 255],
        );
        assert_eq!(pixel(&s, 0, 0), [5, 6, 7, 255]);
        assert_eq!(pixel(&s, 3, 3), [5, 6, 7, 255]);
        assert_eq!(pixel(&s, 1, 1), [0, 0, 0, 255]);
        assert_eq!(
            s.damage.take(),
            vec![Rect::new(0, 0, 1, 1), Rect::new(3, 3, 1, 1)]
        );
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
