//! Image decoding — Milestone 7. Pure decode + a small caching layer,
//! platform-agnostic: this crate never touches a GPU, it only ever
//! produces plain RGBA8 byte buffers. Uploading those into a GPU
//! texture is `gpu_backend`'s job, same separation `text`'s
//! `GlyphAtlas` already established for glyphs.
//!
//! Milestone 7 scope: PNG and JPEG, decoded from local files. Phase E
//! (deferred-items plan) adds **GIF** (via `gif`) and **WebP** (via
//! `image-webp`) — both pure-Rust decoders, no exception to the
//! `#![forbid(unsafe_code)]` policy. For an animated GIF/WebP only the
//! FIRST frame is decoded (real animation timing is deferred — see
//! Phase E (`milestones/milestones-01-17.md`)). AVIF is still out of scope.

use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Row-major, top-to-bottom, RGBA8 — 4 bytes per pixel, always,
    /// regardless of the source format's actual channel layout
    /// (grayscale, indexed, no-alpha RGB, etc. are all normalized to
    /// this on decode, once, here — not left for `gpu_backend` to
    /// puzzle out per format).
    ///
    /// `Arc`-wrapped (not a plain `Vec<u8>`) specifically so
    /// `rendering::display_list` can hand the display list an O(1)
    /// `Arc::clone` of these bytes every frame instead of deep-copying
    /// a potentially multi-MB decoded image on every single redraw —
    /// this crate's own `ImageCache` already returns the SAME
    /// `Arc<DecodedImage>` for a given path across frames, so sharing
    /// the byte buffer this way costs nothing extra here and removes a
    /// real per-frame allocation downstream. Every read site
    /// (`.len()`, indexing, `.iter()`, `.as_slice()`) works unchanged
    /// through `Arc<Vec<u8>>`'s `Deref`.
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Debug)]
pub enum DecodeError {
    Png(String),
    Jpeg(String),
    Gif(String),
    Webp(String),
    UnrecognizedFormat,
    Io(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Png(e) => write!(f, "PNG decode error: {e}"),
            DecodeError::Jpeg(e) => write!(f, "JPEG decode error: {e}"),
            DecodeError::Gif(e) => write!(f, "GIF decode error: {e}"),
            DecodeError::Webp(e) => write!(f, "WebP decode error: {e}"),
            DecodeError::UnrecognizedFormat => write!(f, "unrecognized image format (not PNG/JPEG/GIF/WebP)"),
            DecodeError::Io(e) => write!(f, "I/O error reading image: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub fn decode_png(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    let mut decoder = png::Decoder::new(bytes);
    // Normalizes palette/grayscale/16-bit down to plain 8-bit
    // grayscale or RGB(A), and expands tRNS chunks into a real alpha
    // channel — everything this crate's own RGBA8 normalization below
    // still needs to handle is just "how many channels, in what order".
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| DecodeError::Png(e.to_string()))?;
    // `png`'s own `Limits` bound only ITS allocations, not the output
    // buffer we allocate below from the header's declared size.
    let (width, height) = reader.info().size();
    check_decode_dimensions(width, height, DecodeError::Png)?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| DecodeError::Png(e.to_string()))?;
    let bytes = &buf[..info.buffer_size()];

    let rgba = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => expand_channels(bytes, 3, |px| [px[0], px[1], px[2], 255]),
        png::ColorType::Grayscale => expand_channels(bytes, 1, |px| [px[0], px[0], px[0], 255]),
        png::ColorType::GrayscaleAlpha => expand_channels(bytes, 2, |px| [px[0], px[0], px[0], px[1]]),
        png::ColorType::Indexed => {
            // `normalize_to_color8`'s EXPAND flag should have already
            // converted indexed images to RGB(A) — reaching this arm
            // would mean that didn't happen for some reason. Treat as
            // a decode error rather than silently misinterpreting
            // palette indices as color channels.
            return Err(DecodeError::Png("indexed color type survived EXPAND transform".into()));
        }
    };

    Ok(DecodedImage { width: info.width, height: info.height, rgba: Arc::new(rgba) })
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(bytes, options);
    let rgba = decoder.decode().map_err(|e| DecodeError::Jpeg(e.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| DecodeError::Jpeg("decoded successfully but no header info available".into()))?;

    Ok(DecodedImage { width: info.width as u32, height: info.height as u32, rgba: Arc::new(rgba) })
}

