//! The host side of process isolation (ADR 016).
//!
//! A page in process mode keeps the same mailbox as a page thread (ADR 006):
//! the engine drives it identically. Behind it, instead of the page loop,
//! run two threads — a *bridge* that sends the mailbox's work (commands,
//! ticks, shutdown) to the page's renderer process, and a *reader* that
//! publishes what the renderer sends back. Everything read from a renderer
//! is decoded by the bounded codec in `ipc` and treated as hostile: a
//! malformed message ends the page as crashed.
//!
//! Starting a process is not something the engine does on its own: the host
//! grants it as a capability, a [`RendererLauncher`] (the `renderer` crate
//! provides the one that starts the `browser-renderer` binary).

use crate::ipc::{self, ToHost, ToRenderer, WireContext, WireFrame};
use crate::page_thread::{Mailbox, WakerSlot};
use crate::renderer::{from_nanos, to_nanos};
use crate::{EngineEvent, FrameStats, ImageLoading, LoadRequest, PageCommand, PageId, Scene, Viewport};
use diagnostics::Guard;
use network::{Initiator, NetworkContext, NetworkError, RequestContext};
use origin::Origin;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Starts renderer processes — a capability the host grants the engine.
pub trait RendererLauncher: Send + Sync {
    fn launch(&self) -> std::io::Result<RendererConnection>;
}

/// A started renderer: its two pipes and a handle to end it.
pub struct RendererConnection {
    pub to_renderer: Box<dyn Write + Send>,
    pub from_renderer: Box<dyn Read + Send>,
    pub control: Box<dyn RendererControl>,
}

/// Control over one renderer process.
pub trait RendererControl: Send {
    /// Ends the renderer now (idempotent); reaps it.
    fn terminate(&mut self);
    /// How it ended, if it has — without waiting long.
    fn exit_status(&mut self) -> Option<String>;
    /// Its operating-system process id, if it has one.
    fn pid(&self) -> Option<u32>;
}

/// Where a page's engine state lives.
#[derive(Clone, Default)]
pub enum Isolation {
    /// On a thread of the host process (ADR 006).
    #[default]
    Threads,
    /// In a renderer process of its own, started with this launcher (ADR 016).
    Processes(Arc<dyn RendererLauncher>),
}

/// The largest total of image bytes a host holds for one renderer.
const MAX_IMAGE_BYTES_PER_PAGE: usize = 1024 * 1024 * 1024;

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
type SharedControl = Arc<Mutex<Box<dyn RendererControl>>>;

fn send(out: &SharedWriter, message: &ToRenderer) -> bool {
    let bytes = message.encode();
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    ipc::write_frame(&mut *out, &bytes).is_ok()
}

fn wake(slot: &WakerSlot) {
    let waker = slot.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if let Some(waker) = waker {
        waker();
    }
}

/// What the engine keeps of a page's renderer.
pub(crate) struct ProcessPage {
    control: SharedControl,
    pid: Option<u32>,
    sandboxed: Arc<Mutex<Option<(bool, String)>>>,
}

