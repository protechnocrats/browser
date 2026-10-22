//! The real-site conformance harness — Milestone 32.
//!
//! Real pages, recorded once ([`routes::RecordingRoute`]) into offline
//! snapshots ([`snapshot::Snapshot`]), then replayed through the engine
//! ([`routes::ReplayRoute`], [`runner::run`]) on a fixed clock: every run is
//! reproducible and needs no network, in CI included. Each page yields its
//! scene (JSON and a rasterised PNG), whether it crashed or timed out, and
//! the unsupported features it used; [`report::Report`] ranks those across
//! the corpus.

#![forbid(unsafe_code)]

pub mod report;
pub mod routes;
pub mod runner;
pub mod snapshot;

use std::path::{Path, PathBuf};

/// The corpus: `conformance/` at the workspace root.
pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance")
}

/// Milestone 35: the fixed font set pages are rendered with (`test-fonts/`),
/// so goldens do not depend on the machine's fonts. Scanned once.
pub fn test_fonts() -> std::sync::Arc<dyn text::FontSource> {
    static FONTS: std::sync::OnceLock<std::sync::Arc<text::DirectoryFonts>> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| std::sync::Arc::new(text::DirectoryFonts::scan(&[Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fonts")]))).clone()
}

/// Writes `image` as a PNG.
pub fn write_png(path: &Path, image: &engine::RasterImage) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&image.rgba).map_err(|e| e.to_string())
}
