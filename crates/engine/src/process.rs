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
//!
//! Milestone 41 (ADR 025): a renderer holds documents of **one site**. Its
//! site lock is set by its first document; a navigation that lands on a
//! document of another site is not handed to it — the page moves to a
//! renderer that has held nothing, which is given the response, while the
//! bridge (and so the order of the page's commands) stays the same. The
//! host also serves the page's `localStorage` from the profile's store, and
//! enforces the page's network grant and what each request's stated
//! purpose may receive (`response_policy`).

use crate::ipc::{self, StorageAnswer, StorageOp, ToHost, ToRenderer, WireContext, WireFrame};
use crate::page_thread::{Mailbox, WakerSlot};
use crate::renderer::{from_nanos, to_nanos};
use crate::response_policy;
use crate::{EngineEvent, FrameStats, ImageLoading, LoadRequest, PageCommand, PageId, Scene, Viewport};
use capability::PageGrants;
use diagnostics::Guard;
use network::{FetchResponse, Initiator, NetworkContext, NetworkError, RequestContext};
use origin::{Origin, Site, StorageKey};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn send(out: &SharedWriter, message: &ToRenderer) -> bool {
    let bytes = message.encode();
    let mut out = lock(out);
    ipc::write_frame(&mut *out, &bytes).is_ok()
}

fn wake(slot: &WakerSlot) {
    let waker = lock(slot).clone();
    if let Some(waker) = waker {
        waker();
    }
}

/// Renderers started ahead of need — one, shared by the engine's new pages
/// and by pages moving to a renderer for another site (Milestone 41). A
/// spare has never held a document, so it suits any site: it is locked to
/// one by the first document it is given (ADR 025 §2).
pub(crate) struct Spares {
    launcher: Arc<dyn RendererLauncher>,
    spare: Mutex<Option<RendererConnection>>,
    closed: AtomicBool,
}

impl Spares {
    /// Starts the first spare now.
    pub fn new(launcher: Arc<dyn RendererLauncher>) -> Arc<Spares> {
        let spare = launcher.launch().ok();
        Arc::new(Spares { launcher, spare: Mutex::new(spare), closed: AtomicBool::new(false) })
    }

    /// A renderer that has never held a document: the spare, with the next
    /// one started behind it, or a new one if there is no spare.
    fn take(&self) -> std::io::Result<RendererConnection> {
        let taken = lock(&self.spare).take();
        let connection = match taken {
            Some(spare) => spare,
            None => self.launcher.launch()?,
        };
        if !self.closed.load(Ordering::Acquire) {
            let next = self.launcher.launch().ok();
            let mut spare = lock(&self.spare);
            // Checked again under the lock: `close` may have run meanwhile.
            match next {
                Some(mut next) if self.closed.load(Ordering::Acquire) || spare.is_some() => next.control.terminate(),
                Some(next) => *spare = Some(next),
                None => {}
            }
        }
        Ok(connection)
    }

    /// Ends the spare, and starts no more (the engine is going away).
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(mut spare) = lock(&self.spare).take() {
            spare.control.terminate();
        }
    }
}

/// Numbers every renderer the hosts in this process start (diagnostics:
/// [`ProcessPage::serial`]).
static RENDERER_SERIAL: AtomicU64 = AtomicU64::new(1);

/// One renderer process, as its page's host sees it.
struct Renderer {
    serial: u64,
    out: SharedWriter,
    control: Mutex<Box<dyn RendererControl>>,
    pid: Option<u32>,
    sandboxed: Mutex<Option<(bool, String)>>,
    lock: Mutex<SiteLock>,
    progress: Mutex<Progress>,
    /// The host-clock instant this renderer's tick times count from.
    epoch: Instant,
    /// Replaced by a renderer for another site (Milestone 41): what it still
    /// sends is ignored, and it is ended.
    retired: AtomicBool,
    /// A renderer that took over its page for another site draws nothing
    /// until its first document commits or fails: the old document stays
    /// on screen meanwhile (ADR 025 §3).
    awaiting_commit: AtomicBool,
}

impl Renderer {
    fn send(&self, message: &ToRenderer) -> bool {
        send(&self.out, message)
    }

    /// Asks the renderer to advance to `tick` (host clock); it owes the tick
    /// from now (ADR 013).
    fn tick(&self, tick: Instant) -> bool {
        {
            let mut progress = lock(&self.progress);
            if !progress.owes_a_tick() {
                progress.since = Instant::now(); // it starts owing now
            }
            progress.sent = Some(progress.sent.map_or(tick, |t| t.max(tick)));
        }
        self.send(&ToRenderer::Tick(to_nanos(self.epoch, tick)))
    }

    fn terminate(&self) {
        lock(&self.control).terminate();
    }

    fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    /// Takes the renderer out of service: told to shut down, then ended.
    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.send(&ToRenderer::Shutdown);
        self.terminate();
    }
}

/// What a renderer that takes over a page must be told to carry on where
/// the old one was.
#[derive(Clone, Copy)]
struct Carry {
    viewport: Viewport,
    visible: bool,
    grants: u8,
}

/// A page in process mode, on the host's side: the renderer it runs in now,
/// and what a renderer that takes over from it needs (Milestone 41).
struct PageHost {
    id: PageId,
    mailbox: Arc<Mailbox>,
    spares: Arc<Spares>,
    network: NetworkContext,
    waker: WakerSlot,
    image_loading: ImageLoading,
    fonts: Option<Arc<dyn text::FontSource>>,
    catalog: Vec<text::CatalogEntry>,
    fingerprint: privacy::FingerprintPolicy,
    unresponsive_after: Duration,
    state: Mutex<HostState>,
}

