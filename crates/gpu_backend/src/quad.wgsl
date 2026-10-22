// Solid rects and box shadows (a shadow is a rect whose edge fades over its
// blur). Needs common.wgsl.

struct Instance {
    @location(0) local: vec4<f32>,      // x, y, w, h (local px)
    @location(1) color: vec4<f32>,
    @location(2) radius_blur: vec2<f32>,
    @location(3) m0: vec4<f32>,
    @location(4) m1: vec2<f32>,
    @location(5) clip: vec4<f32>,
    @location(6) clip_extra: vec4<f32>,
    @location(7) local_clip: vec4<f32>,
    @location(8) lm0: vec4<f32>,
    @location(9) lm1: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    // Position relative to the rect's centre, local px.
    @location(1) local_px: vec2<f32>,
    @location(2) @interpolate(flat) half_size: vec2<f32>,
    @location(3) @interpolate(flat) radius_blur: vec2<f32>,
    @location(4) @interpolate(flat) clip: vec4<f32>,
    @location(5) @interpolate(flat) clip_extra: vec4<f32>,
    @location(6) @interpolate(flat) local_clip: vec4<f32>,
    @location(7) @interpolate(flat) lm0: vec4<f32>,
    @location(8) @interpolate(flat) lm1: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: Instance) -> VertexOutput {
    let blur = instance.radius_blur.y;
    // The quad is the shape inflated by the blur on every side.
    let origin = instance.local.xy - vec2<f32>(blur, blur);
    let size = instance.local.zw + vec2<f32>(2.0 * blur, 2.0 * blur);
    let local = origin + unit_corner(vertex_index) * size;
    var out: VertexOutput;
    out.position = world_to_clip_space(apply_affine(instance.m0, instance.m1, local));
    out.color = instance.color;
    out.local_px = local - (instance.local.xy + instance.local.zw * 0.5);
    out.half_size = instance.local.zw * 0.5;
    out.radius_blur = instance.radius_blur;
    out.clip = instance.clip;
    out.clip_extra = instance.clip_extra;
    out.local_clip = instance.local_clip;
    out.lm0 = instance.lm0;
    out.lm1 = instance.lm1;
    return out;
}

// Signed distance to a rounded rectangle centred at the origin.
fn rounded_rect_sdf(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - (half - vec2<f32>(r, r));
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let radius = clamp(in.radius_blur.x, 0.0, min(in.half_size.x, in.half_size.y));
    let d = rounded_rect_sdf(in.local_px, in.half_size, radius);
    // ~0.75px antialiased edge; a box shadow fades over its blur instead.
    let feather = max(0.75, in.radius_blur.y);
    let shape = 1.0 - smoothstep(-feather, feather, d);
    let clip = clip_cover(fragment_world(in.position), in.clip, in.clip_extra, in.local_clip, in.lm0, in.lm1);
    return vec4<f32>(in.color.rgb, in.color.a * shape * clip);
}
