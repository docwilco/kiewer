// The one shader: flat-coloured triangles in millimetres, placed by the
// camera's centre and scale. The centre is subtracted first, in f32 on
// values that are both small near the board, so the product that follows
// never sees a large coordinate.

struct View {
    center: vec2<f32>,
    scale: vec2<f32>,
}

@group(0) @binding(0) var<uniform> view: View;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let clip = (input.position - view.center) * view.scale;
    out.clip = vec4<f32>(clip, 0.0, 1.0);
    out.color = input.color;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