struct HostState {
    current: Option<Arc<Renderer>>,
    viewport: Viewport,
    visible: bool,
    /// What the host granted the page, enforced here too (ADR 025 §4–5): a
    /// revocation reaches it at once (the handles share their token with
    /// the engine's), a new grant as its command passes the bridge.
    grants: PageGrants,
    /// The latest tick sent to the page.
    last_tick: Option<Instant>,
    /// The URL of the navigation last announced for the page
    /// (`NavigationStarted`), whichever renderer announced it: a renderer
    /// that takes the navigation over does not announce it again.
    announced: Option<String>,
}

impl HostState {
    fn carry(&self) -> Carry {
        Carry { viewport: self.viewport, visible: self.visible, grants: self.grants.bits() }
    }

    fn is_current(&self, renderer: &Arc<Renderer>) -> bool {
        self.current.as_ref().is_some_and(|c| Arc::ptr_eq(c, renderer))
    }
}

/// What the engine keeps of a page in process mode.
pub(crate) struct ProcessPage {
    host: Arc<PageHost>,
}

impl ProcessPage {
    fn current(&self) -> Option<Arc<Renderer>> {
        lock(&self.host.state).current.clone()
    }

    pub fn pid(&self) -> Option<u32> {
        self.current()?.pid
    }

    /// Whether the page's renderer reported running sandboxed, and how.
    pub fn sandbox(&self) -> Option<(bool, String)> {
        lock(&self.current()?.sandboxed).clone()
    }

    /// The number the host gave the page's current renderer: a new one when
    /// the page moved to a renderer for another site.
    pub fn serial(&self) -> Option<u64> {
        Some(self.current()?.serial)
    }
}

impl Drop for ProcessPage {
    fn drop(&mut self) {
        if let Some(renderer) = self.current() {
            renderer.terminate();
        }
    }
}

pub(crate) struct ProcessSetup {
    /// Milestone 41: renderers started ahead of need, shared with the engine.
    pub spares: Arc<Spares>,
    pub network: NetworkContext,
    pub viewport: Viewport,
    pub image_loading: ImageLoading,
    pub waker: WakerSlot,
    /// Milestone 35: the host's fonts, served to the renderer by key.
    pub fonts: Option<Arc<dyn text::FontSource>>,
    /// Milestone 36: the page's grants. The renderer is told their kinds
    /// and mints its own handles; the host keeps these and enforces them
    /// too (Milestone 41, ADR 025 §4–5).
    pub grants: PageGrants,
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

/// How often the watchdog looks: at least once a second, and four times per
/// bound for short bounds (tests).
fn watchdog_interval(bound: Duration) -> Duration {
    (bound / 4).clamp(Duration::from_millis(10), Duration::from_secs(1))
}

/// Starts page `id`'s renderer and the bridge that connects it to `mailbox`.
pub(crate) fn spawn(id: PageId, mailbox: Arc<Mailbox>, setup: ProcessSetup) -> std::io::Result<ProcessPage> {
    let catalog = setup.fonts.as_ref().map(|f| f.catalog()).unwrap_or_default();
    let host = Arc::new(PageHost {
        id,
        mailbox,
        spares: setup.spares,
        network: setup.network,
        waker: setup.waker,
        image_loading: setup.image_loading,
        fonts: setup.fonts,
        catalog,
        fingerprint: setup.fingerprint,
        unresponsive_after: setup.unresponsive_after,
        state: Mutex::new(HostState { current: None, viewport: setup.viewport, visible: true, grants: setup.grants, last_tick: None, announced: None }),
    });
    let carry = lock(&host.state).carry();
    let renderer = host.start(host.spares.take()?, SiteLock::default(), false, carry)?;
    lock(&host.state).current = Some(renderer);
    {
        let host = Arc::clone(&host);
        std::thread::Builder::new().name(format!("bridge-{}", id.0)).spawn(move || host.bridge())?;
    }
    Ok(ProcessPage { host })
}

impl PageHost {
    /// Sets up a started renderer for this page — `lock` says which
    /// documents it may hold, `carry` what it must know — and starts the
    /// threads that read it and watch it.
    fn start(self: &Arc<Self>, connection: RendererConnection, lock: SiteLock, awaiting_commit: bool, carry: Carry) -> std::io::Result<Arc<Renderer>> {
        let RendererConnection { to_renderer, from_renderer, control } = connection;
        let renderer = Arc::new(Renderer {
            serial: RENDERER_SERIAL.fetch_add(1, Ordering::Relaxed),
            out: Arc::new(Mutex::new(to_renderer)),
            pid: control.pid(),
            control: Mutex::new(control),
            sandboxed: Mutex::new(None),
            lock: Mutex::new(lock),
            progress: Mutex::new(Progress { sent: None, done: None, since: Instant::now() }),
            epoch: Instant::now(),
            retired: AtomicBool::new(false),
            awaiting_commit: AtomicBool::new(awaiting_commit),
        });
        let setup = ToRenderer::Setup { viewport: carry.viewport, image_loading: self.image_loading, fonts: self.catalog.clone(), grants: carry.grants, fingerprint: self.fingerprint };
        if !renderer.send(&setup) || (!carry.visible && !renderer.send(&ToRenderer::Command(PageCommand::SetVisible(false)))) {
            renderer.terminate();
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the renderer did not start"));
        }
        {
            let (host, renderer) = (Arc::clone(self), Arc::clone(&renderer));
            std::thread::Builder::new().name(format!("reader-{}", self.id.0)).spawn(move || host.read(&renderer, from_renderer))?;
        }
        {
            let (host, renderer) = (Arc::clone(self), Arc::clone(&renderer));
            std::thread::Builder::new().name(format!("watchdog-{}", self.id.0)).spawn(move || host.watch(&renderer))?;
        }
        Ok(renderer)
    }

