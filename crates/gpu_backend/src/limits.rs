//! GPU resource limits, kept apart from the renderer so they are testable
//! without a GPU.
//!
//! Page content decides how large and how many textures the renderer is
//! asked for (image dimensions, glyph sizes), so every texture is checked
//! against the device's limits before it is created: a texture past
//! `max_texture_dimension_2d` is a wgpu validation error, and wgpu's default
//! response to one is a panic on the GPU thread — outside the per-page
//! crash containment, taking the whole window down (zero-trust: the page
//! is hostile, the GPU boundary validates).

/// The most bytes of image textures kept on the GPU at once: past it, the
/// least recently used are evicted (`image_store`, Milestone 34); a page
/// cannot grow GPU memory without bound.
pub const MAX_IMAGE_TEXTURE_BYTES: u64 = 256 * 1024 * 1024;

/// Whether a `width`×`height` texture can be created on a device whose
/// largest 2D texture side is `max_dim`. Zero-sized textures are invalid
/// too.
pub fn texture_fits(width: u32, height: u32, max_dim: u32) -> bool {
    width > 0 && height > 0 && width <= max_dim && height <= max_dim
}

/// The size a `width`×`height` image is scaled to so both sides fit in
/// `max_dim`, keeping its aspect ratio (each side at least 1).
pub fn fitted_size(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    if width <= max_dim && height <= max_dim {
        return (width, height);
    }
    let scale = max_dim as f64 / width.max(height) as f64;
    let fit = |side: u32| ((side as f64 * scale).floor() as u32).clamp(1, max_dim);
    (fit(width), fit(height))
}

/// `rgba` (tightly packed RGBA8, `width`×`height`) scaled down by nearest
/// sampling so both sides fit in `max_dim`. Images are drawn with
/// normalised texture coordinates, so a smaller texture draws in the same
/// place, at lower resolution. `None` if `rgba` is shorter than its
/// dimensions claim.
pub fn downscale_to_fit(rgba: &[u8], width: u32, height: u32, max_dim: u32) -> Option<(Vec<u8>, u32, u32)> {
    let needed = width as usize * height as usize * 4;
    if width == 0 || height == 0 || rgba.len() < needed {
        return None;
    }
    let (new_w, new_h) = fitted_size(width, height, max_dim);
    let mut out = Vec::with_capacity(new_w as usize * new_h as usize * 4);
    for y in 0..new_h {
        let src_y = (y as u64 * height as u64 / new_h as u64) as usize;
        for x in 0..new_w {
            let src_x = (x as u64 * width as u64 / new_w as u64) as usize;
            let at = (src_y * width as usize + src_x) * 4;
            out.extend_from_slice(&rgba[at..at + 4]);
        }
    }
    Some((out, new_w, new_h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textures_past_the_device_limit_or_empty_do_not_fit() {
        assert!(texture_fits(8192, 8192, 8192));
        assert!(!texture_fits(8193, 1, 8192), "the audit's case: a wide image under the decode cap");
        assert!(!texture_fits(1, 16384, 8192));
        assert!(!texture_fits(0, 10, 8192));
        assert!(!texture_fits(10, 0, 8192));
    }

    #[test]
    fn fitted_sizes_keep_aspect_and_never_exceed_the_limit() {
        assert_eq!(fitted_size(100, 50, 8192), (100, 50), "small images are untouched");
        assert_eq!(fitted_size(10_000, 10, 8192), (8192, 8));
        assert_eq!(fitted_size(16_384, 16_384, 8192), (8192, 8192));
        assert_eq!(fitted_size(100_000, 1, 8192), (8192, 1), "never rounds a side to zero");
        for (w, h) in [(8193, 8193), (u32::MAX, 3), (3, u32::MAX)] {
            let (fw, fh) = fitted_size(w, h, 2048);
            assert!(texture_fits(fw, fh, 2048), "{w}x{h} -> {fw}x{fh}");
        }
    }

    #[test]
    fn downscaling_samples_the_source_and_fits() {
        // 4x2: left half red, right half blue.
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        let mut src = Vec::new();
        for _ in 0..2 {
            src.extend(red.iter().chain(&red).chain(&blue).chain(&blue));
        }
        let (out, w, h) = downscale_to_fit(&src, 4, 2, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, [red, blue].concat());
    }

    #[test]
    fn downscaling_refuses_a_buffer_shorter_than_its_dimensions() {
        assert!(downscale_to_fit(&[0; 12], 2, 2, 1).is_none());
        assert!(downscale_to_fit(&[], 0, 0, 1).is_none());
    }
}
