// Glyph quads, tinted coverage from the glyph atlas. Needs common.wgsl.

struct Instance {
    @location(0) local: vec4<f32>,  // x, y, w, h (local px)
    @location(1) uv: vec4<f32>,     // offset (xy) and size (zw) in the atlas, normalized
    @location(2) color: vec4<f32>,
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
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) clip: vec4<f32>,
    @location(3) @interpolate(flat) clip_extra: vec4<f32>,
    @location(4) @interpolate(flat) local_clip: vec4<f32>,
    @location(5) @interpolate(flat) lm0: vec4<f32>,
    @location(6) @interpolate(flat) lm1: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: Instance) -> VertexOutput {
    let corner = unit_corner(vertex_index);
    var out: VertexOutput;
    out.position = world_to_clip_space(apply_affine(instance.m0, instance.m1, instance.local.xy + corner * instance.local.zw));
    out.uv = instance.uv.xy + corner * instance.uv.zw;
    out.color = instance.color;
    out.clip = instance.clip;
    out.clip_extra = instance.clip_extra;
    out.local_clip = instance.local_clip;
    out.lm0 = instance.lm0;
    out.lm1 = instance.lm1;
    return out;
}

@group(1) @binding(0) var atlas_texture: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Single-channel coverage (R8Unorm), tinted with the glyph's colour.
    let coverage = textureSample(atlas_texture, atlas_sampler, in.uv).r;
    let clip = clip_cover(fragment_world(in.position), in.clip, in.clip_extra, in.local_clip, in.lm0, in.lm1);
    return vec4<f32>(in.color.rgb, in.color.a * coverage * clip);
}
