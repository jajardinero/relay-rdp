//! AVC444 (H.264 4:4:4) reconstruction on the CPU (MS-RDPEGFX 3.3.8.3).
//!
//! The server splits a 4:4:4 picture into two 4:2:0 H.264 pictures: a *main* view (full luma
//! plus 2×2-averaged chroma) and an *auxiliary* view that carries the remaining chroma samples.
//! Each surface keeps one full-resolution YUV444 picture that the views update in place:
//!
//! - a luma (main) update rewrites Y and spreads its averaged chroma over each 2×2 block;
//! - a chroma (auxiliary) update rewrites every chroma sample except the top-left of each
//!   2×2 block, which stays as the main view's average.
//!
//! When converting to RGB, the top-left sample is recovered from the block average
//! (`4·avg − the other three`) unless that differs from the average by less than 30, which keeps
//! smooth areas noise-free. The layouts and filter follow FreeRDP's decoder, which is proven
//! against Windows servers.

use crate::session::framebuffer::Rect;

/// One decoded 4:2:0 picture (planes as the decoder hands them out).
pub struct Planes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

impl Planes<'_> {
    fn luma(&self, x: usize, y: usize) -> Option<u8> {
        self.y.get(y * self.y_stride + x).copied()
    }

    fn chroma(&self, plane: &[u8], x: usize, y: usize) -> Option<u8> {
        plane.get(y * self.uv_stride + x).copied()
    }
}

/// How the auxiliary view packs its chroma samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// AVC444: odd chroma rows in 8-row U/V bands of the auxiliary luma.
    V1,
    /// AVC444v2: odd chroma columns split into left (U) and right (V) halves.
    V2,
}

/// A persistent full-resolution YUV444 picture for one surface.
pub struct Yuv444 {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Yuv444 {
    pub fn new(width: usize, height: usize) -> Self {
        let n = width * height;
        Self {
            width,
            height,
            y: vec![0; n],
            u: vec![128; n],
            v: vec![128; n],
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Clips `rect` to the picture, as pixel ranges.
    fn bounds(&self, rect: Rect) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
        let r = rect.clip(self.width as u32, self.height as u32)?;
        Some((
            r.x as usize..(r.x + r.w) as usize,
            r.y as usize..(r.y + r.h) as usize,
        ))
    }

    /// Main view: full luma, and its averaged chroma spread over each 2×2 block.
    pub fn apply_luma(&mut self, main: &Planes<'_>, rect: Rect) {
        let Some((xs, ys)) = self.bounds(rect) else {
            return;
        };
        for y in ys {
            let row = y * self.width;
            for x in xs.clone() {
                let i = row + x;
                if let Some(l) = main.luma(x, y) {
                    self.y[i] = l;
                }
                if let (Some(u), Some(v)) = (
                    main.chroma(main.u, x / 2, y / 2),
                    main.chroma(main.v, x / 2, y / 2),
                ) {
                    self.u[i] = u;
                    self.v[i] = v;
                }
            }
        }
    }

    /// Auxiliary view: every chroma sample except the top-left of each 2×2 block.
    pub fn apply_chroma(&mut self, aux: &Planes<'_>, rect: Rect, layout: Layout) {
        let Some((xs, ys)) = self.bounds(rect) else {
            return;
        };
        let half = self.width / 2;
        let quarter = self.width / 4;
        for y in ys {
            let row = y * self.width;
            for x in xs.clone() {
                if x % 2 == 0 && y % 2 == 0 {
                    continue;
                }
                let sample = match layout {
                    Layout::V1 => v1_sample(aux, x, y),
                    Layout::V2 => v2_sample(aux, x, y, half, quarter),
                };
                if let Some((u, v)) = sample {
                    self.u[row + x] = u;
                    self.v[row + x] = v;
                }
            }
        }
    }

    /// Converts `rect` to tightly packed RGBA (full-range BT.709, the AVC420 default).
    pub fn to_rgba(&self, rect: Rect, out: &mut Vec<u8>) -> Option<Rect> {
        let (xs, ys) = self.bounds(rect)?;
        out.clear();
        out.reserve(xs.len() * ys.len() * 4);
        for y in ys.clone() {
            for x in xs.clone() {
                let i = y * self.width + x;
                let (mut u, mut v) = (self.u[i], self.v[i]);
                if x % 2 == 0 && y % 2 == 0 && x + 1 < self.width && y + 1 < self.height {
                    u = recover(&self.u, i, self.width);
                    v = recover(&self.v, i, self.width);
                }
                out.extend_from_slice(&yuv_to_rgba(self.y[i], u, v));
            }
        }
        Some(Rect::new(
            xs.start as u32,
            ys.start as u32,
            xs.len() as u32,
            ys.len() as u32,
        ))
    }
}

/// v1: odd rows come from the auxiliary luma (rows 0–7 of each 16 hold U, 8–15 hold V);
/// odd columns of even rows come from the auxiliary chroma planes.
fn v1_sample(aux: &Planes<'_>, x: usize, y: usize) -> Option<(u8, u8)> {
    if y % 2 == 1 {
        let k = (y - 1) / 2;
        let u_row = 16 * (k / 8) + k % 8;
        Some((aux.luma(x, u_row)?, aux.luma(x, u_row + 8)?))
    } else {
        Some((
            aux.chroma(aux.u, x / 2, y / 2)?,
            aux.chroma(aux.v, x / 2, y / 2)?,
        ))
    }
}

/// v2: odd columns come from the auxiliary luma (U left half, V right half); even columns
/// of odd rows come from the auxiliary chroma planes (U for x%4==0, V for x%4==2, each split
/// into U and V quarters).
fn v2_sample(
    aux: &Planes<'_>,
    x: usize,
    y: usize,
    half: usize,
    quarter: usize,
) -> Option<(u8, u8)> {
    if x % 2 == 1 {
        Some((aux.luma(x / 2, y)?, aux.luma(half + x / 2, y)?))
    } else {
        let plane = if x.is_multiple_of(4) { aux.u } else { aux.v };
        let row = y / 2;
        Some((
            aux.chroma(plane, x / 4, row)?,
            aux.chroma(plane, quarter + x / 4, row)?,
        ))
    }
}

/// Undo the 2×2 averaging for the top-left sample, unless the change is small.
fn recover(plane: &[u8], i: usize, width: usize) -> u8 {
    let avg = i32::from(plane[i]);
    let others =
        i32::from(plane[i + 1]) + i32::from(plane[i + width]) + i32::from(plane[i + width + 1]);
    let recovered = (4 * avg - others).clamp(0, 255) as u8;
    if recovered.abs_diff(plane[i]) < 30 {
        plane[i]
    } else {
        recovered
    }
}

/// Full-range BT.709 in the same fixed point as FreeRDP (and within ±1 of the GPU path).
fn yuv_to_rgba(y: u8, u: u8, v: u8) -> [u8; 4] {
    let y = 256 * i32::from(y);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let clip = |x: i32| (x >> 8).clamp(0, 255) as u8;
    [
        clip(y + 403 * e),
        clip(y - 48 * d - 120 * e),
        clip(y + 475 * d),
        0xFF,
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 4:4:4 picture split into main and auxiliary 4:2:0 views, the way the server does it.
    pub(crate) struct Split {
        pub main: [Vec<u8>; 3],
        pub aux: [Vec<u8>; 3],
        pub y_stride: usize,
        pub uv_stride: usize,
    }

    impl Split {
        pub(crate) fn main(&self) -> Planes<'_> {
            Planes {
                y: &self.main[0],
                u: &self.main[1],
                v: &self.main[2],
                y_stride: self.y_stride,
                uv_stride: self.uv_stride,
            }
        }

        pub(crate) fn aux(&self) -> Planes<'_> {
            Planes {
                y: &self.aux[0],
                u: &self.aux[1],
                v: &self.aux[2],
                y_stride: self.y_stride,
                uv_stride: self.uv_stride,
            }
        }
    }

