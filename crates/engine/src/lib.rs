//! The engine facade — Milestone 29 (`docs/adr/005-engine-api-shape.md`).
//!
//! The one crate a host talks to. A host creates pages, sends them
//! [`PageCommand`]s (load a URL or an HTML string, deliver input, resize),
//! calls [`Engine::update`] with the current time, and receives
//! [`EngineEvent`]s back — above all [`EngineEvent::Frame`], a finished
//! [`Scene`] for the GPU. Nothing a host calls returns data computed by a
//! page.
//!
//! Since Milestone 30 each page runs on its own thread (`page_thread`,
//! ADR 006): `update` posts ticks and collects what pages have finished,
//! without waiting for them, so a page stuck in a script cannot freeze the
//! host. A panicking page becomes an [`EngineEvent::PageCrashed`] (ADR 007).
//! With [`Isolation::Processes`] (ADR 016) that thread runs inside a
//! sandboxed renderer process of the page's own, bridged to the same mailbox
//! over a bounded protocol (`ipc`, `process`, `renderer`); the API does not
//! change.
//!
//! What lives behind this API (and nowhere in a host any more): page loading
//! and navigation (`loader`), the per-page state (`page`), the frame
//! pipeline — cascade, transitions, animations, layout, display list
//! (`frame`) — and input handling: hit-testing, click-to-position, focus
//! and tab order, scrolling (`input`).
//!
//! Every type in this API is plain data and `Send` (checked at the bottom of
//! this file); the engine-internal state uses `Rc`, which never crosses it.

#![forbid(unsafe_code)]

mod frame;
mod input;
mod ipc;
mod loader;
mod page;
mod page_thread;
mod webfonts;
mod process;
mod renderer;
mod response_policy;

pub use ipc::fuzz_decode as fuzz_decode_ipc_message;
pub use loader::ImageLoading;
pub use process::{InProcessLauncher, Isolation, RendererConnection, RendererControl, RendererLauncher};
pub use renderer::{prepare_renderer, run_renderer};
pub use network::{NetworkContext, RequestContext};
/// Milestone 36 (ADR 010): what a host grants each page, and the kinds it
/// can revoke.
pub use capability::{CapabilityKind, PageGrants};
/// Milestone 37 (ADR 019 §3): what a page's scripts may observe.
pub use privacy::{FingerprintPolicy, FontExposure, TimezonePolicy};
pub use page_thread::{Waker, MAX_HTML_BYTES, MAX_KEY_BYTES, MAX_QUEUED_COMMANDS};
pub use rendering::{rasterize, scene_to_json, Damage, RasterImage, Scene};
/// Milestone 40 (ADR 023): the script runtime interface. A page's script runs
/// in a `ScriptRuntime` (Boa, from `js`) over `script`'s host side.
pub use script::{InterruptHandle, ScriptError, ScriptRuntime};

use page_thread::{Mailbox, PageSetup, PageStatus, WakerSlot};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the engine waits between frames while something is animating
/// (`requestAnimationFrame`, a transition or `@keyframes`): 60 Hz.
pub const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// Largest viewport dimension accepted, in CSS pixels. A `Resize` beyond it
/// is clamped: no real display is this big, and an unbounded size would let
/// a host (or, from Milestone 30, a corrupted message) ask the pipeline for
/// an arbitrarily large layout.
pub const MAX_VIEWPORT_PX: f32 = 16_384.0;

/// Milestone 38a (ADR 013): how long a page in a renderer process may owe a
/// tick without finishing one before its renderer is ended and the page
/// reported crashed. Pages on threads are not bounded by it (a thread cannot
/// be killed).
pub const UNRESPONSIVE_AFTER: Duration = Duration::from_secs(10);

/// A page's handle. Plain data: meaningful only to the engine that issued
/// it, and never reused within that engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(u64);