    /// The mailbox's work, to the page's current renderer — the same thread
    /// across renderers, so commands keep their order.
    fn bridge(self: &Arc<Self>) {
        while let Some((commands, tick)) = self.mailbox.wait_for_work() {
            for command in commands {
                self.forward(command);
            }
            // Under the state lock, so a renderer taking over cannot miss it.
            let mut state = lock(&self.state);
            state.last_tick = Some(state.last_tick.map_or(tick, |t| t.max(tick)));
            if let Some(renderer) = &state.current {
                renderer.tick(tick);
            }
        }
        // Shut down: tell it, then make sure.
        if let Some(renderer) = lock(&self.state).current.take() {
            renderer.retire();
        }
    }

    /// Sends one command to the current renderer, keeping what a renderer
    /// taking over would need. A host-made HTML document of another site
    /// goes to a renderer for that site.
    fn forward(self: &Arc<Self>, command: PageCommand) {
        let mut state = lock(&self.state);
        match &command {
            PageCommand::Resize(viewport) => state.viewport = viewport.sanitized(),
            PageCommand::SetVisible(visible) => state.visible = *visible,
            PageCommand::Grant(grants) => state.grants.merge(grants.clone()),
            PageCommand::Revoke(kind) => state.grants.revoke(*kind),
            _ => {}
        }
        let Some(current) = state.current.clone() else { return };
        match &command {
            // The one browser-initiated navigation this page may make.
            PageCommand::Load(LoadRequest::Url(url)) => lock(&current.lock).expect_typed(url),
            PageCommand::Load(LoadRequest::Html { url, .. }) if !lock(&current.lock).admit_html(url.as_deref()) => {
                let mut site_lock = SiteLock::default();
                site_lock.admit_html(url.as_deref());
                let started = self.spares.take().and_then(|c| self.start(c, site_lock, true, state.carry()));
                let renderer = match started {
                    Ok(renderer) => renderer,
                    Err(e) => {
                        // Never into a renderer of another site.
                        drop(state);
                        return self.crash(&current, format!("could not start a renderer for the document's site: {e}"));
                    }
                };
                renderer.send(&ToRenderer::Command(command));
                if let Some(tick) = state.last_tick {
                    renderer.tick(tick);
                }
                state.current = Some(Arc::clone(&renderer));
                drop(state);
                self.moved(&current, &renderer);
                return;
            }
            _ => {}
        }
        current.send(&ToRenderer::Command(command));
    }

    /// The page left `old` for `new`, a renderer for its new site.
    fn moved(&self, old: &Arc<Renderer>, new: &Arc<Renderer>) {
        diagnostics::tracing::info!(target: "engine::process", page = self.id.0, from = old.serial, to = new.serial, "page moved to a renderer for its new site");
        old.retire();
    }

    /// Ends the page as crashed — unless `renderer` is no longer its own.
    fn crash(&self, renderer: &Arc<Renderer>, reason: String) {
        if renderer.is_retired() {
            return;
        }
        // Marked first: ending the process closes its pipe, and its reader
        // must not report that as the reason.
        self.mailbox.mark_crashed(reason);
        renderer.terminate();
        wake(&self.waker);
    }

