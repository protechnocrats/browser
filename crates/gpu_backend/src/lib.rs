//! The only crate (besides `shell_desktop`) allowed to depend on
//! platform (wgpu). Implements `rendering::Renderer`.
//!
//! Milestone 34 (`docs/adr/017-compositing.md`): a frame is drawn into a
//! **back buffer** kept across frames, then copied to the window. Three
//! pipelines draw the display list — solid and shadow quads, glyphs from the
//! glyph atlas, images from the image store's textures — each instance
//! carrying its world transform and clip (`instances`), in the order
//! `rendering::plan_draw_passes` gives. A group (`opacity` < 1) is drawn into
//! a **layer** and composited back at its opacity (`passes`). When the back
//! buffer holds the frame a scene's damage is measured against, only the
//! damage is redrawn (scissored). Images live in shared atlas pages or
//! textures of their own, evicted least recently used (`image_store`).
//!
//! What can be tested without a GPU is kept out of this file: the instance
//! data, the pass plan, the image store, the upload decisions and the limits.

pub mod image_store;
pub mod instances;
pub mod limits;
pub mod passes;
pub mod uploads;

use image_store::{Change, ImageStore, Placement, TextureId};
use rendering::{Damage, ImageResource, Renderer, Scene};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The back buffer's and layers' format. Colours are sRGB-encoded values
/// composited as-is (see [`choose_surface_format`]): a non-sRGB format.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// What the last frame cost (tests, benchmarks and the frame HUD read it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuFrameStats {
    /// Draw calls, including composites and the copy to the window.
    pub draw_calls: u32,
    pub render_passes: u32,
    /// Groups drawn into layers.
    pub layers: u32,
    /// Only the damage was redrawn (the back buffer held the previous frame).
    pub partial: bool,
}

/// A render target that can be sampled: the back buffer, or a layer.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Binds it for the composite and present pipelines.
    bind: wgpu::BindGroup,
    size: (u32, u32),
}

struct ImageTexture {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    size: (u32, u32),
}

pub struct GpuRenderer {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    /// The window's surface; `None` for a headless renderer (tests), which
    /// draws into its back buffer only.
    surface: Option<wgpu::Surface<'static>>,
    surface_format: wgpu::TextureFormat,
    /// A headless renderer's size (a windowed one takes the surface's).
    headless_size: (u32, u32),
    /// `device.limits().max_texture_dimension_2d`: every texture (and the
    /// surface) is checked against it before creation.
    max_texture_dim: u32,

    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    target_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,

    quad_pipeline: wgpu::RenderPipeline,
    text_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    /// A layer onto the target around it (premultiplied).
    composite_pipeline: wgpu::RenderPipeline,
    /// The back buffer onto the window (replacing).
    present_pipeline: wgpu::RenderPipeline,

    // The glyph atlas: recreated whenever its size changes (it grows, up to
    // `text::MAX_ATLAS_HEIGHT`, and shrinks back when cleared).
    atlas_texture: wgpu::Texture,
    atlas_bind: wgpu::BindGroup,
    atlas_dims: (u32, u32),
    /// `None` until the first upload, so the first frame always uploads.
    uploaded_atlas_generation: Option<u64>,
    /// Whether the atlas texture holds the current atlas. False when the
    /// atlas exceeded the device's texture limit and was not uploaded;
    /// glyphs are then not drawn rather than drawn from stale texels.
    atlas_ready: bool,
    /// Which atlas bands the texture holds (only changed ones are uploaded).
    uploaded_atlas: uploads::UploadedAtlas,

    images: ImageStore,
    image_textures: HashMap<TextureId, ImageTexture>,
    /// Images with no usable pixels: reported once, never retried.
    unusable_images: HashSet<u64>,
    /// Images refused for lack of room this frame (reported once each).
    refused_images: HashSet<u64>,
    /// The last frame left an image out: the next one redraws everything,
    /// so the image appears once it fits.
    refused_last_frame: bool,

    back: Option<Target>,
    layers: Vec<Target>,
    /// The `Scene::sequence` the back buffer holds, and what it was drawn
    /// at: size, viewport, background.
    back_holds: Option<(u64, BackKey)>,

    /// Quad, glyph, image and composite instances, kept across frames.
    instance_buffers: [InstanceBuffer; 4],
    last_stats: GpuFrameStats,
}

type BackKey = (u32, u32, u32, u32, style::Color);

/// A vertex buffer reused frame to frame, grown when a frame needs more
/// (see [`uploads::grown_capacity`]).
#[derive(Default)]
struct InstanceBuffer {
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
}