/// The visible size of a page, in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn new(width: f32, height: f32) -> Self {
        Viewport { width, height }.sanitized()
    }

    /// Zero-trust: a viewport is finite, at least 1x1, and at most
    /// [`MAX_VIEWPORT_PX`] on each side, whatever was asked for.
    fn sanitized(self) -> Self {
        let clamp = |v: f32| if v.is_finite() { v.clamp(1.0, MAX_VIEWPORT_PX) } else { 1.0 };
        Viewport { width: clamp(self.width), height: clamp(self.height) }
    }
}

/// What the host supplies. The font is the engine's bundled one; fonts from
/// the host arrive with the text subsystem (Milestone 35).
pub struct EngineConfig {
    /// The profile's network context — one cookie jar and HTTP cache shared
    /// by every page this engine creates.
    pub network: NetworkContext,
    /// Called (from a page thread) whenever a page has published results
    /// the host has not collected yet. A desktop host wakes its event loop;
    /// `None` is fine for a host that polls (it can also be set later with
    /// [`Engine::set_waker`]).
    pub waker: Option<Waker>,
    /// Whether a page is shown before its images have arrived (the default)
    /// or only once all have — see [`ImageLoading`].
    pub image_loading: ImageLoading,
    /// Where pages run: on threads of this process (the default), or each in
    /// a sandboxed renderer process of its own (ADR 016).
    pub isolation: Isolation,
    /// Milestone 35: the fonts the host grants content (a capability — the
    /// engine never enumerates the system's). `None`: the bundled font only.
    pub fonts: Option<Arc<dyn text::FontSource>>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig { network: NetworkContext::new(), waker: None, image_loading: ImageLoading::default(), isolation: Isolation::default(), fonts: None }
    }
}

/// Parameters for a new page.
#[derive(Debug, Clone)]
pub struct PageParams {
    pub viewport: Viewport,
    /// Milestone 36 (ADR 010): what the page's scripts may reach. A page
    /// granted nothing can neither `fetch` nor use `localStorage`; the
    /// engine itself still loads the documents and subresources the host
    /// asks for. Grants come from the profile (`browser_core`).
    pub grants: PageGrants,
    /// Milestone 37: what the page's scripts may observe of the machine —
    /// the clock's resolution, the timezone, which host fonts are used
    /// (ADR 019 §3).
    pub fingerprint: FingerprintPolicy,
}

impl PageParams {
    /// A page of this size whose scripts are granted nothing, under the
    /// standard fingerprint policy.
    pub fn new(viewport: Viewport) -> PageParams {
        PageParams { viewport, grants: PageGrants::none(), fingerprint: FingerprintPolicy::standard() }
    }

    pub fn with_grants(self, grants: PageGrants) -> PageParams {
        PageParams { grants, ..self }
    }

    pub fn with_fingerprint(self, fingerprint: FingerprintPolicy) -> PageParams {
        PageParams { fingerprint, ..self }
    }
}

/// The host's fonts as a page may see them: only the families its
/// fingerprint policy admits (Milestone 37). The bundled font and a
/// document's web fonts are not filtered — they say nothing about the
/// machine.
struct PolicyFonts {
    inner: Arc<dyn text::FontSource>,
    policy: FingerprintPolicy,
}

impl PolicyFonts {
    fn allowed(&self, entry: &text::CatalogEntry) -> bool {
        entry.families.iter().any(|f| self.policy.font_allowed(f))
    }
}

impl text::FontSource for PolicyFonts {
    fn catalog(&self) -> Vec<text::CatalogEntry> {
        self.inner.catalog().into_iter().filter(|e| self.allowed(e)).collect()
    }
    fn load(&self, entry: &text::CatalogEntry) -> Option<Vec<u8>> {
        if self.allowed(entry) {
            self.inner.load(entry)
        } else {
            None
        }
    }
}

/// `fonts` as a page under `policy` may see them.
fn fonts_for(fonts: &Option<Arc<dyn text::FontSource>>, policy: FingerprintPolicy) -> Option<Arc<dyn text::FontSource>> {
    let fonts = fonts.as_ref()?;
    Some(match policy.fonts {
        FontExposure::All => Arc::clone(fonts),
        FontExposure::Standard => Arc::new(PolicyFonts { inner: Arc::clone(fonts), policy }),
    })
}

