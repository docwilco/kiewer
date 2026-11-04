// The one shader: flat triangles in millimetres, placed by the camera's
// centre and scale, coloured by the paint of the draw they belong to. The
// centre is subtracted first, in f32 on values that are both small near
// the board, so the product that follows never sees a large coordinate.

struct View {
    center: vec2<f32>,
    scale: vec2<f32>,
}

// The paint of one draw: its colour, its layer's opacity as alpha, and
// its group's depth, which a later group has nearer (render.rs says why);
// one uniform buffer holds every draw's at 256-byte steps, bound with a
// dynamic offset per draw.
struct Paint {
    color: vec4<f32>,
    depth: f32,
}

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var<uniform> paint: Paint;

struct VertexInput {
    @location(0) position: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let clip = (input.position - view.center) * view.scale;
    out.clip = vec4<f32>(clip, paint.depth, 1.0);
    return out;
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return paint.color;
}