impl InstanceBuffer {
    /// Writes `bytes` into the buffer, growing it first if it is too small.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, label: &'static str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(capacity) = uploads::grown_capacity(self.capacity, bytes.len() as u64) {
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
        }
        if let Some(buffer) = &self.buffer {
            queue.write_buffer(buffer, 0, bytes);
        }
    }
}

/// Why the GPU renderer could not start. Reported to the host instead of
/// panicking: a machine with no usable GPU (a VM, a bare server, missing
/// drivers) is an expected environment, and "GPU-first" (architecture doc
/// §5) means saying so clearly, not crashing with a backtrace.
#[derive(Debug)]
pub enum GpuError {
    /// The window surface could not be created.
    Surface(String),
    /// No graphics adapter compatible with the surface was found.
    NoAdapter,
    /// The adapter refused to create a device.
    Device(String),
    /// The surface reports no supported texture format.
    NoSurfaceFormat,
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::Surface(e) => write!(f, "could not create a GPU surface for the window: {e}"),
            GpuError::NoAdapter => write!(f, "no suitable GPU adapter was found (is a graphics driver installed?)"),
            GpuError::Device(e) => write!(f, "could not create a GPU device: {e}"),
            GpuError::NoSurfaceFormat => write!(f, "the window surface reports no supported texture format"),
        }
    }
}

impl std::error::Error for GpuError {}