/// What to load into a page.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadRequest {
    /// Fetch a URL (a typed/startup navigation: no initiating document), on
    /// a background thread; the current document stays live until it lands.
    Url(String),
    /// Load an HTML string directly. `url` is the document's address, if it
    /// has one — it decides the origin (none: an opaque origin) and resolves
    /// relative links. Linked stylesheets and images are not fetched; inline
    /// `<style>` and `<script>` are applied.
    Html { html: String, url: Option<String> },
}

/// Input, already normalised by the host (ADR 005): positions in viewport
/// CSS pixels, scroll deltas in CSS pixels with positive `dy` meaning "move
/// down the document". Natural scrolling and scroll speed are the host's
/// input transform, never the engine's.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineInput {
    PointerMove { x: f32, y: f32 },
    /// The primary button (or a touch) went down at this position.
    PointerDown { x: f32, y: f32 },
    /// The primary button (or the touch) came up (M4's gap: it ends
    /// `:active`). The click itself is the press.
    PointerUp { x: f32, y: f32 },
    Scroll { dx: f32, dy: f32 },
    /// A key press, as a DOM `KeyboardEvent.key` value (`"a"`, `"Enter"`,
    /// `"Tab"`, `"ArrowLeft"` …).
    Key { key: String },
}

/// A command for one page.
#[derive(Debug, Clone, PartialEq)]
pub enum PageCommand {
    Load(LoadRequest),
    Input(EngineInput),
    Resize(Viewport),
    /// Diagnostic, like a browser's `about:crash`: panics the page's thread
    /// on purpose so crash containment is exercised, not assumed (ADR 007).
    /// The page reports [`EngineEvent::PageCrashed`]; nothing else is
    /// affected. Page content can never send this — only a host can.
    SimulateCrash,
    /// Milestone 36 (ADR 010 §4): takes back the page's grant of this kind.
    /// Every handle of it — the scripts of the document showing now, and of
    /// every later one — refuses from the next tick on; a revoked network
    /// grant also refuses the page's requests in flight. Never dropped from
    /// a full inbox. (In thread mode [`Engine::send`] also revokes the shared
    /// grant at once.)
    Revoke(CapabilityKind),
    /// M36's gap (ADR 010 §4): grants the page these handles again, after a
    /// revoke or for the first time. Each kind present replaces the page's
    /// grant of that kind — a fresh token, so handles revoked before stay
    /// revoked — for the document showing now (its script's storage
    /// narrowed to its own key) and every later one. Minted by the host's
    /// Browser Core (`Session::grant`); a renderer process receives the
    /// kinds and mints its own handles (ADR 010 §2). Never dropped from a
    /// full inbox.
    Grant(PageGrants),
    /// Milestone 38b (ADR 020): whether anyone can see the page. A hidden
    /// page runs no `requestAnimationFrame` callbacks, advances no CSS
    /// animation and builds no frames, but still lands navigations and
    /// settles fetches; shown again, it draws at once. Pages start visible.
    /// Never dropped from a full inbox.
    SetVisible(bool),
}

/// How much layout work a frame did (Milestone 31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutWork {
    /// Nothing that affects layout changed (a scroll, caret or focus
    /// frame): the previous layout was reused.
    Reused,
    /// Layout ran for the whole page, producing this many boxes.
    Full { boxes: usize },
    /// Layout ran for `laid_out` elements (the changed ones and their
    /// ancestors); `reused_boxes` boxes were moved over unchanged from the
    /// previous layout.
    Incremental { laid_out: usize, reused_boxes: usize },
}