impl ProcessPage {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Whether the renderer reported running sandboxed, and how.
    pub fn sandbox(&self) -> Option<(bool, String)> {
        self.sandboxed.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for ProcessPage {
    fn drop(&mut self) {
        self.control.lock().unwrap_or_else(PoisonError::into_inner).terminate();
    }
}

pub(crate) struct ProcessSetup {
    pub launcher: Arc<dyn RendererLauncher>,
    /// A renderer started ahead of need, used instead of launching one.
    pub spare: Option<RendererConnection>,
    pub network: NetworkContext,
    pub viewport: Viewport,
    pub image_loading: ImageLoading,
    pub waker: WakerSlot,
    /// Milestone 35: the host's fonts, served to the renderer by key.
    pub fonts: Option<Arc<dyn text::FontSource>>,
    /// Milestone 36: the page's grants, as the renderer protocol lists them.
    pub grants: u8,
    /// Milestone 37: what the page's scripts may observe.
    pub fingerprint: privacy::FingerprintPolicy,
    /// Milestone 38a: how long the renderer may owe a tick without progress
    /// before it is ended (ADR 013).
    pub unresponsive_after: Duration,
}

/// What the watchdog knows of a renderer's progress (ADR 013): believed only
/// from what the host itself sent and received.
struct Progress {
    /// The latest tick sent to the renderer.
    sent: Option<Instant>,
    /// The latest tick it reported finished.
    done: Option<Instant>,
    /// When it last finished a tick, or started owing one.
    since: Instant,
}

impl Progress {
    fn owes_a_tick(&self) -> bool {
        self.sent.is_some_and(|sent| self.done.is_none_or(|done| done < sent))
    }
}

type SharedProgress = Arc<Mutex<Progress>>;

/// How often the watchdog looks: at least once a second, and four times per
/// bound for short bounds (tests).
fn watchdog_interval(bound: Duration) -> Duration {
    (bound / 4).clamp(Duration::from_millis(10), Duration::from_secs(1))
}

/// Starts page `id`'s renderer and the two threads that connect it to
/// `mailbox`.
pub(crate) fn spawn(id: PageId, mailbox: Arc<Mailbox>, setup: ProcessSetup) -> std::io::Result<ProcessPage> {
    let connection = match setup.spare {
        Some(spare) => spare,
        None => setup.launcher.launch()?,
    };
    let RendererConnection { to_renderer, from_renderer, control } = connection;
    let pid = control.pid();
    let out: SharedWriter = Arc::new(Mutex::new(to_renderer));
    let control: SharedControl = Arc::new(Mutex::new(control));
    let epoch = Instant::now();
    let lock = Arc::new(Mutex::new(SiteLock::default()));
    let sandboxed = Arc::new(Mutex::new(None));
    let progress: SharedProgress = Arc::new(Mutex::new(Progress { sent: None, done: None, since: Instant::now() }));
    let catalog = setup.fonts.as_ref().map(|f| f.catalog()).unwrap_or_default();
    if !send(&out, &ToRenderer::Setup { viewport: setup.viewport, image_loading: setup.image_loading, fonts: catalog.clone(), grants: setup.grants, fingerprint: setup.fingerprint }) {
        control.lock().unwrap_or_else(PoisonError::into_inner).terminate();
        return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the renderer did not start"));
    }

    // Bridge: the mailbox's work, to the renderer.
    {
        let (mailbox, out, control, lock, progress) = (Arc::clone(&mailbox), Arc::clone(&out), Arc::clone(&control), Arc::clone(&lock), Arc::clone(&progress));
        std::thread::Builder::new().name(format!("bridge-{}", id.0)).spawn(move || {
            while let Some((commands, tick)) = mailbox.wait_for_work() {
                for command in commands {
                    if let PageCommand::Load(LoadRequest::Url(url)) = &command {
                        // The one browser-initiated navigation this page may make.
                        lock.lock().unwrap_or_else(PoisonError::into_inner).expect_typed(url);
                    }
                    if !send(&out, &ToRenderer::Command(command)) {
                        return; // the reader reports why
                    }
                }
                {
                    let mut progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
                    if !progress.owes_a_tick() {
                        progress.since = Instant::now(); // it starts owing now
                    }
                    progress.sent = Some(progress.sent.map_or(tick, |t| t.max(tick)));
                }
                if !send(&out, &ToRenderer::Tick(to_nanos(epoch, tick))) {
                    return;
                }
            }
            // Shut down: tell it, then make sure.
            send(&out, &ToRenderer::Shutdown);
            control.lock().unwrap_or_else(PoisonError::into_inner).terminate();
        })?;
    }

    // Reader: the renderer's results, into the mailbox.
    {
        let (mailbox, out, control, sandboxed, progress) = (Arc::clone(&mailbox), Arc::clone(&out), Arc::clone(&control), Arc::clone(&sandboxed), Arc::clone(&progress));
        let (network, waker) = (setup.network, Arc::clone(&setup.waker));
        let setup_fonts = setup.fonts;
        let mut input = from_renderer;
        std::thread::Builder::new().name(format!("reader-{}", id.0)).spawn(move || {
            let mut frames = FrameReceiver::default();
            let crash = |reason: String| {
                control.lock().unwrap_or_else(PoisonError::into_inner).terminate();
                mailbox.mark_crashed(reason);
                wake(&waker);
            };
            loop {
                let bytes = match ipc::read_frame(&mut input) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) | Err(_) => {
                        if mailbox.is_shut_down() || mailbox.is_crashed() {
                            return; // closed on purpose, or ended by the watchdog
                        }
                        let status = control.lock().unwrap_or_else(PoisonError::into_inner).exit_status();
                        diagnostics::guard(Guard::PageCrashed, "a renderer process ended");
                        return crash(format!("the renderer process ended ({})", status.unwrap_or_else(|| "no exit status".into())));
                    }
                };
                let message = match ToHost::decode(&bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        diagnostics::guard(Guard::RendererMessageRejected, e.0);
                        return crash(format!("the renderer sent a malformed message: {}", e.0));
                    }
                };
                match message {
                    ToHost::Published { events, frame, next_wake, completed_tick } => {
                        let events: Vec<EngineEvent> = events.into_iter().map(|e| with_page(e, id)).collect();
                        let frame = match frame.map(|f| frames.rebuild(*f)) {
                            Some(Err(reason)) => {
                                diagnostics::guard(Guard::RendererMessageRejected, reason);
                                return crash(format!("the renderer sent a malformed frame: {reason}"));
                            }
                            Some(Ok(frame)) => Some(frame),
                            None => None,
                        };
                        let completed = from_nanos(epoch, completed_tick);
                        {
                            let mut progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
                            // Only a tick finished counts as progress: a renderer
                            // repeating an old report cannot hold off the
                            // watchdog (ADR 013).
                            if progress.done.is_none_or(|done| completed > done) {
                                progress.done = Some(completed);
                                progress.since = Instant::now();
                            }
                        }
                        mailbox.publish(events, frame, next_wake.map(|n| from_nanos(epoch, n)), completed);
                        wake(&waker);
                    }
                    ToHost::Image { id: image, width, height, rgba } => {
                        if let Err(reason) = frames.store_image(image, width, height, rgba) {
                            diagnostics::guard(Guard::RendererMessageRejected, reason);
                            return crash(format!("the renderer sent too much image data: {reason}"));
                        }
                    }
                    ToHost::Crashed { reason } => return crash(reason),
                    ToHost::FetchRequest { id: request, url, context } => {
                        let checked = lock.lock().unwrap_or_else(PoisonError::into_inner).check(&url, &context);
                        match checked {
                            Err(reason) => {
                                diagnostics::guard(Guard::RendererRequestRefused, reason);
                                send(&out, &ToRenderer::FetchReply { id: request, result: Err(NetworkError::Transport(format!("refused: {reason}"))) });
                            }
                            Ok(ctx) => {
                                let (out, lock, network) = (Arc::clone(&out), Arc::clone(&lock), network.clone());
                                // On the bounded fetch pool, as any fetch.
                                let submitted = network::pool::FetchPool::global().submit(
                                    network::pool::host_of(&url),
                                    Box::new(move || {
                                        let result = network.fetch_with(&url, &ctx);
                                        if let (Ok(response), true) = (&result, ctx.is_navigation) {
                                            lock.lock().unwrap_or_else(PoisonError::into_inner).document(&response.final_url);
                                        }
                                        send(&out, &ToRenderer::FetchReply { id: request, result });
                                    }),
                                );
                                if submitted.is_err() {
                                    diagnostics::guard(Guard::FetchQueueFull, "too many fetches waiting; request refused");
                                }
                            }
                        }
                    }
                    ToHost::FontRequest { id: request, key, index } => {
                        // Only a font of the catalog this renderer was given.
                        let entry = catalog.iter().find(|e| e.key == key && e.index == index).cloned();
                        let (reply_out, source) = (Arc::clone(&out), setup_fonts.clone());
                        let spawned = std::thread::Builder::new().name("font-load".into()).spawn(move || {
                            let bytes = entry.and_then(|e| source.and_then(|s| s.load(&e))).filter(|b| b.len() <= text::MAX_FONT_BYTES);
                            send(&reply_out, &ToRenderer::FontReply { id: request, bytes });
                        });
                        if spawned.is_err() {
                            send(&out, &ToRenderer::FontReply { id: request, bytes: None });
                        }
                    }
                    ToHost::Started { sandboxed: on, detail } => {
                        diagnostics::tracing::info!(target: "engine::process", page = id.0, sandboxed = on, detail = %detail, "renderer started");
                        *sandboxed.lock().unwrap_or_else(PoisonError::into_inner) = Some((on, detail));
                    }
                }
            }
        })?;
    }
    // Watchdog (ADR 013): a renderer that owes a tick and makes no progress
    // for `unresponsive_after` is ended. Nothing the renderer says pauses it.
    {
        let (mailbox, control, waker, bound) = (Arc::clone(&mailbox), Arc::clone(&control), Arc::clone(&setup.waker), setup.unresponsive_after);
        std::thread::Builder::new().name(format!("watchdog-{}", id.0)).spawn(move || loop {
            std::thread::sleep(watchdog_interval(bound));
            if mailbox.is_shut_down() || mailbox.is_crashed() {
                return;
            }
            let stalled = {
                let progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
                progress.owes_a_tick() && progress.since.elapsed() >= bound
            };
            if stalled {
                diagnostics::guard(Guard::PageUnresponsive, "a renderer made no progress for the unresponsive bound; it was ended");
                mailbox.mark_crashed(format!("the page stopped responding for {} s; its renderer was ended", bound.as_secs_f32()));
                control.lock().unwrap_or_else(PoisonError::into_inner).terminate();
                wake(&waker);
                return;
            }
        })?;
    }
    Ok(ProcessPage { control, pid, sandboxed })
}