impl GpuRenderer {
    /// Creates the renderer for `window`. This crate contains no `unsafe`
    /// (and `forbid`s it): surface creation goes through `wgpu`'s safe
    /// `create_surface(Arc<Window>)`, which keeps the window alive for the
    /// surface's lifetime.
    pub async fn new(window: Arc<winit::window::Window>, width: u32, height: u32) -> Result<Self, GpuError> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window).map_err(|e| GpuError::Surface(e.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or(GpuError::NoAdapter)?;
        let (device, queue) = request_device(&adapter).await?;
        let max_texture_dim = device.limits().max_texture_dimension_2d;
        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = choose_surface_format(&surface_caps.formats).ok_or(GpuError::NoSurfaceFormat)?;
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: surface_format,
                width: width.clamp(1, max_texture_dim),
                height: height.clamp(1, max_texture_dim),
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: surface_caps.alpha_modes[0],
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );
        Ok(Self::with_device(device, queue, Some(surface), surface_format, (width, height)))
    }

    /// A renderer with no window, drawing into an offscreen `width` ×
    /// `height` back buffer ([`Self::render_to_image`] reads it) — for tests
    /// that check the GPU path against the CPU rasteriser on real hardware.
    pub async fn headless(width: u32, height: u32) -> Result<Self, GpuError> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: None, force_fallback_adapter: false })
            .await
            .ok_or(GpuError::NoAdapter)?;
        let (device, queue) = request_device(&adapter).await?;
        let max = device.limits().max_texture_dimension_2d;
        Ok(Self::with_device(device, queue, None, TARGET_FORMAT, (width.clamp(1, max), height.clamp(1, max))))
    }

    fn with_device(device: wgpu::Device, queue: wgpu::Queue, surface: Option<wgpu::Surface<'static>>, surface_format: wgpu::TextureFormat, size: (u32, u32)) -> Self {
        let max_texture_dim = device.limits().max_texture_dimension_2d;
        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let texture_layout = build_texture_layout(&device, true);
        let target_layout = build_texture_layout(&device, false);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let draw_blend = Some(wgpu::BlendState::ALPHA_BLENDING);
        let quad_pipeline = build_pipeline(&device, "quad", QUAD_SHADER, &[&globals_layout], &QUAD_LAYOUT, TARGET_FORMAT, draw_blend);
        let text_pipeline = build_pipeline(&device, "text", TEXT_SHADER, &[&globals_layout, &texture_layout], &GLYPH_LAYOUT, TARGET_FORMAT, draw_blend);
        let image_pipeline = build_pipeline(&device, "image", IMAGE_SHADER, &[&globals_layout, &texture_layout], &IMAGE_LAYOUT, TARGET_FORMAT, draw_blend);
        let layouts = [&globals_layout, &target_layout];
        let composite_pipeline =
            build_pipeline(&device, "composite", COMPOSITE_SHADER, &layouts, &instances::COMPOSITE, TARGET_FORMAT, Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING));
        let present_pipeline = build_pipeline(&device, "present", COMPOSITE_SHADER, &layouts, &instances::COMPOSITE, surface_format, None);

        // A 1x1 placeholder; the first frame with text creates the real one.
        let (atlas_texture, atlas_bind) = create_atlas_texture(&device, &texture_layout, &sampler, 1, 1);

        GpuRenderer {
            device,
            queue,
            surface,
            surface_format,
            headless_size: size,
            max_texture_dim,
            globals,
            globals_bind,
            texture_layout,
            target_layout,
            sampler,
            quad_pipeline,
            text_pipeline,
            image_pipeline,
            composite_pipeline,
            present_pipeline,
            atlas_texture,
            atlas_bind,
            atlas_dims: (1, 1),
            uploaded_atlas_generation: None,
            atlas_ready: false,
            uploaded_atlas: uploads::UploadedAtlas::default(),
            images: ImageStore::new(limits::MAX_IMAGE_TEXTURE_BYTES),
            image_textures: HashMap::new(),
            unusable_images: HashSet::new(),
            refused_images: HashSet::new(),
            refused_last_frame: false,
            back: None,
            layers: Vec::new(),
            back_holds: None,
            instance_buffers: Default::default(),
            last_stats: GpuFrameStats::default(),
        }
    }

    /// What the last frame cost.
    pub fn last_frame_stats(&self) -> GpuFrameStats {
        self.last_stats
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let Some(surface) = &self.surface else {
            self.headless_size = (width.min(self.max_texture_dim), height.min(self.max_texture_dim));
            return;
        };
        surface.configure(
            &self.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.surface_format,
                // A window larger than the device's texture limit (a wall
                // of monitors) would be a validation error: clamp.
                width: width.min(self.max_texture_dim),
                height: height.min(self.max_texture_dim),
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );
    }

    /// Recreates the atlas texture if its size changed, and uploads the
    /// bands that changed since the texture last got them.
    fn sync_atlas_texture(&mut self, snapshot: &rendering::GlyphAtlasSnapshot) {
        let needed_dims = (snapshot.width as u32, snapshot.height as u32);
        // `text` caps the atlas far below any device limit, so this only
        // trips on a device with unusually small limits — or a bug.
        let fits = limits::texture_fits(needed_dims.0, needed_dims.1, self.max_texture_dim)
            && snapshot.pixels.len() >= needed_dims.0 as usize * needed_dims.1 as usize;
        if !fits {
            if self.atlas_ready || self.uploaded_atlas_generation != Some(snapshot.generation) {
                diagnostics::guard(
                    diagnostics::Guard::GpuTextureTooLarge,
                    &format!("glyph atlas {}x{} exceeds the device limit {}; text not drawn", needed_dims.0, needed_dims.1, self.max_texture_dim),
                );
            }
            self.atlas_ready = false;
            self.uploaded_atlas_generation = Some(snapshot.generation);
            return;
        }

        if needed_dims != self.atlas_dims {
            let (texture, bind) = create_atlas_texture(&self.device, &self.texture_layout, &self.sampler, needed_dims.0, needed_dims.1);
            self.atlas_texture = texture;
            self.atlas_bind = bind;
            self.atlas_dims = needed_dims;
            self.uploaded_atlas_generation = None; // force the upload below
            self.uploaded_atlas.reset();
        }

        if self.uploaded_atlas_generation != Some(snapshot.generation) {
            for (y, rows, band) in self.uploaded_atlas.changed_bands(&snapshot.pixels) {
                self.queue.write_texture(
                    wgpu::ImageCopyTexture { texture: &self.atlas_texture, mip_level: 0, origin: wgpu::Origin3d { x: 0, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                    &band[..(rows * needed_dims.0) as usize],
                    // R8Unorm: 1 byte a pixel.
                    wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(needed_dims.0), rows_per_image: Some(rows) },
                    wgpu::Extent3d { width: needed_dims.0, height: rows, depth_or_array_layers: 1 },
                );
            }
            self.uploaded_atlas_generation = Some(snapshot.generation);
        }
        self.atlas_ready = true;
    }

    /// Puts the frame's images on the GPU (those not there already) and
    /// says where each is. An image with no room is left out — reported
    /// once — and not drawn this frame.
    fn sync_images(&mut self, images: &[ImageResource]) -> HashMap<u64, Placement> {
        self.images.begin_frame();
        let mut placed = HashMap::with_capacity(images.len());
        let mut refused = false;
        let mut changes = Vec::new();
        for img in images {
            if let Some(p) = self.images.get(img.id) {
                placed.insert(img.id, p);
                continue;
            }
            if self.unusable_images.contains(&img.id) {
                continue;
            }
            let Some((width, height, rgba)) = self.admit_image(img) else {
                self.unusable_images.insert(img.id);
                continue;
            };
            let placement = self.images.place(img.id, width, height, &mut changes);
            self.apply_image_changes(&mut changes);
            let Some(placement) = placement else {
                refused = true;
                if self.refused_images.insert(img.id) {
                    diagnostics::guard(
                        diagnostics::Guard::GpuImageBudget,
                        &format!("{} MiB of image textures are in use by this frame; a {width}x{height} image was not drawn", self.images.used() >> 20),
                    );
                }
                continue;
            };
            self.refused_images.remove(&img.id);
            if let Some(texture) = self.image_textures.get(&placement.texture) {
                let [x0, y0, x1, y1] = placement.rect;
                self.queue.write_texture(
                    wgpu::ImageCopyTexture { texture: &texture.texture, mip_level: 0, origin: wgpu::Origin3d { x: x0, y: y0, z: 0 }, aspect: wgpu::TextureAspect::All },
                    &rgba,
                    wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: Some(height) },
                    wgpu::Extent3d { width: x1 - x0, height: y1 - y0, depth_or_array_layers: 1 },
                );
            }
            placed.insert(img.id, placement);
        }
        self.refused_last_frame = refused;
        placed
    }

    fn apply_image_changes(&mut self, changes: &mut Vec<Change>) {
        for change in changes.drain(..) {
            match change {
                Change::Create { texture: id, width, height } => {
                    let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("image texture"),
                        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("image texture"),
                        layout: &self.texture_layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                        ],
                    });
                    self.image_textures.insert(id, ImageTexture { texture, bind, size: (width, height) });
                }
                Change::Destroy(id) => {
                    self.image_textures.remove(&id);
                }
            }
        }
    }

    /// The size and pixels to upload for `img` — scaled down to the device's
    /// texture limit if larger — or `None` (reported) if it has no usable
    /// pixels.
    fn admit_image<'a>(&self, img: &'a ImageResource) -> Option<(u32, u32, std::borrow::Cow<'a, [u8]>)> {
        if limits::texture_fits(img.width, img.height, self.max_texture_dim) && img.rgba.len() >= img.width as usize * img.height as usize * 4 {
            return Some((img.width, img.height, std::borrow::Cow::Borrowed(&img.rgba[..img.width as usize * img.height as usize * 4])));
        }
        let Some((scaled, w, h)) = limits::downscale_to_fit(&img.rgba, img.width, img.height, self.max_texture_dim) else {
            diagnostics::guard(diagnostics::Guard::GpuTextureTooLarge, &format!("image {}x{} has no usable pixels; not drawn", img.width, img.height));
            return None;
        };
        diagnostics::guard(
            diagnostics::Guard::GpuTextureTooLarge,
            &format!("image {}x{} exceeds the device limit {}; scaled to {w}x{h}", img.width, img.height, self.max_texture_dim),
        );
        Some((w, h, std::borrow::Cow::Owned(scaled)))
    }

    /// A render target of `size`.
    fn create_target(&self, label: &'static str, size: (u32, u32)) -> Target {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.target_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
        });
        Target { texture, view, bind, size }
    }

    /// The back buffer at `size` and `layers` layers, (re)created as needed.
    fn ensure_targets(&mut self, size: (u32, u32), layers: usize) {
        if self.back.as_ref().map(|b| b.size) != Some(size) {
            self.back = Some(self.create_target("back buffer", size));
            self.back_holds = None;
            self.layers.clear();
        }
        while self.layers.len() < layers {
            let layer = self.create_target("layer", size);
            self.layers.push(layer);
        }
    }

    /// The part of the back buffer to redraw: `None` for all of it.
    fn damage_to_redraw(&self, scene: &Scene, key: BackKey, force_full: bool) -> Option<[f32; 4]> {
        let holds_base = matches!((self.back_holds, scene.damage_base), (Some((held, held_key)), Some(base)) if held == base && held_key == key);
        if !holds_base || force_full {
            return None;
        }
        match scene.damage {
            Damage::Full => None,
            Damage::None => Some([0.0; 4]),
            // A pixel's margin: antialiased edges reach just past a box.
            Damage::Rect { x, y, width, height } => Some([x - 1.0, y - 1.0, x + width + 1.0, y + height + 1.0]),
        }
    }
}

