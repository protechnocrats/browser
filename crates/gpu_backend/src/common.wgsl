// Milestone 34: shared by every pipeline. Instances carry their geometry in
// their own (local) coordinates, a world transform, and the clip they are
// drawn under — the rules `rendering::walk` gives, applied per pixel here and
// in `rendering::raster` alike.

struct Globals {
    // The scene's viewport (CSS px), and the size of the target drawn into
    // (device px): world coordinates are CSS px.
    viewport: vec2<f32>,
    target_size: vec2<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;

// Unit quad corners, x right and y down, generated from the vertex index.
fn unit_corner(i: u32) -> vec2<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    return corners[i];
}

// x' = a x + c y + e, y' = b x + d y + f, with m0 = (a, b, c, d), m1 = (e, f).
fn apply_affine(m0: vec4<f32>, m1: vec2<f32>, p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(m0.x * p.x + m0.z * p.y + m1.x, m0.y * p.x + m0.w * p.y + m1.y);
}

fn world_to_clip_space(world: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(world.x / globals.viewport.x * 2.0 - 1.0, 1.0 - world.y / globals.viewport.y * 2.0, 0.0, 1.0);
}

// A fragment's position in world (CSS px) coordinates.
fn fragment_world(frag: vec4<f32>) -> vec2<f32> {
    return frag.xy * globals.viewport / globals.target_size;
}

// Coverage of point p by the box r = (x0, y0, x1, y1) with rounded corners.
fn box_cover(r: vec4<f32>, radius: f32, p: vec2<f32>) -> f32 {
    if (p.x < r.x || p.y < r.y || p.x >= r.z || p.y >= r.w) {
        return 0.0;
    }
    if (radius <= 0.0) {
        return 1.0;
    }
    let rr = min(radius, min((r.z - r.x) * 0.5, (r.w - r.y) * 0.5));
    let nearest = clamp(p, r.xy + vec2<f32>(rr, rr), r.zw - vec2<f32>(rr, rr));
    return clamp(rr + 0.5 - length(p - nearest), 0.0, 1.0);
}

// The clip: `clip` in world space with corner radius extra.x; when extra.y
// is 1, also `local_rect` (radius extra.z) in the space world points map to
// through (lm0, lm1).
fn clip_cover(world: vec2<f32>, clip: vec4<f32>, extra: vec4<f32>, local_rect: vec4<f32>, lm0: vec4<f32>, lm1: vec2<f32>) -> f32 {
    var c = box_cover(clip, extra.x, world);
    if (extra.y > 0.5) {
        c = c * box_cover(local_rect, extra.z, apply_affine(lm0, lm1, world));
    }
    return c;
}