fn with_page(event: EngineEvent, page: PageId) -> EngineEvent {
    match event {
        EngineEvent::NavigationStarted { url, .. } => EngineEvent::NavigationStarted { page, url },
        EngineEvent::NavigationCommitted { url, .. } => EngineEvent::NavigationCommitted { page, url },
        EngineEvent::NavigationFailed { url, error, .. } => EngineEvent::NavigationFailed { page, url, error },
        EngineEvent::PageCrashed { reason, .. } => EngineEvent::PageCrashed { page, reason },
        EngineEvent::Frame { scene, stats, .. } => EngineEvent::Frame { page, scene, stats },
    }
}

/// Rebuilds scenes from what the renderer sends: the atlas bands and images
/// it sent before are kept here.
#[derive(Default)]
struct FrameReceiver {
    atlas_dims: (u32, u32),
    bands: Vec<Option<Arc<Vec<u8>>>>,
    images: HashMap<u64, (u32, u32, Arc<Vec<u8>>)>,
    image_bytes: usize,
    /// The sequence given to the last frame rebuilt: a renderer's frame
    /// numbers are its own, so the host numbers frames again, and a frame's
    /// damage is relative to the one before it.
    last_sequence: Option<u64>,
}

impl FrameReceiver {
    fn store_image(&mut self, id: u64, width: u32, height: u32, rgba: Vec<u8>) -> Result<(), &'static str> {
        if let Some((_, _, old)) = self.images.remove(&id) {
            self.image_bytes -= old.len();
        }
        if self.image_bytes + rgba.len() > MAX_IMAGE_BYTES_PER_PAGE {
            return Err("more than the per-page image limit");
        }
        self.image_bytes += rgba.len();
        self.images.insert(id, (width, height, Arc::new(rgba)));
        Ok(())
    }

    fn rebuild(&mut self, frame: WireFrame) -> Result<(Scene, FrameStats), &'static str> {
        let dims = (frame.atlas_width, frame.atlas_height);
        if dims != self.atlas_dims {
            self.atlas_dims = dims;
            self.bands = vec![None; (dims.1 as usize).div_ceil(text::ATLAS_BAND_ROWS)];
        }
        for (index, bytes) in frame.atlas_bands {
            // The codec checked the index against these dimensions.
            self.bands[index as usize] = Some(Arc::new(bytes));
        }
        let bands: Option<Vec<Arc<Vec<u8>>>> = self.bands.iter().cloned().collect();
        let pixels = bands
            .and_then(|b| text::AtlasPixels::from_bands(dims.0 as usize, dims.1 as usize, b))
            .ok_or("a glyph-atlas band was never sent")?;
        let mut images = Vec::with_capacity(frame.images.len());
        for (id, width, height) in &frame.images {
            match self.images.get(id) {
                Some((w, h, rgba)) if w == width && h == height => {
                    images.push(rendering::ImageResource { id: *id, width: *width, height: *height, rgba: Arc::clone(rgba) });
                }
                _ => return Err("a frame uses an image that was never sent"),
            }
        }
        // Keep only what this frame uses (the renderer does the same).
        let used: std::collections::HashSet<u64> = frame.images.iter().map(|(id, _, _)| *id).collect();
        let mut freed = 0;
        self.images.retain(|id, (_, _, rgba)| {
            let keep = used.contains(id);
            if !keep {
                freed += rgba.len();
            }
            keep
        });
        self.image_bytes -= freed;
        let scene = Scene {
            viewport_width: frame.viewport_width,
            viewport_height: frame.viewport_height,
            background: frame.background,
            items: Arc::new(frame.items),
            glyph_atlas: rendering::GlyphAtlasSnapshot {
                width: dims.0 as usize,
                height: dims.1 as usize,
                generation: frame.atlas_generation,
                pixels,
            },
            images,
            damage: frame.damage,
            sequence: Scene::next_sequence(),
            damage_base: if frame.damage == rendering::Damage::Full { None } else { self.last_sequence },
        };
        self.last_sequence = Some(scene.sequence);
        Ok((scene, frame.stats))
    }
}