    /// The renderer's results, into the mailbox.
    fn read(self: &Arc<Self>, renderer: &Arc<Renderer>, mut input: Box<dyn Read + Send>) {
        let mut frames = FrameReceiver::default();
        loop {
            let bytes = match ipc::read_frame(&mut input) {
                Ok(Some(bytes)) => bytes,
                Ok(None) | Err(_) => {
                    if renderer.is_retired() || self.mailbox.is_shut_down() || self.mailbox.is_crashed() {
                        return; // replaced, closed on purpose, or ended by the watchdog
                    }
                    let status = lock(&renderer.control).exit_status();
                    diagnostics::guard(Guard::PageCrashed, "a renderer process ended");
                    return self.crash(renderer, format!("the renderer process ended ({})", status.unwrap_or_else(|| "no exit status".into())));
                }
            };
            if renderer.is_retired() {
                continue; // read on to the end of its output, and ignore it
            }
            let message = match ToHost::decode(&bytes) {
                Ok(m) => m,
                Err(e) => {
                    diagnostics::guard(Guard::RendererMessageRejected, e.0);
                    return self.crash(renderer, format!("the renderer sent a malformed message: {}", e.0));
                }
            };
            match message {
                ToHost::Published { events, frame, next_wake, completed_tick } => {
                    // Rebuilt even when not shown: later frames send only
                    // what changed since this one.
                    let mut frame = match frame.map(|f| frames.rebuild(*f)) {
                        Some(Err(reason)) => {
                            diagnostics::guard(Guard::RendererMessageRejected, reason);
                            return self.crash(renderer, format!("the renderer sent a malformed frame: {reason}"));
                        }
                        Some(Ok(frame)) => Some(frame),
                        None => None,
                    };
                    let completed = from_nanos(renderer.epoch, completed_tick);
                    {
                        let mut progress = lock(&renderer.progress);
                        // Only a tick finished counts as progress: a renderer
                        // repeating an old report cannot hold off the
                        // watchdog (ADR 013).
                        if progress.done.is_none_or(|done| completed > done) {
                            progress.done = Some(completed);
                            progress.since = Instant::now();
                        }
                    }
                    let mut events: Vec<EngineEvent> = events.into_iter().map(|e| with_page(e, self.id)).collect();
                    {
                        // Under the state lock, so a renderer the page left
                        // while this was in flight publishes nothing, and its
                        // successor sees what it did publish.
                        let mut state = lock(&self.state);
                        if !state.is_current(renderer) {
                            continue;
                        }
                        if renderer.awaiting_commit.load(Ordering::Acquire) {
                            // Announced already, by the renderer the page left.
                            events.retain(|e| !matches!(e, EngineEvent::NavigationStarted { url, .. } if state.announced.as_ref() == Some(url)));
                            if events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. } | EngineEvent::NavigationFailed { .. })) {
                                renderer.awaiting_commit.store(false, Ordering::Release);
                            } else {
                                frame = None; // the old document stays on screen
                            }
                        }
                        for event in &events {
                            if let EngineEvent::NavigationStarted { url, .. } = event {
                                state.announced = Some(url.clone());
                            }
                        }
                        self.mailbox.publish(events, frame, next_wake.map(|n| from_nanos(renderer.epoch, n)), completed);
                    }
                    wake(&self.waker);
                }
                ToHost::Image { id: image, width, height, rgba } => {
                    if let Err(reason) = frames.store_image(image, width, height, rgba) {
                        diagnostics::guard(Guard::RendererMessageRejected, reason);
                        return self.crash(renderer, format!("the renderer sent too much image data: {reason}"));
                    }
                }
                ToHost::Crashed { reason } => return self.crash(renderer, reason),
                ToHost::FetchRequest { id, url, context } => self.fetch(renderer, id, url, &context),
                ToHost::FontRequest { id: request, key, index } => {
                    // Only a font of the catalog this renderer was given.
                    let entry = self.catalog.iter().find(|e| e.key == key && e.index == index).cloned();
                    let (reply_to, source) = (Arc::clone(renderer), self.fonts.clone());
                    let spawned = std::thread::Builder::new().name("font-load".into()).spawn(move || {
                        let bytes = entry.and_then(|e| source.and_then(|s| s.load(&e))).filter(|b| b.len() <= text::MAX_FONT_BYTES);
                        reply_to.send(&ToRenderer::FontReply { id: request, bytes });
                    });
                    if spawned.is_err() {
                        renderer.send(&ToRenderer::FontReply { id: request, bytes: None });
                    }
                }
                ToHost::Started { sandboxed: on, detail } => {
                    diagnostics::tracing::info!(target: "engine::process", page = self.id.0, renderer = renderer.serial, sandboxed = on, detail = %detail, "renderer started");
                    *lock(&renderer.sandboxed) = Some((on, detail));
                }
                ToHost::Storage { id, origin, op } => {
                    let answer = self.storage(renderer, &origin, op);
                    renderer.send(&ToRenderer::StorageReply { id, answer });
                }
            }
        }
    }

    /// Watchdog (ADR 013): a renderer that owes a tick and makes no progress
    /// for `unresponsive_after` is ended. Nothing the renderer says pauses it.
    fn watch(&self, renderer: &Arc<Renderer>) {
        loop {
            std::thread::sleep(watchdog_interval(self.unresponsive_after));
            if renderer.is_retired() || self.mailbox.is_shut_down() || self.mailbox.is_crashed() {
                return;
            }
            let stalled = {
                let progress = lock(&renderer.progress);
                progress.owes_a_tick() && progress.since.elapsed() >= self.unresponsive_after
            };
            if stalled {
                diagnostics::guard(Guard::PageUnresponsive, "a renderer made no progress for the unresponsive bound; it was ended");
                return self.crash(renderer, format!("the page stopped responding for {} s; its renderer was ended", self.unresponsive_after.as_secs_f32()));
            }
        }
    }

    /// Whether the page holds its network capability now.
    fn network_granted(&self) -> bool {
        lock(&self.state).grants.network.as_ref().is_some_and(|n| !n.is_revoked())
    }

    /// A request the renderer asks the host to make (ADR 016 §5, ADR 025).
    fn fetch(self: &Arc<Self>, renderer: &Arc<Renderer>, id: u64, url: String, context: &WireContext) {
        let refuse = |reason: &str| {
            diagnostics::guard(Guard::RendererRequestRefused, reason);
            renderer.send(&ToRenderer::FetchReply { id, result: Err(NetworkError::Transport(format!("refused: {reason}"))) });
        };
        let checked = lock(&renderer.lock).check(&url, context);
        let request = match checked {
            Err(reason) => return refuse(reason),
            Ok(Checked::Prefetched(response)) => {
                // The document this renderer was started for, fetched before
                // it existed.
                lock(&renderer.lock).admit(&response.final_url);
                renderer.send(&ToRenderer::FetchReply { id, result: Ok(response) });
                return;
            }
            Ok(Checked::Fetch(request)) => request,
        };
        // ADR 025 §5: a page without the network capability gets only what
        // its document loader asks for — whatever its renderer says.
        if !request.is_navigation && !response_policy::is_loader_request(request.destination) && !self.network_granted() {
            return refuse("a script request from a page that holds no network capability");
        }
        let (host, asker) = (Arc::clone(self), Arc::clone(renderer));
        // On the bounded fetch pool, as any fetch.
        let submitted = network::pool::FetchPool::global().submit(
            network::pool::host_of(&url),
            Box::new(move || {
                let result = match host.network.fetch_with(&url, &request) {
                    Ok(response) if request.is_navigation => {
                        if !lock(&asker.lock).admit(&response.final_url) {
                            // A document of another site: never this
                            // renderer's. The page moves to one for its site.
                            return host.move_to_site(&asker, id, url, response);
                        }
                        Ok(response)
                    }
                    Ok(response) => match response_policy::withheld(&request, &response) {
                        Some(reason) => {
                            diagnostics::guard(Guard::RendererResponseWithheld, reason);
                            Ok(response_policy::emptied(response))
                        }
                        None => Ok(response),
                    },
                    Err(e) => Err(e),
                };
                asker.send(&ToRenderer::FetchReply { id, result });
            }),
        );
        if submitted.is_err() {
            diagnostics::guard(Guard::FetchQueueFull, "too many fetches waiting; request refused");
            renderer.send(&ToRenderer::FetchReply { id, result: Err(NetworkError::Transport("too many requests waiting".into())) });
        }
    }

    /// Milestone 41 (ADR 025 §3): `old`'s navigation to `url` landed on a
    /// document of another site. A renderer that has held no document takes
    /// the page over, locked to that site, and is handed the response — the
    /// browser does not fetch it twice; `old` is ended. If no renderer can
    /// be started, the navigation fails and the old document stays.
    fn move_to_site(self: &Arc<Self>, old: &Arc<Renderer>, id: u64, url: String, response: FetchResponse) {
        let mut state = lock(&self.state);
        if !state.is_current(old) {
            return; // the page already left it
        }
        let started = self.spares.take().and_then(|c| self.start(c, SiteLock::for_document(&url, response), true, state.carry()));
        let renderer = match started {
            Ok(renderer) => renderer,
            Err(e) => {
                drop(state);
                old.send(&ToRenderer::FetchReply { id, result: Err(NetworkError::Transport(format!("no renderer could be started for the document's site: {e}"))) });
                return;
            }
        };
        renderer.send(&ToRenderer::Command(PageCommand::Load(LoadRequest::Url(url))));
        if let Some(tick) = state.last_tick {
            renderer.tick(tick);
        }
        state.current = Some(Arc::clone(&renderer));
        drop(state);
        self.moved(old, &renderer);
    }

    /// Milestone 41 (ADR 025 §4): one `localStorage` operation, made in the
    /// profile's store through the page's own storage grant — only for an
    /// origin this renderer has loaded.
    fn storage(&self, renderer: &Arc<Renderer>, origin: &str, op: StorageOp) -> StorageAnswer {
        let parsed = Origin::parse(origin);
        if parsed.is_opaque() || parsed.serialize() != origin || !lock(&renderer.lock).has_document(&parsed) {
            diagnostics::guard(Guard::RendererRequestRefused, "storage of an origin this renderer never loaded");
            return StorageAnswer::Refused;
        }
        let Some(grant) = lock(&self.state).grants.storage.clone() else {
            diagnostics::guard(Guard::RendererRequestRefused, "storage from a page that holds no storage capability");
            return StorageAnswer::Refused;
        };
        let scoped = grant.scope(&StorageKey::first_party(&parsed));
        let Ok(area) = scoped.area() else { return StorageAnswer::Refused };
        match op {
            StorageOp::Get(name) => StorageAnswer::Value(area.get(&name)),
            StorageOp::Set(name, value) => StorageAnswer::Written(area.set(name, value).is_ok()),
            StorageOp::Remove(name) => {
                area.remove(&name);
                StorageAnswer::Done
            }
            StorageOp::Clear => {
                area.clear();
                StorageAnswer::Done
            }
            StorageOp::Length => StorageAnswer::Length(u32::try_from(area.len()).unwrap_or(u32::MAX)),
        }
    }
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

