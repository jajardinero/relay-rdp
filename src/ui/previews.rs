//! Snapshots of hosts' screens, shown on their tiles in the host library.
//!
//! A live session is captured 10 seconds after its picture appears, then every two minutes,
//! and once more when the user closes it. The frame is copied on the UI thread (a readback
//! for GPU frames); shrinking and saving it as a PNG happen on a worker thread.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use egui_wgpu::wgpu;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::gpu::GpuContext;
use crate::session::SharedFrame;
use crate::store::paths;

/// The first snapshot waits for the desktop to settle.
const FIRST_CAPTURE: Duration = Duration::from_secs(10);
const CAPTURE_EVERY: Duration = Duration::from_secs(120);
/// Snapshots are shrunk to at most this width.
const PREVIEW_WIDTH: u32 = 400;

/// Wakes the UI when a snapshot is saved.
type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct Previews {
    dir: Option<PathBuf>,
    wake: Wake,
    last_capture: HashMap<Uuid, Instant>,
    /// Hosts with a snapshot being saved, and hosts whose saved snapshot changed since loaded.
    in_flight: Arc<Mutex<HashSet<Uuid>>>,
    updated: Arc<Mutex<HashSet<Uuid>>>,
    /// Loaded snapshots; `None` when a host has none.
    textures: HashMap<Uuid, Option<egui::TextureHandle>>,
    /// Bumped by deletions, so a snapshot taken before one isn't saved after it.
    epoch: Arc<AtomicU64>,
}

/// A frame's pixels, on their way to a snapshot.
enum Pixels {
    Cpu(Vec<u8>),
    Gpu(Arc<GpuContext>, wgpu::Buffer),
}

struct Frame {
    pixels: Pixels,
    width: u32,
    height: u32,
    stride: usize,
}

impl Previews {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::in_dir(paths::previews_dir(), Arc::new(wake))
    }

    fn in_dir(dir: Option<PathBuf>, wake: Wake) -> Self {
        Self {
            dir,
            wake,
            last_capture: HashMap::new(),
            in_flight: Arc::default(),
            updated: Arc::default(),
            textures: HashMap::new(),
            epoch: Arc::default(),
        }
    }

    fn path(dir: &Path, id: Uuid) -> PathBuf {
        dir.join(format!("{id}.png"))
    }

    /// Whether a session live since `since` is due for a snapshot.
    fn due(&self, id: Uuid, since: Instant, now: Instant) -> bool {
        if self.in_flight.lock().contains(&id) {
            return false;
        }
        match self.last_capture.get(&id) {
            // A capture before this live period started doesn't count.
            Some(&at) if at >= since => now.duration_since(at) >= CAPTURE_EVERY,
            _ => now.duration_since(since) >= FIRST_CAPTURE,
        }
    }

    /// Captures a live session when its next snapshot is due.
    pub fn capture_if_due(
        &mut self,
        id: Uuid,
        since: Instant,
        frame: &SharedFrame,
        gpu: Option<&Arc<GpuContext>>,
    ) {
        if self.due(id, since, Instant::now()) {
            self.capture(id, frame, gpu);
        }
    }

    /// Captures the session's current picture and saves it in the background.
    pub fn capture(&mut self, id: Uuid, frame: &SharedFrame, gpu: Option<&Arc<GpuContext>>) {
        let Some(dir) = self.dir.clone() else { return };
        if !self.in_flight.lock().insert(id) {
            return;
        }
        self.last_capture.insert(id, Instant::now());
        let Some(frame) = grab(frame, gpu) else {
            self.in_flight.lock().remove(&id);
            return;
        };
        let (in_flight, updated) = (Arc::clone(&self.in_flight), Arc::clone(&self.updated));
        let (epoch, started) = (Arc::clone(&self.epoch), self.epoch.load(Ordering::Acquire));
        let wake = Arc::clone(&self.wake);
        let spawned = std::thread::Builder::new()
            .name("relay-preview".into())
            .spawn(move || {
                let result = save(&dir, id, frame, || epoch.load(Ordering::Acquire) == started);
                in_flight.lock().remove(&id);
                match result {
                    Ok(true) => {
                        updated.lock().insert(id);
                        wake();
                    }
                    Ok(false) => {}
                    Err(error) => tracing::debug!(%error, "couldn't save a desktop preview"),
                }
            });
        if spawned.is_err() {
            self.in_flight.lock().remove(&id);
        }
    }

    /// The host's snapshot, loaded once and again after a new one is saved.
    pub fn texture(&mut self, ctx: &egui::Context, id: Uuid) -> Option<egui::TextureHandle> {
        let stale = self.updated.lock().remove(&id);
        if stale || !self.textures.contains_key(&id) {
            let loaded = self
                .dir
                .as_deref()
                .and_then(|dir| load(ctx, &Self::path(dir, id)));
            self.textures.insert(id, loaded);
        }
        self.textures.get(&id).cloned().flatten()
    }

    /// Deletes a host's snapshot.
    pub fn remove(&mut self, id: Uuid) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.textures.remove(&id);
        self.last_capture.remove(&id);
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_file(Self::path(dir, id));
        }
    }

    /// Deletes every snapshot (previews were turned off).
    pub fn clear(&mut self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.textures.clear();
        self.last_capture.clear();
        if let Some(dir) = &self.dir
            && let Err(error) = std::fs::remove_dir_all(dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "couldn't delete desktop previews");
        }
    }
}

