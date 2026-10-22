//! Where images live on the GPU (Milestone 34) — the decisions, apart from
//! the GPU calls, so they are tested without one.
//!
//! - **Small images share atlas pages.** An image no larger than
//!   [`ATLAS_MAX_SIDE`] a side is packed into a [`PAGE_SIDE`]² page with
//!   others, so adjacent images on one page draw in one call (every image
//!   used to be its own texture and its own draw call). Larger images get a
//!   texture of their own.
//! - **Memory is bounded, least recently used first.** Every texture counts
//!   against a byte budget. When a new image does not fit, the images (and
//!   pages) not used for the longest are evicted — never one the current
//!   frame uses. Only when nothing can be evicted is an image refused (not
//!   drawn, reported). The audit fixes' budget used to refuse new images
//!   once full, however stale the old ones were.

use std::collections::HashMap;

/// A side of an atlas page, in pixels.
pub const PAGE_SIDE: u32 = 2048;
/// The largest image side packed into a page.
pub const ATLAS_MAX_SIDE: u32 = 256;
const PAGE_BYTES: u64 = PAGE_SIDE as u64 * PAGE_SIDE as u64 * 4;

/// A texture the backend holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TextureId {
    Page(u32),
    Image(u64),
}

/// Where an image is: its texture, and the part of it (`[x0, y0, x1, y1]`
/// in pixels) that holds the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub texture: TextureId,
    pub rect: [u32; 4],
}

/// What the backend must do on the GPU after a [`ImageStore::place`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Create this texture (`width` × `height`, RGBA).
    Create { texture: TextureId, width: u32, height: u32 },
    /// Free this texture.
    Destroy(TextureId),
}

struct Page {
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    images: Vec<u64>,
    last_used: u64,
}

struct Entry {
    placement: Placement,
    bytes: u64,
    last_used: u64,
}

pub struct ImageStore {
    budget: u64,
    used: u64,
    frame: u64,
    next_page: u32,
    pages: HashMap<u32, Page>,
    images: HashMap<u64, Entry>,
}

impl ImageStore {
    pub fn new(budget: u64) -> ImageStore {
        ImageStore { budget, used: 0, frame: 0, next_page: 0, pages: HashMap::new(), images: HashMap::new() }
    }

    /// Bytes of textures held.
    pub fn used(&self) -> u64 {
        self.used
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Starts a frame: what it uses from here on is protected from eviction.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// The image's placement, if it is on the GPU — marking it used.
    pub fn get(&mut self, image: u64) -> Option<Placement> {
        let frame = self.frame;
        let entry = self.images.get_mut(&image)?;
        entry.last_used = frame;
        if let TextureId::Page(p) = entry.placement.texture {
            if let Some(page) = self.pages.get_mut(&p) {
                page.last_used = frame;
            }
        }
        Some(entry.placement)
    }

    /// Finds room for a `width` × `height` image (evicting what the budget
    /// needs), recording the GPU changes in `changes`. `None`: it cannot be
    /// held without evicting something this frame uses — not drawn.
    pub fn place(&mut self, image: u64, width: u32, height: u32, changes: &mut Vec<Change>) -> Option<Placement> {
        if let Some(p) = self.get(image) {
            return Some(p);
        }
        if width == 0 || height == 0 {
            return None;
        }
        let placement = if width <= ATLAS_MAX_SIDE && height <= ATLAS_MAX_SIDE {
            self.place_in_page(width, height, changes)?
        } else {
            let bytes = u64::from(width) * u64::from(height) * 4;
            if !self.make_room(bytes, changes) {
                return None;
            }
            self.used += bytes;
            let texture = TextureId::Image(image);
            changes.push(Change::Create { texture, width, height });
            Placement { texture, rect: [0, 0, width, height] }
        };
        let bytes = if let TextureId::Page(p) = placement.texture {
            if let Some(page) = self.pages.get_mut(&p) {
                page.images.push(image);
                page.last_used = self.frame;
            }
            0 // the page's bytes are counted once, with the page
        } else {
            u64::from(width) * u64::from(height) * 4
        };
        self.images.insert(image, Entry { placement, bytes, last_used: self.frame });
        Some(placement)
    }

    fn place_in_page(&mut self, width: u32, height: u32, changes: &mut Vec<Change>) -> Option<Placement> {
        // The first page with room on its shelf, or a new shelf.
        let mut ids: Vec<u32> = self.pages.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let page = self.pages.get_mut(&id)?;
            if let Some(rect) = shelf_fit(page, width, height) {
                return Some(Placement { texture: TextureId::Page(id), rect });
            }
        }
        if !self.make_room(PAGE_BYTES, changes) {
            return None;
        }
        let id = self.next_page;
        self.next_page += 1;
        self.used += PAGE_BYTES;
        changes.push(Change::Create { texture: TextureId::Page(id), width: PAGE_SIDE, height: PAGE_SIDE });
        let mut page = Page { shelf_x: 0, shelf_y: 0, shelf_h: 0, images: Vec::new(), last_used: self.frame };
        let rect = shelf_fit(&mut page, width, height)?;
        self.pages.insert(id, page);
        Some(Placement { texture: TextureId::Page(id), rect })
    }