/// What building one frame cost — per page and per frame, so a host or a
/// test can see the work a change caused (Milestone 31,
/// `docs/adr/008-dirty-tracking-and-identity.md`). Counters, not timings:
/// deterministic, comparable across machines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStats {
    /// Elements the cascade ran for (0 when nothing relevant changed).
    pub restyled: usize,
    /// Whether the restyle was a full one (first frame, new stylesheet,
    /// viewport width with `@media` rules, a mutation storm).
    pub full_restyle: bool,
    /// Elements whose final style a transition or animation changed.
    pub animated: usize,
    /// Milestone 34: transform layers the compositor moved this frame —
    /// transform-only animation of promoted elements, drawn without layout
    /// or a display-list build (see `display_list_built`).
    pub composited: usize,
    pub layout: LayoutWork,
    /// Display items drawn, after culling.
    pub display_items: usize,
    /// Display items dropped because they were entirely off screen.
    pub culled: usize,
    /// Whether this frame built display items from the layout tree. A frame
    /// that only scrolls or moves the caret or focus ring draws from the
    /// page's display list for its layout instead (performance audit) —
    /// unless the page has a `position: sticky` box.
    pub display_list_built: bool,
    /// What changed on screen since the page's previous frame.
    pub damage: Damage,
    /// Milestone 38b (ADR 020): the animation interval the page was running
    /// at when it drew this frame, in milliseconds — 17 at the full 60 Hz;
    /// 33, 67, 133 or 250 when an animating page could not keep up.
    pub animation_interval_ms: u32,
}

/// Something a page did.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// A finished frame, and what it cost. The host presents the newest one
    /// per page.
    Frame { page: PageId, scene: Scene, stats: FrameStats },
    NavigationStarted { page: PageId, url: String },
    /// A new document is now the page's document. `url` is its address —
    /// for a fetched document, the URL it landed on after any redirects —
    /// or `None` for an HTML string loaded without one.
    NavigationCommitted { page: PageId, url: Option<String> },
    /// The navigation did not produce a document; the old one stays.
    NavigationFailed { page: PageId, url: String, error: String },
    /// The page's thread panicked (or could not be started). The page shows
    /// nothing further and ignores later commands; close it, or create a new
    /// page to "reload". Every other page is unaffected (ADR 007).
    PageCrashed { page: PageId, reason: String },
}

/// The host's handle on one page thread.
struct PageHandle {
    mailbox: Arc<Mailbox>,
    /// Dropped (detached) on close, never joined on the host thread: a page
    /// stuck in a script must not be able to block closing it.
    _thread: Option<std::thread::JoinHandle<()>>,
    /// The page's status as of the last collection.
    status: PageStatus,
    /// A tick posted and not yet finished.
    tick_in_flight: Option<Instant>,
    crash_reported: bool,
    /// In process mode, the page's renderer (dropping it ends the process).
    process: Option<process::ProcessPage>,
    /// What the host granted the page. In thread mode the page holds the
    /// same handles, so revoking here takes effect at once.
    grants: PageGrants,
}

/// The engine: every page of one profile, each on its own thread.
pub struct Engine {
    fonts: Option<Arc<dyn text::FontSource>>,
    network: NetworkContext,
    waker: WakerSlot,
    image_loading: ImageLoading,
    isolation: Isolation,
    /// Process mode: renderers started ahead of need, so a new page — or a
    /// page moving to a renderer for another site (Milestone 41) — does not
    /// wait for a process to start (and parse its font).
    spares: Option<Arc<process::Spares>>,
    unresponsive_after: Duration,
    pages: BTreeMap<PageId, PageHandle>,
    next_page: u64,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Engine {
        // The `rayon` pool that style resolution, intrinsic sizing and image
        // decoding share (intra-page data parallelism, ADR 006 §7). Built
        // here so its threads are named for what they are; otherwise they
        // are created lazily from the first page thread that uses them and,
        // on Linux, inherit its name — every worker showed up as `page-1`.
        // Fails harmlessly if the pool already exists (a second engine).
        let _ = rayon::ThreadPoolBuilder::new().thread_name(|i| format!("worker-{i}")).build_global();
        let spares = match &config.isolation {
            Isolation::Processes(launcher) => Some(process::Spares::new(Arc::clone(launcher))),
            Isolation::Threads => None,
        };
        Engine {
            fonts: config.fonts,
            network: config.network,
            waker: Arc::new(Mutex::new(config.waker)),
            image_loading: config.image_loading,
            isolation: config.isolation,
            spares,
            unresponsive_after: UNRESPONSIVE_AFTER,
            pages: BTreeMap::new(),
            next_page: 1,
        }
    }