/// What a page's renderer may ask the network for (ADR 016 §5). The renderer
/// states each request's initiator; the host believes only what it has seen:
///
/// - a request with **no initiator** (a typed URL: no `SameSite` limits) only
///   if it is a navigation to a URL the host itself told the page to load;
/// - a request from a **tuple origin** only if a document of that origin has
///   been loaded into this page — by a navigation the host performed;
/// - an **opaque** origin always (it is cross-site to everything, so it
///   carries nothing a forged one could gain).
#[derive(Default)]
pub(crate) struct SiteLock {
    typed: VecDeque<String>,
    documents: VecDeque<Origin>,
    opaque: HashMap<u64, Origin>,
}

impl SiteLock {
    const MAX_TYPED: usize = 16;
    /// A page's current document and the ones being navigated to.
    const MAX_DOCUMENTS: usize = 4;
    const MAX_OPAQUE: usize = 256;

    /// The host told the page to load `url`.
    pub fn expect_typed(&mut self, url: &str) {
        if self.typed.len() == Self::MAX_TYPED {
            self.typed.pop_front();
        }
        self.typed.push_back(url.to_string());
    }

    /// A navigation the host made for this page landed on `final_url`.
    pub fn document(&mut self, final_url: &str) {
        let origin = Origin::parse(final_url);
        if origin.is_opaque() || self.documents.iter().any(|d| d.same_origin(&origin)) {
            return;
        }
        if self.documents.len() == Self::MAX_DOCUMENTS {
            self.documents.pop_front();
        }
        self.documents.push_back(origin);
    }

