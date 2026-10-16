// Fills, uploads and copies from other images drawn into one image as instanced quads, all in
// one render pass.
//
// data: [image width, image height, first pixel word, tile table], then 8 words per quad
// (x, y, width, height, kind, payload..., 0), the tile table (see progressive.wgsl), then
// the pixels. A fill's payload is its packed color; an upload's is its first word among the
// pixels, whose rows of packed RGBA8 are `width` words apart; a copy's is the x and y it
// reads in `source`. A progressive tile's is the tile's first word among the pixels and its
// x and y within the tile (16 bits each); progressive.wgsl has made its samples by then.
@group(0) @binding(0) var<storage, read> data: array<u32>;
@group(1) @binding(0) var source: texture_2d<f32>;

const FILL: u32 = 0u;
const COPY: u32 = 2u;
const TILE: u32 = 3u;

// Sample `k` of a progressive tile's component (0 Y, 1 Cb, 2 Cr), stored as packed i16 pairs.
fn sample(tile: u32, component: u32, k: u32) -> i32 {
    let pair = data[tile + component * 2048u + k / 2u];
    return select(i32(pair << 16u) >> 16u, i32(pair) >> 16u, (k & 1u) == 1u);
}

struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) quad: u32,
}

@vertex fn vertex(@builtin(vertex_index) corner: u32, @builtin(instance_index) quad: u32) -> Varyings {
    let base = 4u + quad * 8u;
    let corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let origin = vec2(f32(data[base]), f32(data[base + 1u]));
    let size = vec2(f32(data[base + 2u]), f32(data[base + 3u]));
    let extent = vec2(f32(data[0]), f32(data[1]));
    let pixel = origin + corners[corner] * size;
    return Varyings(vec4(pixel / extent * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0), quad);
}

@fragment fn fragment(in: Varyings) -> @location(0) vec4<f32> {
    let base = 4u + in.quad * 8u;
    let kind = data[base + 4u];
    if kind == FILL {
        return unpack4x8unorm(data[base + 5u]);
    }
    let p = vec2<u32>(in.position.xy) - vec2(data[base], data[base + 1u]);
    if kind == COPY {
        return textureLoad(source, vec2(data[base + 5u], data[base + 6u]) + p, 0);
    }
    if kind == TILE {
        // RemoteFX fixed point (x32, Y biased by -4096), converted as the CPU decoder does.
        let at = data[base + 6u];
        let k = ((at >> 16u) + p.y) * 64u + (at & 0xffffu) + p.x;
        let tile = data[2] + data[base + 5u];
        let y = (sample(tile, 0u, k) + 4096) * 65536;
        let cb = sample(tile, 1u, k);
        let cr = sample(tile, 2u, k);
        let rgb = vec3(y + 91915 * cr, y - 46818 * cr - 22526 * cb, y + 115992 * cb);
        return vec4(vec3<f32>(clamp(rgb >> vec3(21u), vec3(0), vec3(255))) / 255.0, 1.0);
    }
    return unpack4x8unorm(data[data[2] + data[base + 5u] + p.y * data[base + 2u] + p.x]);
}
