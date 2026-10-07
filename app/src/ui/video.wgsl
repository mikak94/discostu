struct Uniforms {
    rect: vec4<f32>,
    size: vec4<f32>,
    cursor: vec4<f32>,
    tex: vec4<f32>,
    mode: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var plane0: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
@group(0) @binding(3) var plane1: texture_2d<f32>;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[i];
    var out: VOut;
    out.pos = vec4<f32>(mix(u.rect.x, u.rect.z, c.x), mix(u.rect.y, u.rect.w, c.y), 0.0, 1.0);
    out.uv = c;
    return out;
}

fn sd_round_rect(p: vec2<f32>, half_size: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - half_size + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Signed distance to a classic arrow pointer, in logical px (tip at origin).
fn sd_arrow(p: vec2<f32>) -> f32 {
    var v = array<vec2<f32>, 7>(
        vec2<f32>(0.0, 0.0), vec2<f32>(0.0, 16.0), vec2<f32>(4.0, 12.5),
        vec2<f32>(7.0, 19.0), vec2<f32>(9.5, 18.0), vec2<f32>(6.5, 11.5),
        vec2<f32>(11.5, 11.5),
    );
    var d = dot(p - v[0], p - v[0]);
    var s = 1.0;
    var j = 6u;
    for (var i = 0u; i < 7u; i = i + 1u) {
        let e = v[j] - v[i];
        let w = p - v[i];
        let b = w - e * clamp(dot(w, e) / dot(e, e), 0.0, 1.0);
        d = min(d, dot(b, b));
        let c = vec3<bool>(p.y >= v[i].y, p.y < v[j].y, e.x * w.y > e.y * w.x);
        if (all(c) || all(!c)) {
            s = -s;
        }
        j = i;
    }
    return s * sqrt(d);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    // Sample both planes unconditionally (uniform control flow), then pick.
    let p0 = textureSample(plane0, smp, in.uv);
    let p1 = textureSample(plane1, smp, in.uv);

    var col = p0.rgb;
    if (u.mode.x > 0.5) {
        // BT.709 limited range → gamma-encoded RGB.
        let y = (p0.r - 16.0 / 255.0) * (255.0 / 219.0);
        let cb = (p1.r - 128.0 / 255.0) * (255.0 / 224.0);
        let cr = (p1.g - 128.0 / 255.0) * (255.0 / 224.0);
        let rgb = vec3<f32>(
            y + 1.5748 * cr,
            y - 0.1873 * cb - 0.4681 * cr,
            y + 1.8556 * cb,
        );
        col = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
        if (u.mode.y > 0.5) {
            col = srgb_to_linear(col);
        }
    } else if (u.cursor.z > 0.5) {
        let sf = u.tex.z;
        let p = (in.uv * u.tex.xy - u.cursor.xy) * u.cursor.w / sf;
        let d = sd_arrow(p);
        let aa = 0.8 / sf;
        let outline = 1.0 - smoothstep(-aa, aa, d - 1.2);
        let fill = 1.0 - smoothstep(-aa, aa, d);
        col = mix(col, vec3<f32>(0.0), outline * 0.85);
        col = mix(col, vec3<f32>(1.0), fill);
    }

    let half_size = u.size.xy * 0.5;
    let d = sd_round_rect(in.uv * u.size.xy - half_size, half_size, u.size.z);
    let a = 1.0 - smoothstep(-0.5, 0.5, d);
    return vec4<f32>(col * a, a);
}
