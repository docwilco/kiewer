// The text pass: quads in millimetres placed by the same camera as the
// flat shader, each sampling a face's coverage atlas (R8, one channel).
// A glyph's alpha is the coverage; a knockout's cells and strips invert
// it, so the colour fills the box and the glyph shows what is beneath.

struct View {
    center: vec2<f32>,
    scale: vec2<f32>,
}

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var atlas: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) invert: f32,
}

struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) invert: f32,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let clip = (input.position - view.center) * view.scale;
    out.clip = vec4<f32>(clip, 0.0, 1.0);
    out.uv = input.uv;
    out.color = input.color;
    out.invert = input.invert;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let coverage = textureSample(atlas, atlas_sampler, input.uv).r;
    let alpha = mix(coverage, 1.0 - coverage, input.invert);
    return vec4<f32>(input.color.rgb, input.color.a * alpha);
}