    /// Evicts least-recently-used textures (never ones used this frame)
    /// until `bytes` more fit the budget. `false` if they cannot.
    fn make_room(&mut self, bytes: u64, changes: &mut Vec<Change>) -> bool {
        if bytes > self.budget {
            return false;
        }
        while self.used + bytes > self.budget {
            // The stalest candidate: a standalone image or a whole page.
            let stale_image = self
                .images
                .iter()
                .filter(|(_, e)| matches!(e.placement.texture, TextureId::Image(_)) && e.last_used < self.frame)
                .min_by_key(|(id, e)| (e.last_used, **id))
                .map(|(id, e)| (e.last_used, TextureId::Image(*id)));
            let stale_page = self
                .pages
                .iter()
                .filter(|(_, p)| p.last_used < self.frame)
                .min_by_key(|(id, p)| (p.last_used, **id))
                .map(|(id, p)| (p.last_used, TextureId::Page(*id)));
            let victim = match (stale_image, stale_page) {
                (Some(a), Some(b)) => a.min(b).1,
                (Some(a), None) => a.1,
                (None, Some(b)) => b.1,
                (None, None) => return false,
            };
            self.evict(victim, changes);
        }
        true
    }

    fn evict(&mut self, texture: TextureId, changes: &mut Vec<Change>) {
        match texture {
            TextureId::Image(id) => {
                if let Some(e) = self.images.remove(&id) {
                    self.used -= e.bytes;
                }
            }
            TextureId::Page(p) => {
                if let Some(page) = self.pages.remove(&p) {
                    for image in page.images {
                        self.images.remove(&image);
                    }
                    self.used -= PAGE_BYTES;
                }
            }
        }
        changes.push(Change::Destroy(texture));
    }
}