impl Renderer for GpuRenderer {
    fn render(&mut self, scene: &Scene) {
        let Some(surface) = &self.surface else {
            let size = self.headless_size;
            self.draw_frame(scene, size, None);
            return;
        };
        let Ok(frame) = surface.get_current_texture() else {
            return; // transient acquire failure (e.g. mid-resize) — skip this frame
        };
        let size = (frame.texture.width(), frame.texture.height());
        self.draw_frame(scene, size, Some(&frame.texture));
        frame.present();
    }
}

impl GpuRenderer {
    /// Draws `scene` into the back buffer (`size` device pixels) and, given
    /// the window's texture, copies it there.
    fn draw_frame(&mut self, scene: &Scene, size: (u32, u32), window: Option<&wgpu::Texture>) {
        let was_ready = self.atlas_ready;
        // A text-free page keeps the 1x1 placeholder: no upload at all.
        if scene.glyph_atlas.width > 1 || scene.glyph_atlas.height > 1 {
            self.sync_atlas_texture(&scene.glyph_atlas);
        }
        let refused_before = self.refused_last_frame;
        let placements = self.sync_images(&scene.images);
        // Glyphs appearing or vanishing, or an image that could not be drawn
        // last frame: what is on screen is not the previous frame.
        let force_full = was_ready != self.atlas_ready || refused_before;

        // The textures images are drawn from, numbered for the pass plan.
        let mut textures: Vec<TextureId> = placements.values().map(|p| p.texture).collect();
        textures.sort_unstable();
        textures.dedup();
        let texture_index = |id: u64| placements.get(&id).and_then(|p| textures.binary_search(&p.texture).ok()).map_or(u64::MAX, |i| i as u64);
        let ops = rendering::plan_draw_passes(&scene.items, texture_index);
        let image_rect = |id: u64| {
            let p = placements.get(&id)?;
            let size = self.image_textures.get(&p.texture)?.size;
            let (w, h) = (size.0 as f32, size.1 as f32);
            Some([p.rect[0] as f32 / w, p.rect[1] as f32 / h, p.rect[2] as f32 / w, p.rect[3] as f32 / h])
        };
        let mut inst = instances::build(&scene.items, (scene.glyph_atlas.width as f32, scene.glyph_atlas.height as f32), image_rect);

        let (vw, vh) = (scene.viewport_width.max(1.0), scene.viewport_height.max(1.0));
        let key: BackKey = (size.0, size.1, vw.to_bits(), vh.to_bits(), scene.background);
        let damage = self.damage_to_redraw(scene, key, force_full);
        let (segments, composites) = passes::segment(&ops, damage);
        let layer_count = segments.iter().filter_map(|s| if let passes::Target::Layer(l) = s.target { Some(l + 1) } else { None }).max().unwrap_or(0);
        self.ensure_targets(size, layer_count);

        let bg = scene.background;
        let background = inst.push_plain_rect([0.0, 0.0, vw, vh], [f32::from(bg.r) / 255.0, f32::from(bg.g) / 255.0, f32::from(bg.b) / 255.0, 1.0]);
        let mut composite_data: Vec<f32> = Vec::with_capacity((composites.len() + 1) * 5);
        for c in &composites {
            composite_data.extend_from_slice(&c.bounds);
            composite_data.push(c.opacity);
        }
        let present_index = composites.len() as u32;
        composite_data.extend_from_slice(&[0.0, 0.0, vw, vh, 1.0]);

        self.queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&[vw, vh, size.0 as f32, size.1 as f32]));
        {
            let [quads, glyphs, images, comps] = &mut self.instance_buffers;
            quads.upload(&self.device, &self.queue, "quad instances", bytemuck::cast_slice(&inst.quads));
            glyphs.upload(&self.device, &self.queue, "glyph instances", bytemuck::cast_slice(&inst.glyphs));
            images.upload(&self.device, &self.queue, "image instances", bytemuck::cast_slice(&inst.images));
            comps.upload(&self.device, &self.queue, "composite instances", bytemuck::cast_slice(&composite_data));
        }
        let [quads, glyphs, images, comps] = &self.instance_buffers;
        fn buffer(b: &InstanceBuffer, used: bool) -> Option<wgpu::BufferSlice<'_>> {
            b.buffer.as_ref().filter(|_| used).map(|b| b.slice(..))
        }
        let (quad_buf, glyph_buf, image_buf, comp_buf) =
            (buffer(quads, !inst.quads.is_empty()), buffer(glyphs, !inst.glyphs.is_empty()), buffer(images, !inst.images.is_empty()), buffer(comps, true));
        let Some(back) = &self.back else { return };

        let mut stats = GpuFrameStats { partial: damage.is_some(), layers: composites.len() as u32, ..GpuFrameStats::default() };
        let scale = (size.0 as f32 / vw, size.1 as f32 / vh);
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        for segment in &segments {
            let (view, clear) = match segment.target {
                passes::Target::Back => (&back.view, wgpu::Color { r: f64::from(bg.r) / 255.0, g: f64::from(bg.g) / 255.0, b: f64::from(bg.b) / 255.0, a: 1.0 }),
                passes::Target::Layer(l) => (&self.layers[l].view, wgpu::Color::TRANSPARENT),
            };
            let scissor = match segment.scissor {
                Some(r) => device_scissor(r, scale, size),
                None => Some((0, 0, size.0, size.1)),
            };
            if scissor.is_none() && !segment.clear {
                continue; // nothing to draw, nothing to clear
            }
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("paint"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: if segment.clear { wgpu::LoadOp::Clear(clear) } else { wgpu::LoadOp::Load }, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            stats.render_passes += 1;
            let Some((x, y, w, h)) = scissor else { continue };
            pass.set_scissor_rect(x, y, w, h);
            pass.set_bind_group(0, &self.globals_bind, &[]);
            for command in &segment.commands {
                match *command {
                    passes::Command::Background => {
                        if let Some(buf) = quad_buf {
                            pass.set_pipeline(&self.quad_pipeline);
                            pass.set_vertex_buffer(0, buf);
                            pass.draw(0..6, background..background + 1);
                            stats.draw_calls += 1;
                        }
                    }
                    passes::Command::Composite { index, layer } => {
                        if let Some(buf) = comp_buf {
                            pass.set_pipeline(&self.composite_pipeline);
                            pass.set_bind_group(1, &self.layers[layer].bind, &[]);
                            pass.set_vertex_buffer(0, buf);
                            pass.draw(0..6, index as u32..index as u32 + 1);
                            stats.draw_calls += 1;
                        }
                    }
                    passes::Command::Draw(batch) => {
                        let range = batch.first..batch.first + batch.count;
                        match batch.kind {
                            rendering::BatchKind::Rects => {
                                let Some(buf) = quad_buf else { continue };
                                pass.set_pipeline(&self.quad_pipeline);
                                pass.set_vertex_buffer(0, buf);
                            }
                            rendering::BatchKind::Glyphs => {
                                let (Some(buf), true) = (glyph_buf, self.atlas_ready) else { continue };
                                pass.set_pipeline(&self.text_pipeline);
                                pass.set_bind_group(1, &self.atlas_bind, &[]);
                                pass.set_vertex_buffer(0, buf);
                            }
                            rendering::BatchKind::Image(index) => {
                                // An image with no room has no texture: not drawn.
                                let texture = usize::try_from(index).ok().and_then(|i| textures.get(i)).and_then(|t| self.image_textures.get(t));
                                let (Some(buf), Some(texture)) = (image_buf, texture) else { continue };
                                pass.set_pipeline(&self.image_pipeline);
                                pass.set_bind_group(1, &texture.bind, &[]);
                                pass.set_vertex_buffer(0, buf);
                            }
                        }
                        pass.draw(0..6, range);
                        stats.draw_calls += 1;
                    }
                }
            }
        }

        // The back buffer onto the window.
        if let Some(window) = window {
            let view = window.create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let Some(buf) = comp_buf {
                pass.set_pipeline(&self.present_pipeline);
                pass.set_bind_group(0, &self.globals_bind, &[]);
                pass.set_bind_group(1, &back.bind, &[]);
                pass.set_vertex_buffer(0, buf);
                pass.draw(0..6, present_index..present_index + 1);
                stats.draw_calls += 1;
            }
            stats.render_passes += 1;
        }

        self.queue.submit(Some(encoder.finish()));
        self.back_holds = Some((scene.sequence, key));
        self.last_stats = stats;
    }
}