    /// The request context the host will use, or why the request is refused.
    pub fn check(&mut self, url: &str, ctx: &WireContext) -> Result<RequestContext, &'static str> {
        let Some(initiator) = &ctx.initiator else {
            let expected = self.typed.iter().position(|u| u == url);
            return match (ctx.is_navigation, expected) {
                (true, Some(i)) => {
                    self.typed.remove(i);
                    Ok(RequestContext { initiator: None, is_navigation: true, credentials: ctx.credentials, destination: network::Destination::Document })
                }
                _ => Err("a browser-initiated request the browser did not make"),
            };
        };
        let origin = match initiator.opaque {
            Some(id) => {
                if !self.opaque.contains_key(&id) && self.opaque.len() >= Self::MAX_OPAQUE {
                    self.opaque.clear();
                }
                self.opaque.entry(id).or_insert_with(Origin::new_opaque).clone()
            }
            None => {
                let Some(page_url) = &initiator.url else { return Err("a request from a document with no origin") };
                let origin = Origin::parse(page_url);
                if origin.is_opaque() || !self.documents.iter().any(|d| d.same_origin(&origin)) {
                    return Err("a request on behalf of an origin this page never loaded");
                }
                origin
            }
        };
        Ok(RequestContext {
            initiator: Some(Initiator::document(origin, initiator.url.as_deref())),
            is_navigation: ctx.is_navigation,
            credentials: ctx.credentials,
            // What the renderer says a request is for only narrows what the
            // filter's type options match; a navigation is a document.
            destination: if ctx.is_navigation { network::Destination::Document } else { ctx.destination },
        })
    }
}

/// A launcher that runs the renderer on a thread of this process, over OS
/// pipes — the process protocol, end to end, without a binary (tests).
#[doc(hidden)]
pub struct InProcessLauncher;