fn shelf_fit(page: &mut Page, width: u32, height: u32) -> Option<[u32; 4]> {
    if page.shelf_x + width > PAGE_SIDE {
        page.shelf_y += page.shelf_h;
        page.shelf_x = 0;
        page.shelf_h = 0;
    }
    if page.shelf_y + height > PAGE_SIDE {
        return None;
    }
    let rect = [page.shelf_x, page.shelf_y, page.shelf_x + width, page.shelf_y + height];
    page.shelf_x += width;
    page.shelf_h = page.shelf_h.max(height);
    Some(rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acceptance (Milestone 34): GPU image memory stays bounded under an
    /// adversarial page — thousands of distinct images, small and large —
    /// and images still in use survive while stale ones are evicted.
    #[test]
    fn thousands_of_images_stay_within_the_budget_and_recent_ones_survive() {
        let budget = 64 * 1024 * 1024;
        let mut store = ImageStore::new(budget);
        let mut changes = Vec::new();
        let (mut created, mut destroyed) = (0usize, 0usize);
        for frame in 0..200u64 {
            store.begin_frame();
            // A logo used every frame.
            assert!(store.place(1, 64, 64, &mut changes).is_some(), "frame {frame}: the logo");
            // Twenty new images a frame, small and large.
            for k in 0..20u64 {
                let id = 1000 + frame * 20 + k;
                let (w, h) = if k % 5 == 0 { (1024, 768) } else { (100 + k as u32, 80) };
                let _ = store.place(id, w, h, &mut changes);
            }
            assert!(store.used() <= budget, "frame {frame}: {} bytes held", store.used());
            for c in changes.drain(..) {
                match c {
                    Change::Create { .. } => created += 1,
                    Change::Destroy(_) => destroyed += 1,
                }
            }
        }
        assert!(destroyed > 0, "stale images were evicted");
        assert!(created - destroyed <= (budget / (256 * 256 * 4)) as usize, "only what the budget holds is alive");
        store.begin_frame();
        assert!(store.get(1).is_some(), "the image used every frame survived every eviction");
    }

    #[test]
    fn small_images_share_pages_and_large_ones_stand_alone() {
        let mut store = ImageStore::new(256 * 1024 * 1024);
        let mut changes = Vec::new();
        store.begin_frame();
        let a = store.place(1, 32, 32, &mut changes).unwrap();
        let b = store.place(2, 200, 100, &mut changes).unwrap();
        let big = store.place(3, 1000, 10, &mut changes).unwrap();
        assert_eq!(a.texture, b.texture, "one page");
        assert_eq!(big.texture, TextureId::Image(3));
        assert_ne!(a.rect, b.rect);
        assert_eq!(changes.iter().filter(|c| matches!(c, Change::Create { .. })).count(), 2, "one page and one texture");
    }

    #[test]
    fn what_the_current_frame_uses_is_never_evicted() {
        let mut store = ImageStore::new(3 * 1000 * 1000 * 4);
        let mut changes = Vec::new();
        store.begin_frame();
        assert!(store.place(1, 1000, 1000, &mut changes).is_some());
        assert!(store.place(2, 1000, 1000, &mut changes).is_some());
        assert!(store.place(3, 1000, 1000, &mut changes).is_some());
        assert!(store.place(4, 1000, 1000, &mut changes).is_none(), "full of this frame's images: refused, not evicted");
        store.begin_frame();
        assert!(store.get(3).is_some());
        assert!(store.place(4, 1000, 1000, &mut changes).is_some(), "next frame: a stale one goes");
        assert!(store.get(1).is_none(), "the least recently used");
        assert!(store.get(3).is_some());
    }
}

#[cfg(test)]
mod draw_call_tests {
    use super::*;
    use rendering::display_list::GlyphInstance;
    use rendering::{BatchKind, DisplayItem, PassOp};
    use style::Color;

    const INK: Color = Color { r: 0, g: 0, b: 0, a: 255 };

    /// An image-heavy page: a toolbar of 30 icons side by side, then a
    /// gallery of 60 thumbnails, each with a background and a caption.
    fn image_heavy_page() -> Vec<DisplayItem> {
        let image = |id: u64, x: f32, y: f32, size: f32| DisplayItem::Image { x, y, width: size, height: size, image_id: id, sampling: None };
        let caption = |x: f32, y: f32| DisplayItem::Glyphs {
            instances: vec![GlyphInstance { x, y, width: 8.0, height: 10.0, atlas_x: 0, atlas_y: 0, atlas_w: 8, atlas_h: 10, color: INK }; 12],
            atlas_generation: 0,
        };
        let mut items = vec![DisplayItem::Rect { x: 0.0, y: 0.0, width: 1024.0, height: 40.0, color: INK, radius: 0.0 }];
        for i in 0..30u64 {
            items.push(image(i, 4.0 + i as f32 * 34.0, 4.0, 32.0));
        }
        for i in 0..60u64 {
            let (x, y) = ((i % 6) as f32 * 170.0, 50.0 + (i / 6) as f32 * 190.0);
            items.push(DisplayItem::Rect { x, y, width: 160.0, height: 180.0, color: INK, radius: 4.0 });
            items.push(image(100 + i, x, y, 160.0));
            items.push(caption(x, y + 165.0));
        }
        items
    }

    fn draw_calls(ops: &[PassOp]) -> usize {
        ops.iter().filter(|op| matches!(op, PassOp::Draw(_))).count()
    }

    /// Milestone 34 acceptance: an image-heavy page's draw calls drop
    /// against the recorded baseline — every image its own texture, so
    /// every image its own draw call (Milestones 7–33).
    #[test]
    fn an_image_heavy_page_needs_fewer_draw_calls_than_the_baseline() {
        // Recorded before Milestone 34: every image its own texture, and only
        // adjacent items batched — each card's background, thumbnail and
        // caption a draw call each, and each icon one.
        const BASELINE_DRAW_CALLS: usize = 211;
        let items = image_heavy_page();

        let mut store = ImageStore::new(256 << 20);
        let mut changes = Vec::new();
        store.begin_frame();
        let mut texture = std::collections::HashMap::new();
        for item in &items {
            if let DisplayItem::Image { image_id, width, height, .. } = item {
                let p = store.place(*image_id, *width as u32, *height as u32, &mut changes).expect("fits");
                texture.insert(*image_id, p.texture);
            }
        }
        let key = |t: TextureId| match t {
            TextureId::Page(p) => u64::from(p),
            TextureId::Image(id) => 1 << 40 | id,
        };
        let ops = rendering::plan_draw_passes(&items, |id| key(texture[&id]));
        let now = draw_calls(&ops);
        eprintln!("draw calls: {BASELINE_DRAW_CALLS} before, {now} now");
        // The icons and thumbnails share an atlas page, and no card overlaps
        // another: one draw of backgrounds, one of images, one of captions.
        assert_eq!(now, 3, "{ops:?}");
        assert!(ops.iter().any(|op| matches!(op, PassOp::Draw(b) if matches!(b.kind, BatchKind::Image(_)) && b.count == 90)));
        // Each image its own texture (as before), the merging alone:
        // images still break per texture.
        assert!(draw_calls(&rendering::plan_draw_passes(&items, |id| id)) < BASELINE_DRAW_CALLS);
    }
}
