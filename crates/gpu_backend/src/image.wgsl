// Images: an image, or a part of it (clipped to its box), or a range beyond
// 0..1 that wraps (a repeating background — one instance however many
// tiles). Milestone 34: the image may sit in a shared atlas page, at
// `texture_rect`. Needs common.wgsl.

struct Instance {
    @location(0) local: vec4<f32>,         // x, y, w, h (local px)
    @location(1) uv: vec4<f32>,            // u0, v0, u1, v1 in the image
    @location(2) tiled_alpha: vec2<f32>,   // 1: coordinates wrap; opacity
    @location(3) texture_rect: vec4<f32>,  // the image in its texture: x0, y0, x1, y1 normalized
    @location(4) m0: vec4<f32>,
    @location(5) m1: vec2<f32>,
    @location(6) clip: vec4<f32>,
    @location(7) clip_extra: vec4<f32>,
    @location(8) local_clip: vec4<f32>,
    @location(9) lm0: vec4<f32>,
    @location(10) lm1: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) tiled_alpha: vec2<f32>,
    @location(2) @interpolate(flat) texture_rect: vec4<f32>,
    @location(3) @interpolate(flat) clip: vec4<f32>,
    @location(4) @interpolate(flat) clip_extra: vec4<f32>,
    @location(5) @interpolate(flat) local_clip: vec4<f32>,
    @location(6) @interpolate(flat) lm0: vec4<f32>,
    @location(7) @interpolate(flat) lm1: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: Instance) -> VertexOutput {
    let corner = unit_corner(vertex_index);
    var out: VertexOutput;
    out.position = world_to_clip_space(apply_affine(instance.m0, instance.m1, instance.local.xy + corner * instance.local.zw));
    out.uv = instance.uv.xy + corner * (instance.uv.zw - instance.uv.xy);
    out.tiled_alpha = instance.tiled_alpha;
    out.texture_rect = instance.texture_rect;
    out.clip = instance.clip;
    out.clip_extra = instance.clip_extra;
    out.local_clip = instance.local_clip;
    out.lm0 = instance.lm0;
    out.lm1 = instance.lm1;
    return out;
}

@group(1) @binding(0) var image_texture: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Wrap in the shader; stay half a texel inside the image's own texels, so
    // neither the opposite edge nor an atlas neighbour bleeds in.
    let uv = select(in.uv, fract(in.uv), in.tiled_alpha.x > 0.5);
    let image_px = (in.texture_rect.zw - in.texture_rect.xy) * vec2<f32>(textureDimensions(image_texture));
    let half_texel = vec2<f32>(0.5, 0.5) / max(image_px, vec2<f32>(1.0, 1.0));
    let inside = clamp(uv, half_texel, vec2<f32>(1.0, 1.0) - half_texel);
    let texel = textureSample(image_texture, image_sampler, mix(in.texture_rect.xy, in.texture_rect.zw, inside));
    let clip = clip_cover(fragment_world(in.position), in.clip, in.clip_extra, in.local_clip, in.lm0, in.lm1);
    return vec4<f32>(texel.rgb, texel.a * in.tiled_alpha.y * clip);
}
