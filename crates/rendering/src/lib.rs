//! Renderer-agnostic rendering types: display list, scene, and the
//! `Renderer` trait a GPU (or, for tests, null) backend implements.
//!
//! This crate is the seam from the "freeze the dependency direction"
//! decision: it depends on `layout`, never on `wgpu`/`winit`. A
//! concrete backend (`gpu_backend`) depends on THIS crate and on
//! platform, implementing `Renderer` — so `layout`/`paint`-side code
//! never has to change if the GPU backend does, and a `NullRenderer`
//! (below) can stand in for GPU-less environments: unit tests, CI,
//! and future headless/golden-image testing.

pub mod affine;
pub mod batches;
pub mod display_list;
pub mod page_list;
pub mod scene;
pub mod raster;
pub mod snapshot;
pub mod walk;

pub use batches::{plan_draw_batches, plan_draw_passes, BatchKind, DrawBatch, PassOp};
pub use display_list::{build_display_list_for_frame, build_display_list_with_scrolls, cull_to_viewport, effective_bounds, damage_between, item_bounds, outline_items, update_scroll_layers, update_transform_layers, uv_tiles, Damage, DisplayItem, ImageResource, ImageSampling, FULL_UV};
pub use affine::{css_transform, Affine};
pub use display_list::TransformLayer;
pub use page_list::PageDisplayList;
pub use scene::{GlyphAtlasSnapshot, Scene};
pub use raster::{rasterize, RasterImage};
pub use snapshot::scene_to_json;

/// Anything that can consume a `Scene` and produce pixels (or nothing,
/// in the case of `NullRenderer`) implements this. `layout`/`paint`
/// code depends on this trait, never on a concrete backend.
pub trait Renderer {
    fn render(&mut self, scene: &Scene);
}

/// A `Renderer` that does nothing — useful for testing the pipeline
/// up to (but not including) actual GPU work, and as the seed of a
/// future headless backend for golden-image display-list tests.
#[derive(Debug, Default)]
pub struct NullRenderer {
    pub last_scene: Option<Scene>,
}

impl Renderer for NullRenderer {
    fn render(&mut self, scene: &Scene) {
        self.last_scene = Some(scene.clone());
    }
}