/// The device, with uncaptured errors reported instead of panicking.
async fn request_device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), GpuError> {
    let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor::default(), None).await.map_err(|e| GpuError::Device(e.to_string()))?;
    // wgpu's default for an uncaptured error (a validation error, out of
    // memory) is to panic — on the GPU thread, outside any page's crash
    // containment. Report it instead; the frame it spoiled is lost, the
    // window carries on.
    device.on_uncaptured_error(Box::new(|error| {
        diagnostics::guard(diagnostics::Guard::GpuError, &error.to_string());
    }));
    Ok((device, queue))
}

impl GpuRenderer {
    /// Draws `scene` and reads the back buffer back (RGBA, device pixels):
    /// what the window would show. For tests; `None` if nothing was drawn.
    pub fn render_to_image(&mut self, scene: &Scene) -> Option<rendering::RasterImage> {
        let size = match &self.surface {
            Some(_) => self.back.as_ref().map(|b| b.size)?,
            None => self.headless_size,
        };
        self.draw_frame(scene, size, None);
        let back = self.back.as_ref()?;
        let (w, h) = back.size;
        // Rows are copied at a multiple of 256 bytes.
        let row = (w * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("read back"),
            size: u64::from(row) * u64::from(h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture { texture: &back.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::ImageCopyBuffer { buffer: &buffer, layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().ok()?.ok()?;
        let data = slice.get_mapped_range();
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            rgba.extend_from_slice(&data[y * row as usize..y * row as usize + w as usize * 4]);
        }
        drop(data);
        buffer.unmap();
        Some(rendering::RasterImage { width: w, height: h, rgba })
    }
}

/// `r` (`[x0, y0, x1, y1]`, CSS px) as a scissor rectangle `(x, y, w, h)`
/// in device pixels of a `size` target, grown to whole pixels; `None` if
/// empty.
fn device_scissor(r: [f32; 4], scale: (f32, f32), size: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let clamp = |v: f32, max: u32| if v.is_nan() { 0 } else { v.clamp(0.0, max as f32) as u32 };
    let x0 = clamp((r[0] * scale.0).floor(), size.0);
    let y0 = clamp((r[1] * scale.1).floor(), size.1);
    let x1 = clamp((r[2] * scale.0).ceil(), size.0);
    let y1 = clamp((r[3] * scale.1).ceil(), size.1);
    (x1 > x0 && y1 > y0).then(|| (x0, y0, x1 - x0, y1 - y0))
}

fn create_atlas_texture(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler, width: u32, height: u32) -> (wgpu::Texture, wgpu::BindGroup) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glyph atlas"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // Single-channel coverage — matches text::GlyphAtlas's buffer format exactly.
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("glyph atlas"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    });
    (texture, bind)
}

/// A texture (and, `with_sampler`, a filtering sampler) bound at group 1.
fn build_texture_layout(device: &wgpu::Device, with_sampler: bool) -> wgpu::BindGroupLayout {
    let texture = wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: with_sampler },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let sampler = wgpu::BindGroupLayoutEntry {
        binding: 1,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    let entries: &[wgpu::BindGroupLayoutEntry] = if with_sampler { &[texture, sampler] } else { &[texture] };
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(if with_sampler { "texture layout" } else { "target layout" }), entries })
}

