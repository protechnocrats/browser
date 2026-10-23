//! The renderer side of process isolation (ADR 016): what runs inside a
//! page's own process.
//!
//! The unchanged page loop (`page_thread`) runs here, on its own thread,
//! against a local mailbox. This module feeds that mailbox from the host's
//! messages, sends back what it publishes — frames reduced to what changed
//! (see [`FrameSender`]) — and carries the page's network requests to the
//! host through [`IpcRoute`]: the renderer makes no connections of its own.

use crate::ipc::{self, StorageAnswer, StorageOp, ToHost, ToRenderer, WireContext, WireFrame, WireInitiator};
use crate::page_thread::{self, Mailbox, PageSetup};
use crate::{PageCommand, PageId, Scene};
use network::{FetchResponse, NetworkContext, NetworkError, NetworkRoute, RequestContext};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a request waits for the host's answer before failing.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
/// Requests waiting for the host's answer, by id.
type PendingFetches = Arc<Mutex<HashMap<u64, Sender<Result<FetchResponse, NetworkError>>>>>;

fn send(out: &SharedWriter, message: &ToHost) -> bool {
    let bytes = message.encode();
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    ipc::write_frame(&mut *out, &bytes).is_ok()
}

/// Nanoseconds from `epoch` (negative: before it).
pub(crate) fn to_nanos(epoch: Instant, t: Instant) -> i64 {
    match t.checked_duration_since(epoch) {
        Some(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        None => -i64::try_from(epoch.duration_since(t).as_nanos()).unwrap_or(i64::MAX),
    }
}

/// The inverse of [`to_nanos`] (clamped at the representable range).
pub(crate) fn from_nanos(epoch: Instant, nanos: i64) -> Instant {
    let d = Duration::from_nanos(nanos.unsigned_abs());
    if nanos >= 0 { epoch.checked_add(d).unwrap_or(epoch) } else { epoch.checked_sub(d).unwrap_or(epoch) }
}

/// Runs a renderer: reads the host's messages from `input` and answers on
/// `output` until the host shuts it down or goes away. Returns the process
/// exit code. `sandbox` says whether (and how) the process was sandboxed —
/// reported to the host as a diagnostic.
///
/// Call it from the renderer binary after start-up work that needs the
/// filesystem (see [`prepare_renderer`]) and after entering the sandbox.
pub fn run_renderer(mut input: impl Read, output: impl Write + Send + 'static, sandboxed: bool, sandbox_detail: &str) -> i32 {
    let out: SharedWriter = Arc::new(Mutex::new(Box::new(output)));
    // Start-up work before waiting for the host: a renderer started ahead
    // of need (the engine's spare) is ready by the time a page needs it.
    let (viewport, image_loading, catalog, grant_bits, fingerprint) = match ipc::read_frame(&mut input).ok().flatten().map(|b| ToRenderer::decode(&b)) {
        Some(Ok(ToRenderer::Setup { viewport, image_loading, fonts, grants, fingerprint })) => (viewport, image_loading, fonts, grants, fingerprint),
        _ => return 2, // not a host speaking our protocol
    };
    // Milestone 35: the host's fonts, fetched from it by key when needed —
    // the sandbox keeps the filesystem closed.
    let pending_fonts: PendingFonts = Arc::default();
    let fonts = IpcFonts { catalog, out: Arc::clone(&out), pending: Arc::clone(&pending_fonts), next_id: AtomicU64::new(1) };
    let fonts: Arc<dyn text::FontSource> = Arc::new(fonts);
    if !send(&out, &ToHost::Started { sandboxed, detail: sandbox_detail.to_string() }) {
        return 3;
    }
    let epoch = Instant::now();

    let pending: PendingFetches = Arc::default();
    let route = Arc::new(IpcRoute { out: Arc::clone(&out), pending: Arc::clone(&pending), next_id: AtomicU64::new(1), opaque: Mutex::default() });
    let network = NetworkContext::with_route(move |_store| route);

    let mailbox = Arc::new(Mailbox::default());
    let (signal, signalled) = mpsc::channel::<()>();
    let waker: page_thread::Waker = Arc::new(move || {
        let _ = signal.send(());
    });
    // ADR 010 §2: the grants the host listed, re-created on this side of
    // the boundary — the network capability over the IPC route, storage over
    // a store whose areas the browser process keeps (Milestone 41, ADR 025
    // §4: it checks every operation against the page's grant, and pages of
    // one origin share it). Nothing the host did not list.
    // A grant made later (`ToRenderer::Grant`) is minted the same way, over
    // the same route and store.
    let authority = capability::Authority::new();
    let pending_storage: PendingStorage = Arc::default();
    let remote = IpcStorage { out: Arc::clone(&out), pending: Arc::clone(&pending_storage), next_id: AtomicU64::new(1) };
    let store = storage::StorageStore::remote(Arc::new(remote));
    let mint = {
        let network = network.clone();
        move |bits: u8| {
            let granted = |kind: capability::CapabilityKind| bits & kind.bit() != 0;
            capability::PageGrants {
                network: granted(capability::CapabilityKind::Network).then(|| authority.network(network.clone())),
                storage: granted(capability::CapabilityKind::Storage).then(|| authority.storage(store.clone())),
            }
        }
    };
    let grants = mint(grant_bits);
    let setup = PageSetup {
        network,
        grants,
        fingerprint,
        fonts: Some(fonts),
        viewport,
        waker: Arc::new(Mutex::new(Some(waker))),
        image_loading,
    };
    if page_thread::spawn(PageId(1), Arc::clone(&mailbox), setup).is_err() {
        send(&out, &ToHost::Crashed { reason: "the renderer could not start its page thread".into() });
        return 4;
    }

    // Forward what the page publishes.
    {
        let (mailbox, out) = (Arc::clone(&mailbox), Arc::clone(&out));
        std::thread::Builder::new()
            .name("renderer-out".into())
            .spawn(move || {
                let mut frames = FrameSender::default();
                let (mut last_tick, mut last_wake) = (None, None);
                while signalled.recv().is_ok() {
                    while signalled.try_recv().is_ok() {}
                    let (events, frame, status) = mailbox.collect();
                    if let Some(reason) = status.crashed {
                        send(&out, &ToHost::Crashed { reason });
                        return;
                    }
                    let Some(tick) = status.completed_tick else { continue };
                    // A new wake time alone is worth sending: finished
                    // background work asks the host for a tick (ADR 020).
                    if events.is_empty() && frame.is_none() && last_tick == Some(tick) && last_wake == status.next_wake {
                        continue;
                    }
                    last_tick = Some(tick);
                    last_wake = status.next_wake;
                    let frame = frame.map(|(scene, stats)| {
                        let (wire, images) = frames.prepare(&scene, stats);
                        for image in images {
                            if !send(&out, &image) {
                                return None;
                            }
                        }
                        Some(Box::new(wire))
                    });
                    let frame = match frame {
                        Some(None) => return, // the host is gone
                        Some(Some(f)) => Some(f),
                        None => None,
                    };
                    let message = ToHost::Published {
                        events,
                        frame,
                        next_wake: status.next_wake.map(|t| to_nanos(epoch, t)),
                        completed_tick: to_nanos(epoch, tick),
                    };
                    if !send(&out, &message) {
                        return;
                    }
                }
            })
            .ok();
    }

    // Feed the host's messages to the page.
    let code = loop {
        let message = match ipc::read_frame(&mut input) {
            Ok(Some(bytes)) => match ToRenderer::decode(&bytes) {
                Ok(m) => m,
                Err(_) => break 5,
            },
            Ok(None) | Err(_) => break 0, // the host went away
        };
        match message {
            ToRenderer::Command(command) => mailbox.post(command),
            ToRenderer::Grant(bits) => mailbox.post(PageCommand::Grant(mint(bits))),
            ToRenderer::Tick(nanos) => mailbox.post_tick(from_nanos(epoch, nanos)),
            ToRenderer::FetchReply { id, result } => {
                if let Some(reply) = pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                    let _ = reply.send(result);
                }
            }
            ToRenderer::FontReply { id, bytes } => {
                if let Some(reply) = pending_fonts.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                    let _ = reply.send(bytes);
                }
            }
            ToRenderer::StorageReply { id, answer } => {
                if let Some(reply) = pending_storage.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                    let _ = reply.send(answer);
                }
            }
            ToRenderer::Shutdown => break 0,
            ToRenderer::Setup { .. } => break 5,
        }
    };
    mailbox.shutdown();
    // Unblock every request still waiting for the host.
    pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    pending_fonts.lock().unwrap_or_else(PoisonError::into_inner).clear();
    pending_storage.lock().unwrap_or_else(PoisonError::into_inner).clear();
    code
}

