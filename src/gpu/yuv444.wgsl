// AVC444 on the GPU (MS-RDPEGFX 3.3.8.3); the CPU reference is session/gfx/avc444.rs.
//
// Each surface keeps a 4:4:4 store texture (r = Y, g = U, b = V). The main view rewrites Y and
// spreads its averaged chroma over each 2×2 block; the auxiliary view rewrites every chroma
// sample except the top-left of each block (the pipeline masks out the Y channel). Conversion
// recovers the top-left sample from the block average and writes RGB to the surface.

struct Params {
    // x, y: where the picture's origin lands in the render target.
    // z: picture width (v2 splits the auxiliary view at half and quarter of it).
    // w: 1 for the v2 auxiliary layout, 0 for v1.
    view: vec4<f32>,
    red: vec4<f32>,
    green: vec4<f32>,
    blue: vec4<f32>,
}

// Views: a decoded NV12 picture (plane 0: Y, plane 1: interleaved U/V).
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var<uniform> params: Params;

@vertex fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}

fn y_at(x: i32, y: i32) -> f32 {
    return textureLoad(luma, vec2(x, y), 0).r;
}

fn uv_at(x: i32, y: i32) -> vec2<f32> {
    return textureLoad(chroma, vec2(x, y), 0).rg;
}

@fragment fn main_view(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(pos.xy);
    return vec4(y_at(p.x, p.y), uv_at(p.x / 2, p.y / 2), 1.0);
}

@fragment fn auxiliary_view(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(pos.xy);
    if (p.x % 2 == 0 && p.y % 2 == 0) {
        discard;
    }
    var uv: vec2<f32>;
    if (params.view.w < 0.5) {
        // v1: odd rows in 8-row U/V bands of the auxiliary luma; odd columns of even rows in
        // the auxiliary chroma.
        if (p.y % 2 == 1) {
            let k = (p.y - 1) / 2;
            let row = 16 * (k / 8) + k % 8;
            uv = vec2(y_at(p.x, row), y_at(p.x, row + 8));
        } else {
            uv = uv_at(p.x / 2, p.y / 2);
        }
    } else {
        // v2: odd columns in the auxiliary luma (U left half, V right half); even columns of
        // odd rows in the auxiliary chroma (U plane for x % 4 == 0, V plane for x % 4 == 2,
        // each split into U and V quarters).
        let width = i32(params.view.z);
        let half = width / 2;
        let quarter = width / 4;
        if (p.x % 2 == 1) {
            uv = vec2(y_at(p.x / 2, p.y), y_at(half + p.x / 2, p.y));
        } else {
            let a = uv_at(p.x / 4, p.y / 2);
            let b = uv_at(quarter + p.x / 4, p.y / 2);
            if (p.x % 4 == 0) {
                uv = vec2(a.r, b.r);
            } else {
                uv = vec2(a.g, b.g);
            }
        }
    }
    return vec4(0.0, uv, 1.0);
}

// Conversion: binding 0 is the 4:4:4 store.
fn recover(average: f32, others: f32) -> f32 {
    let value = clamp(4.0 * average - others, 0.0, 1.0);
    // Keep the (noise-free) average unless the sample clearly differs (≥ 30 of 255).
    return select(value, average, abs(value - average) < 29.5 / 255.0);
}

@fragment fn convert(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(pos.xy - params.view.xy);
    let size = vec2<i32>(textureDimensions(luma));
    let s = textureLoad(luma, p, 0);
    var u = s.g;
    var v = s.b;
    if (p.x % 2 == 0 && p.y % 2 == 0 && p.x + 1 < size.x && p.y + 1 < size.y) {
        let right = textureLoad(luma, p + vec2(1, 0), 0);
        let below = textureLoad(luma, p + vec2(0, 1), 0);
        let diagonal = textureLoad(luma, p + vec2(1, 1), 0);
        u = recover(s.g, right.g + below.g + diagonal.g);
        v = recover(s.b, right.b + below.b + diagonal.b);
    }
    let yuv = vec4(s.r, u, v, 1.0);
    return vec4(dot(params.red, yuv), dot(params.green, yuv), dot(params.blue, yuv), 1.0);
}
