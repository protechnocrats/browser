// Milestone 34: a group's layer (premultiplied, the size of the target)
// drawn onto the target around it at an opacity — and, at opacity 1 over
// the whole viewport, the back buffer onto the window. Needs common.wgsl.

struct Instance {
    @location(0) rect: vec4<f32>,  // x0, y0, x1, y1 (world px)
    @location(1) opacity: f32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) opacity: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: Instance) -> VertexOutput {
    let p = mix(instance.rect.xy, instance.rect.zw, unit_corner(vertex_index));
    var out: VertexOutput;
    out.position = world_to_clip_space(p);
    out.opacity = instance.opacity;
    return out;
}

@group(1) @binding(0) var layer: texture_2d<f32>;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // The layer is the target's size: its texel is the fragment's pixel.
    let texel = textureLoad(layer, vec2<i32>(floor(in.position.xy)), 0);
    return texel * in.opacity;
}
