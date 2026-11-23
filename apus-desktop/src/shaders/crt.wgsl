@group(0) @binding(0) var scene: texture_2d<f32>;
@group(0) @binding(1) var scene_sampler: sampler;
@group(0) @binding(2) var<uniform> background_and_mix: vec4<f32>;

override encode_srgb: bool = false;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

fn luminance(rgb: vec3<f32>) -> f32 {
    return dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn scene_color(uv: vec2<f32>) -> vec3<f32> {
    let outside = any(abs(uv - 0.5) > vec2<f32>(0.5));
    return select(textureSampleLevel(scene, scene_sampler, uv, 0.0).rgb, background_and_mix.rgb, outside);
}

fn signal(uv: vec2<f32>, light_palette: bool) -> f32 {
    let light = luminance(scene_color(uv));
    // Invert in roughly perceptual space so light theme panels stay close to black.
    let inverted = 1.0 - sqrt(light);
    return select(light, inverted * inverted, light_palette);
}

fn bloom(uv: vec2<f32>, texel: vec2<f32>, light_palette: bool) -> f32 {
    let dx = vec2<f32>(texel.x * 1.5, 0.0);
    let dy = vec2<f32>(0.0, texel.y * 1.5);
    let diagonal = texel * vec2<f32>(3.5, 2.5);
    let other = diagonal * vec2<f32>(1.0, -1.0);
    let near = signal(uv + dx, light_palette) + signal(uv - dx, light_palette)
        + signal(uv + dy, light_palette) + signal(uv - dy, light_palette);
    let far = signal(uv + diagonal, light_palette) + signal(uv - diagonal, light_palette)
        + signal(uv + other, light_palette) + signal(uv - other, light_palette);
    return max(near * 0.25 - 0.06, 0.0) * 0.28 + max(far * 0.25 - 0.06, 0.0) * 0.13;
}

// PCG output permutation described at https://www.reedbeta.com/blog/hash-functions-for-gpu-rendering/
fn grain(pixel: vec2<f32>) -> f32 {
    let xy = vec2<u32>(floor(pixel));
    let seed = xy.x * 1973u + xy.y * 9277u + 89173u;
    let bits = ((seed >> ((seed >> 28u) + 4u)) ^ seed) * 277803737u;
    return f32(((bits >> 22u) ^ bits) & 255u) / 255.0;
}

fn srgb(rgb: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(rgb, vec3<f32>(1.0 / 2.4)) - 0.055, rgb * 12.92, rgb <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(scene));
    let screen = position.xy / size * 2.0 - 1.0;
    let full_curve = screen * (1.0 + screen.yx * screen.yx * vec2<f32>(0.022, 0.030))
        * vec2<f32>(1.08, 1.09);
    let amount = background_and_mix.a;
    let curved = mix(screen, full_curve, amount);
    let uv = curved * 0.5 + 0.5;

    let glass_radius = 0.065;
    let glass_extent = 1.03;
    let corner = abs(curved) - (glass_extent - glass_radius);
    let edge = length(max(corner, vec2<f32>(0.0))) + min(max(corner.x, corner.y), 0.0) - glass_radius;
    let glass = 1.0 - smoothstep(-0.004, 0.002, edge);
    var rgb = vec3<f32>(0.0);
    if glass > 0.0 {
        let light_palette = luminance(background_and_mix.rgb) > 0.45;
        let core = signal(uv, light_palette);
        let scanline = 0.93 + 0.07 * cos(uv.y * size.y * (6.2831853 / 3.2));
        let vignette = 1.0 - 0.26 * smoothstep(0.25, 1.85, dot(curved, curved));
        let noise = (grain(position.xy) - 0.5) * 0.007 * (1.0 - smoothstep(0.015, 0.16, core));
        let light = clamp((core + bloom(uv, 1.0 / size, light_palette)) * scanline * vignette + noise, 0.0, 1.0);
        let phosphor = mix(vec3<f32>(0.035, 1.0, 0.24), vec3<f32>(0.55, 1.0, 0.70), light);
        let rim = 0.005 * (1.0 - smoothstep(0.0, 0.020, abs(edge)));
        rgb = (vec3<f32>(0.001, 0.004, 0.0015) + phosphor * (light + rim)) * glass;
    }
    if amount < 1.0 {
        rgb = mix(scene_color(uv), rgb, amount);
    }
    return vec4<f32>(select(rgb, srgb(rgb), encode_srgb), 1.0);
}
