struct Parameters { red: vec4<f32>, green: vec4<f32>, blue: vec4<f32>, chroma_shift: vec4<f32> }
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var<uniform> params: Parameters;
@group(0) @binding(3) var filtering: sampler;
@vertex fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}
@fragment fn fragment(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let y = textureLoad(luma, vec2<i32>(pos.xy), 0).r;
    let chroma_pos = clamp((pos.xy + params.chroma_shift.xy) * 0.5,
        vec2(0.5), ceil(params.chroma_shift.zw * 0.5) - vec2(0.5));
    let uv = textureSampleLevel(chroma, filtering, chroma_pos / vec2<f32>(textureDimensions(chroma)), 0.0).rg;
    let yuv = vec4(y, uv, 1.0);
    return vec4(dot(params.red, yuv), dot(params.green, yuv), dot(params.blue, yuv), 1.0);
}