/// Copies the frame's picture: its pixels, or a GPU readback the worker maps.
fn grab(frame: &SharedFrame, gpu: Option<&Arc<GpuContext>>) -> Option<Frame> {
    let data = frame.lock();
    let (width, height) = (data.width(), data.height());
    if width == 0 || height == 0 {
        return None;
    }
    if let Some(image) = &data.gpu {
        let gpu = gpu?;
        let (buffer, stride) = gpu.read_image(image);
        return Some(Frame {
            pixels: Pixels::Gpu(Arc::clone(gpu), buffer),
            width,
            height,
            stride: stride as usize,
        });
    }
    (!data.data().is_empty()).then(|| Frame {
        pixels: Pixels::Cpu(data.data().to_vec()),
        width,
        height,
        stride: data.stride(),
    })
}

/// Shrinks and writes a snapshot, unless `current()` says it was deleted meanwhile.
fn save(dir: &Path, id: Uuid, frame: Frame, current: impl Fn() -> bool) -> anyhow::Result<bool> {
    let Frame {
        pixels,
        width,
        height,
        stride,
    } = frame;
    let rgba = match pixels {
        Pixels::Cpu(data) => data,
        Pixels::Gpu(gpu, buffer) => {
            let slice = buffer.slice(..);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            gpu.device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(5)),
            })?;
            rx.recv()??;
            slice.get_mapped_range()?.to_vec()
        }
    };
    let (small, w, h) = shrink(&rgba, stride, width, height, PREVIEW_WIDTH);
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!("{id}.png.tmp"));
    let (rgba, png) = (image::ExtendedColorType::Rgba8, image::ImageFormat::Png);
    image::save_buffer_with_format(&temp, &small, w, h, rgba, png)?;
    if !current() {
        let _ = std::fs::remove_file(&temp);
        return Ok(false);
    }
    std::fs::rename(&temp, Previews::path(dir, id))?;
    Ok(true)
}

/// Shrinks an RGBA image (rows `stride` bytes apart) to at most `max_width`, keeping its
/// aspect ratio. Each pixel averages the block it covers, so text doesn't shimmer.
fn shrink(
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    max_width: u32,
) -> (Vec<u8>, u32, u32) {
    let w = width.min(max_width).max(1);
    let h = ((u64::from(height) * u64::from(w)).div_ceil(u64::from(width)) as u32).max(1);
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        let (y0, y1) = span(y, h, height);
        for x in 0..w {
            let (x0, x1) = span(x, w, width);
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                let row = &src[sy as usize * stride..];
                for sx in x0..x1 {
                    let px = &row[sx as usize * 4..sx as usize * 4 + 3];
                    for (s, &c) in sum.iter_mut().zip(px) {
                        *s += u32::from(c);
                    }
                }
            }
            let count = (y1 - y0) * (x1 - x0);
            out.extend(sum.map(|s| ((s + count / 2) / count) as u8));
            out.push(255);
        }
    }
    (out, w, h)
}

/// The source pixels destination pixel `i` of `n` covers out of `total`: at least one.
fn span(i: u32, n: u32, total: u32) -> (u32, u32) {
    let start = (u64::from(i) * u64::from(total) / u64::from(n)) as u32;
    let end = ((u64::from(i + 1) * u64::from(total) / u64::from(n)) as u32).max(start + 1);
    (start, end.min(total))
}

