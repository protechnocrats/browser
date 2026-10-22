//! The page thread — Milestone 30 (`docs/adr/006-page-thread-actor-model.md`,
//! `docs/adr/007-panic-containment.md`).
//!
//! Each page runs on its own OS thread, which builds and exclusively owns
//! the page's `!Send` state (the `Rc` DOM, the Boa context, caches, the glyph
//! atlas). The host reaches it only through a [`Mailbox`] of plain `Send`
//! data:
//!
//! - an **inbox**: queued [`PageCommand`]s (bounded, coalescing) and the
//!   latest requested tick time;
//! - an **outbox**: events, one latest-wins frame slot, and the page's status.
//!
//! The thread sleeps on a condition variable until the host posts a tick
//! (the host drives the clock — ADR 005), then applies the queued commands,
//! advances the page to the tick time, publishes the results, and calls the
//! waker. Its whole body runs under `catch_unwind`: a panic becomes that
//! page's crash, never the browser's.

use crate::{frame, input, EngineEvent, FrameStats, PageCommand, PageId, Scene, Viewport, FRAME_INTERVAL};
use crate::page::Page;
use diagnostics::Guard;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Most commands a page's inbox holds. Past it, the oldest pointer move or
/// scroll is dropped (and failing that, the new command) — a page that has
/// stopped consuming its inbox (stuck in a script) costs bounded memory.
pub const MAX_QUEUED_COMMANDS: usize = 256;

/// Longest key name accepted in `EngineInput::Key` (DOM key values are
/// short: `"a"`, `"ArrowLeft"`, `"AudioVolumeMute"`).
pub const MAX_KEY_BYTES: usize = 64;

/// Largest HTML string `LoadRequest::Html` accepts — the network layer's
/// response-body cap, so a string load is no bigger than a fetched one.
pub const MAX_HTML_BYTES: usize = 100 * 1024 * 1024;

/// A page thread's stack. A spawned thread gets 2 MiB by default, less than
/// the 8 MiB main thread every recursion bound in the pipeline (layout and
/// style depth caps, the parser's nesting cap, Boa's recursion limit) was
/// sized against; a stack overflow aborts the whole process, so page
/// threads get headroom instead (ADR 007).
const PAGE_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;
// Layout recursion needs at least this much (see `layout::MIN_STACK_BYTES`).
const _: () = assert!(PAGE_THREAD_STACK_BYTES >= layout::MIN_STACK_BYTES);

/// Called by a page thread after it publishes results, so a sleeping host
/// can wake up and collect them (the desktop shell turns it into an
/// event-loop wake-up).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// The engine's current waker, shared with every page thread so a host can
/// install one after pages exist.
pub(crate) type WakerSlot = Arc<Mutex<Option<Waker>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // ADR 007: a panic elsewhere must not poison the mailbox for everyone.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct Inbox {
    commands: VecDeque<PageCommand>,
    /// The latest tick the host asked for; `None` = nothing to do.
    tick: Option<Instant>,
    shutdown: bool,
}

