// Fills, uploads and copies from other images drawn into one image as instanced quads, all in
// one render pass.
//
// data: [image width, image height, first pixel word, 0], then 8 words per quad
// (x, y, width, height, kind, payload..., 0), then the uploaded pixels as packed RGBA8.
// A fill's payload is its packed color; an upload's is its first word among the pixels,
// whose rows are packed `width` words apart; a copy's is the x and y it reads in `source`.
@group(0) @binding(0) var<storage, read> data: array<u32>;
@group(1) @binding(0) var source: texture_2d<f32>;

const FILL: u32 = 0u;
const COPY: u32 = 2u;

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
    return unpack4x8unorm(data[data[2] + data[base + 5u] + p.y * data[base + 2u] + p.x]);
}