    /// The server side: main chroma is the 2×2 average; the auxiliary view packs the rest
    /// (the inverse of `v1_sample` / `v2_sample`). Planes are padded to 16 like H.264 pictures.
    pub(crate) fn split(w: usize, h: usize, yuv: &[Vec<u8>; 3], layout: Layout) -> Split {
        let pw = w.div_ceil(16) * 16;
        let ph = h.div_ceil(16) * 16 + 16;
        let (y_stride, uv_stride) = (pw, pw / 2);
        let mut main = [
            vec![0u8; pw * ph],
            vec![128u8; pw * ph / 4],
            vec![128u8; pw * ph / 4],
        ];
        let mut aux = [
            vec![0u8; pw * ph],
            vec![128u8; pw * ph / 4],
            vec![128u8; pw * ph / 4],
        ];
        let at = |plane: &Vec<u8>, x: usize, y: usize| i32::from(plane[y * w + x]);
        for y in 0..h {
            main[0][y * y_stride..y * y_stride + w].copy_from_slice(&yuv[0][y * w..(y + 1) * w]);
        }
        for by in 0..h.div_ceil(2) {
            for bx in 0..w.div_ceil(2) {
                for c in 1..3 {
                    let mut sum = 0;
                    let mut n = 0;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let (x, y) = (2 * bx + dx, 2 * by + dy);
                        if x < w && y < h {
                            sum += at(&yuv[c], x, y);
                            n += 1;
                        }
                    }
                    main[c][by * uv_stride + bx] = (sum / n) as u8;
                }
            }
        }
        let (half, quarter) = (w / 2, w / 4);
        for y in 0..h {
            for x in 0..w {
                if x % 2 == 0 && y % 2 == 0 {
                    continue;
                }
                let (u, v) = (yuv[1][y * w + x], yuv[2][y * w + x]);
                match layout {
                    Layout::V1 => {
                        if y % 2 == 1 {
                            let k = (y - 1) / 2;
                            let row = 16 * (k / 8) + k % 8;
                            aux[0][row * y_stride + x] = u;
                            aux[0][(row + 8) * y_stride + x] = v;
                        } else {
                            aux[1][(y / 2) * uv_stride + x / 2] = u;
                            aux[2][(y / 2) * uv_stride + x / 2] = v;
                        }
                    }
                    Layout::V2 => {
                        if x % 2 == 1 {
                            aux[0][y * y_stride + x / 2] = u;
                            aux[0][y * y_stride + half + x / 2] = v;
                        } else {
                            let plane = if x % 4 == 0 { 1 } else { 2 };
                            aux[plane][(y / 2) * uv_stride + x / 4] = u;
                            aux[plane][(y / 2) * uv_stride + quarter + x / 4] = v;
                        }
                    }
                }
            }
        }
        Split {
            main,
            aux,
            y_stride,
            uv_stride,
        }
    }