/// Phase E: decodes the FIRST frame of a GIF to RGBA8. Animated GIFs
/// decode only their first frame (real animation timing is deferred —
/// see Phase E (`milestones/milestones-01-17.md`)). The `gif` crate already hands us
/// indexed→RGBA expansion when `set_color_output(RGBA)` is set.
pub fn decode_gif(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(bytes).map_err(|e| DecodeError::Gif(e.to_string()))?;
    let frame = decoder
        .read_next_frame()
        .map_err(|e| DecodeError::Gif(e.to_string()))?
        .ok_or_else(|| DecodeError::Gif("GIF has no frames".into()))?;
    // A first frame smaller than the logical screen is possible; use
    // the frame's own dimensions (the common case is that they match).
    Ok(DecodedImage { width: frame.width as u32, height: frame.height as u32, rgba: Arc::new(frame.buffer.to_vec()) })
}

/// Cap on decoded pixel count, shared by every format that doesn't
/// already enforce one of its own (`zune-jpeg`'s 16384x16384 default max
/// dimensions, `gif`'s 50MB memory limit). PNG needs it too: `png`'s
/// `Limits::bytes` does not cover the caller's output buffer. Without
/// a cap, a PNG or WebP header can declare an enormous canvas in a
/// few-byte file and force a multi-gigabyte allocation before any real
/// frame data is even validated — a classic decompression-bomb DoS.
const MAX_DECODED_PIXELS: u64 = 16384 * 16384;

/// Refuses an image whose DECLARED pixel count exceeds
/// `MAX_DECODED_PIXELS`, before any pixel buffer is allocated — reporting
/// the `ImageDimensions` guard. Split out of `decode_webp` so the cap is
/// testable directly — and `err` names the format being decoded:
/// `image-webp` rejects a bare VP8X header bomb in its
/// own header validation before this is ever reached, so only a
/// well-formed animated WebP with a huge canvas gets here.
fn check_decode_dimensions(width: u32, height: u32, err: fn(String) -> DecodeError) -> Result<(), DecodeError> {
    if (width as u64) * (height as u64) > MAX_DECODED_PIXELS {
        diagnostics::guard(diagnostics::Guard::ImageDimensions, "declared pixel count exceeds the decode cap");
        return Err(err(format!("image dimensions {width}x{height} exceed the decode size limit")));
    }
    Ok(())
}

/// Phase E: decodes a still WebP (or the first frame of an animated
/// one) to RGBA8 via the pure-Rust `image-webp` decoder.
pub fn decode_webp(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|e| DecodeError::Webp(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    check_decode_dimensions(width, height, DecodeError::Webp)?;
    // Belt-and-suspenders: also cap the decoder's own internal
    // allocations (ICCP/EXIF/XMP chunks, its internal working buffers),
    // independent of the dimension check above.
    decoder.set_memory_limit(MAX_DECODED_PIXELS as usize * 4);
    let has_alpha = decoder.has_alpha();
    let mut rgba;
    if has_alpha {
        rgba = vec![0u8; width as usize * height as usize * 4];
        decoder.read_image(&mut rgba).map_err(|e| DecodeError::Webp(e.to_string()))?;
    } else {
        let mut rgb = vec![0u8; width as usize * height as usize * 3];
        decoder.read_image(&mut rgb).map_err(|e| DecodeError::Webp(e.to_string()))?;
        rgba = expand_channels(&rgb, 3, |px| [px[0], px[1], px[2], 255]);
    }
    Ok(DecodedImage { width, height, rgba: Arc::new(rgba) })
}

/// Sniffs the format from magic bytes and dispatches. Each supported
/// format has an unambiguous fixed byte signature, so this is exact,
/// not a guess.
pub fn decode_auto(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(&PNG_MAGIC) {
        return decode_png(bytes);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        return decode_jpeg(bytes);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return decode_gif(bytes);
    }
    // RIFF container with a "WEBP" fourCC at offset 8.
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return decode_webp(bytes);
    }
    Err(DecodeError::UnrecognizedFormat)
}

fn expand_channels(src: &[u8], src_channels: usize, expand: impl Fn(&[u8]) -> [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity((src.len() / src_channels) * 4);
    for px in src.chunks_exact(src_channels) {
        out.extend_from_slice(&expand(px));
    }
    out
}

/// Whether an [`ImageCache`] may read the local filesystem.
///
/// **Capability-based (architecture doc §1):** reading a file is
/// authority, and image sources are attacker-controlled text — an inline
/// `style="background-image:url(/some/path)"`, or a script-set `img.src`,
/// used to make the engine read (and decode) any local file the browser
/// user could, and `url(/dev/zero)` would read until memory ran out. A
/// cache therefore has NO file access unless whoever built it explicitly
/// grants it. Everything content-driven (`app`) uses [`FileAccess::Denied`];
/// images arrive as bytes the network layer fetched, keyed by URL, via
/// [`ImageCache::load_from_bytes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileAccess {
    /// Never touch the filesystem. The default.
    #[default]
    Denied,
    /// Read regular files (only), up to [`MAX_IMAGE_FILE_BYTES`]. For
    /// tests and trusted local tooling — never for a cache whose image
    /// sources come from web content.
    LocalFiles,
}