/// The surface format to render into: a NON-sRGB one when available.
///
/// Every colour this renderer receives — CSS colours, decoded image pixels,
/// glyph coverage blended over them — is already sRGB-encoded, and web
/// content is composited in that space (as browsers do). An `*Srgb` surface
/// format would make the GPU encode those values a second time on write,
/// washing every colour out: measured on real hardware before this fix,
/// `#1e1e2e` came out as `(96,96,118)` and `#4ecdc4` as `(150,232,227)`,
/// exactly the double encoding. Falls back to the first format if every
/// one is sRGB (colours are then too light, but the page still draws).
fn choose_surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    formats.iter().copied().find(|f| !f.is_srgb()).or_else(|| formats.first().copied())
}

const COMMON: &str = include_str!("common.wgsl");
const QUAD_SHADER: &str = include_str!("quad.wgsl");
const TEXT_SHADER: &str = include_str!("text.wgsl");
const IMAGE_SHADER: &str = include_str!("image.wgsl");
const COMPOSITE_SHADER: &str = include_str!("composite.wgsl");

const fn layout<const N: usize>(head: &[u32]) -> [u32; N] {
    let mut out = [0u32; N];
    let mut i = 0;
    while i < head.len() {
        out[i] = head[i];
        i += 1;
    }
    let mut j = 0;
    while j < instances::TAIL.len() {
        out[i + j] = instances::TAIL[j];
        j += 1;
    }
    out
}

