// Port of Eric Lengyel's Slug reference shaders via sokol-game's integer-attribute variant.
// Slug shader code Copyright 2017 by Eric Lengyel. See Slug-MIT-LICENSE.txt.
//
// Both textures share the packer width and use `textureLoad` (no sampler).

struct GlyphParams {
    mvp: array<vec4<f32>, 4>,
    // xy = framebuffer size in pixels; z = optical weight boost (1.0 enables sqrt(coverage)); w = unused.
    viewport: vec4<f32>,
}

@group(0) @binding(0)
var<uniform> params: GlyphParams;

@group(0) @binding(1)
var curve_tex: texture_2d<f32>;

@group(0) @binding(2)
var band_tex: texture_2d<u32>;

struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) normal: vec2<f32>,
    @location(2) em: vec2<f32>,
    @location(3) inverse_jacobian: vec4<f32>,
    @location(4) band_transform: vec4<f32>,
    @location(5) colour: vec4<f32>,
    @location(6) band_lookup: vec2<u32>,
}

struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) colour: vec4<f32>,
    @location(1) em: vec2<f32>,
    @location(2) @interpolate(flat) band_transform: vec4<f32>,
    @location(3) @interpolate(flat) glyph: vec4<i32>,
}

override LOG_TEXTURE_WIDTH: u32;

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    let n = normalize(in.normal);
    let s = dot(params.mvp[3].xy, in.position) + params.mvp[3].w;
    let t = dot(params.mvp[3].xy, n);

    let u = (s * dot(params.mvp[0].xy, n) - t * (dot(params.mvp[0].xy, in.position) + params.mvp[0].w)) * params.viewport.x;
    let v = (s * dot(params.mvp[1].xy, n) - t * (dot(params.mvp[1].xy, in.position) + params.mvp[1].w)) * params.viewport.y;

    let s2 = s * s;
    let st = s * t;
    let uv = u * u + v * v;
    let d = in.normal * (s2 * (st + sqrt(uv)) / (uv - st * st));

    let p = in.position + d;
    let em = vec2<f32>(
        in.em.x + dot(d, in.inverse_jacobian.xy),
        in.em.y + dot(d, in.inverse_jacobian.zw),
    );

    var out: VertexOut;
    out.clip_position = vec4<f32>(
        dot(params.mvp[0].xy, p) + params.mvp[0].w,
        dot(params.mvp[1].xy, p) + params.mvp[1].w,
        dot(params.mvp[2].xy, p) + params.mvp[2].w,
        dot(params.mvp[3].xy, p) + params.mvp[3].w,
    );
    out.em = em;
    out.glyph = vec4<i32>(
        i32(in.band_lookup.x & 0xffffu),
        i32(in.band_lookup.x >> 16u),
        i32(in.band_lookup.y & 0xffffu),
        i32(in.band_lookup.y >> 16u),
    );
    out.band_transform = in.band_transform;
    out.colour = in.colour;
    return out;
}

fn curve_texel(at: vec2<i32>) -> vec4<f32> {
    return textureLoad(curve_tex, at, 0);
}

fn band_texel(at: vec2<i32>) -> vec2<u32> {
    return textureLoad(band_tex, at, 0).xy;
}

fn root_code(y1: f32, y2: f32, y3: f32) -> u32 {
    let i1 = bitcast<u32>(y1) >> 31u;
    let i2 = bitcast<u32>(y2) >> 30u;
    let i3 = bitcast<u32>(y3) >> 29u;

    var shift = (i2 & 2u) | (i1 & ~2u);
    shift = (i3 & 4u) | (shift & ~4u);

    return (0x2E74u >> shift) & 0x0101u;
}

fn solve_horizontal(p12: vec4<f32>, p3: vec2<f32>) -> vec2<f32> {
    let a = p12.xy - p12.zw * 2.0 + p3;
    let b = p12.xy - p12.zw;
    let ra = 1.0 / a.y;
    let rb = 0.5 / b.y;

    let d = sqrt(max(b.y * b.y - a.y * p12.y, 0.0));
    var t1 = (b.y - d) * ra;
    var t2 = (b.y + d) * ra;

    if abs(a.y) < (1.0 / 65536.0) {
        t1 = p12.y * rb;
        t2 = t1;
    }

    return vec2<f32>(
        (a.x * t1 - b.x * 2.0) * t1 + p12.x,
        (a.x * t2 - b.x * 2.0) * t2 + p12.x,
    );
}

