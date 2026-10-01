// Raw source tile shader (PREV-03).
//
// Draws one decoded YUV420P camera frame (a plane-texture pair) into a quad
// whose geometry was already letterboxed (contain) by the CPU-side
// `letterbox_quad`. `uv` runs 0..1 across the tile; v=0 is the top.
//
// The pipeline is built with `blend: None` and a `LoadOp::Clear` so both tiles
// composite directly onto the target view without alpha math; the 1px
// separator between the two quads is simply left un-drawn and shows the clear
// colour.

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Uniforms {
    // Extra padding so the struct matches wgpu's minimum uniform alignment if a
    // future revision adds fields; currently unused.
    _pad: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
) -> VertexOut {
    var out: VertexOut;
    // Input is already in clip space ([-1,1]); map to a full-depth quad.
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.uv = uv;
    return out;
}

// BT.709 limited-range YCbCr -> RGB, matching the stitch shader's contract.
fn yuv_to_rgb(y: f32, u: f32, v: f32) -> vec3<f32> {
    let yf = (y - 0.0625) * 1.164384;
    let uf = u - 0.5;
    let vf = v - 0.5;
    let r = yf + 1.792741 * vf;
    let g = yf - 0.213249 * uf - 0.532909 * vf;
    let b = yf + 2.112402 * uf;
    return vec3<f32>(r, g, b);
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var plane_sampler: sampler;

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let y = textureSample(y_tex, plane_sampler, in.uv).r;
    let u = textureSample(u_tex, plane_sampler, in.uv).r;
    let v = textureSample(v_tex, plane_sampler, in.uv).r;
    let rgb = yuv_to_rgb(y, u, v);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
