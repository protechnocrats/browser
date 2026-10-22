//! A `Scene` is what actually crosses from the platform-agnostic side
//! into a `Renderer` implementation — a display list plus the
//! viewport it was built for. Kept deliberately small for Milestone 1;
//! later milestones extend it (compositing layers, scroll offsets)
//! rather than replacing it.

use crate::display_list::{DisplayItem, ImageResource};

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub viewport_width: f32,
    pub viewport_height: f32,
    /// Milestone 33: the canvas colour — opaque; what the renderer clears
    /// to before drawing `items`. The root element's background, or the
    /// body's when the root has none (CSS 2 §14.2), over white.
    pub background: style::Color,
    /// Shared (not copied) with whatever keeps the frame's items — the
    /// engine keeps them to measure the next frame's damage.
    pub items: std::sync::Arc<Vec<DisplayItem>>,
    pub glyph_atlas: GlyphAtlasSnapshot,
    pub images: Vec<ImageResource>,
    /// What changed since the previous frame of the same page (Milestone
    /// 31). A renderer may repaint just this; `Full` is always correct.
    pub damage: crate::display_list::Damage,
    /// Milestone 34: this frame's number, unique within the process
    /// ([`Scene::next_sequence`]).
    pub sequence: u64,
    /// The `sequence` of the frame `damage` is measured against; `None`
    /// with `Damage::Full`. A renderer whose target still holds that frame
    /// may redraw only the damage — any other frame in between (another
    /// page's, a dropped one) and it redraws everything.
    pub damage_base: Option<u64>,
}

impl Scene {
    /// A new frame number, never returned before in this process.
    pub fn next_sequence() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// A CPU-side copy of the glyph atlas's current state, for whatever
/// `Renderer` is consuming this `Scene` to upload into its own GPU
/// texture. `generation` lets a renderer skip re-uploading when
/// nothing changed since the last frame.
///
/// `pixels` shares the atlas's bands (see `text::AtlasPixels`) — O(1) per
/// frame, and a renderer can tell which bands changed by pointer.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphAtlasSnapshot {
    pub width: usize,
    pub height: usize,
    pub generation: u64,
    pub pixels: text::AtlasPixels,
}

impl GlyphAtlasSnapshot {
    pub fn from_atlas(atlas: &text::GlyphAtlas) -> Self {
        GlyphAtlasSnapshot {
            width: atlas.width(),
            height: atlas.height(),
            generation: atlas.generation(),
            pixels: atlas.pixels().clone(),
        }
    }
}