/// Start-up work a renderer must do before its sandbox closes the
/// filesystem: sizing and naming the shared worker pool (which reads the
/// CPU and cgroup configuration).
pub fn prepare_renderer() {
    let _ = rayon::ThreadPoolBuilder::new().thread_name(|i| format!("worker-{i}")).build_global();
}

/// Reduces frames to what the host does not have yet: glyph-atlas bands it
/// was not sent (compared by identity — the atlas copies a band when it
/// writes to one a snapshot holds), and images by id.
#[derive(Default)]
struct FrameSender {
    atlas_dims: (u32, u32),
    sent_bands: Vec<Option<Arc<Vec<u8>>>>,
    sent_images: HashMap<u64, Arc<Vec<u8>>>,
}

impl FrameSender {
    fn prepare(&mut self, scene: &Scene, stats: crate::FrameStats) -> (WireFrame, Vec<ToHost>) {
        let atlas = &scene.glyph_atlas;
        let dims = (atlas.width as u32, atlas.height as u32);
        if dims != self.atlas_dims {
            self.atlas_dims = dims;
            self.sent_bands.clear();
        }
        let mut atlas_bands = Vec::new();
        for (i, (_, band)) in atlas.pixels.bands().enumerate() {
            if self.sent_bands.len() <= i {
                self.sent_bands.resize(i + 1, None);
            }
            if self.sent_bands[i].as_ref().is_some_and(|b| Arc::ptr_eq(b, band)) {
                continue;
            }
            atlas_bands.push((i as u32, band.as_ref().clone()));
            self.sent_bands[i] = Some(Arc::clone(band));
        }
        let mut images = Vec::new();
        let mut used = Vec::new();
        for image in &scene.images {
            let sendable = image.width >= 1
                && image.height >= 1
                && image.width <= ipc::MAX_IMAGE_SIDE
                && image.height <= ipc::MAX_IMAGE_SIDE
                && image.rgba.len() <= ipc::MAX_IMAGE_BYTES
                && image.rgba.len() as u64 == u64::from(image.width) * u64::from(image.height) * 4;
            if !sendable {
                continue; // too large to send: not drawn (the GPU would refuse it too)
            }
            used.push((image.id, image.width, image.height));
            if self.sent_images.get(&image.id).is_some_and(|sent| Arc::ptr_eq(sent, &image.rgba)) {
                continue;
            }
            images.push(ToHost::Image { id: image.id, width: image.width, height: image.height, rgba: image.rgba.as_ref().clone() });
            self.sent_images.insert(image.id, Arc::clone(&image.rgba));
        }
        // The host keeps only what this frame uses; so does this record.
        self.sent_images.retain(|id, _| used.iter().any(|(u, _, _)| u == id));
        let wire = WireFrame {
            viewport_width: scene.viewport_width,
            viewport_height: scene.viewport_height,
            background: scene.background,
            items: scene.items.as_ref().clone(),
            atlas_width: dims.0,
            atlas_height: dims.1,
            atlas_generation: atlas.generation,
            atlas_bands,
            images: used,
            damage: scene.damage,
            stats,
        };
        (wire, images)
    }
}