/// Each pipeline's instance attributes, as vector widths in location order.
const QUAD_LAYOUT: [u32; 10] = layout(&instances::QUAD_HEAD);
const GLYPH_LAYOUT: [u32; 10] = layout(&instances::GLYPH_HEAD);
const IMAGE_LAYOUT: [u32; 11] = layout(&instances::IMAGE_HEAD);

/// A shader's full source: the shared functions, then its own.
fn shader_source(own: &str) -> String {
    format!("{COMMON}\n{own}")
}

fn vertex_attributes(widths: &[u32]) -> Vec<wgpu::VertexAttribute> {
    let mut offset = 0;
    widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let format = match w {
                1 => wgpu::VertexFormat::Float32,
                2 => wgpu::VertexFormat::Float32x2,
                3 => wgpu::VertexFormat::Float32x3,
                _ => wgpu::VertexFormat::Float32x4,
            };
            let attribute = wgpu::VertexAttribute { format, offset, shader_location: i as u32 };
            offset += u64::from(*w) * 4;
            attribute
        })
        .collect()
}

fn build_pipeline(
    device: &wgpu::Device,
    label: &'static str,
    own_source: &str,
    bind_groups: &[&wgpu::BindGroupLayout],
    widths: &[u32],
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(shader_source(own_source).into()) });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: bind_groups, push_constant_ranges: &[] });
    let attributes = vertex_attributes(widths);
    let stride: u64 = widths.iter().map(|w| u64::from(*w) * 4).sum();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[wgpu::VertexBufferLayout { array_stride: stride, step_mode: wgpu::VertexStepMode::Instance, attributes: &attributes }],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_srgb_surface_format_is_preferred() {
        use wgpu::TextureFormat::*;
        assert_eq!(choose_surface_format(&[Bgra8UnormSrgb, Bgra8Unorm]), Some(Bgra8Unorm));
        assert_eq!(choose_surface_format(&[Rgba8Unorm, Bgra8UnormSrgb]), Some(Rgba8Unorm));
        assert_eq!(choose_surface_format(&[Bgra8UnormSrgb]), Some(Bgra8UnormSrgb), "all-sRGB: still draw");
        assert_eq!(choose_surface_format(&[]), None);
    }

    #[test]
    fn scissors_cover_whole_pixels_inside_the_target() {
        assert_eq!(device_scissor([1.5, 2.2, 10.1, 4.0], (1.0, 1.0), (100, 100)), Some((1, 2, 10, 2)));
        assert_eq!(device_scissor([1.0, 1.0, 2.0, 2.0], (2.0, 2.0), (100, 100)), Some((2, 2, 2, 2)), "device pixels");
        assert_eq!(device_scissor([-5.0, -5.0, 1e30, 1e30], (1.0, 1.0), (40, 30)), Some((0, 0, 40, 30)), "clamped to the target");
        assert_eq!(device_scissor([0.0; 4], (1.0, 1.0), (40, 30)), None, "no damage: nothing to draw");
        assert_eq!(device_scissor([f32::NAN, 0.0, 5.0, 5.0], (1.0, 1.0), (40, 30)), Some((0, 0, 5, 5)));
    }
}