/// What a page's renderer may ask the network for (ADR 016 §5), and which
/// documents it may hold (Milestone 41, ADR 025 §2). The renderer states each
/// request's initiator; the host believes only what it has seen:
///
/// - a request with **no initiator** (a typed URL: no `SameSite` limits) only
///   if it is a navigation to a URL the host itself told the page to load;
/// - a request from a **tuple origin** only if a document of that origin has
///   been loaded into this renderer — by a navigation the host performed, or
///   an HTML document the host gave it;
/// - an **opaque** origin always (it is cross-site to everything, so it
///   carries nothing a forged one could gain).
///
/// Every document a renderer holds is of one **site**: the first sets it,
/// and the host never hands it a document of another.
#[derive(Default)]
pub(crate) struct SiteLock {
    site: Option<Site>,
    typed: VecDeque<String>,
    documents: VecDeque<Origin>,
    opaque: HashMap<u64, Origin>,
    /// A navigation response the host fetched before this renderer existed:
    /// the document the page moved to it for.
    prefetched: Option<(String, FetchResponse)>,
}

/// What the host does with a request it checked.
pub(crate) enum Checked {
    /// Makes it, with this context.
    Fetch(RequestContext),
    /// Answers it with a response it already has.
    Prefetched(FetchResponse),
}

impl SiteLock {
    const MAX_TYPED: usize = 16;
    /// A page's current document and the ones being navigated to.
    const MAX_DOCUMENTS: usize = 4;
    const MAX_OPAQUE: usize = 256;