/// The renderer's only way onto the network: every request goes to the host
/// (ADR 016 §5), which makes it with the profile's context — and checks it.
type PendingFonts = Arc<Mutex<HashMap<u64, Sender<Option<Vec<u8>>>>>>;

/// How long a renderer waits for the host to send a font. The page thread
/// waits for it inside a tick, so this stays well under
/// the unresponsive bound (ADR 013): a slow host means a fallback font, not
/// an ended page.
const FONT_WAIT: Duration = Duration::from_secs(3);

/// The host's fonts, as the renderer sees them: the catalog it was given,
/// and each font's bytes asked of the host when first needed.
struct IpcFonts {
    catalog: Vec<text::CatalogEntry>,
    out: SharedWriter,
    pending: PendingFonts,
    next_id: AtomicU64,
}

impl text::FontSource for IpcFonts {
    fn catalog(&self) -> Vec<text::CatalogEntry> {
        self.catalog.clone()
    }

    fn load(&self, entry: &text::CatalogEntry) -> Option<Vec<u8>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = mpsc::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, reply);
        if !send(&self.out, &ToHost::FontRequest { id, key: entry.key.clone(), index: entry.index }) {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return None;
        }
        let bytes = answer.recv_timeout(FONT_WAIT).ok().flatten();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        bytes
    }
}