fn load(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let image = image::open(path).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    let name = format!("preview-{}", path.display());
    Some(ctx.load_texture(name, pixels, egui::TextureOptions::LINEAR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinking_keeps_the_aspect_ratio_and_averages_blocks() {
        // A 4x2 checker of black and white shrinks to 2x1 gray.
        let mut checker = Vec::new();
        for y in 0..2u32 {
            for x in 0..4u32 {
                let v = if (x + y) % 2 == 0 { 0 } else { 255 };
                checker.extend([v, v, v, 255]);
            }
        }
        let (small, w, h) = shrink(&checker, 16, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(small, [128, 128, 128, 255, 128, 128, 128, 255]);

        let solid: Vec<u8> = [10, 20, 30, 255].repeat(1920 * 1080);
        let (small, w, h) = shrink(&solid, 1920 * 4, 1920, 1080, PREVIEW_WIDTH);
        assert_eq!((w, h), (400, 225));
        assert!(small.chunks(4).all(|px| px == [10, 20, 30, 255]));

        // Small pictures keep their size.
        let (_, w, h) = shrink(&solid, 1920 * 4, 300, 200, PREVIEW_WIDTH);
        assert_eq!((w, h), (300, 200));
    }

    /// A GPU frame's snapshot is its picture, read back and shrunk.
    #[test]
    #[ignore = "requires a Vulkan GPU and access to /dev/dri"]
    fn gpu_frames_are_captured() {
        use crate::session::framebuffer::Rect;
        let gpu = crate::gpu::tests::context().unwrap();
        let image = gpu.image(800, 450);
        gpu.fill(&image, &[Rect::new(0, 0, 800, 450)], [200, 30, 40, 255]);
        gpu.fill(&image, &[Rect::new(400, 0, 400, 450)], [10, 220, 90, 255]);
        gpu.flush().unwrap();
        let frame = SharedFrame::new();
        frame.lock().publish_gpu(image);

        let dir = std::env::temp_dir().join(format!("relay-previews-{}", Uuid::new_v4()));
        let mut previews = previews(Some(dir.clone()));
        let id = Uuid::new_v4();
        previews.capture(id, &frame, Some(&gpu));
        let deadline = Instant::now() + Duration::from_secs(10);
        while previews.in_flight.lock().contains(&id) {
            assert!(Instant::now() < deadline, "the snapshot was saved");
            std::thread::sleep(Duration::from_millis(10));
        }
        let saved = image::open(Previews::path(&dir, id)).unwrap().to_rgba8();
        assert_eq!(saved.dimensions(), (400, 225));
        assert_eq!(saved.get_pixel(100, 100).0, [200, 30, 40, 255]);
        assert_eq!(saved.get_pixel(300, 100).0, [10, 220, 90, 255]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn previews(dir: Option<PathBuf>) -> Previews {
        Previews::in_dir(dir, Arc::new(|| {}))
    }

    #[test]
    fn snapshots_are_due_after_ten_seconds_then_every_two_minutes() {
        let mut previews = previews(None);
        let id = Uuid::new_v4();
        let since = Instant::now();
        assert!(!previews.due(id, since, since + Duration::from_secs(9)));
        assert!(previews.due(id, since, since + FIRST_CAPTURE));
        previews.last_capture.insert(id, since + FIRST_CAPTURE);
        assert!(!previews.due(id, since, since + Duration::from_secs(100)));
        assert!(previews.due(id, since, since + FIRST_CAPTURE + CAPTURE_EVERY));
        // Live again later (a reconnect): the first snapshot waits again.
        let again = since + Duration::from_secs(300);
        assert!(!previews.due(id, again, again + Duration::from_secs(5)));
    }

    #[test]
    fn saved_snapshots_can_be_deleted() {
        let dir = std::env::temp_dir().join(format!("relay-previews-{}", Uuid::new_v4()));
        let id = Uuid::new_v4();
        let frame = Frame {
            pixels: Pixels::Cpu([1, 2, 3, 255].repeat(64 * 32)),
            width: 64,
            height: 32,
            stride: 64 * 4,
        };
        assert!(save(&dir, id, frame, || true).unwrap());
        let path = Previews::path(&dir, id);
        let saved = image::open(&path).unwrap().to_rgba8();
        assert_eq!(saved.dimensions(), (64, 32));
        assert_eq!(saved.get_pixel(5, 5).0, [1, 2, 3, 255]);

        // A snapshot finished after a deletion is dropped.
        let late = Frame {
            pixels: Pixels::Cpu(vec![0; 16]),
            width: 2,
            height: 2,
            stride: 8,
        };
        assert!(!save(&dir, Uuid::new_v4(), late, || false).unwrap());

        let mut previews = previews(Some(dir.clone()));
        previews.remove(id);
        assert!(!path.exists());
        let other = Frame {
            pixels: Pixels::Cpu([9; 4].repeat(4)),
            width: 2,
            height: 2,
            stride: 8,
        };
        assert!(save(&dir, Uuid::new_v4(), other, || true).unwrap());
        previews.clear();
        assert!(!dir.exists());
    }
}