    /// A deterministic picture with detail in every channel.
    pub(crate) fn picture(w: usize, h: usize) -> [Vec<u8>; 3] {
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 24) as u8
        };
        let n = w * h;
        [
            (0..n).map(|_| next()).collect(),
            (0..n).map(|_| next()).collect(),
            (0..n).map(|_| next()).collect(),
        ]
    }

    fn reconstruct(w: usize, h: usize, layout: Layout) -> ([Vec<u8>; 3], Yuv444) {
        let source = picture(w, h);
        let split = split(w, h, &source, layout);
        let mut store = Yuv444::new(w, h);
        let all = Rect::new(0, 0, w as u32, h as u32);
        store.apply_luma(&split.main(), all);
        store.apply_chroma(&split.aux(), all, layout);
        (source, store)
    }

    fn check_exact_except_top_left(w: usize, h: usize, layout: Layout) {
        let (source, store) = reconstruct(w, h, layout);
        assert_eq!(store.y, source[0], "luma");
        for y in 0..h {
            for x in 0..w {
                if x % 2 == 0 && y % 2 == 0 {
                    continue;
                }
                let i = y * w + x;
                assert_eq!(
                    (store.u[i], store.v[i]),
                    (source[1][i], source[2][i]),
                    "{layout:?} chroma at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn v1_round_trips() {
        check_exact_except_top_left(64, 48, Layout::V1);
        check_exact_except_top_left(70, 37, Layout::V1);
    }

    #[test]
    fn v2_round_trips() {
        check_exact_except_top_left(64, 48, Layout::V2);
        check_exact_except_top_left(72, 38, Layout::V2);
    }

    #[test]
    fn top_left_sample_is_recovered_from_the_average() {
        // A sharp chroma edge inside one 2×2 block: the average alone would smear it.
        let (w, h) = (4, 4);
        let mut source = [vec![100u8; w * h], vec![128u8; w * h], vec![128u8; w * h]];
        source[1][0] = 240;
        let split = split(w, h, &source, Layout::V1);
        let mut store = Yuv444::new(w, h);
        let all = Rect::new(0, 0, w as u32, h as u32);
        store.apply_luma(&split.main(), all);
        store.apply_chroma(&split.aux(), all, Layout::V1);
        assert_eq!(recover(&store.u, 0, w), 240);
        // Small differences keep the (noise-free) average.
        let mut smooth = vec![128u8; 16];
        smooth[0] = 131;
        assert_eq!(recover(&smooth, 0, 4), 131);
    }

    #[test]
    fn luma_update_replaces_refined_chroma_in_its_region_only() {
        let (w, h) = (32, 32);
        let (_, mut store) = reconstruct(w, h, Layout::V1);
        let before = store.u.clone();
        let flat = [
            vec![50u8; 64 * 64],
            vec![90u8; 32 * 32],
            vec![160u8; 32 * 32],
        ];
        let planes = Planes {
            y: &flat[0],
            u: &flat[1],
            v: &flat[2],
            y_stride: 64,
            uv_stride: 32,
        };
        store.apply_luma(&planes, Rect::new(0, 0, 16, 16));
        assert!(store.u[..16].iter().all(|&u| u == 90), "updated region");
        assert_eq!(store.u[20], before[20], "outside the region");
        assert_eq!(store.y[0], 50);
    }

    #[test]
    fn rgba_conversion_matches_bt709_full_range() {
        assert_eq!(yuv_to_rgba(128, 128, 128), [128, 128, 128, 255]);
        assert_eq!(yuv_to_rgba(0, 128, 128), [0, 0, 0, 255]);
        let red = yuv_to_rgba(54, 99, 255);
        assert!(red[0] > 240 && red[1] < 20 && red[2] < 20, "{red:?}");
        let mut out = Vec::new();
        let store = Yuv444::new(3, 2);
        let rect = store.to_rgba(Rect::new(1, 0, 10, 10), &mut out).unwrap();
        assert_eq!(rect, Rect::new(1, 0, 2, 2));
        assert_eq!(out.len(), 2 * 2 * 4);
    }
}