type PendingStorage = Arc<Mutex<HashMap<u64, Sender<StorageAnswer>>>>;

/// How long a storage operation waits for the browser. `localStorage` is
/// synchronous, so the page thread waits inside a tick: well under the
/// unresponsive bound (ADR 013), as for fonts.
const STORAGE_WAIT: Duration = Duration::from_secs(3);

/// Milestone 41 (ADR 025 §4): `localStorage` kept by the browser process.
/// Every operation is a request it checks and answers; one it does not
/// answer reads as empty and refuses writes.
struct IpcStorage {
    out: SharedWriter,
    pending: PendingStorage,
    next_id: AtomicU64,
}

impl IpcStorage {
    fn ask(&self, key: &origin::StorageKey, op: StorageOp) -> StorageAnswer {
        // First-party areas only: no frames exist, so no other kind can be
        // asked for honestly.
        if key.top_level_site != key.origin.site() {
            return StorageAnswer::Refused;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = mpsc::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, reply);
        let sent = send(&self.out, &ToHost::Storage { id, origin: key.origin.serialize(), op });
        let answer = if sent { answer.recv_timeout(STORAGE_WAIT).unwrap_or(StorageAnswer::Refused) } else { StorageAnswer::Refused };
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        answer
    }
}

impl storage::RemoteAreas for IpcStorage {
    fn get(&self, key: &origin::StorageKey, name: &str) -> Option<String> {
        match self.ask(key, StorageOp::Get(name.to_string())) {
            StorageAnswer::Value(v) => v,
            _ => None,
        }
    }

    fn set(&self, key: &origin::StorageKey, name: String, value: String) -> Result<(), storage::QuotaExceeded> {
        match self.ask(key, StorageOp::Set(name, value)) {
            StorageAnswer::Written(true) => Ok(()),
            _ => Err(storage::QuotaExceeded),
        }
    }

    fn remove(&self, key: &origin::StorageKey, name: &str) {
        self.ask(key, StorageOp::Remove(name.to_string()));
    }

    fn clear(&self, key: &origin::StorageKey) {
        self.ask(key, StorageOp::Clear);
    }

    fn len(&self, key: &origin::StorageKey) -> usize {
        match self.ask(key, StorageOp::Length) {
            StorageAnswer::Length(n) => n as usize,
            _ => 0,
        }
    }
}

struct IpcRoute {
    out: SharedWriter,
    pending: PendingFetches,
    next_id: AtomicU64,
    /// Opaque origins seen, with the process-local ids they travel as (an
    /// opaque origin equals only itself: the host needs a stable name).
    opaque: Mutex<Vec<(origin::Origin, u64)>>,
}

impl IpcRoute {
    fn opaque_id(&self, origin: &origin::Origin) -> u64 {
        let mut known = self.opaque.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, id)) = known.iter().find(|(o, _)| o.same_origin(origin)) {
            return *id;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if known.len() >= 256 {
            known.remove(0);
        }
        known.push((origin.clone(), id));
        id
    }
}

impl NetworkRoute for IpcRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.fetch_with(url, &RequestContext::default())
    }

    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let initiator = ctx.initiator.as_ref().map(|i| WireInitiator {
            url: i.url.clone(),
            opaque: i.origin.is_opaque().then(|| self.opaque_id(&i.origin)),
        });
        let context = WireContext { initiator, is_navigation: ctx.is_navigation, credentials: ctx.credentials, destination: ctx.destination };
        let (reply, answer) = mpsc::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, reply);
        if !send(&self.out, &ToHost::FetchRequest { id, url: url.to_string(), context }) {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(NetworkError::Transport("the browser process is gone".into()));
        }
        match answer.recv_timeout(FETCH_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                Err(NetworkError::Transport("no answer from the browser process".into()))
            }
        }
    }
}