/// Largest image file [`FileAccess::LocalFiles`] will read.
pub const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Decode-once, reuse-many-times cache, keyed by source path.
/// `decode_count` exists so tests (and, later, real diagnostics) can
/// verify caching is actually happening rather than silently
/// re-decoding every call.
#[derive(Default)]
pub struct ImageCache {
    cache: HashMap<String, Arc<DecodedImage>>,
    decode_count: usize,
    file_access: FileAccess,
    /// Sources already reported as denied, so a broken image reports
    /// once, not once per frame.
    denied_reported: std::collections::HashSet<String>,
    /// Keys whose bytes are on their way over the network (progressive
    /// image loading): not decoded yet, and never looked for on disk.
    pending: std::collections::HashSet<String>,
}

/// Reads one image file safely: regular files only (a device like
/// `/dev/zero`, a FIFO or a directory is refused, not read), and never
/// more than [`MAX_IMAGE_FILE_BYTES`].
fn read_image_file(path: &str) -> Result<Vec<u8>, DecodeError> {
    use std::io::Read;
    let io = |e: std::io::Error| DecodeError::Io(e.to_string());
    let before = std::fs::metadata(path).map_err(io)?;
    if !before.is_file() {
        return Err(DecodeError::Io("not a regular file".to_string()));
    }
    let file = std::fs::File::open(path).map_err(io)?;
    // Re-check on the OPEN handle: the path may have been swapped since.
    if !file.metadata().map_err(io)?.is_file() {
        return Err(DecodeError::Io("not a regular file".to_string()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_FILE_BYTES + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return Err(DecodeError::Io(format!("image file exceeds the {MAX_IMAGE_FILE_BYTES}-byte limit")));
    }
    Ok(bytes)
}

impl ImageCache {
    /// A cache with NO filesystem access — see [`FileAccess`].
    pub fn new() -> Self {
        ImageCache::default()
    }

    /// A cache that may read local image files. Tests and trusted local
    /// tooling only; see [`FileAccess::LocalFiles`].
    pub fn with_local_file_access() -> Self {
        ImageCache { file_access: FileAccess::LocalFiles, ..ImageCache::default() }
    }

    pub fn file_access(&self) -> FileAccess {
        self.file_access
    }

    /// Reports (once per source) that content asked for a local file
    /// this cache will not read.
    fn deny(&mut self, path: &str) -> DecodeError {
        if self.denied_reported.insert(path.to_string()) {
            diagnostics::guard(diagnostics::Guard::FileAccessDenied, "content requested a local file; the image cache has no file access");
        }
        DecodeError::Io("local file access is not permitted".to_string())
    }

    pub fn decode_count(&self) -> usize {
        self.decode_count
    }

    /// Loads and decodes an image from a local file path, or returns
    /// the already-decoded, cached result if this exact path was
    /// loaded before. Network paths are explicitly out of scope this
    /// milestone (Milestone 8) — this always reads from the local
    /// filesystem.
    pub fn load_from_file(&mut self, path: &str) -> Result<Arc<DecodedImage>, DecodeError> {
        if let Some(cached) = self.cache.get(path) {
            diagnostics::metrics::IMAGE_CACHE_HITS.inc();
            return Ok(Arc::clone(cached));
        }
        diagnostics::metrics::IMAGE_CACHE_MISSES.inc();

        if self.file_access == FileAccess::Denied {
            return Err(self.deny(path));
        }
        let bytes = read_image_file(path)?;
        let decoded = decode_auto(&bytes)?;
        self.decode_count += 1;

        let arc = Arc::new(decoded);
        self.cache.insert(path.to_string(), Arc::clone(&arc));
        Ok(arc)
    }

    /// Decodes and caches already-in-memory bytes under an arbitrary
    /// string key — e.g. a network URL once Milestone 8's fetch path
    /// has the bytes in hand. Same cache, same dedup behavior as
    /// `load_from_file`; the two differ only in where the bytes come
    /// from, which is exactly the point: decoding/caching shouldn't
    /// need to know or care whether an image came from disk or the
    /// network.
    pub fn load_from_bytes(&mut self, key: &str, bytes: &[u8]) -> Result<Arc<DecodedImage>, DecodeError> {
        if let Some(cached) = self.cache.get(key) {
            diagnostics::metrics::IMAGE_CACHE_HITS.inc();
            return Ok(Arc::clone(cached));
        }
        diagnostics::metrics::IMAGE_CACHE_MISSES.inc();

        let decoded = decode_auto(bytes)?;
        self.decode_count += 1;

        let arc = Arc::new(decoded);
        self.cache.insert(key.to_string(), Arc::clone(&arc));
        Ok(arc)
    }

    /// Inserts an image decoded elsewhere (in parallel, off this cache) under
    /// `key`, and ends `key`'s pending state.
    pub fn insert_decoded(&mut self, key: &str, image: DecodedImage) -> Arc<DecodedImage> {
        self.pending.remove(key);
        self.decode_count += 1;
        let arc = Arc::new(image);
        self.cache.insert(key.to_string(), Arc::clone(&arc));
        arc
    }

    /// Marks `key` as arriving later over the network: until
    /// [`ImageCache::finish_pending`] or [`ImageCache::insert_decoded`],
    /// preloading skips it rather than treating it as a local file.
    pub fn mark_pending(&mut self, key: &str) {
        if !self.cache.contains_key(key) {
            self.pending.insert(key.to_string());
        }
    }

    /// `key` will not arrive (the fetch failed, or its bytes did not decode).
    pub fn finish_pending(&mut self, key: &str) {
        self.pending.remove(key);
    }

    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.contains(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.cache.contains_key(key)
    }

    /// The already-decoded image for `key`, if it's in the cache.
    /// Never triggers a decode or filesystem access — a pure lookup,
    /// for read-only consumers like `layout`'s parallel intrinsic-size
    /// measurement pass (which runs after `preload_parallel` has
    /// warmed the cache and cannot take `&mut self`).
    pub fn get(&self, key: &str) -> Option<Arc<DecodedImage>> {
        let found = self.cache.get(key).map(Arc::clone);
        if found.is_some() {
            diagnostics::metrics::IMAGE_CACHE_HITS.inc();
        } else {
            diagnostics::metrics::IMAGE_CACHE_MISSES.inc();
        }
        found
    }

    /// Concurrency work (post-Milestone-17 audit): decodes a whole
    /// BATCH of local-file images in parallel across CPU cores via
    /// `rayon`, before layout ever walks the tree — real multi-core
    /// parallelism for the case that used to be the single biggest
    /// unparallelized decode cost in the pipeline (`layout` used to
    /// call `load_from_file` one image at a time, inline, exactly
    /// when its tree walk reached each `<img>`/`background-image`).
    /// See `layout::layout_document`'s doc comment for how the caller
    /// collects the path list this consumes.
    ///
    /// Paths already cached are skipped (no redundant re-decode, same
    /// dedup guarantee `load_from_file` gives one path at a time).
    /// A path that fails to read or decode is silently dropped — same
    /// graceful-degradation policy as `load_from_file`: the caller's
    /// SUBSEQUENT normal `load_from_file` call for that same path
    /// (during the real layout walk) will attempt it again, fail
    /// again, and degrade to "nothing painted there" exactly as if
    /// this preload pass had never run at all. This method never
    /// returns a per-path error precisely because there's nothing
    /// useful to do with one mid-batch — every failure just falls
    /// through to that same existing single-path behavior.
    pub fn preload_parallel(&mut self, paths: &[String]) {
        let to_decode: Vec<&String> = {
            let mut seen = std::collections::HashSet::new();
            // A pending key is arriving over the network: not a local file.
            paths
                .iter()
                .filter(|p| !self.cache.contains_key(p.as_str()) && !self.pending.contains(p.as_str()) && seen.insert(p.as_str()))
                .collect()
        };
        if self.file_access == FileAccess::Denied {
            // Nothing here may be read from disk: report and stop. (The
            // layout walk that follows finds the same misses and degrades
            // to "nothing painted there".)
            for path in to_decode {
                let _ = self.deny(path);
            }
            return;
        }

        let decoded: Vec<(String, DecodedImage)> = to_decode
            .par_iter()
            .filter_map(|path| {
                let bytes = read_image_file(path.as_str()).ok()?;
                let image = decode_auto(&bytes).ok()?;
                Some((path.to_string(), image))
            })
            .collect();

        for (path, image) in decoded {
            self.decode_count += 1;
            self.cache.insert(path, Arc::new(image));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> String {
        format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
    }

    fn read_fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("failed to read fixture {name}: {e}"))
    }

    #[test]
    fn decodes_png_with_exact_pixel_values() {
        let bytes = read_fixture("quadrants.png");
        let img = decode_png(&bytes).expect("should decode");
        assert_eq!(img.width, 8);
        assert_eq!(img.height, 4);
        assert_eq!(img.rgba.len(), 8 * 4 * 4);

        let pixel_at = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * img.width + x) * 4) as usize;
            [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
        };
        assert_eq!(pixel_at(0, 0), [255, 0, 0, 255]); // red quadrant
        assert_eq!(pixel_at(5, 0), [0, 255, 0, 255]); // green quadrant
        assert_eq!(pixel_at(0, 3), [0, 0, 255, 255]); // blue quadrant
        assert_eq!(pixel_at(5, 3), [255, 255, 255, 255]); // white quadrant
    }

    #[test]
    fn decodes_png_alpha_transparency() {
        let bytes = read_fixture("transparent.png");
        let img = decode_png(&bytes).expect("should decode");
        let pixel_at = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * img.width + x) * 4) as usize;
            [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
        };
        assert_eq!(pixel_at(0, 0), [255, 0, 0, 255]); // opaque top half
        assert_eq!(pixel_at(0, 3)[3], 0); // transparent bottom half
    }

    #[test]
    fn decodes_jpeg_and_produces_rgba() {
        let bytes = read_fixture("solid.jpg");
        let img = decode_jpeg(&bytes).expect("should decode");
        assert_eq!(img.width, 16);
        assert_eq!(img.height, 16);
        assert_eq!(img.rgba.len(), 16 * 16 * 4);

        // JPEG is lossy — check the decoded color is CLOSE to the
        // source (128, 96, 200), not byte-exact.
        let i = 0;
        let (r, g, b, a) = (img.rgba[i] as i32, img.rgba[i + 1] as i32, img.rgba[i + 2] as i32, img.rgba[i + 3]);
        assert!((r - 128).abs() < 20, "red channel too far off: {r}");
        assert!((g - 96).abs() < 20, "green channel too far off: {g}");
        assert!((b - 200).abs() < 20, "blue channel too far off: {b}");
        assert_eq!(a, 255); // JPEG has no alpha — must be fully opaque
    }

    #[test]
    fn decodes_jpeg_gradient_fixture() {
        let bytes = read_fixture("gradient.jpg");
        let img = decode_jpeg(&bytes).expect("should decode");
        assert_eq!(img.width, 32);
        assert_eq!(img.height, 24);
        // Just a sanity check that decoding produced varied output,
        // not e.g. an all-zero buffer from a silent failure.
        let distinct_values: std::collections::HashSet<u8> = img.rgba.iter().copied().collect();
        assert!(distinct_values.len() > 10);
    }

    #[test]
    fn decode_auto_dispatches_by_magic_bytes_not_extension() {
        let png_bytes = read_fixture("quadrants.png");
        let jpeg_bytes = read_fixture("solid.jpg");
        assert!(decode_auto(&png_bytes).is_ok());
        assert!(decode_auto(&jpeg_bytes).is_ok());
    }

    // ---- Phase E: GIF + WebP ----

    #[test]
    fn decodes_gif_first_frame_to_rgba() {
        let bytes = read_fixture("solid.gif");
        let img = decode_gif(&bytes).expect("gif should decode");
        assert_eq!((img.width, img.height), (8, 8));
        assert_eq!(img.rgba.len(), 8 * 8 * 4);
        assert_eq!(&img.rgba[0..4], &[200, 40, 90, 255]);
    }

    /// Performance regression: cloning a `DecodedImage` (what
    /// `ImageCache::get`-style lookups and `rendering::display_list`
    /// both do every frame) must be an O(1) `Arc::clone` of the pixel
    /// buffer, not a deep copy — see `DecodedImage::rgba`'s doc
    /// comment for why this field is `Arc<Vec<u8>>` rather than a
    /// plain `Vec<u8>`.
    #[test]
    fn cloning_a_decoded_image_shares_the_same_pixel_buffer_allocation() {
        let bytes = read_fixture("solid.webp");
        let img = decode_webp(&bytes).expect("webp should decode");
        let cloned = img.clone();
        assert!(std::sync::Arc::ptr_eq(&img.rgba, &cloned.rgba), "cloning a DecodedImage must not deep-copy its pixel buffer");
    }

    #[test]
    fn decodes_webp_rgb() {
        let bytes = read_fixture("solid.webp");
        let img = decode_webp(&bytes).expect("webp should decode");
        assert_eq!((img.width, img.height), (8, 8));
        assert_eq!(img.rgba.len(), 8 * 8 * 4);
        let (r, g, b, a) = (img.rgba[0], img.rgba[1], img.rgba[2], img.rgba[3]);
        assert!((r as i32 - 200).abs() < 4 && (g as i32 - 40).abs() < 4 && (b as i32 - 90).abs() < 4);
        assert_eq!(a, 255);
    }

    #[test]
    fn decodes_webp_with_alpha() {
        let bytes = read_fixture("alpha.webp");
        let img = decode_webp(&bytes).expect("alpha webp should decode");
        assert_eq!((img.width, img.height), (4, 4));
        assert_eq!(img.rgba[3], 128, "alpha channel preserved");
    }

    #[test]
    fn decode_auto_handles_gif_and_webp_by_magic_bytes() {
        assert!(decode_auto(&read_fixture("solid.gif")).is_ok());
        assert!(decode_auto(&read_fixture("solid.webp")).is_ok());
        assert!(decode_auto(&read_fixture("alpha.webp")).is_ok());
    }

    #[test]
    fn gif_and_webp_decoders_error_not_panic_on_garbage() {
        assert!(decode_gif(&[0u8; 64]).is_err());
        assert!(decode_webp(&[0u8; 64]).is_err());
        assert!(decode_gif(&[]).is_err());
        assert!(decode_webp(&[]).is_err());
    }

    #[test]
    fn decode_auto_rejects_unrecognized_bytes_without_panicking() {
        let garbage = vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let result = decode_auto(&garbage);
        assert!(matches!(result, Err(DecodeError::UnrecognizedFormat)));
    }

    #[test]
    fn decode_png_on_non_png_bytes_errors_not_panics() {
        let garbage = vec![0u8; 100];
        assert!(decode_png(&garbage).is_err());
    }

    /// Regression (fuzz target `decode_auto`): a well-formed PNG (valid
    /// CRCs, real IDAT) whose IHDR declares 3 x 4294443012 pixels made us
    /// allocate a ~51 GB output buffer — `png`'s own limits do not cover
    /// it. The pixel cap refuses it first, as a PNG error.
    #[test]
    fn a_png_declaring_billions_of_pixels_is_refused_before_allocating() {
        use diagnostics::{collector::capture, Guard};
        let bomb = read_fixture("png_header_bomb_valid_crc.png");
        let (result, c) = capture(|| decode_png(&bomb));
        assert!(matches!(result, Err(DecodeError::Png(_))), "{result:?}");
        assert!(c.saw_guard(Guard::ImageDimensions), "{result:?} {:?}", c.events);
    }

    #[test]
    fn decode_jpeg_on_non_jpeg_bytes_errors_not_panics() {
        let garbage = vec![0u8; 100];
        assert!(decode_jpeg(&garbage).is_err());
    }

    #[test]
    fn empty_bytes_do_not_panic_any_decoder() {
        assert!(decode_png(&[]).is_err());
        assert!(decode_jpeg(&[]).is_err());
        assert!(decode_auto(&[]).is_err());
    }

    #[test]
    fn cache_decodes_once_and_reuses_on_repeated_loads() {
        let mut cache = ImageCache::with_local_file_access();
        let path = fixture_path("quadrants.png");

        let first = cache.load_from_file(&path).expect("first load should succeed");
        assert_eq!(cache.decode_count(), 1);

        let second = cache.load_from_file(&path).expect("second load should hit cache");
        assert_eq!(cache.decode_count(), 1, "must not re-decode on a cache hit");

        // Same underlying data (Arc::ptr_eq confirms it's literally
        // the same allocation, not just equal-by-value).
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn cache_distinguishes_different_paths() {
        let mut cache = ImageCache::with_local_file_access();
        cache.load_from_file(&fixture_path("quadrants.png")).unwrap();
        cache.load_from_file(&fixture_path("solid.jpg")).unwrap();
        assert_eq!(cache.decode_count(), 2);
    }

    #[test]
    fn missing_file_errors_gracefully() {
        let mut cache = ImageCache::with_local_file_access();
        let result = cache.load_from_file("/nonexistent/path/does_not_exist.png");
        assert!(matches!(result, Err(DecodeError::Io(_))));
    }

    #[test]
    fn load_from_bytes_decodes_and_caches_by_key() {
        let bytes = read_fixture("quadrants.png");
        let mut cache = ImageCache::with_local_file_access();
        let first = cache.load_from_bytes("https://example.com/img.png", &bytes).expect("should decode");
        assert_eq!(cache.decode_count(), 1);
        let second = cache.load_from_bytes("https://example.com/img.png", &bytes).expect("cache hit");
        assert_eq!(cache.decode_count(), 1, "must not re-decode on a cache hit");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn load_from_bytes_and_load_from_file_share_one_cache_keyed_by_string() {
        let path = fixture_path("quadrants.png");
        let bytes = read_fixture("quadrants.png");
        let mut cache = ImageCache::with_local_file_access();
        cache.load_from_file(&path).unwrap();
        // Different key (the raw bytes call uses a URL-shaped key, not
        // the file path) — correctly treated as a distinct cache entry.
        cache.load_from_bytes("https://example.com/img.png", &bytes).unwrap();
        assert_eq!(cache.decode_count(), 2);
    }

    // ---- Concurrency work: preload_parallel ----

    #[test]
    fn preload_parallel_decodes_a_batch_and_populates_the_cache() {
        let mut cache = ImageCache::with_local_file_access();
        let paths = vec![fixture_path("quadrants.png"), fixture_path("solid.jpg")];
        cache.preload_parallel(&paths);

        assert_eq!(cache.decode_count(), 2);
        assert!(cache.contains(&paths[0]));
        assert!(cache.contains(&paths[1]));
    }

    #[test]
    fn preload_parallel_then_load_from_file_is_a_cache_hit_not_a_redecode() {
        let mut cache = ImageCache::with_local_file_access();
        let path = fixture_path("quadrants.png");
        cache.preload_parallel(&[path.clone()]);
        assert_eq!(cache.decode_count(), 1);

        cache.load_from_file(&path).unwrap();
        assert_eq!(cache.decode_count(), 1, "should not have re-decoded an already-preloaded path");
    }

    #[test]
    fn preload_parallel_skips_already_cached_paths() {
        let mut cache = ImageCache::with_local_file_access();
        let path = fixture_path("quadrants.png");
        cache.load_from_file(&path).unwrap();
        assert_eq!(cache.decode_count(), 1);

        cache.preload_parallel(&[path]);
        assert_eq!(cache.decode_count(), 1);
    }

    #[test]
    fn preload_parallel_deduplicates_repeated_paths_in_the_same_batch() {
        let mut cache = ImageCache::with_local_file_access();
        let path = fixture_path("quadrants.png");
        cache.preload_parallel(&[path.clone(), path.clone(), path]);
        assert_eq!(cache.decode_count(), 1, "the same path listed 3 times should only decode once");
    }

    #[test]
    fn preload_parallel_with_a_broken_path_does_not_panic_and_still_decodes_the_rest() {
        let mut cache = ImageCache::with_local_file_access();
        let good_path = fixture_path("quadrants.png");
        let paths = vec!["/nonexistent/broken.png".to_string(), good_path.clone()];
        cache.preload_parallel(&paths);

        assert_eq!(cache.decode_count(), 1, "only the real file should have decoded");
        assert!(cache.contains(&good_path));
        assert!(!cache.contains("/nonexistent/broken.png"));
    }

    #[test]
    fn preload_parallel_on_an_empty_batch_does_not_panic() {
        let mut cache = ImageCache::with_local_file_access();
        cache.preload_parallel(&[]);
        assert_eq!(cache.decode_count(), 0);
    }

    #[test]
    fn preload_parallel_result_matches_sequential_load_for_the_same_images() {
        let path = fixture_path("quadrants.png");

        let mut sequential_cache = ImageCache::with_local_file_access();
        let sequential_result = sequential_cache.load_from_file(&path).unwrap();

        let mut parallel_cache = ImageCache::with_local_file_access();
        parallel_cache.preload_parallel(&[path.clone()]);
        let parallel_result = parallel_cache.load_from_file(&path).unwrap();

        assert_eq!(sequential_result.width, parallel_result.width);
        assert_eq!(sequential_result.height, parallel_result.height);
        assert_eq!(sequential_result.rgba, parallel_result.rgba);
    }

    // ---- Milestone 27: every silent cap is observable ----

    /// A WebP whose VP8X chunk declares a 2^24 x 2^24 canvas with no
    /// pixel data — the "header bomb" the decode cap exists for.
    fn webp_header_bomb() -> Vec<u8> {
        let (w, h) = (1u32 << 24, 1u32 << 24);
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"VP8X");
        chunk.extend_from_slice(&10u32.to_le_bytes());
        chunk.extend_from_slice(&[0, 0, 0, 0]);
        chunk.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        chunk.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&((4 + chunk.len()) as u32).to_le_bytes());
        file.extend_from_slice(b"WEBP");
        file.extend_from_slice(&chunk);
        file
    }

    #[test]
    fn a_webp_header_bomb_is_refused_without_allocating_or_panicking() {
        // (Rejected by `image-webp`'s own header validation, before the
        // dimension cap below — either refusal is correct.)
        assert!(decode_webp(&webp_header_bomb()).is_err());
        assert!(decode_auto(&webp_header_bomb()).is_err());
    }

    #[test]
    fn the_pixel_cap_refuses_and_reports_the_dimension_guard() {
        use diagnostics::{collector::capture, Guard};
        let (result, c) = capture(|| check_decode_dimensions(1 << 24, 1 << 24, DecodeError::Webp));
        assert!(result.is_err());
        assert!(c.saw_guard(Guard::ImageDimensions), "{:?}", c.events);

        // At the cap exactly is fine, and trips nothing.
        let (result, c) = capture(|| check_decode_dimensions(16384, 16384, DecodeError::Webp));
        assert!(result.is_ok());
        assert!(c.events.is_empty());
        // One pixel over trips it (and the product cannot overflow u32 math).
        assert!(check_decode_dimensions(16385, 16384, DecodeError::Webp).is_err());
        assert!(check_decode_dimensions(u32::MAX, u32::MAX, DecodeError::Webp).is_err());
    }

    #[test]
    fn image_cache_hits_and_misses_are_counted() {
        let before = diagnostics::metrics::snapshot();
        let mut cache = ImageCache::with_local_file_access();
        let bytes = read_fixture("quadrants.png");
        cache.load_from_bytes("k", &bytes).unwrap(); // miss
        cache.load_from_bytes("k", &bytes).unwrap(); // hit
        let _ = cache.get("k"); // hit
        let _ = cache.get("absent"); // miss
        let after = diagnostics::metrics::snapshot();
        assert!(after.delta(&before, "image_cache_hits") >= 2);
        assert!(after.delta(&before, "image_cache_misses") >= 2);
    }

    // ---- Hardening pass: content can never make the engine read files ----

    #[test]
    fn a_default_cache_refuses_local_files_and_reports_once_per_path() {
        use diagnostics::{collector::capture, Guard};
        let png = fixture_path("quadrants.png");
        let mut cache = ImageCache::new();
        assert_eq!(cache.file_access(), FileAccess::Denied, "no file access unless explicitly granted");

        let (results, c) = capture(|| (cache.load_from_file(&png), cache.load_from_file(&png), cache.load_from_file("/etc/hostname")));
        assert!(results.0.is_err() && results.1.is_err() && results.2.is_err());
        assert_eq!(cache.decode_count(), 0, "nothing was read or decoded");
        assert_eq!(c.guard_events(Guard::FileAccessDenied).len(), 2, "one event per distinct path, not per call: {:?}", c.events);
    }

    #[test]
    fn preloading_never_touches_the_filesystem_when_denied() {
        let png = fixture_path("quadrants.png");
        let mut cache = ImageCache::new();
        cache.preload_parallel(&[png.clone(), png]);
        assert_eq!(cache.decode_count(), 0);
    }

    #[test]
    fn images_arriving_as_bytes_still_work_with_no_file_access() {
        // The content-driven path: the network layer fetched bytes; the
        // cache never needs a filesystem.
        let mut cache = ImageCache::new();
        let bytes = read_fixture("quadrants.png");
        cache.load_from_bytes("https://example.com/q.png", &bytes).unwrap();
        assert!(cache.load_from_file("https://example.com/q.png").is_ok(), "a URL key already in the cache is a plain cache hit");
    }

    #[test]
    fn even_with_file_access_devices_directories_and_oversized_files_are_refused() {
        let mut cache = ImageCache::with_local_file_access();
        // Reading this would never terminate / would allocate without bound.
        for special in ["/dev/zero", "/dev/urandom", "/dev/null", "/tmp", "/proc/self/environ"] {
            if std::path::Path::new(special).exists() {
                assert!(cache.load_from_file(special).is_err(), "{special} must be refused, not read");
            }
        }

        // A regular file over the cap is refused (sparse, so cheap to make).
        let dir = std::env::temp_dir().join(format!("images-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.png");
        std::fs::File::create(&big).unwrap().set_len(MAX_IMAGE_FILE_BYTES + 1).unwrap();
        let err = cache.load_from_file(big.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, DecodeError::Io(m) if m.contains("limit")), "over-cap file refused");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
