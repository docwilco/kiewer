// The label pass: every character of every net label is one instance of
// its glyph's mesh (em units), scaled by the label's height, turned by
// its angle, placed at its text's centre and then through the same
// camera as the flat shader. The label gate is here, per vertex: a
// label whose text height on screen is under the threshold is clipped
// away (its vertices go behind the far plane), and one between the
// threshold and half again fades in. A class the page toggled off is
// gated the same way. Nothing on the CPU changes per frame but the
// uniform.
//
// A mirrored view (the camera's x scale is negative) would show the
// text backwards: the glyph's x is flipped and the angle negated first,
// so the text reads normally, and a label at exactly a quarter turn
// keeps reading bottom to top.

struct View {
    center: vec2<f32>,
    scale: vec2<f32>,
}

// gate: device pixels per millimetre, the threshold in pixels, 1 when
// the view is mirrored, and how far past the threshold the fade ends
// (as a factor of it). classes_a and classes_b: 1 to show a class, 0
// to hide it, in the order pad number, pad net, via net, track net;
// zone net. halo and fill: the two colours.
struct Labels {
    gate: vec4<f32>,
    classes_a: vec4<f32>,
    classes_b: vec4<f32>,
    halo: vec4<f32>,
    fill: vec4<f32>,
}

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var<uniform> labels: Labels;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) halo: f32,
    @location(2) center: vec2<f32>,
    @location(3) cursor: f32,
    @location(4) height: f32,
    @location(5) direction: vec2<f32>,
    @location(6) kind: f32,
}

struct VertexOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
}

// The baseline's distance below the text's centre, in em: KiCad's
// centred text (kicad_core::labels::BASELINE_BELOW_CENTER).
const BASELINE: f32 = 0.415;

fn class_shown(kind: f32) -> f32 {
    let index = i32(round(kind));
    if index == 0 { return labels.classes_a.x; }
    if index == 1 { return labels.classes_a.y; }
    if index == 2 { return labels.classes_a.z; }
    if index == 3 { return labels.classes_a.w; }
    return labels.classes_b.x;
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    let px_per_mm = labels.gate.x;
    let threshold = labels.gate.y;
    let mirrored = labels.gate.z > 0.5;
    let fade_end = labels.gate.w;
    let height_px = input.height * px_per_mm;
    let alpha = smoothstep(threshold, threshold * fade_end, height_px) * class_shown(input.kind);
    if alpha <= 0.0 {
        // Behind the far plane: clipped, never rasterised.
        out.clip = vec4<f32>(0.0, 0.0, 2.0, 1.0);
        out.color = vec4<f32>(0.0);
        return out;
    }
    var local = vec2<f32>(input.cursor + input.position.x, BASELINE + input.position.y);
    var c = input.direction.x;
    var s = input.direction.y;
    if mirrored {
        local.x = -local.x;
        s = -s;
        // A quarter turn negated is minus a quarter turn, which the
        // (-90, 90] rule folds back to plus: still bottom to top.
        if abs(c) < 1e-5 && s < 0.0 {
            s = -s;
        }
    }
    // KiCad's rotation on a y-down frame: positive turns
    // counter-clockwise on the screen (kicad_core::geometry::Transform).
    let turned = vec2<f32>(local.x * c + local.y * s, -local.x * s + local.y * c);
    let position = input.center + turned * input.height;
    let clip = (position - view.center) * view.scale;
    out.clip = vec4<f32>(clip, 0.0, 1.0);
    let color = select(labels.fill, labels.halo, input.halo > 0.5);
    out.color = vec4<f32>(color.rgb, color.a * alpha);
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
