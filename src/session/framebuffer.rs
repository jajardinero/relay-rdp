//! The hand-off point between a session thread (which decodes) and the UI thread (which uploads
//! to the GPU).
//!
//! The session thread copies freshly decoded regions into [`FrameData`] and records them as dirty.
//! The UI thread, once per displayed frame, takes the accumulated dirty rectangles and uploads
//! just those regions. Any number of server updates between two vsyncs collapse into one upload,
//! so there is never a backlog of stale frames: the latest state always wins.

use crate::gpu::GpuImage;
use std::sync::Arc;

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, MutexGuard};

/// A pixel rectangle with exclusive right/bottom edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    fn right(self) -> u32 {
        self.x + self.w
    }

    fn bottom(self) -> u32 {
        self.y + self.h
    }

    fn area(self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    fn contains(self, other: Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    /// Clips to `width` x `height`; `None` if nothing remains.
    pub fn clip(self, width: u32, height: u32) -> Option<Rect> {
        let right = self.right().min(width);
        let bottom = self.bottom().min(height);
        (self.x < right && self.y < bottom)
            .then(|| Rect::new(self.x, self.y, right - self.x, bottom - self.y))
    }
}

/// Upper bound on tracked rectangles; beyond it they collapse into their bounding box. Many tiny
/// uploads cost more than one slightly larger one.
const MAX_DIRTY_RECTS: usize = 24;

#[derive(Debug, Default)]
pub struct DirtyRegion {
    rects: Vec<Rect>,
}

impl DirtyRegion {
    pub fn add(&mut self, rect: Rect) {
        if rect.w == 0 || rect.h == 0 || self.rects.iter().any(|r| r.contains(rect)) {
            return;
        }
        self.rects.retain(|r| !rect.contains(*r));

        // Merge with an overlapping/adjacent rectangle when the union wastes little area.
        if let Some(existing) = self.rects.iter_mut().find(|r| {
            let u = r.union(rect);
            u.area() <= (r.area() + rect.area()) * 5 / 4
        }) {
            *existing = existing.union(rect);
        } else {
            self.rects.push(rect);
        }

        if self.rects.len() > MAX_DIRTY_RECTS {
            let bounds = self
                .rects
                .iter()
                .copied()
                .reduce(Rect::union)
                .expect("non-empty");
            self.rects.clear();
            self.rects.push(bounds);
        }
    }

    pub fn take(&mut self) -> Vec<Rect> {
        std::mem::take(&mut self.rects)
    }
}

/// RGBA8 pixels of the remote desktop plus what changed since the last upload.
pub struct FrameData {
    pub gpu: Option<Arc<GpuImage>>,
    width: u32,
    height: u32,
    data: Vec<u8>,
    dirty: DirtyRegion,
}

impl FrameData {
    pub fn publish_gpu(&mut self, image: Arc<GpuImage>) {
        self.width = image.width;
        self.height = image.height;
        self.data.clear();
        self.dirty = DirtyRegion::default();
        self.gpu = Some(image);
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Resizes to `width` x `height`, clearing to opaque black and marking everything dirty.
    pub fn resize(&mut self, width: u32, height: u32) {
        if self.gpu.is_none() && self.width == width && self.height == height {
            return;
        }
        self.gpu = None;
        self.width = width;
        self.height = height;
        self.data.clear();
        self.data.resize(width as usize * height as usize * 4, 0);
        for alpha in self.data.iter_mut().skip(3).step_by(4) {
            *alpha = 0xFF;
        }
        self.dirty = DirtyRegion::default();
        self.dirty.add(Rect::new(0, 0, width, height));
    }

    /// Copies `rect` out of an RGBA image with the same dimensions, forcing alpha to opaque
    /// (decoders leave the alpha byte undefined).
    pub fn blit_rgba(&mut self, src: &[u8], src_stride: usize, rect: Rect) {
        let Some(rect) = rect.clip(self.width, self.height) else {
            return;
        };
        let stride = self.stride();
        let row_bytes = rect.w as usize * 4;
        for row in rect.y..rect.bottom() {
            let src_start = row as usize * src_stride + rect.x as usize * 4;
            let dst_start = row as usize * stride + rect.x as usize * 4;
            let (Some(src_row), Some(dst_row)) = (
                src.get(src_start..src_start + row_bytes),
                self.data.get_mut(dst_start..dst_start + row_bytes),
            ) else {
                break;
            };
            dst_row.copy_from_slice(src_row);
            for alpha in dst_row.iter_mut().skip(3).step_by(4) {
                *alpha = 0xFF;
            }
        }
        self.dirty.add(rect);
    }

    /// Copies a `w` x `h` RGBA block (rows `src_stride` bytes apart, alpha already opaque) to
    /// (`x`, `y`), clipped to the frame.
    pub fn blit_rgba_at(&mut self, src: &[u8], src_stride: usize, w: u32, h: u32, x: u32, y: u32) {
        let Some(clipped) = Rect::new(x, y, w, h).clip(self.width, self.height) else {
            return;
        };
        let stride = self.stride();
        let row_bytes = clipped.w as usize * 4;
        for row in 0..clipped.h as usize {
            let src_start = row * src_stride;
            let dst_start = (clipped.y as usize + row) * stride + clipped.x as usize * 4;
            let (Some(src_row), Some(dst_row)) = (
                src.get(src_start..src_start + row_bytes),
                self.data.get_mut(dst_start..dst_start + row_bytes),
            ) else {
                break;
            };
            dst_row.copy_from_slice(src_row);
        }
        self.dirty.add(clipped);
    }

    pub fn mark_all_dirty(&mut self) {
        self.dirty.add(Rect::new(0, 0, self.width, self.height));
    }

    pub fn take_dirty(&mut self) -> Vec<Rect> {
        self.dirty.take()
    }
}

pub struct SharedFrame {
    inner: Mutex<FrameData>,
    /// Set by the session when it has signalled the UI; cleared by the UI before it takes the
    /// dirty region. Keeps the event loop from being flooded with redundant wake-ups.
    wake_pending: AtomicBool,
}

impl SharedFrame {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FrameData {
                gpu: None,
                width: 0,
                height: 0,
                data: Vec::new(),
                dirty: DirtyRegion::default(),
            }),
            wake_pending: AtomicBool::new(false),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, FrameData> {
        self.inner.lock()
    }

    /// Returns true if the caller should wake the UI (no wake-up is already in flight).
    pub fn request_wake(&self) -> bool {
        !self.wake_pending.swap(true, Ordering::AcqRel)
    }

    pub fn clear_wake(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contained_rects_are_dropped() {
        let mut region = DirtyRegion::default();
        region.add(Rect::new(0, 0, 100, 100));
        region.add(Rect::new(10, 10, 5, 5));
        assert_eq!(region.take(), vec![Rect::new(0, 0, 100, 100)]);
    }

    #[test]
    fn adjacent_rects_merge() {
        let mut region = DirtyRegion::default();
        region.add(Rect::new(0, 0, 64, 64));
        region.add(Rect::new(64, 0, 64, 64));
        assert_eq!(region.take(), vec![Rect::new(0, 0, 128, 64)]);
    }

    #[test]
    fn distant_rects_stay_separate() {
        let mut region = DirtyRegion::default();
        region.add(Rect::new(0, 0, 10, 10));
        region.add(Rect::new(500, 500, 10, 10));
        assert_eq!(region.take().len(), 2);
    }

    #[test]
    fn many_rects_collapse_to_bounds() {
        let mut region = DirtyRegion::default();
        for i in 0..40 {
            region.add(Rect::new(i * 100, i * 100, 4, 4));
        }
        let rects = region.take();
        assert_eq!(rects.len(), 1);
        assert!(rects[0].contains(Rect::new(3900, 3900, 4, 4)));
        assert!(rects[0].contains(Rect::new(0, 0, 4, 4)));
    }

    #[test]
    fn blit_forces_opaque_alpha_and_clips() {
        let frame = SharedFrame::new();
        let mut f = frame.lock();
        f.resize(4, 2);
        f.take_dirty();

        let src = vec![7u8; 4 * 2 * 4];
        f.blit_rgba(&src, 16, Rect::new(2, 1, 10, 10));
        assert_eq!(f.take_dirty(), vec![Rect::new(2, 1, 2, 1)]);
        let px = &f.data()[(4 + 2) * 4..(4 + 3) * 4];
        assert_eq!(px, &[7, 7, 7, 0xFF]);
        assert_eq!(&f.data()[0..4], &[0, 0, 0, 0xFF]);
    }
}
