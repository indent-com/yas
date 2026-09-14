#version 450
layout(location = 0) in vec2 v_tc;
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(location = 0) out vec4 color;
layout(set = 1, binding = 0) uniform sampler2D icc_lut;
layout(push_constant) uniform Color {
    layout(offset = 16) vec4 row0;
    vec4 row1;
    vec4 row2;
    // transfer, peak/reference, reference/203, peak nits
    vec4 params;
} pc;

vec3 srgb_decode(vec3 v) {
    return mix(v / 12.92, pow(max((v + 0.055) / 1.055, vec3(0)), vec3(2.4)), greaterThan(v, vec3(0.04045)));
}
vec3 pq_decode(vec3 v) {
    vec3 p = pow(clamp(v, 0.0, 1.0), vec3(32.0 / 2523.0));
    return pow(max(p - 3424.0 / 4096.0, 0.0) / max(2413.0 / 128.0 - 2392.0 / 128.0 * p, 0.00001), vec3(16384.0 / 2610.0)) * (10000.0 / 203.0);
}
vec3 lut_fetch(ivec3 v, int n) {
    return texelFetch(icc_lut, ivec2(v.x + (v.z % 8) * n, v.y + (v.z / 8) * n), 0).rgb;
}
vec3 icc_decode(vec3 rgb) {
    int n = textureSize(icc_lut, 0).x / 8;
    vec3 p = clamp(rgb, 0.0, 1.0) * float(n - 1);
    ivec3 a = min(ivec3(floor(p)), ivec3(n-2));
    vec3 f = p-vec3(a);
    return mix(mix(mix(lut_fetch(a,n), lut_fetch(a+ivec3(1,0,0),n),f.x),
                   mix(lut_fetch(a+ivec3(0,1,0),n),lut_fetch(a+ivec3(1,1,0),n),f.x),f.y),
               mix(mix(lut_fetch(a+ivec3(0,0,1),n),lut_fetch(a+ivec3(1,0,1),n),f.x),
                   mix(lut_fetch(a+ivec3(0,1,1),n),lut_fetch(a+ivec3(1,1,1),n),f.x),f.y),f.z);
}
void main() {
    vec4 sample_color = texture(tex, v_tc);
    float alpha = clamp(sample_color.a, 0.0, 1.0);
    vec3 rgb = alpha > 0.0 ? sample_color.rgb / alpha : vec3(0);
    int tf = int(pc.params.x);
    int intent = int(pc.row2.w);
    if (tf == 14) { color = vec4(icc_decode(rgb) * alpha, alpha); return; }
    if (tf == 0) rgb = srgb_decode(rgb) * pc.params.y;
    else if (tf == 1) rgb = pow(max(rgb, 0.0), vec3(2.2)) * pc.params.y;
    else if (tf == 2) rgb = pow(max(rgb, 0.0), vec3(2.6)) * pc.params.y;
    else if (tf == 3) rgb *= pc.params.y;
    else if (tf == 6) rgb *= 80.0 / 203.0;
    else if (tf == 4) rgb = pq_decode(rgb) / pc.params.z;
    else if (tf == 7) rgb = sign(rgb) * pow(abs(rgb), vec3(pc.row0.w)) * pc.params.y;
    else if (tf == 8) {
        float black = pow(pc.row1.w, 1.0 / 2.4);
        float swing = pow(pc.params.w, 1.0 / 2.4) - black;
        rgb = (pow(max(rgb * swing + black, 0.0), vec3(2.4)) - pc.row1.w) / (pc.params.z * 203.0);
    }
    else if (tf == 9) rgb = mix(rgb / 4.0, pow(max((rgb + 0.1115) / 1.1115, 0.0), vec3(1.0 / 0.45)), greaterThan(rgb, vec3(0.0912))) * pc.params.y;
    else if (tf == 10 || tf == 11) rgb = mix(vec3(0), pow(vec3(10), (rgb - 1.0) * (tf == 10 ? 2.0 : 2.5)), greaterThan(rgb, vec3(0))) * pc.params.y;
    else if (tf == 12) rgb = sign(rgb) * mix(abs(rgb) / 4.5, pow((abs(rgb) + 0.099) / 1.099, vec3(1.0 / 0.45)), greaterThan(abs(rgb), vec3(0.081))) * pc.params.y;
    else if (tf == 13) rgb = sign(rgb) * srgb_decode(abs(rgb)) * pc.params.y;
    else {
        const float a = 0.17883277;
        const float b = 0.28466892;
        const float c = 0.55991073;
        rgb = mix(rgb * rgb / 3.0, (exp((rgb - c) / a) + b) / 12.0, greaterThan(rgb, vec3(0.5)));
        // HLG inverse OETF followed by the display OOTF at the declared peak.
        vec3 scene = vec3(dot(pc.row0.xyz, rgb), dot(pc.row1.xyz, rgb), dot(pc.row2.xyz, rgb));
        float y = max(dot(scene, vec3(0.2627, 0.6780, 0.0593)), 0.000001);
        float gamma = 1.2 + 0.42 * log2(max(pc.params.w, 1.0) / 1000.0) / log2(10.0);
        rgb *= pow(y, gamma - 1.0) * pc.params.y;
    }
    rgb = vec3(dot(pc.row0.xyz, rgb), dot(pc.row1.xyz, rgb), dot(pc.row2.xyz, rgb));
    // Relative and absolute intents retain the source black; perceptual and
    // relative-BPC map black to black. Absolute also preserves luminance.
    if ((intent == 1 || intent == 2) && tf != 6) rgb += pc.row1.w / (pc.params.z * 203.0);
    if (intent == 2 && tf != 6) rgb *= pc.params.z;
    if (intent == 4) {
        // Prefer saturation when the input extends outside the working gamut.
        float peak = max(max(rgb.r, rgb.g), rgb.b);
        rgb = max(rgb, vec3(0));
        if (peak > 0.0) rgb *= peak / max(max(max(rgb.r, rgb.g), rgb.b), 0.00001);
    }
    color = vec4(rgb * alpha, alpha);
}