    /// The lock of a renderer started for the document `response` (fetched
    /// for `url`): its site, and the one navigation it will make.
    fn for_document(url: &str, response: FetchResponse) -> SiteLock {
        let mut lock = SiteLock { site: Some(Site::of_url(&response.final_url)), ..SiteLock::default() };
        lock.expect_typed(url);
        lock.prefetched = Some((url.to_string(), response));
        lock
    }

    /// The host told the page to load `url`.
    pub fn expect_typed(&mut self, url: &str) {
        if self.typed.len() == Self::MAX_TYPED {
            self.typed.pop_front();
        }
        self.typed.push_back(url.to_string());
    }

    fn record(&mut self, origin: Origin) {
        if origin.is_opaque() || self.documents.iter().any(|d| d.same_origin(&origin)) {
            return;
        }
        if self.documents.len() == Self::MAX_DOCUMENTS {
            self.documents.pop_front();
        }
        self.documents.push_back(origin);
    }

    /// A navigation landed on `final_url` (after any redirects): whether
    /// this renderer may hold that document — one of its site, or its first
    /// (which sets its site). If so its origin is recorded: a page names a
    /// document by the URL it landed on, never the one it asked for.
    pub fn admit(&mut self, final_url: &str) -> bool {
        let landed = Origin::parse(final_url);
        if landed.is_opaque() {
            return true; // cross-site to everything: it can claim nothing
        }
        let site = landed.site();
        match &self.site {
            Some(locked) if *locked != site => return false,
            Some(_) => {}
            None => self.site = Some(site.clone()),
        }
        self.record(landed);
        true
    }

    /// The host gives this renderer an HTML document with address `url`:
    /// whether it may hold it (as [`Self::admit`]). One without an address
    /// has an opaque origin and fits anywhere.
    pub fn admit_html(&mut self, url: Option<&str>) -> bool {
        url.is_none_or(|url| self.admit(url))
    }

    /// Whether a document of `origin` was loaded into this renderer.
    pub fn has_document(&self, origin: &Origin) -> bool {
        !origin.is_opaque() && self.documents.iter().any(|d| d.same_origin(origin))
    }

