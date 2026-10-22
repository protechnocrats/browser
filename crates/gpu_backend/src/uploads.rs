//! What to upload each frame — the decisions, apart from the GPU calls, so
//! they are tested without a GPU (performance audit).
//!
//! - The glyph atlas is uploaded by band: only bands whose pixels changed
//!   since this renderer last uploaded them (compared by pointer — the
//!   atlas copies a band when it writes to one a snapshot shares). It used
//!   to re-upload the whole atlas (up to 2 MiB) whenever any glyph was added.
//! - Per-frame instance data goes into buffers kept across frames, grown
//!   geometrically, instead of three new GPU buffers every frame.

use std::sync::Arc;
use text::AtlasPixels;

/// The atlas bands this renderer's texture holds, by pointer.
#[derive(Default)]
pub struct UploadedAtlas {
    bands: Vec<Option<Arc<Vec<u8>>>>,
}

impl UploadedAtlas {
    /// Forgets everything uploaded (the texture was recreated).
    pub fn reset(&mut self) {
        self.bands.clear();
    }

    /// The bands of `pixels` to upload — `(first row, rows, band)` — and
    /// records them as uploaded. Rows are clamped to the atlas's height.
    pub fn changed_bands(&mut self, pixels: &AtlasPixels) -> Vec<(u32, u32, Arc<Vec<u8>>)> {
        let mut out = Vec::new();
        for (i, (y, band)) in pixels.bands().enumerate() {
            if self.bands.len() <= i {
                self.bands.resize(i + 1, None);
            }
            if self.bands[i].as_ref().is_some_and(|b| Arc::ptr_eq(b, band)) {
                continue;
            }
            let rows = (pixels.height() - y).min(text::ATLAS_BAND_ROWS);
            out.push((y as u32, rows as u32, Arc::clone(band)));
            self.bands[i] = Some(Arc::clone(band));
        }
        self.bands.truncate(pixels.bands().count());
        out
    }
}

/// The capacity (bytes) an instance buffer of `capacity` needs to hold
/// `needed` bytes: `None` if it already does, else room to grow into (a
/// power of two, at least 4 KiB), so a page's buffers settle after a few
/// frames instead of being recreated as the item count wobbles.
pub fn grown_capacity(capacity: u64, needed: u64) -> Option<u64> {
    (needed > capacity).then(|| needed.max(4096).next_power_of_two())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas_with_rows(rows: usize) -> text::GlyphAtlas {
        let font = text::Font::dejavu_sans();
        let mut atlas = text::GlyphAtlas::new(128, 64);
        let mut size = 20.0;
        while atlas.height() < rows {
            for ch in 'A'..='Z' {
                atlas.get_or_insert(&font, ch, size);
            }
            size += 1.0;
        }
        atlas
    }

    #[test]
    fn only_changed_bands_are_uploaded_again() {
        let font = text::Font::dejavu_sans();
        let mut atlas = atlas_with_rows(3 * text::ATLAS_BAND_ROWS);
        let mut uploaded = UploadedAtlas::default();
        let first = uploaded.changed_bands(&atlas.pixels().clone());
        assert_eq!(first.len(), atlas.pixels().bands().count(), "the first frame uploads every band");
        assert!(uploaded.changed_bands(&atlas.pixels().clone()).is_empty(), "nothing changed: nothing uploaded");

        // A snapshot is alive (as the renderer's is) while a glyph is added.
        let held = atlas.pixels().clone();
        let entry = atlas.get_or_insert(&font, 'a', 11.0);
        let again = uploaded.changed_bands(&atlas.pixels().clone());
        assert_eq!(again.len(), 1, "one glyph, one band");
        let (y, rows, _) = &again[0];
        assert!(*y <= entry.atlas_y && entry.atlas_y + entry.atlas_h <= y + rows, "the band holding the glyph");
        drop(held);
    }

    #[test]
    fn a_skipped_scene_is_caught_up_and_a_cleared_atlas_is_re_uploaded() {
        let font = text::Font::dejavu_sans();
        let mut atlas = atlas_with_rows(2 * text::ATLAS_BAND_ROWS);
        let mut uploaded = UploadedAtlas::default();
        let _ = uploaded.changed_bands(&atlas.pixels().clone());
        // Two changes, the renderer sees only the second scene.
        let _older = atlas.pixels().clone();
        atlas.get_or_insert(&font, 'x', 12.0);
        let _newer = atlas.pixels().clone();
        atlas.get_or_insert(&font, 'y', 12.0);
        let caught_up = uploaded.changed_bands(&atlas.pixels().clone());
        assert!(!caught_up.is_empty());
        assert!(uploaded.changed_bands(&atlas.pixels().clone()).is_empty());

        // A cleared (smaller) atlas: every band is new.
        let fresh = text::GlyphAtlas::new(128, 64);
        let bands = uploaded.changed_bands(&fresh.pixels().clone());
        assert_eq!(bands.len(), 1);
        assert_eq!(bands[0].1, 64, "rows clamped to the atlas height");
    }

    #[test]
    fn instance_buffers_grow_geometrically_and_are_otherwise_reused() {
        assert_eq!(grown_capacity(0, 10), Some(4096));
        assert_eq!(grown_capacity(4096, 4096), None);
        assert_eq!(grown_capacity(4096, 5000), Some(8192));
        assert_eq!(grown_capacity(1 << 20, 3), None, "shrinking reuses the buffer");
    }
}