    /// Installs (or replaces) the waker for every page, current and future.
    pub fn set_waker(&mut self, waker: Waker) {
        *self.waker.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waker);
    }

    /// Changes the unresponsive bound ([`UNRESPONSIVE_AFTER`] by default) for
    /// pages created from now on. Process mode only (ADR 013).
    pub fn set_unresponsive_after(&mut self, bound: Duration) {
        self.unresponsive_after = bound;
    }

    /// A new page, on its own thread, showing an empty document (with an
    /// opaque origin) until something is loaded into it. If the operating
    /// system refuses a new thread, the page reports
    /// [`EngineEvent::PageCrashed`] at the next update.
    pub fn create_page(&mut self, params: PageParams) -> PageId {
        let id = PageId(self.next_page);
        self.next_page += 1;
        let mailbox = Arc::new(Mailbox::default());
        let (mut thread, mut process) = (None, None);
        match &self.isolation {
            Isolation::Threads => {
                let setup = PageSetup {
                    network: self.network.clone(),
                    grants: params.grants.clone(),
                    fingerprint: params.fingerprint,
                    fonts: fonts_for(&self.fonts, params.fingerprint),
                    viewport: params.viewport.sanitized(),
                    waker: Arc::clone(&self.waker),
                    image_loading: self.image_loading,
                };
                match page_thread::spawn(id, Arc::clone(&mailbox), setup) {
                    Ok(handle) => thread = Some(handle),
                    Err(e) => mailbox.mark_crashed(format!("could not start the page's thread: {e}")),
                }
            }
            Isolation::Processes(launcher) => {
                let spares = Arc::clone(self.spares.get_or_insert_with(|| process::Spares::new(Arc::clone(launcher))));
                let setup = process::ProcessSetup {
                    spares,
                    network: self.network.clone(),
                    viewport: params.viewport.sanitized(),
                    image_loading: self.image_loading,
                    waker: Arc::clone(&self.waker),
                    fonts: fonts_for(&self.fonts, params.fingerprint),
                    grants: params.grants.clone(),
                    fingerprint: params.fingerprint,
                    unresponsive_after: self.unresponsive_after,
                };
                // The renderer is the spare (the next one starts behind it).
                match process::spawn(id, Arc::clone(&mailbox), setup) {
                    Ok(page) => process = Some(page),
                    Err(e) => mailbox.mark_crashed(format!("could not start the page's renderer process: {e}")),
                }
            }
        }
        // Due immediately: the first tick draws the (blank) first frame.
        let status = PageStatus { next_wake: Some(Instant::now()), ..PageStatus::default() };
        self.pages.insert(id, PageHandle { mailbox, _thread: thread, status, tick_in_flight: None, crash_reported: false, process, grants: params.grants });
        id
    }

    /// Shuts the page down. Its thread drops the page — abandoning its
    /// in-flight fetches and navigation — as soon as it is not busy; the
    /// host does not wait for that.
    pub fn close_page(&mut self, page: PageId) {
        if let Some(handle) = self.pages.remove(&page) {
            handle.mailbox.shutdown();
        }
    }

    /// The capabilities `page` holds now (granted and not revoked).
    pub fn page_grants(&self, page: PageId) -> Vec<CapabilityKind> {
        self.pages.get(&page).map_or_else(Vec::new, |h| h.grants.kinds())
    }

    /// The operating-system process id of `page`'s renderer (process mode).
    pub fn renderer_pid(&self, page: PageId) -> Option<u32> {
        self.pages.get(&page)?.process.as_ref()?.pid()
    }

    /// A number identifying `page`'s current renderer (process mode): the
    /// page gets a new one when it moves to a renderer for another site
    /// (Milestone 41, ADR 025) — a same-site navigation keeps it. Diagnostics
    /// and tests; meaningful only within this process.
    pub fn renderer_serial(&self, page: PageId) -> Option<u64> {
        self.pages.get(&page)?.process.as_ref()?.serial()
    }

    /// Whether `page`'s renderer reported running sandboxed, and how
    /// (process mode; `None` until it has reported).
    pub fn renderer_sandbox(&self, page: PageId) -> Option<(bool, String)> {
        self.pages.get(&page)?.process.as_ref()?.sandbox()
    }

    pub fn page_ids(&self) -> Vec<PageId> {
        self.pages.keys().copied().collect()
    }

    /// How many commands are waiting in `page`'s inbox (diagnostics; never
    /// more than [`MAX_QUEUED_COMMANDS`]).
    pub fn queued_commands(&self, page: PageId) -> usize {
        self.pages.get(&page).map_or(0, |h| h.mailbox.queued_commands())
    }

    /// Queues `command` for `page`; it takes effect at the page's next tick,
    /// in the order sent (with pointer moves, scrolls and resizes
    /// coalesced). Never blocks. A command for a page that does not exist
    /// (closed, or never created) or has crashed is dropped.
    pub fn send(&mut self, page: PageId, command: PageCommand) {
        if let Some(handle) = self.pages.get_mut(&page) {
            match &command {
                PageCommand::Revoke(kind) => handle.grants.revoke(*kind),
                PageCommand::Grant(grants) => handle.grants.merge(grants.clone()),
                _ => {}
            }
            if handle.status.crashed.is_none() {
                handle.mailbox.post(command);
            }
        }
    }

    /// Collects what the pages have finished, then asks every page that is
    /// due (queued commands, an animation frame, a fetch to poll) to advance
    /// to `now`. Never waits for a page: results of the ticks posted here
    /// arrive in a later `update` (the waker says when). Use
    /// [`Engine::update_blocking`] to wait for them.
    pub fn update(&mut self, now: Instant) -> Vec<EngineEvent> {
        let mut events = self.collect();
        for handle in self.pages.values_mut() {
            if handle.status.crashed.is_some() {
                continue;
            }
            let due = handle.mailbox.queued_commands() > 0 || handle.status.next_wake.is_some_and(|at| at <= now);
            if due {
                handle.mailbox.post_tick(now);
                handle.tick_in_flight = Some(now);
                handle.status.next_wake = None;
            }
        }
        events.extend(self.collect_crashes());
        events
    }

    /// Like [`Engine::update`], then waits — at most `timeout` in total —
    /// for the pages it ticked to finish, and returns their results too. What
    /// a headless host or a test wants; a desktop host should not block.
    pub fn update_blocking(&mut self, now: Instant, timeout: Duration) -> Vec<EngineEvent> {
        let mut events = self.update(now);
        let deadline = Instant::now() + timeout;
        for handle in self.pages.values() {
            if let Some(tick) = handle.tick_in_flight {
                handle.mailbox.wait_for_tick(tick, deadline);
            }
        }
        events.extend(self.collect());
        events.extend(self.collect_crashes());
        events
    }

    /// When the host should next call [`Engine::update`] if nothing else
    /// happens: `None` means "not until something happens" (input, a resize,
    /// or the waker). Background work a page waits for — a response, a
    /// decode, a navigation — is not polled: it calls the waker when it
    /// finishes (ADR 020), so a host without a waker must call `update` from
    /// time to time while pages load. Without a waker, a tick in flight is
    /// polled for.
    pub fn next_wake(&self) -> Option<Instant> {
        let has_waker = self.waker.lock().map(|w| w.is_some()).unwrap_or(true);
        self.pages
            .values()
            .filter(|h| h.status.crashed.is_none())
            .filter_map(|h| {
                // Queued commands need a tick — unless one is posted already
                // (Milestone 38b: the host used to spin on `update` until the
                // page thread picked it up).
                if h.mailbox.queued_commands() > 0 && !h.mailbox.tick_posted() {
                    Some(Instant::now())
                } else if let Some(tick) = h.tick_in_flight {
                    (!has_waker).then(|| tick.max(Instant::now()) + Duration::from_millis(1))
                } else if h.mailbox.wants_tick() {
                    // Finished background work, not yet collected (ADR 020).
                    Some(Instant::now())
                } else {
                    h.status.next_wake
                }
            })
            .min()
    }

    /// Takes every page's published events and newest frame.
    fn collect(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        for (&id, handle) in self.pages.iter_mut() {
            let (published, frame, status) = handle.mailbox.collect();
            events.extend(published);
            if let Some((scene, stats)) = frame {
                events.push(EngineEvent::Frame { page: id, scene, stats });
            }
            if handle.tick_in_flight.is_some_and(|tick| status.completed_tick.is_some_and(|done| done >= tick)) {
                handle.tick_in_flight = None;
            }
            // `next_wake` is re-armed from the page's report only when no tick
            // is in flight (the report describes the state after its tick) —
            // and only once the page has reported at all: until its first
            // tick, the page is due at creation (the blank first frame), and
            // an empty report must not cancel that.
            let next_wake = match (handle.tick_in_flight, status.completed_tick) {
                (Some(_), _) => None,
                (None, None) => handle.status.next_wake,
                (None, Some(_)) => status.next_wake,
            };
            handle.status = PageStatus { next_wake, ..status };
        }
        events
    }

    /// A `PageCrashed` event for each page that crashed since last asked.
    fn collect_crashes(&mut self) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        for (&id, handle) in self.pages.iter_mut() {
            if handle.crash_reported {
                continue;
            }
            if let Some(reason) = &handle.status.crashed {
                handle.crash_reported = true;
                handle.tick_in_flight = None;
                events.push(EngineEvent::PageCrashed { page: id, reason: reason.clone() });
            }
        }
        events
    }
}