/// What a page has published and the host has not yet collected.
#[derive(Default)]
pub(crate) struct Outbox {
    pub events: Vec<EngineEvent>,
    /// Latest wins: a newer frame replaces an uncollected older one.
    pub frame: Option<(Scene, FrameStats)>,
    pub status: PageStatus,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PageStatus {
    /// When the page next needs a tick, if nothing else happens.
    pub next_wake: Option<Instant>,
    /// The tick time the page most recently finished.
    pub completed_tick: Option<Instant>,
    /// Set when the page's thread panicked (or could not start): the reason.
    pub crashed: Option<String>,
}

/// Commands a full inbox never drops: losing a key press beats a page
/// keeping authority the host took back (Milestone 36), or animating where
/// nobody can see it (Milestone 38b). The host bounds how many it sends.
fn never_dropped(command: &PageCommand) -> bool {
    matches!(command, PageCommand::Revoke(_) | PageCommand::Grant(_) | PageCommand::SetVisible(_))
}

/// The only thing a host and a page thread share.
#[derive(Default)]
pub(crate) struct Mailbox {
    inbox: Mutex<Inbox>,
    inbox_ready: Condvar,
    outbox: Mutex<Outbox>,
    outbox_ready: Condvar,
    /// Milestone 38b (ADR 020): background work for the page finished and it
    /// wants a tick. Set from any thread; cleared when a tick starts.
    wanted: AtomicBool,
}

impl Mailbox {
    /// Queues `command`, coalescing with what is already queued: a pointer
    /// move replaces the latest queued pointer move, a scroll adds into the
    /// latest queued scroll, a resize replaces the latest queued resize — as
    /// long as nothing order-sensitive (a click, a key, a load) was queued
    /// after it. Those three kinds do not depend on each other's order, so
    /// merging across one another is safe. Never blocks.
    pub fn post(&self, command: PageCommand) {
        use crate::EngineInput::{PointerMove, Scroll};
        let mut inbox = lock(&self.inbox);
        let queue = &mut inbox.commands;
        let coalescable = |c: &PageCommand| matches!(c, PageCommand::Input(PointerMove { .. } | Scroll { .. }) | PageCommand::Resize(_));
        if coalescable(&command) {
            let same_kind = |c: &PageCommand| std::mem::discriminant(c) == std::mem::discriminant(&command)
                && match (c, &command) {
                    (PageCommand::Input(a), PageCommand::Input(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
                    _ => true,
                };
            // Walk back over the trailing run of coalescable commands only.
            if let Some(queued) = queue.iter_mut().rev().take_while(|c| coalescable(c)).find(|c| same_kind(c)) {
                match (queued, command) {
                    (PageCommand::Input(Scroll { dx: qx, dy: qy }), PageCommand::Input(Scroll { dx, dy })) => {
                        *qx += dx;
                        *qy += dy;
                    }
                    (queued, command) => *queued = command,
                }
                return;
            }
        }
        if queue.len() >= MAX_QUEUED_COMMANDS {
            let coalescable = queue.iter().position(|c| matches!(c, PageCommand::Input(PointerMove { .. } | Scroll { .. })));
            diagnostics::guard(Guard::InputDropped, "a page's inbox was full; input was dropped");
            // Milestone 36: a revocation is never the command dropped — it
            // takes the oldest input instead (losing a key press beats a page
            // keeping authority the host took back).
            let droppable = coalescable.or_else(|| {
                never_dropped(&command).then(|| queue.iter().position(|c| matches!(c, PageCommand::Input(_)))).flatten()
            });
            match droppable {
                Some(index) => {
                    queue.remove(index);
                }
                // Everything queued matters (clicks, keys, loads): drop the
                // newcomer rather than reorder or grow.
                None if !never_dropped(&command) => return,
                // Only revocations and loads queued: a revocation still goes
                // in (at most one per kind matters, so this is bounded by
                // the host, never by content).
                None => {}
            }
        }
        queue.push_back(command);
    }

    pub fn queued_commands(&self) -> usize {
        lock(&self.inbox).commands.len()
    }

    /// Whether a tick has been posted that the page has not started yet —
    /// it will take every queued command with it.
    pub fn tick_posted(&self) -> bool {
        lock(&self.inbox).tick.is_some()
    }

    /// Asks the page to advance to `now` (a later request replaces an
    /// earlier one it has not started yet) and wakes its thread.
    pub fn post_tick(&self, now: Instant) {
        let mut inbox = lock(&self.inbox);
        inbox.tick = Some(inbox.tick.map_or(now, |t| t.max(now)));
        self.inbox_ready.notify_one();
    }

    pub fn shutdown(&self) {
        lock(&self.inbox).shutdown = true;
        self.inbox_ready.notify_one();
    }

    /// Everything published since the last call.
    pub fn collect(&self) -> (Vec<EngineEvent>, Option<(Scene, FrameStats)>, PageStatus) {
        let mut outbox = lock(&self.outbox);
        (std::mem::take(&mut outbox.events), outbox.frame.take(), outbox.status.clone())
    }

    /// Waits until the page has finished a tick at or after `tick`, has
    /// crashed, or `deadline` passes. Returns whether it finished in time.
    pub fn wait_for_tick(&self, tick: Instant, deadline: Instant) -> bool {
        let mut outbox = lock(&self.outbox);
        loop {
            if outbox.status.crashed.is_some() || outbox.status.completed_tick.is_some_and(|done| done >= tick) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            outbox = self.outbox_ready.wait_timeout(outbox, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Marks the page crashed with `reason` (called by the page thread's
    /// panic handler, or by the host if the thread could not start).
    pub fn mark_crashed(&self, reason: String) {
        let mut outbox = lock(&self.outbox);
        outbox.status.crashed = Some(reason);
        outbox.status.next_wake = None;
        self.outbox_ready.notify_all();
    }

    /// Asks the host for a tick: background work the page waits for (a
    /// response, a decode, a navigation) is ready. Callable from any thread;
    /// the caller then wakes the host. A request made while a tick runs is
    /// kept for that tick's report, so it is never lost (ADR 020).
    pub fn request_tick(&self) {
        self.wanted.store(true, Ordering::SeqCst);
        let mut outbox = lock(&self.outbox);
        // Due at once, in the host's clock: the last tick the page finished
        // (never later than the host's "now").
        let due = outbox.status.completed_tick.unwrap_or_else(Instant::now);
        outbox.status.next_wake = Some(outbox.status.next_wake.map_or(due, |t| t.min(due)));
        self.outbox_ready.notify_all();
    }

    /// Whether the page asked for a tick it has not started yet.
    pub fn wants_tick(&self) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }

    /// Whether the page has been marked crashed.
    pub fn is_crashed(&self) -> bool {
        lock(&self.outbox).status.crashed.is_some()
    }

    /// Whether the host shut the page down.
    pub fn is_shut_down(&self) -> bool {
        lock(&self.inbox).shutdown
    }

    /// Blocks the page thread until there is a tick to run (returning the
    /// queued commands and the tick) or the page is shut down (`None`).
    pub(crate) fn wait_for_work(&self) -> Option<(Vec<PageCommand>, Instant)> {
        let mut inbox = lock(&self.inbox);
        loop {
            if inbox.shutdown {
                return None;
            }
            if let Some(tick) = inbox.tick.take() {
                // This tick sees everything finished so far.
                self.wanted.store(false, Ordering::SeqCst);
                return Some((inbox.commands.drain(..).collect(), tick));
            }
            inbox = self.inbox_ready.wait(inbox).unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) fn publish(&self, events: Vec<EngineEvent>, frame: Option<(Scene, FrameStats)>, next_wake: Option<Instant>, tick: Instant) {
        let mut outbox = lock(&self.outbox);
        outbox.events.extend(events);
        if frame.is_some() {
            outbox.frame = frame;
        }
        // Work that finished during this tick, after the page looked.
        let next_wake = if self.wanted.load(Ordering::SeqCst) { Some(next_wake.map_or(tick, |t| t.min(tick))) } else { next_wake };
        outbox.status.next_wake = next_wake;
        outbox.status.completed_tick = Some(tick);
        self.outbox_ready.notify_all();
    }
}

// ADR 006: the mailbox is the ONLY state a host and a page thread share,
// so it must be shareable — and it holds only plain data (no `Rc`), which
// this proves at compile time.
const _: () = {
    const fn assert_shared<T: Send + Sync>() {}
    assert_shared::<Mailbox>();
    assert_shared::<PageSetup>();
};

/// What a page thread starts with — all `Send`, cloned from the engine.
pub(crate) struct PageSetup {
    pub network: network::NetworkContext,
    /// Milestone 36: what the page's documents' scripts may reach (ADR 010).
    pub grants: capability::PageGrants,
    /// Milestone 37: what the page's scripts may observe (ADR 019 §3).
    pub fingerprint: privacy::FingerprintPolicy,
    /// Milestone 35: the host's fonts; each document gets a font set of its
    /// own over them.
    pub fonts: Option<Arc<dyn text::FontSource>>,
    pub viewport: Viewport,
    pub waker: WakerSlot,
    pub image_loading: crate::ImageLoading,
}

/// Starts page `id`'s thread.
pub(crate) fn spawn(id: PageId, mailbox: Arc<Mailbox>, setup: PageSetup) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("page-{}", id.0))
        .stack_size(PAGE_THREAD_STACK_BYTES)
        .spawn(move || run(id, mailbox, setup))
}

fn wake(slot: &WakerSlot) {
    let waker = lock(slot).clone();
    if let Some(waker) = waker {
        waker();
    }
}

/// The thread body: the page loop under `catch_unwind` (ADR 007).
fn run(id: PageId, mailbox: Arc<Mailbox>, setup: PageSetup) {
    let waker = Arc::clone(&setup.waker);
    let outcome = catch_unwind(AssertUnwindSafe(|| page_loop(id, &mailbox, setup)));
    // The page (and its script environment) is gone, dropped normally or
    // by the unwind: free its script heap while this thread is running,
    // not in the thread's exit teardown.
    js::collect_garbage();
    if let Err(payload) = outcome {
        let reason = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "the page's thread panicked".to_string());
        diagnostics::guard(Guard::PageCrashed, "a page thread panicked; the page is marked crashed");
        mailbox.mark_crashed(reason);
        wake(&waker);
    }
}

/// One page's engine-side state. Lives on the page thread only.
struct PageState {
    page: Page,
    atlas: text::GlyphAtlas,
    /// This document's fonts, and the document they are for.
    font: Arc<text::Font>,
    font_generation: u64,
    viewport: Viewport,
    /// Something changed that the last frame does not show.
    needs_frame: bool,
    /// The page's time origin for `requestAnimationFrame` timestamps: the
    /// first tick it saw.
    time_origin: Option<Instant>,
    /// The latest tick time; `next_wake` schedules from it.
    last_tick: Option<Instant>,
    /// When rAF callbacks and CSS animations next advance: at most once per
    /// [`FRAME_INTERVAL`] however often ticks arrive (architecture §5, power
    /// budget). Input and other changes still get a frame immediately.
    next_animation_tick: Option<Instant>,
    /// Milestone 38b (ADR 020): whether anyone can see the page. Hidden, it
    /// does no animation work and builds no frames.
    visible: bool,
    /// Milestone 38b (ADR 020): the step of [`DEGRADE_STEPS`] the animation
    /// interval is at, and how many animation frames in a row were fast.
    budget_level: usize,
    fast_frames: u32,
}

fn page_loop(id: PageId, mailbox: &Arc<Mailbox>, setup: PageSetup) {
    let PageSetup { network, grants, fingerprint, fonts, viewport, waker, image_loading } = setup;
    // Milestone 38b (ADR 020): finished background work wakes the page.
    let notify: network::Notify = {
        let (mailbox, waker) = (Arc::clone(mailbox), Arc::clone(&waker));
        Arc::new(move || {
            mailbox.request_tick();
            wake(&waker);
        })
    };
    let network = network.with_notify(notify);
    let new_font = move || Arc::new(match &fonts {
        Some(source) => text::Font::with_source(Arc::clone(source)),
        None => text::Font::dejavu_sans(),
    });
    let page = Page::blank(network, grants, fingerprint);
    page.image_loading.set(image_loading);
    let mut state = PageState {
        page,
        atlas: text::GlyphAtlas::new(512, 128),
        font: new_font(),
        font_generation: 0,
        viewport,
        needs_frame: true,
        time_origin: None,
        last_tick: None,
        next_animation_tick: None,
        visible: true,
        budget_level: 0,
        fast_frames: 0,
    };
    while let Some((commands, now)) = mailbox.wait_for_work() {
        let mut events = Vec::new();
        for command in commands {
            match validate(command) {
                Ok(command) => {
                    let font = Arc::clone(&state.font);
                    apply(id, &mut state, command, &font, &mut events)
                }
                Err(rejected) => reject(id, rejected, &mut events),
            }
        }
        // A new document: a font set (and a glyph atlas, whose entries name
        // faces) of its own.
        if state.page.document_generation.get() != state.font_generation {
            state.font_generation = state.page.document_generation.get();
            state.font = new_font();
            state.atlas = text::GlyphAtlas::new(512, 128);
        }
        let font = Arc::clone(&state.font);
        let frame = tick(id, &mut state, &font, now, &mut events);
        mailbox.publish(events, frame, next_wake(&state), now);
        wake(&waker);
    }
}

/// A command that failed validation on receipt, and why.
struct Rejected {
    /// For a rejected load: the URL to report the failure against.
    load_url: Option<String>,
    reason: &'static str,
}

/// Zero-trust at the thread boundary (ADR 006 §6): every command is checked
/// on receipt, whoever sent it.
fn validate(command: PageCommand) -> Result<PageCommand, Rejected> {
    use crate::{EngineInput, LoadRequest};
    let reject = |reason| Err(Rejected { load_url: None, reason });
    match command {
        PageCommand::Load(LoadRequest::Url(url)) if url.len() > origin::MAX_URL_BYTES => {
            Err(Rejected { load_url: Some(String::new()), reason: "the URL is too long" })
        }
        PageCommand::Load(LoadRequest::Html { html, url }) if html.len() > MAX_HTML_BYTES || url.as_ref().is_some_and(|u| u.len() > origin::MAX_URL_BYTES) => {
            let _ = html;
            Err(Rejected { load_url: Some(String::new()), reason: "the HTML document or its URL is too large" })
        }
        PageCommand::Input(EngineInput::PointerMove { x, y } | EngineInput::PointerDown { x, y } | EngineInput::PointerUp { x, y }) if !(x.is_finite() && y.is_finite()) => {
            reject("a pointer position was not finite")
        }
        PageCommand::Input(EngineInput::Scroll { dx, dy }) if !(dx.is_finite() && dy.is_finite()) => reject("a scroll delta was not finite"),
        PageCommand::Input(EngineInput::Key { key }) if key.is_empty() || key.len() > MAX_KEY_BYTES => reject("a key name was empty or too long"),
        PageCommand::Resize(viewport) => Ok(PageCommand::Resize(viewport.sanitized())),
        other => Ok(other),
    }
}

fn reject(id: PageId, rejected: Rejected, events: &mut Vec<EngineEvent>) {
    diagnostics::guard(Guard::EngineMessageRejected, rejected.reason);
    if let Some(url) = rejected.load_url {
        events.push(EngineEvent::NavigationFailed { page: id, url, error: rejected.reason.to_string() });
    }
}

fn apply(id: PageId, state: &mut PageState, command: PageCommand, font: &text::Font, events: &mut Vec<EngineEvent>) {
    match command {
        PageCommand::Load(request) => {
            input::load(&state.page, id, request, events);
            state.needs_frame = true;
        }
        PageCommand::Input(event) => {
            if input::handle(&state.page, id, state.viewport, event, font, events) {
                state.needs_frame = true;
            }
        }
        PageCommand::Resize(viewport) => {
            if viewport != state.viewport {
                state.viewport = viewport;
                state.needs_frame = true;
            }
        }
        PageCommand::SimulateCrash => panic!("simulated crash requested by the host (PageCommand::SimulateCrash)"),
        // Milestone 36: every handle of the grant — the current document's
        // script's included — refuses from now on; later documents of this
        // page get the revoked handle.
        PageCommand::Revoke(kind) => {
            state.page.grants.revoke(kind);
            diagnostics::tracing::info!(target: "engine::permission", page = id.0, capability = kind.name(), "capability revoked");
        }
        PageCommand::Grant(grants) => {
            let kinds = grants.kinds();
            state.page.grant(grants);
            for kind in kinds {
                diagnostics::tracing::info!(target: "engine::permission", page = id.0, capability = kind.name(), "capability granted");
            }
        }
        PageCommand::SetVisible(visible) => {
            if visible && !state.visible {
                // Draw at once; animations restart from now, without a burst
                // to catch up (ADR 020).
                state.needs_frame = true;
                state.next_animation_tick = None;
            }
            state.visible = visible;
        }
    }
}

/// A `requestAnimationFrame` callback is waiting, or a transition or
/// `@keyframes` animation is in flight that someone could see.
fn is_animating(state: &PageState) -> bool {
    state.page.js_env.borrow().has_pending_animation_frame_callbacks() || state.page.animation_registry.borrow().any_active() || transitions_visible(state)
}

/// Whether a transition in flight could change what is on screen (M38b's
/// gap, ADR 020's first degrade step). Not when every one animates only
/// `background-color` — which paints inside its element's border box and
/// moves nothing — on an element whose box, in the last layout, lies
/// outside the viewport and that only a new layout could move (no
/// transformed, fixed, sticky or scrolling ancestor). Such a page builds
/// no frames for them and sleeps; a scroll, resize or change brings a
/// frame, and the transitions' values follow from the time.
fn transitions_visible(state: &PageState) -> bool {
    let registry = state.page.transition_registry.borrow();
    if !registry.any_active() {
        return false;
    }
    let tree = state.page.last_layout.borrow();
    let Some(tree) = tree.as_ref() else { return true };
    let reuse = state.page.layout_reuse.borrow();
    let top = state.page.scroll_offset_y.get();
    let (bottom, right) = (top + state.viewport.height, state.viewport.width);
    let mut active = registry.active();
    let off_screen = active.all(|(node, property)| {
        property == style::TransitionProperty::BackgroundColor
            && reuse.still_rect_of(tree, node).is_some_and(|(x0, y0, x1, y1)| y1 <= top || y0 >= bottom || x1 <= 0.0 || x0 >= right)
    });
    !off_screen
}

/// Milestone 38b (ADR 020): the animation interval, as multiples of
/// [`FRAME_INTERVAL`] — 60, 30, 15, 7.5 and 4 frames a second.
const DEGRADE_STEPS: [u32; 5] = [1, 2, 4, 8, 15];

/// Fast animation frames in a row (under an eighth of the interval) before
/// the rate steps back up.
const RECOVER_AFTER: u32 = 30;

fn animation_interval(state: &PageState) -> Duration {
    FRAME_INTERVAL * DEGRADE_STEPS[state.budget_level]
}

/// Charges one animation frame's work to the page's budget (ADR 020): more
/// than half the interval steps the frame rate down; a run of frames under an
/// eighth of it steps back up. The gap keeps a steady load from oscillating.
fn account(id: PageId, state: &mut PageState, work: Duration) {
    let interval = animation_interval(state);
    if work > interval / 2 {
        state.fast_frames = 0;
        if state.budget_level + 1 < DEGRADE_STEPS.len() {
            state.budget_level += 1;
            diagnostics::guard(Guard::FrameBudgetExceeded, "an animating page could not keep up; its frame rate was lowered");
            diagnostics::tracing::info!(target: "engine::budget", page = id.0, interval_ms = animation_interval(state).as_millis() as u64, "frame rate lowered");
        }
    } else if work < interval / 8 {
        state.fast_frames += 1;
        if state.fast_frames >= RECOVER_AFTER && state.budget_level > 0 {
            state.budget_level -= 1;
            state.fast_frames = 0;
            diagnostics::tracing::info!(target: "engine::budget", page = id.0, interval_ms = animation_interval(state).as_millis() as u64, "frame rate raised");
        }
    } else {
        state.fast_frames = 0;
    }
}

/// Slack when deciding whether a frame interval has passed, so a host that
/// wakes a hair early (timer granularity) does not skip a whole frame.
const FRAME_SLACK: Duration = Duration::from_millis(1);

/// Advances the page to `now`; returns a frame if the picture changed.
fn tick(id: PageId, state: &mut PageState, font: &text::Font, now: Instant, events: &mut Vec<EngineEvent>) -> Option<(Scene, FrameStats)> {
    let time_origin = *state.time_origin.get_or_insert(now);
    state.last_tick = Some(now);

    if input::land_navigation(&state.page, id, events) {
        state.needs_frame = true;
    }

    // Hidden: no animation work at all (ADR 020).
    let animation_due = state.visible && state.next_animation_tick.is_none_or(|due| now + FRAME_SLACK >= due);
    // The page thread's own clock: the budget is real time (ADR 020).
    let animation_work = (animation_due && is_animating(state)).then(Instant::now);
    if animation_work.is_some() {
        // Keep the ideal cadence (due, due + interval, …) rather than
        // rescheduling from `now`, which would drift faster by the slack;
        // after a stall, restart from now instead of bursting to catch up.
        let interval = animation_interval(state);
        state.next_animation_tick = Some(match state.next_animation_tick {
            Some(due) if now < due + interval => due + interval,
            _ => now + interval,
        });
        {
            let mut env = state.page.js_env.borrow_mut();
            if env.has_pending_animation_frame_callbacks() {
                let timestamp_ms = now.saturating_duration_since(time_origin).as_secs_f64() * 1000.0;
                env.run_animation_frame_callbacks(timestamp_ms);
            }
        }
        // A transition or animation in flight changes the picture every
        // frame without any script running — unless nobody can see it.
        if state.page.animation_registry.borrow().any_active() || transitions_visible(state) {
            state.needs_frame = true;
        }
    }
    {
        let mut env = state.page.js_env.borrow_mut();
        env.poll_pending_fetches();
        if env.take_dirty() {
            state.needs_frame = true;
        }
    }
    // Images arriving (progressive loading) change the picture.
    if state.page.poll_images() {
        state.needs_frame = true;
    }
    // Milestone 35: web fonts that arrived join the page's font set; text
    // already laid out with a fallback is laid out again.
    state.page.poll_fonts();
    let ready = std::mem::take(&mut *state.page.ready_fonts.borrow_mut());
    if !ready.is_empty() {
        for f in ready {
            if let Err(e) = font.add_web_font(&f.family, f.bytes, f.weight, f.style) {
                diagnostics::guard(Guard::RendererRequestRefused, &format!("web font for {} refused: {e}", f.family));
            }
        }
        *state.page.last_layout.borrow_mut() = None;
        *state.page.last_frame.borrow_mut() = None;
        state.atlas.begin_frame();
        state.needs_frame = true;
    }

    // Hidden: what changed is drawn when the page is shown again.
    let frame = (state.needs_frame && state.visible).then(|| {
        state.needs_frame = false;
        let (scene, mut stats) = frame::build_frame(&state.page, font, &mut state.atlas, state.viewport.width, state.viewport.height, now);
        stats.animation_interval_ms = (animation_interval(state).as_secs_f64() * 1000.0).round() as u32;
        (scene, stats)
    });
    if let Some(started) = animation_work {
        account(id, state, started.elapsed());
    }
    frame
}

/// When the page next needs a tick if nothing else happens.
fn next_wake(state: &PageState) -> Option<Instant> {
    let last = state.last_tick?;
    if !state.visible {
        // Only finished background work wakes a hidden page (ADR 020).
        return None;
    }
    if state.needs_frame {
        return Some(last);
    }
    if is_animating(state) {
        return Some(state.next_animation_tick.unwrap_or(last));
    }
    // Fetches, decodes, web fonts and navigations in flight are not polled:
    // each wakes the page when it finishes (ADR 020).
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineInput, LoadRequest};

    fn input(i: EngineInput) -> PageCommand {
        PageCommand::Input(i)
    }

    #[test]
    fn pointer_moves_scrolls_and_resizes_coalesce() {
        let mailbox = Mailbox::default();
        for i in 0..100 {
            mailbox.post(input(EngineInput::PointerMove { x: i as f32, y: 0.0 }));
        }
        mailbox.post(input(EngineInput::Scroll { dx: 0.0, dy: 10.0 }));
        mailbox.post(input(EngineInput::Scroll { dx: 1.0, dy: 5.0 }));
        mailbox.post(PageCommand::Resize(Viewport::new(100.0, 100.0)));
        mailbox.post(input(EngineInput::Key { key: "a".into() }));
        mailbox.post(PageCommand::Resize(Viewport::new(200.0, 100.0)));
        let queued: Vec<PageCommand> = lock(&mailbox.inbox).commands.iter().cloned().collect();
        assert_eq!(
            queued,
            vec![
                input(EngineInput::PointerMove { x: 99.0, y: 0.0 }),
                input(EngineInput::Scroll { dx: 1.0, dy: 15.0 }),
                PageCommand::Resize(Viewport::new(100.0, 100.0)),
                input(EngineInput::Key { key: "a".into() }),
                // Not merged into the earlier resize: that would move it
                // before the key press. Order-sensitive commands are barriers.
                PageCommand::Resize(Viewport::new(200.0, 100.0)),
            ]
        );
        // Moves and scrolls interleaved still collapse to one of each.
        let mailbox = Mailbox::default();
        for i in 0..50 {
            mailbox.post(input(EngineInput::PointerMove { x: i as f32, y: 0.0 }));
            mailbox.post(input(EngineInput::Scroll { dx: 0.0, dy: 1.0 }));
        }
        assert_eq!(mailbox.queued_commands(), 2);
    }

    #[test]
    fn a_full_inbox_is_bounded_and_drops_coalescable_input_first() {
        let mailbox = Mailbox::default();
        mailbox.post(input(EngineInput::PointerMove { x: 0.0, y: 0.0 }));
        for i in 0..10_000 {
            mailbox.post(input(EngineInput::Key { key: format!("k{}", i % 10) }));
        }
        let queue = &lock(&mailbox.inbox).commands;
        assert_eq!(queue.len(), MAX_QUEUED_COMMANDS);
        assert!(!queue.iter().any(|c| matches!(c, PageCommand::Input(EngineInput::PointerMove { .. }))), "the pointer move went first");
    }

    #[test]
    fn hostile_commands_are_rejected_on_receipt() {
        let bad = [
            input(EngineInput::PointerDown { x: f32::NAN, y: 0.0 }),
            input(EngineInput::PointerMove { x: 0.0, y: f32::INFINITY }),
            input(EngineInput::Scroll { dx: 0.0, dy: f32::NEG_INFINITY }),
            input(EngineInput::Key { key: "x".repeat(10 * 1024 * 1024) }),
            input(EngineInput::Key { key: String::new() }),
            PageCommand::Load(LoadRequest::Url("https://a.test/".to_string() + &"a".repeat(origin::MAX_URL_BYTES))),
            PageCommand::Load(LoadRequest::Html { html: String::new(), url: Some("x".repeat(origin::MAX_URL_BYTES + 1)) }),
        ];
        for command in bad {
            assert!(validate(command).is_err());
        }
        // A 4 GiB-ish viewport is not rejected but clamped.
        match validate(PageCommand::Resize(Viewport { width: 4.0e9, height: 4.0e9 })) {
            Ok(PageCommand::Resize(v)) => assert_eq!((v.width, v.height), (crate::MAX_VIEWPORT_PX, crate::MAX_VIEWPORT_PX)),
            _ => panic!("a resize is clamped, not dropped"),
        }
        assert!(validate(input(EngineInput::Key { key: "ArrowLeft".into() })).is_ok());
    }
}
