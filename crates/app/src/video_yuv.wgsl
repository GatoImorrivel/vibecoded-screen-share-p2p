struct Uniforms {
    // Fraction of the viewport covered by the image (letterboxing).
    scale: vec2<f32>,
    // 1.0 when the render target is sRGB and expects linear output.
    linear_out: f32,
    // 1.0 for NV12: tex_u holds interleaved U/V and tex_v is unused.
    nv12: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex_y: texture_2d<f32>;
@group(0) @binding(3) var tex_u: texture_2d<f32>;
@group(0) @binding(4) var tex_v: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
    );
    let c = corners[i];
    var out: VsOut;
    out.pos = vec4<f32>(c * u.scale, 0.0, 1.0);
    out.uv = vec2<f32>((c.x + 1.0) * 0.5, (1.0 - c.y) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // BT.709 limited range.
    let y = (textureSample(tex_y, samp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let chroma = textureSample(tex_u, samp, in.uv).rg;
    let v_planar = textureSample(tex_v, samp, in.uv).r;
    let raw_uv = select(vec2<f32>(chroma.r, v_planar), chroma, u.nv12 > 0.5);
    let cb = (raw_uv.x - 128.0 / 255.0) * (255.0 / 224.0);
    let cr = (raw_uv.y - 128.0 / 255.0) * (255.0 / 224.0);
    var rgb = clamp(
        vec3<f32>(y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    if (u.linear_out > 0.5) {
        let lo = rgb / 12.92;
        let hi = pow((rgb + 0.055) / 1.055, vec3<f32>(2.4));
        rgb = select(hi, lo, rgb <= vec3<f32>(0.04045));
    }
    return vec4<f32>(rgb, 1.0);
}