impl Drop for Engine {
    /// Every page thread is told to finish (not waited for); an unused
    /// spare renderer is ended.
    fn drop(&mut self) {
        if let Some(spares) = &self.spares {
            spares.close();
        }
        for handle in self.pages.values() {
            handle.mailbox.shutdown();
        }
    }
}

// Compile-time proof that everything crossing the API can cross a thread
// (ADR 005, point 3) — Milestone 30 depends on it.
const _: () = {
    const fn assert_send<T: Send + 'static>() {}
    assert_send::<PageId>();
    assert_send::<Viewport>();
    assert_send::<PageParams>();
    assert_send::<LoadRequest>();
    assert_send::<EngineInput>();
    assert_send::<PageCommand>();
    assert_send::<EngineEvent>();
    assert_send::<Scene>();
    assert_send::<EngineConfig>();
    assert_send::<Waker>();
    assert_send::<FrameStats>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A font source with one font per family name given.
    struct Families(Vec<&'static str>);

    impl text::FontSource for Families {
        fn catalog(&self) -> Vec<text::CatalogEntry> {
            self.0
                .iter()
                .map(|f| text::CatalogEntry { key: f.to_string(), index: 0, families: vec![f.to_ascii_lowercase()], weight: 400, style: text::FontStyle::Normal, langs: Vec::new() })
                .collect()
        }
        fn load(&self, _entry: &text::CatalogEntry) -> Option<Vec<u8>> {
            Some(vec![1])
        }
    }

    #[test]
    fn the_standard_font_set_hides_other_system_fonts_from_a_private_page() {
        let source: Option<Arc<dyn text::FontSource>> = Some(Arc::new(Families(vec!["DejaVu Sans", "Noto Sans Devanagari", "Corporate Sans", "Comic Sans MS"])));
        let all = fonts_for(&source, FingerprintPolicy::standard()).unwrap();
        assert_eq!(all.catalog().len(), 4);
        let private = fonts_for(&source, FingerprintPolicy::private()).unwrap();
        let names: Vec<String> = private.catalog().into_iter().map(|e| e.key).collect();
        assert_eq!(names, ["DejaVu Sans", "Noto Sans Devanagari"]);
        let hidden = text::CatalogEntry { key: "Corporate Sans".into(), index: 0, families: vec!["corporate sans".into()], weight: 400, style: text::FontStyle::Normal, langs: Vec::new() };
        assert_eq!(private.load(&hidden), None, "a hidden font cannot be loaded by key either");
    }
}