#[cfg(test)]
mod shader_tests {
    use super::*;

    fn parse(own: &str) -> naga::Module {
        let source = shader_source(own);
        let module = naga::front::wgsl::parse_str(&source).unwrap_or_else(|e| panic!("{}", e.emit_to_string(&source)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{e:?}"));
        module
    }

    /// Every shader parses and validates — what `create_shader_module`
    /// would reject at startup on a real GPU, caught in `cargo test`.
    #[test]
    fn every_wgsl_shader_is_valid() {
        for own in [QUAD_SHADER, TEXT_SHADER, IMAGE_SHADER, COMPOSITE_SHADER] {
            parse(own);
        }
    }

    /// Each pipeline's vertex layout matches its shader's `vs_main` instance
    /// input: one attribute per `@location`, of the shader's width, in order
    /// — and the instance data `instances` builds is that many floats.
    #[test]
    fn instance_layouts_match_the_shaders() {
        for (own, widths, floats) in [
            (QUAD_SHADER, &QUAD_LAYOUT[..], instances::QUAD_FLOATS),
            (TEXT_SHADER, &GLYPH_LAYOUT[..], instances::GLYPH_FLOATS),
            (IMAGE_SHADER, &IMAGE_LAYOUT[..], instances::IMAGE_FLOATS),
            (COMPOSITE_SHADER, &instances::COMPOSITE[..], 5),
        ] {
            let module = parse(own);
            let entry = module.entry_points.iter().find(|e| e.name == "vs_main").expect("vs_main");
            let mut locations: Vec<(u32, u32)> = Vec::new();
            for arg in &entry.function.arguments {
                if let naga::TypeInner::Struct { members, .. } = &module.types[arg.ty].inner {
                    for m in members {
                        if let Some(naga::Binding::Location { location, .. }) = m.binding {
                            let width = match module.types[m.ty].inner {
                                naga::TypeInner::Vector { size, .. } => size as u32,
                                naga::TypeInner::Scalar(_) => 1,
                                ref other => panic!("unexpected {other:?}"),
                            };
                            locations.push((location, width));
                        }
                    }
                }
            }
            locations.sort_unstable();
            let expected: Vec<(u32, u32)> = widths.iter().enumerate().map(|(i, w)| (i as u32, *w)).collect();
            assert_eq!(locations, expected);
            assert_eq!(widths.iter().sum::<u32>() as usize, floats);
        }
    }
}