    /// What the host does with the request, or why it is refused.
    pub fn check(&mut self, url: &str, ctx: &WireContext) -> Result<Checked, &'static str> {
        let Some(initiator) = &ctx.initiator else {
            let expected = self.typed.iter().position(|u| u == url);
            return match (ctx.is_navigation, expected) {
                (true, Some(i)) => {
                    self.typed.remove(i);
                    if self.prefetched.as_ref().is_some_and(|(u, _)| u == url) {
                        if let Some((_, response)) = self.prefetched.take() {
                            return Ok(Checked::Prefetched(response));
                        }
                    }
                    Ok(Checked::Fetch(RequestContext { initiator: None, is_navigation: true, credentials: ctx.credentials, destination: network::Destination::Document }))
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
                if !self.has_document(&origin) {
                    return Err("a request on behalf of an origin this page never loaded");
                }
                origin
            }
        };
        Ok(Checked::Fetch(RequestContext {
            initiator: Some(Initiator::document(origin, initiator.url.as_deref())),
            is_navigation: ctx.is_navigation,
            credentials: ctx.credentials,
            // What the renderer says a request is for only narrows what the
            // filter's type options match, and what of a cross-origin
            // response it is given (`response_policy`); a navigation is a
            // document.
            destination: if ctx.is_navigation { network::Destination::Document } else { ctx.destination },
        }))
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

    fn fetch_context(checked: Result<Checked, &'static str>) -> RequestContext {
        match checked {
            Ok(Checked::Fetch(ctx)) => ctx,
            Ok(Checked::Prefetched(_)) => panic!("not a prefetched request"),
            Err(reason) => panic!("refused: {reason}"),
        }
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
        assert!(lock.admit("https://news.example/index.html"));
        let ok = fetch_context(lock.check("https://cdn.example/a.png", &ctx(from("https://news.example/index.html"), false)));
        assert!(ok.initiator.unwrap().origin.same_origin(&Origin::parse("https://news.example/")));
        assert!(lock.check("https://bank.example/transfer", &ctx(from("https://bank.example/"), false)).is_err(), "a forged initiator");
        assert!(lock.check("https://x/", &ctx(Some(WireInitiator { url: None, opaque: None }), false)).is_err());
        // An opaque origin is always allowed, and stays the same origin.
        let opaque = Some(WireInitiator { url: None, opaque: Some(7) });
        let a = fetch_context(lock.check("https://x/", &ctx(opaque.clone(), false))).initiator.unwrap().origin;
        let b = fetch_context(lock.check("https://y/", &ctx(opaque, false))).initiator.unwrap().origin;
        assert!(a.is_opaque() && a.same_origin(&b));
    }

    /// Milestone 41 (ADR 025 §2): a renderer holds one site's documents. A
    /// link to another site may be followed — the host makes the request —
    /// but the document it lands on is not admitted, so the renderer never
    /// gains that site's authority; another origin of its own site is.
    #[test]
    fn a_renderer_is_locked_to_the_site_of_its_first_document() {
        let mut lock = SiteLock::default();
        assert!(lock.admit("https://news.example/"));
        assert!(lock.check("https://bank.example/", &ctx(from("https://news.example/"), true)).is_ok(), "a link may be followed");
        assert!(!lock.admit("https://bank.example/"), "but its document is another site's");
        assert!(lock.check("https://bank.example/api", &ctx(from("https://bank.example/"), false)).is_err(), "and the renderer never becomes the bank");
        assert!(!lock.has_document(&Origin::parse("https://bank.example/")));
        // Same site, other origin: admitted.
        assert!(lock.admit("https://www.news.example/"));
        assert!(lock.has_document(&Origin::parse("https://www.news.example/")));
        // A URL that redirects off-site lands elsewhere: not admitted.
        let mut lock = SiteLock::default();
        assert!(lock.admit("https://a.example/"));
        assert!(!lock.admit("https://b.example/"));
        assert!(!lock.has_document(&Origin::parse("https://b.example/")));
        // A redirect within the site: only where it landed names the document.
        assert!(lock.admit("https://www.a.example/home"));
        assert!(lock.has_document(&Origin::parse("https://www.a.example/")));
        // Sites are schemeful (M28): `http:` is another site.
        assert!(!lock.admit("http://a.example/"));
        // Host-given HTML: of the renderer's site, or with no address.
        assert!(lock.admit_html(None));
        assert!(lock.admit_html(Some("https://a.example/page")));
        assert!(!lock.admit_html(Some("https://c.example/")));
    }

    /// A renderer started for a document the host already fetched is
    /// answered with it — once, and only for that URL.
    #[test]
    fn a_prefetched_document_is_handed_over_once() {
        let response = FetchResponse { status: 200, final_url: "https://b.example/".into(), headers: vec![], body: b"<html>".to_vec() };
        let mut lock = SiteLock::for_document("https://b.example/", response);
        assert!(lock.check("https://c.example/", &ctx(None, true)).is_err(), "not the expected navigation");
        assert!(matches!(lock.check("https://b.example/", &ctx(None, true)), Ok(Checked::Prefetched(r)) if r.body == b"<html>"));
        assert!(lock.check("https://b.example/", &ctx(None, true)).is_err(), "only once");
        assert!(!lock.admit("https://a.example/"), "locked to the document's site from the start");
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
            spares: Spares::new(Arc::new(Scripted(Mutex::new(Some(connection))))),
            network: NetworkContext::new(),
            viewport: Viewport::new(100.0, 100.0),
            image_loading: ImageLoading::Progressive,
            waker: Arc::default(),
            fonts: None,
            grants: PageGrants::none(),
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

    /// A local server answering `/doc` with a page and `/secret` with JSON;
    /// counts the requests for each path.
    fn counting_server() -> (String, Arc<Mutex<HashMap<String, usize>>>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                *lock(&counted).entry(path.clone()).or_default() += 1;
                let (kind, body): (&str, &[u8]) = if path == "/secret" { ("application/json", br#"{"balance": 1000}"#) } else { ("text/html", b"<html><body>doc</body></html>") };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(body);
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    /// The test plays the renderer, on the far ends of the pipes: it gets the
    /// page's setup, and then whatever it asks the host for.
    struct FakeRenderer {
        to_host: std::io::PipeWriter,
        from_host: std::io::PipeReader,
    }

    impl FakeRenderer {
        fn start(grants: PageGrants) -> (FakeRenderer, Arc<Mailbox>, ProcessPage) {
            let (host_reads, to_host) = std::io::pipe().unwrap();
            let (from_host, host_writes) = std::io::pipe().unwrap();
            let connection = RendererConnection { to_renderer: Box::new(host_writes), from_renderer: Box::new(host_reads), control: Box::new(NoControl) };
            let setup = ProcessSetup {
                spares: Spares::new(Arc::new(Scripted(Mutex::new(Some(connection))))),
                network: NetworkContext::new(),
                viewport: Viewport::new(100.0, 100.0),
                image_loading: ImageLoading::Progressive,
                waker: Arc::default(),
                fonts: None,
                grants,
                fingerprint: privacy::FingerprintPolicy::standard(),
                unresponsive_after: crate::UNRESPONSIVE_AFTER,
            };
            let mailbox = Arc::new(Mailbox::default());
            let page = spawn(PageId(1), Arc::clone(&mailbox), setup).unwrap();
            let mut fake = FakeRenderer { to_host, from_host };
            assert!(matches!(fake.next(), ToRenderer::Setup { .. }));
            (fake, mailbox, page)
        }

        fn next(&mut self) -> ToRenderer {
            ToRenderer::decode(&ipc::read_frame(&mut self.from_host).unwrap().unwrap()).unwrap()
        }

        fn ask(&mut self, message: ToHost) -> ToRenderer {
            ipc::write_frame(&mut self.to_host, &message.encode()).unwrap();
            self.next()
        }

        /// The host has the page load `url`; this renderer makes the
        /// navigation request, and gets the document.
        fn load(&mut self, mailbox: &Mailbox, url: &str) {
            mailbox.post(PageCommand::Load(LoadRequest::Url(url.into())));
            mailbox.post_tick(Instant::now());
            assert!(matches!(self.next(), ToRenderer::Command(PageCommand::Load(_))));
            assert!(matches!(self.next(), ToRenderer::Tick(_)));
            let navigation = WireContext { initiator: None, is_navigation: true, credentials: Credentials::Include, destination: network::Destination::Document };
            match self.ask(ToHost::FetchRequest { id: 100, url: url.into(), context: navigation }) {
                ToRenderer::FetchReply { result: Ok(response), .. } => assert_eq!(response.status, 200),
                other => panic!("the document was not loaded: {other:?}"),
            }
        }
    }

    fn fetch(fake: &mut FakeRenderer, id: u64, url: &str, from: &str, destination: network::Destination) -> std::result::Result<FetchResponse, NetworkError> {
        let context = WireContext { initiator: Some(WireInitiator { url: Some(from.into()), opaque: None }), is_navigation: false, credentials: Credentials::Include, destination };
        match fake.ask(ToHost::FetchRequest { id, url: url.into(), context }) {
            ToRenderer::FetchReply { id: got, result } => {
                assert_eq!(got, id);
                result
            }
            other => panic!("{other:?}"),
        }
    }

    /// Milestone 41 (ADR 025 §5), the acceptance criterion: a renderer whose
    /// page holds no network grant cannot have the browser make a script
    /// request — a credentialed cross-origin `fetch` — however it labels it.
    /// Asked for as script's, it is refused before the network; called an
    /// image load, the request is an image request, and its JSON is
    /// withheld. With the grant, the browser makes it, and still withholds
    /// what CORS keeps from the document.
    #[test]
    fn a_renderer_without_the_network_grant_cannot_get_a_script_request_made() {
        let (doc_server, _) = counting_server();
        let (other, hits) = counting_server();
        let doc = format!("{doc_server}/doc");
        let secret = format!("{other}/secret");
        let (mut fake, mailbox, _page) = FakeRenderer::start(PageGrants::none());
        fake.load(&mailbox, &doc);
        for (id, destination) in [(1, network::Destination::XmlHttpRequest), (2, network::Destination::Script), (3, network::Destination::Other)] {
            match fetch(&mut fake, id, &secret, &doc, destination) {
                Err(NetworkError::Transport(reason)) => assert!(reason.contains("network capability"), "{reason}"),
                other => panic!("{destination:?} was not refused: {other:?}"),
            }
        }
        assert_eq!(lock(&hits).get("/secret"), None, "nothing reached the network");
        let withheld = metrics_guard(Guard::RendererResponseWithheld);
        let as_image = fetch(&mut fake, 4, &secret, &doc, network::Destination::Image).unwrap();
        assert!(as_image.body.is_empty() && as_image.headers.is_empty(), "an image request gets no JSON");
        assert_eq!(lock(&hits).get("/secret"), Some(&1));
        assert!(metrics_guard(Guard::RendererResponseWithheld) > withheld);

        // Granted: made, and still only what CORS allows the document.
        let authority = capability::Authority::new();
        let grants = PageGrants { network: Some(authority.network(NetworkContext::new())), storage: None };
        let (mut fake, mailbox, _page) = FakeRenderer::start(grants);
        fake.load(&mailbox, &doc);
        let response = fetch(&mut fake, 5, &secret, &doc, network::Destination::XmlHttpRequest).unwrap();
        assert!(response.body.is_empty(), "no Access-Control-Allow-Origin: withheld");
        assert_eq!(lock(&hits).get("/secret"), Some(&2));
        let own = fetch(&mut fake, 6, &format!("{doc_server}/secret"), &doc, network::Destination::XmlHttpRequest).unwrap();
        assert!(!own.body.is_empty(), "the document's own origin: whole");
    }

    /// ADR 025 §4: a renderer's storage requests are served from the page's
    /// own grant, only for an origin it loaded — and a revocation is
    /// enforced by the browser at once.
    #[test]
    fn storage_is_served_by_the_browser_only_for_origins_the_renderer_loaded() {
        let (doc_server, _) = counting_server();
        let doc = format!("{doc_server}/doc");
        let store = storage::StorageStore::new();
        let authority = capability::Authority::new();
        let grants = PageGrants { network: None, storage: Some(authority.storage(store.clone())) };
        let (mut fake, mailbox, _page) = FakeRenderer::start(grants.clone());
        let origin = Origin::parse(&doc).serialize();
        let ask = |fake: &mut FakeRenderer, id: u64, origin: &str, op: StorageOp| match fake.ask(ToHost::Storage { id, origin: origin.into(), op }) {
            ToRenderer::StorageReply { id: got, answer } if got == id => answer,
            other => panic!("{other:?}"),
        };
        assert_eq!(ask(&mut fake, 1, &origin, StorageOp::Set("k".into(), "v".into())), StorageAnswer::Refused, "no document of that origin yet");
        fake.load(&mailbox, &doc);
        assert_eq!(ask(&mut fake, 2, &origin, StorageOp::Set("k".into(), "v".into())), StorageAnswer::Written(true));
        assert_eq!(store.area_for(&StorageKey::first_party(&Origin::parse(&doc))).get("k").as_deref(), Some("v"), "in the profile's store");
        assert_eq!(ask(&mut fake, 3, &origin, StorageOp::Get("k".into())), StorageAnswer::Value(Some("v".into())));
        assert_eq!(ask(&mut fake, 4, &origin, StorageOp::Length), StorageAnswer::Length(1));
        assert_eq!(ask(&mut fake, 5, "https://bank.example", StorageOp::Get("k".into())), StorageAnswer::Refused, "another site's storage");
        assert_eq!(ask(&mut fake, 6, "https://bank.example/", StorageOp::Get("k".into())), StorageAnswer::Refused, "not a serialised origin");
        assert_eq!(ask(&mut fake, 7, &"x".repeat(100), StorageOp::Clear), StorageAnswer::Refused);
        grants.revoke(capability::CapabilityKind::Storage);
        assert_eq!(ask(&mut fake, 8, &origin, StorageOp::Get("k".into())), StorageAnswer::Refused, "revoked: refused by the browser");
    }

    fn metrics_guard(guard: Guard) -> u64 {
        diagnostics::metrics::guard_hits(guard)
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
