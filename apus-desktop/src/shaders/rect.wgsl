// Flat pixel-snapped rectangle shader for cell backgrounds, decorations, and geometric glyphs
// (quadrants, half/full blocks, box drawing, Braille, and Unicode 16 octants).
//
// Square corners cover every pixel inside the quad with no anti-aliasing so adjacent cells never
// show 1 px seams.

struct Uniforms {
    mvp: array<vec4<f32>, 4>,
}

@group(0) @binding(0)
var<uniform> params: Uniforms;

struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) colour: vec4<f32>,
}

struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) colour: vec4<f32>,
}

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    var out: VertexOut;
    out.clip_position = vec4<f32>(
        dot(params.mvp[0].xy, in.position) + params.mvp[0].w,
        dot(params.mvp[1].xy, in.position) + params.mvp[1].w,
        dot(params.mvp[2].xy, in.position) + params.mvp[2].w,
        dot(params.mvp[3].xy, in.position) + params.mvp[3].w,
    );
    out.colour = in.colour;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return in.colour;
}