impl RendererLauncher for InProcessLauncher {
    fn launch(&self) -> std::io::Result<RendererConnection> {
        let (host_reads, renderer_writes) = std::io::pipe()?;
        let (renderer_reads, host_writes) = std::io::pipe()?;
        let done = Arc::new(Mutex::new(None));
        let finished = Arc::clone(&done);
        std::thread::Builder::new().name("in-process-renderer".into()).spawn(move || {
            let code = crate::renderer::run_renderer(renderer_reads, renderer_writes, false, "in-process (tests)");
            *finished.lock().unwrap_or_else(PoisonError::into_inner) = Some(code);
        })?;
        Ok(RendererConnection {
            to_renderer: Box::new(host_writes),
            from_renderer: Box::new(host_reads),
            control: Box::new(InProcessControl { done }),
        })
    }
}

struct InProcessControl {
    done: Arc<Mutex<Option<i32>>>,
}

impl RendererControl for InProcessControl {
    fn terminate(&mut self) {}
    fn exit_status(&mut self) -> Option<String> {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            if let Some(code) = *self.done.lock().unwrap_or_else(PoisonError::into_inner) {
                return Some(format!("exit code {code}"));
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn pid(&self) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::WireInitiator;
    use network::Credentials;

    fn ctx(initiator: Option<WireInitiator>, is_navigation: bool) -> WireContext {
        WireContext { initiator, is_navigation, credentials: Credentials::Include, destination: network::Destination::Other }
    }

    fn from(url: &str) -> Option<WireInitiator> {
        Some(WireInitiator { url: Some(url.to_string()), opaque: None })
    }

    /// ADR 016 §5: a compromised renderer cannot borrow another site's
    /// cookies by claiming to be it, nor make typed navigations of its own.
    #[test]
    fn the_site_lock_believes_only_what_the_host_has_seen() {
        let mut lock = SiteLock::default();
        // A typed navigation only for the URL the host asked for, once.
        assert!(lock.check("https://bank.example/", &ctx(None, true)).is_err(), "not asked for");
        lock.expect_typed("https://news.example/");
        assert!(lock.check("https://news.example/", &ctx(None, false)).is_err(), "not a navigation");
        assert!(lock.check("https://news.example/", &ctx(None, true)).is_ok());
        assert!(lock.check("https://news.example/", &ctx(None, true)).is_err(), "only once");
        // Requests from the document it loaded — not from anyone else.
        lock.document("https://news.example/index.html");
        let ok = lock.check("https://cdn.example/a.png", &ctx(from("https://news.example/index.html"), false)).unwrap();
        assert!(ok.initiator.unwrap().origin.same_origin(&Origin::parse("https://news.example/")));
        assert!(lock.check("https://bank.example/transfer", &ctx(from("https://bank.example/"), false)).is_err(), "a forged initiator");
        assert!(lock.check("https://x/", &ctx(Some(WireInitiator { url: None, opaque: None }), false)).is_err());
        // A link to another site is a navigation from this document; once it
        // lands, the new document's requests are allowed.
        assert!(lock.check("https://bank.example/", &ctx(from("https://news.example/"), true)).is_ok());
        lock.document("https://bank.example/");
        assert!(lock.check("https://bank.example/api", &ctx(from("https://bank.example/"), false)).is_ok());
        // An opaque origin is always allowed, and stays the same origin.
        let opaque = Some(WireInitiator { url: None, opaque: Some(7) });
        let a = lock.check("https://x/", &ctx(opaque.clone(), false)).unwrap().initiator.unwrap().origin;
        let b = lock.check("https://y/", &ctx(opaque, false)).unwrap().initiator.unwrap().origin;
        assert!(a.is_opaque() && a.same_origin(&b));
    }

    /// A launcher whose "renderer" is the test itself, on the other ends of
    /// the pipes.
    struct Scripted(Mutex<Option<RendererConnection>>);
    impl RendererLauncher for Scripted {
        fn launch(&self) -> std::io::Result<RendererConnection> {
            self.0.lock().unwrap().take().ok_or_else(|| std::io::Error::other("launched twice"))
        }
    }
    struct NoControl;
    impl RendererControl for NoControl {
        fn terminate(&mut self) {}
        fn exit_status(&mut self) -> Option<String> {
            None
        }
        fn pid(&self) -> Option<u32> {
            None
        }
    }

    /// End to end: a compromised renderer that claims another site's origin
    /// gets a refusal, and the request never leaves the host.
    #[test]
    fn a_renderer_claiming_another_origin_is_refused_before_the_network() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            for _ in listener.incoming() {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        let (host_reads, mut fake_writes) = std::io::pipe().unwrap();
        let (mut fake_reads, host_writes) = std::io::pipe().unwrap();
        let connection = RendererConnection { to_renderer: Box::new(host_writes), from_renderer: Box::new(host_reads), control: Box::new(NoControl) };
        let setup = ProcessSetup {
            launcher: Arc::new(Scripted(Mutex::new(Some(connection)))),
            spare: None,
            network: NetworkContext::new(),
            viewport: Viewport::new(100.0, 100.0),
            image_loading: ImageLoading::Progressive,
            waker: Arc::default(),
            fonts: None,
            grants: 0,
            fingerprint: privacy::FingerprintPolicy::standard(),
            unresponsive_after: crate::UNRESPONSIVE_AFTER,
        };
        let mailbox = Arc::new(Mailbox::default());
        let _page = spawn(PageId(1), Arc::clone(&mailbox), setup).unwrap();
        let first = ToRenderer::decode(&ipc::read_frame(&mut fake_reads).unwrap().unwrap()).unwrap();
        assert!(matches!(first, ToRenderer::Setup { .. }));

        let target = format!("http://127.0.0.1:{port}/steal");
        for (id, context) in [
            // "I am the bank" — a site this page never loaded.
            (1, WireContext { initiator: from("https://bank.example/"), is_navigation: false, credentials: Credentials::Include, destination: network::Destination::Other }),
            // "The user typed this" — the host asked for no such load.
            (2, WireContext { initiator: None, is_navigation: true, credentials: Credentials::Include, destination: network::Destination::Document }),
        ] {
            let request = ToHost::FetchRequest { id, url: target.clone(), context };
            ipc::write_frame(&mut fake_writes, &request.encode()).unwrap();
            let reply = ToRenderer::decode(&ipc::read_frame(&mut fake_reads).unwrap().unwrap()).unwrap();
            match reply {
                ToRenderer::FetchReply { id: got, result: Err(NetworkError::Transport(reason)) } => {
                    assert_eq!(got, id);
                    assert!(reason.starts_with("refused"), "{reason}");
                }
                other => panic!("request {id} was not refused: {other:?}"),
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a refused request never reaches the network");
    }

    #[test]
    fn a_frame_that_uses_what_was_never_sent_is_refused() {
        let mut frames = FrameReceiver::default();
        let empty = |images: Vec<(u64, u32, u32)>, bands: Vec<(u32, Vec<u8>)>| WireFrame {
            viewport_width: 10.0,
            viewport_height: 10.0,
            background: style::Color { r: 0, g: 0, b: 0, a: 255 },
            items: vec![],
            atlas_width: 512,
            atlas_height: 128,
            atlas_generation: 0,
            atlas_bands: bands,
            images,
            damage: rendering::Damage::Full,
            stats: FrameStats {
                restyled: 0,
                full_restyle: false,
                animated: 0,
                composited: 0,
                layout: crate::LayoutWork::Reused,
                display_items: 0,
                culled: 0,
                display_list_built: false,
                damage: rendering::Damage::Full,
                animation_interval_ms: 17,
            },
        };
        let band = || vec![0u8; 512 * text::ATLAS_BAND_ROWS];
        assert!(frames.rebuild(empty(vec![], vec![(0, band())])).is_err(), "band 1 of 2 missing");
        assert!(frames.rebuild(empty(vec![], vec![(1, band())])).is_ok(), "now both have arrived");
        assert!(frames.rebuild(empty(vec![(5, 1, 1)], vec![])).is_err(), "image 5 never sent");
        frames.store_image(5, 1, 1, vec![1, 2, 3, 4]).unwrap();
        assert!(frames.rebuild(empty(vec![(5, 1, 1)], vec![])).is_ok());
        assert!(frames.store_image(6, 16384, 16384, vec![0; 1]).is_ok());
        let over = vec![0; MAX_IMAGE_BYTES_PER_PAGE];
        assert!(frames.store_image(7, 16384, 16384, over).is_err(), "the per-page image limit");
    }
}
