// RemoteFX progressive tiles: the inverse DWT of one component of one 64x64 tile per
// workgroup. The arithmetic matches ironrdp-graphics' dwt.rs (standard) and
// dwt_extrapolate.rs (reduce-extrapolate) bit for bit, including their 16-bit truncation, so
// GPU tiles equal CPU ones. Their lifting loops run along each row and column, but every even
// sample depends only on the input and every odd one only on its two even neighbours, so here
// each pair of samples (an even one and the odd one after it) is computed on its own, all in
// parallel. That takes 32 KiB of workgroup memory (PROGRESSIVE_WORKGROUP_BYTES in mod.rs).
//
// data is the quad pass's buffer (see quads.wgsl). data[3] is the tile table: the number of
// standard tiles, then each tile's first word among the pixels, standard tiles first. A tile
// is Y, Cb and Cr, 2048 words each: 4096 coefficients as packed i16 pairs, which become
// the component's 64x64 samples, clamped for the color conversion in quads.wgsl.
override REDUCE_EXTRAPOLATE: bool;

const THREADS: u32 = 256u;

@group(0) @binding(0) var<storage, read_write> data: array<u32>;

// The coefficients, becoming samples level by level; from ROWS, one level's rows between
// its horizontal and vertical passes.
var<workgroup> m: array<i32, 8192>;
const ROWS: u32 = 4096u;

// Truncates to i16, as the CPU stores every step.
fn t(v: i32) -> i32 {
    return (v << 16u) >> 16u;
}

// A 1D inverse: n_l low then n_h high samples of `m`, `s` apart from lb and hb.
struct Line {
    lb: u32,
    hb: u32,
    s: u32,
    n_l: u32,
    n_h: u32,
}

fn low(line: Line, j: u32) -> i32 {
    return m[line.lb + j * line.s];
}

fn high(line: Line, j: u32) -> i32 {
    return m[line.hb + j * line.s];
}

// Output sample 2j of the reduce-extrapolate inverse. At j = 0 the high neighbours are both
// the first, (h + h) / 2 = h; at j = n_h, the sample extrapolated when n_l > n_h, both the
// last: h itself when n_l = n_h + 1, else h / 2.
fn extrapolate_even(line: Line, j: u32) -> i32 {
    let before = high(line, max(j, 1u) - 1u);
    let after = high(line, min(j, line.n_h - 1u));
    let half = j == line.n_h && line.n_l == line.n_h + 2u;
    return t(low(line, j) - select((before + after) / 2, after / 2, half));
}

// Output samples 2j and 2j + 1 of the reduce-extrapolate inverse (the second is unused past
// the end of the line).
fn extrapolate_pair(line: Line, j: u32) -> vec2<i32> {
    let even = extrapolate_even(line, j);
    let h = 2 * high(line, min(j, line.n_h - 1u));
    if j + 1u < line.n_h || (j + 1u == line.n_h && line.n_l > line.n_h) {
        return vec2(even, t((even + extrapolate_even(line, j + 1u)) / 2 + h));
    }
    if j + 1u == line.n_h {
        return vec2(even, t(even + h));
    }
    // The last sample when n_l = n_h + 2.
    return vec2(even, t((even + low(line, min(j + 1u, line.n_l - 1u))) / 2));
}

// Output sample 2j of the standard inverse (n_l = n_h). At j = 0 both high neighbours are
// the first: (h + h + 1) >> 1 = h.
fn standard_even(line: Line, j: u32) -> i32 {
    let before = high(line, max(j, 1u) - 1u);
    return t(low(line, j) - ((before + high(line, j) + 1) >> 1u));
}

// Output samples 2j and 2j + 1 of the standard inverse.
fn standard_pair(line: Line, j: u32) -> vec2<i32> {
    let even = standard_even(line, j);
    let h = t(high(line, j) << 1u);
    let following = standard_even(line, min(j + 1u, line.n_h - 1u));
    let next = select(even, (even + following) >> 1u, j + 1u < line.n_h);
    return vec2(even, t(h + next));
}

fn pair(line: Line, j: u32) -> vec2<i32> {
    if REDUCE_EXTRAPOLATE {
        return extrapolate_pair(line, j);
    }
    return standard_pair(line, j);
}

// One level. The block at `off` holds HL, LH, HH and LL (n_h x n_l, n_l x n_h, n_h x n_h,
// n_l x n_l) and becomes w x w samples, w = n_l + n_h. Rows first: row r of the result is
// row r of L (from LL and HL) or, below n_l, of H (from LH and HH); then its columns.
// Invocations make pairs of samples, neighbours making neighbouring pairs of a row, then
// the same pair of neighbouring columns.
fn level(off: u32, n_l: u32, n_h: u32, i: u32) {
    let w = n_l + n_h;
    let pairs = (w + 1u) / 2u;
    let lh = off + n_h * n_l;
    let hh = lh + n_l * n_h;
    let ll = hh + n_h * n_h;
    let j = i % 32u;
    if j < pairs {
        for (var r = i / 32u; r < w; r += THREADS / 32u) {
            var line = Line(ll + r * n_l, off + r * n_h, 1u, n_l, n_h);
            if r >= n_l {
                line = Line(lh + (r - n_l) * n_l, hh + (r - n_l) * n_h, 1u, n_l, n_h);
            }
            let samples = pair(line, j);
            m[ROWS + r * w + 2u * j] = samples.x;
            if 2u * j + 1u < w {
                m[ROWS + r * w + 2u * j + 1u] = samples.y;
            }
        }
    }
    workgroupBarrier();
    let column = i % 64u;
    if column < w {
        for (var j = i / 64u; j < pairs; j += THREADS / 64u) {
            let samples = pair(Line(ROWS + column, ROWS + n_l * w + column, w, n_l, n_h), j);
            m[off + 2u * j * w + column] = samples.x;
            if 2u * j + 1u < w {
                m[off + (2u * j + 1u) * w + column] = samples.y;
            }
        }
    }
    workgroupBarrier();
}

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) i: u32) {
    let table = data[3];
    let entry = table + 1u + select(0u, data[table], REDUCE_EXTRAPOLATE) + group.x;
    let base = data[2] + data[entry] + group.y * 2048u;
    for (var k = i; k < 2048u; k += THREADS) {
        let packed = data[base + k];
        m[2u * k] = i32(packed << 16u) >> 16u;
        m[2u * k + 1u] = i32(packed) >> 16u;
    }
    workgroupBarrier();
    if REDUCE_EXTRAPOLATE {
        level(3807u, 9u, 8u, i);
        level(3007u, 17u, 16u, i);
        level(0u, 33u, 31u, i);
    } else {
        level(3840u, 8u, 8u, i);
        level(3072u, 16u, 16u, i);
        level(0u, 32u, 32u, i);
    }
    for (var k = i; k < 2048u; k += THREADS) {
        let low = clamp(m[2u * k], -8192, 8191);
        let high = clamp(m[2u * k + 1u], -8192, 8191);
        data[base + k] = (u32(low) & 0xffffu) | (u32(high) << 16u);
    }
}