fn solve_vertical(p12: vec4<f32>, p3: vec2<f32>) -> vec2<f32> {
    let a = p12.xy - p12.zw * 2.0 + p3;
    let b = p12.xy - p12.zw;
    let ra = 1.0 / a.x;
    let rb = 0.5 / b.x;

    let d = sqrt(max(b.x * b.x - a.x * p12.x, 0.0));
    var t1 = (b.x - d) * ra;
    var t2 = (b.x + d) * ra;

    if abs(a.x) < (1.0 / 65536.0) {
        t1 = p12.x * rb;
        t2 = t1;
    }

    return vec2<f32>(
        (a.y * t1 - b.y * 2.0) * t1 + p12.y,
        (a.y * t2 - b.y * 2.0) * t2 + p12.y,
    );
}

fn band_location(glyph_loc: vec2<i32>, offset: u32) -> vec2<i32> {
    var loc = vec2<i32>(glyph_loc.x + i32(offset), glyph_loc.y);
    loc.y += loc.x >> LOG_TEXTURE_WIDTH;
    loc.x &= (1 << LOG_TEXTURE_WIDTH) - 1;
    return loc;
}

fn calc_coverage(xcov: f32, ycov: f32, xwgt: f32, ywgt: f32) -> f32 {
    var c = max(
        abs(xcov * xwgt + ycov * ywgt) / max(xwgt + ywgt, 1.0 / 65536.0),
        min(abs(xcov), abs(ycov)),
    );
    c = clamp(c, 0.0, 1.0);
    if params.viewport.z > 0.5 {
        c = sqrt(c);
    }
    return c;
}

fn slug_render(at: vec2<f32>, bands: vec4<f32>, glyph_data: vec4<i32>) -> f32 {
    let pixels_per_em = vec2<f32>(1.0) / max(fwidth(at), vec2<f32>(1e-6));

    var band_max = glyph_data.zw;
    band_max.y &= 0xff;

    let band = clamp(vec2<i32>(floor(at * bands.xy + bands.zw)), vec2<i32>(0), band_max);
    let glyph_loc = glyph_data.xy;

    var xcov = 0.0;
    var xwgt = 0.0;

    let hband = band_texel(vec2<i32>(glyph_loc.x + band.y, glyph_loc.y));
    let hloc = band_location(glyph_loc, hband.y);

    for (var i = 0; i < i32(hband.x); i++) {
        let curve_loc = vec2<i32>(band_texel(vec2<i32>(hloc.x + i, hloc.y)));
        let p12 = curve_texel(curve_loc) - vec4<f32>(at, at);
        let p3 = curve_texel(vec2<i32>(curve_loc.x + 1, curve_loc.y)).xy - at;

        if max(max(p12.x, p12.z), p3.x) * pixels_per_em.x < -0.5 {
            break;
        }

        let code = root_code(p12.y, p12.w, p3.y);
        if code != 0u {
            let r = solve_horizontal(p12, p3) * pixels_per_em.x;

            if (code & 1u) != 0u {
                xcov += clamp(r.x + 0.5, 0.0, 1.0);
                xwgt = max(xwgt, clamp(1.0 - abs(r.x) * 2.0, 0.0, 1.0));
            }

            if code > 1u {
                xcov -= clamp(r.y + 0.5, 0.0, 1.0);
                xwgt = max(xwgt, clamp(1.0 - abs(r.y) * 2.0, 0.0, 1.0));
            }
        }
    }

    var ycov = 0.0;
    var ywgt = 0.0;

    let vband = band_texel(vec2<i32>(glyph_loc.x + band_max.y + 1 + band.x, glyph_loc.y));
    let vloc = band_location(glyph_loc, vband.y);

    for (var i = 0; i < i32(vband.x); i++) {
        let curve_loc = vec2<i32>(band_texel(vec2<i32>(vloc.x + i, vloc.y)));
        let p12 = curve_texel(curve_loc) - vec4<f32>(at, at);
        let p3 = curve_texel(vec2<i32>(curve_loc.x + 1, curve_loc.y)).xy - at;

        if max(max(p12.y, p12.w), p3.y) * pixels_per_em.y < -0.5 {
            break;
        }

        let code = root_code(p12.x, p12.z, p3.x);
        if code != 0u {
            let r = solve_vertical(p12, p3) * pixels_per_em.y;

            if (code & 1u) != 0u {
                ycov -= clamp(r.x + 0.5, 0.0, 1.0);
                ywgt = max(ywgt, clamp(1.0 - abs(r.x) * 2.0, 0.0, 1.0));
            }

            if code > 1u {
                ycov += clamp(r.y + 0.5, 0.0, 1.0);
                ywgt = max(ywgt, clamp(1.0 - abs(r.y) * 2.0, 0.0, 1.0));
            }
        }
    }

    return calc_coverage(xcov, ycov, xwgt, ywgt);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let cov = slug_render(in.em, in.band_transform, in.glyph);
    return in.colour * cov;
}
