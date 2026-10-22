//! A small in-process `tracing` subscriber (feature `collector`).
//!
//! Two users, one implementation:
//!
//! - **Tests** call [`capture`] to run a closure and get back every span
//!   and event it emitted on the calling thread — how the hostile-input
//!   tests assert that "the `:not()` depth cap fired" rather than only
//!   "it did not crash".
//! - **`app --trace`** installs a [`Collector`] as the process-global
//!   subscriber and prints [`Captured::report`] after each frame.
//!
//! Deliberately not `tracing-subscriber`: this needs no output
//! formatting, filtering DSL or layering, only "record everything, hand
//! it back", and a from-scratch ~150 lines avoids a dependency tree
//! (sharded-slab, thread_local, regex-adjacent filtering) that the
//! minimal-dependency principle has no use for. Nothing here ever
//! leaves the process.
//!
//! Records are bounded ([`MAX_RECORDS`]): a page that emits events in a
//! loop cannot grow the collector without limit.

use crate::Guard;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Most spans and most events retained between drains; further records
/// are counted in [`Captured::dropped`] and discarded.
pub const MAX_RECORDS: usize = 100_000;

/// One completed `enter`..`exit` of a span.
#[derive(Clone, Debug)]
pub struct SpanRecord {
    pub name: &'static str,
    pub target: String,
    /// Nesting depth on the emitting thread (0 = outermost).
    pub depth: usize,
    pub fields: Vec<(String, String)>,
    pub duration: Duration,
}

#[derive(Clone, Debug)]
pub struct EventRecord {
    pub level: Level,
    pub target: String,
    /// The event's `message` field, if it had one.
    pub message: String,
    pub fields: Vec<(String, String)>,
}

impl EventRecord {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Everything recorded since the last drain.
#[derive(Clone, Debug, Default)]
pub struct Captured {
    pub spans: Vec<SpanRecord>,
    pub events: Vec<EventRecord>,
    /// Records discarded because [`MAX_RECORDS`] was reached.
    pub dropped: u64,
}

impl Captured {
    /// Every `engine::guard` event for `guard`.
    pub fn guard_events(&self, guard: Guard) -> Vec<&EventRecord> {
        self.events
            .iter()
            .filter(|e| e.target == "engine::guard" && e.field("guard") == Some(guard.name()))
            .collect()
    }

    pub fn saw_guard(&self, guard: Guard) -> bool {
        !self.guard_events(guard).is_empty()
    }

    pub fn span(&self, name: &str) -> Option<&SpanRecord> {
        self.spans.iter().find(|s| s.name == name)
    }

    pub fn span_names(&self) -> Vec<&'static str> {
        self.spans.iter().map(|s| s.name).collect()
    }

    /// A per-frame breakdown suitable for a terminal: spans aggregated
    /// by (depth, name) in first-seen order — the outermost `frame`
    /// span first as the total — then any events.
    pub fn report(&self, label: &str) -> String {
        let mut rows: Vec<(usize, &'static str, Duration, u32)> = Vec::new();
        for s in &self.spans {
            match rows.iter_mut().find(|(d, n, _, _)| *d == s.depth && *n == s.name) {
                Some(row) => {
                    row.2 += s.duration;
                    row.3 += 1;
                }
                None => rows.push((s.depth, s.name, s.duration, 1)),
            }
        }
        // Spans complete innermost-first; show outermost first.
        rows.sort_by_key(|(depth, ..)| *depth);

        let mut out = String::new();
        let _ = writeln!(out, "── {label} ──");
        for (depth, name, total, count) in &rows {
            let indent = "  ".repeat(*depth);
            let times = if *count > 1 { format!("  ×{count}") } else { String::new() };
            let _ = writeln!(out, "{indent}{name:<22} {:>9.3} ms{times}", total.as_secs_f64() * 1000.0);
        }
        for e in &self.events {
            let mut fields = String::new();
            for (k, v) in &e.fields {
                let _ = write!(fields, " {k}={v}");
            }
            let msg = if e.message.is_empty() { String::new() } else { format!(" {}", e.message) };
            let _ = writeln!(out, "  {:>5} {}{msg}{fields}", e.level, e.target);
        }
        if self.dropped > 0 {
            let _ = writeln!(out, "  (+{} records dropped: collector cap reached)", self.dropped);
        }
        out
    }
}

struct OpenSpan {
    name: &'static str,
    target: String,
    fields: Vec<(String, String)>,
    entered_at: Option<Instant>,
    depth: usize,
}

#[derive(Default)]
struct State {
    open: HashMap<u64, OpenSpan>,
    done: Captured,
}

struct Shared {
    next_id: AtomicU64,
    state: Mutex<State>,
}

thread_local! {
    /// Span ids currently entered on this thread — gives each span its depth.
    static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// A `tracing` subscriber that records everything. Cheap to `Clone`
/// (every clone shares the same records).
#[derive(Clone)]
pub struct Collector {
    shared: Arc<Shared>,
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: Vec<(String, String)>,
    message: String,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().to_string(), value.to_string()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push((field.name().to_string(), format!("{value:?}")));
        }
    }
}

impl Collector {
    pub fn new() -> Self {
        Collector { shared: Arc::new(Shared { next_id: AtomicU64::new(1), state: Mutex::new(State::default()) }) }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes everything recorded so far, leaving the collector empty.
    pub fn drain(&self) -> Captured {
        std::mem::take(&mut self.state().done)
    }

    /// Installs this collector as the process-global subscriber. Fails
    /// if one is already installed (only one host component may own it).
    pub fn install_global(&self) -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
        tracing::subscriber::set_global_default(self.clone())
    }
}

impl Subscriber for Collector {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        let meta = attrs.metadata();
        let mut state = self.state();
        // A span that is created but never closed must not leak forever.
        if state.open.len() < MAX_RECORDS {
            state.open.insert(
                id,
                OpenSpan { name: meta.name(), target: meta.target().to_string(), fields: visitor.fields, entered_at: None, depth: 0 },
            );
        }
        Id::from_u64(id)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        if let Some(open) = self.state().open.get_mut(&span.into_u64()) {
            open.fields.extend(visitor.fields);
        }
    }

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let mut state = self.state();
        if state.done.events.len() >= MAX_RECORDS {
            state.done.dropped += 1;
            return;
        }
        state.done.events.push(EventRecord {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        });
    }

    fn enter(&self, span: &Id) {
        let id = span.into_u64();
        let depth = ENTERED.with(|s| {
            let mut s = s.borrow_mut();
            let depth = s.len();
            s.push(id);
            depth
        });
        if let Some(open) = self.state().open.get_mut(&id) {
            open.entered_at = Some(Instant::now());
            open.depth = depth;
        }
    }

    fn exit(&self, span: &Id) {
        let id = span.into_u64();
        ENTERED.with(|s| {
            let mut s = s.borrow_mut();
            if let Some(pos) = s.iter().rposition(|x| *x == id) {
                s.truncate(pos);
            }
        });
        let mut state = self.state();
        let Some(open) = state.open.get(&id) else { return };
        let Some(entered_at) = open.entered_at else { return };
        let record = SpanRecord {
            name: open.name,
            target: open.target.clone(),
            depth: open.depth,
            fields: open.fields.clone(),
            duration: entered_at.elapsed(),
        };
        if state.done.spans.len() >= MAX_RECORDS {
            state.done.dropped += 1;
        } else {
            state.done.spans.push(record);
        }
    }

    fn try_close(&self, id: Id) -> bool {
        self.state().open.remove(&id.into_u64());
        true
    }
}

/// Runs `f` with a fresh collector as THIS THREAD's subscriber and
/// returns its result together with everything it recorded.
///
/// Thread-local by design so parallel tests never see each other's
/// events. The corollary: events emitted from OTHER threads while `f`
/// runs (e.g. a `rayon` worker inside parallel style resolution, or a
/// background fetch thread) are not captured — assert on events that are
/// emitted on the calling thread, or install a global [`Collector`].
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Captured) {
    ensure_interest_sink();
    let collector = Collector::new();
    let result = tracing::subscriber::with_default(collector.clone(), f);
    (result, collector.drain())
}

/// A permanent, do-nothing global subscriber whose only job is to keep
/// `tracing`'s per-callsite interest cache honest under parallel tests.
///
/// The problem: `tracing` caches, per callsite, whether any subscriber is
/// interested. When exactly ONE dispatcher is registered, that cache is
/// (re)built against the CALLING thread's dispatcher alone. So if a
/// thread with no subscriber (say, an unrelated parallel test) is the
/// first to hit a callsite while another thread's `capture` is
/// registered, the callsite is cached as "never" — and the capturing
/// thread's events there silently vanish. Found the hard way: a test
/// passed alone and failed 24 runs in 25 in the full suite with an EMPTY
/// capture; calling `rebuild_interest_cache()` up front does not help
/// because the callsite is registered later, mid-test.
///
/// The fix: once a global sink exists there are always at least two
/// dispatchers (the sink plus any scoped collector), so `tracing` merges
/// interest across all of them. The sink answers `sometimes` for every
/// callsite ("ask me per event") and then `enabled() == false`, so it
/// records nothing and threads with no scoped collector pay nothing but
/// that check.
///
/// Best effort: if the host already installed a global subscriber
/// (`app --trace`), `set_global_default` fails and that one serves the
/// same purpose.
fn ensure_interest_sink() {
    use std::sync::Once;
    use tracing::subscriber::Interest;

    struct Sink;
    impl Subscriber for Sink {
        fn register_callsite(&self, _: &Metadata<'_>) -> Interest {
            Interest::sometimes()
        }
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(Sink);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard;
    use tracing::{info, info_span, warn};

    #[test]
    fn capture_records_events_with_fields_and_message() {
        let (_, c) = capture(|| {
            warn!(target: "test::target", code = 7u64, what = "thing", "something happened");
        });
        assert_eq!(c.events.len(), 1);
        let e = &c.events[0];
        assert_eq!(e.level, Level::WARN);
        assert_eq!(e.target, "test::target");
        assert_eq!(e.message, "something happened");
        assert_eq!(e.field("code"), Some("7"));
        assert_eq!(e.field("what"), Some("thing"));
    }

    #[test]
    fn spans_record_duration_nesting_and_fields() {
        let (_, c) = capture(|| {
            let _frame = info_span!("frame").entered();
            {
                let _layout = info_span!("layout", boxes = 3u64).entered();
                std::thread::sleep(Duration::from_millis(5));
            }
            let _paint = info_span!("paint").entered();
        });
        let frame = c.span("frame").expect("frame span");
        let layout = c.span("layout").expect("layout span");
        assert_eq!(frame.depth, 0);
        assert_eq!(layout.depth, 1);
        assert_eq!(layout.fields, vec![("boxes".to_string(), "3".to_string())]);
        assert!(layout.duration >= Duration::from_millis(5));
        assert!(frame.duration >= layout.duration, "an outer span covers its inner ones");
        assert_eq!(c.span("paint").unwrap().depth, 1);
    }

    #[test]
    fn guard_emits_a_findable_event_and_bumps_the_counter() {
        let before = crate::metrics::snapshot();
        let (_, c) = capture(|| guard(Guard::HtmlNestingDepth, "depth 512"));
        let events = c.guard_events(Guard::HtmlNestingDepth);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("detail"), Some("depth 512"));
        assert!(!c.saw_guard(Guard::SopBlocked), "only the guard that fired is reported");
        assert!(crate::metrics::snapshot().delta(&before, "guard.html_nesting_depth") >= 1);
    }

    #[test]
    fn capture_is_isolated_per_thread_so_parallel_tests_cannot_interfere() {
        let handles: Vec<_> = (0..8u64)
            .map(|i| {
                std::thread::spawn(move || {
                    let (_, c) = capture(|| {
                        for _ in 0..50 {
                            info!(worker = i, "tick");
                        }
                    });
                    assert_eq!(c.events.len(), 50);
                    assert!(c.events.iter().all(|e| e.field("worker") == Some(&i.to_string())));
                })
            })
            .collect();
        handles.into_iter().for_each(|h| h.join().unwrap());
    }

    #[test]
    fn nothing_is_recorded_outside_a_capture() {
        // With no subscriber installed this must be a harmless no-op.
        info!("nobody is listening");
        let (_, c) = capture(|| {});
        assert!(c.events.is_empty() && c.spans.is_empty());
    }

    #[test]
    fn the_collector_is_bounded_and_counts_what_it_drops() {
        let (_, c) = capture(|| {
            for _ in 0..(MAX_RECORDS + 25) {
                info!("flood");
            }
        });
        assert_eq!(c.events.len(), MAX_RECORDS);
        assert_eq!(c.dropped, 25);
    }

    #[test]
    fn report_lists_stages_outermost_first_and_includes_events() {
        let (_, c) = capture(|| {
            let _f = info_span!("frame").entered();
            {
                let _s = info_span!("cascade").entered();
            }
            guard(Guard::CssNotNestingDepth, "20");
        });
        let report = c.report("frame #1");
        assert!(report.contains("frame #1"));
        let frame_at = report.find("frame ").unwrap();
        let cascade_at = report.find("cascade").unwrap();
        assert!(frame_at < cascade_at, "outermost first:\n{report}");
        assert!(report.contains("css_not_nesting_depth"), "events are listed:\n{report}");
    }

    #[test]
    fn a_global_style_collector_sees_events_from_other_threads() {
        // `capture` is thread-local; a collector installed via
        // `with_default` on the main thread does NOT see a spawned
        // thread's events — documenting the difference `install_global`
        // exists to bridge.
        let collector = Collector::new();
        tracing::subscriber::with_default(collector.clone(), || {
            std::thread::spawn(|| info!("from another thread")).join().unwrap();
            info!("from this thread");
        });
        let seen = collector.drain();
        assert_eq!(seen.events.len(), 1);
        assert_eq!(seen.events[0].message, "from this thread");
    }

    /// Regression: `tracing` caches callsite interest, and a callsite
    /// first hit by a thread with no subscriber while another thread's
    /// `capture` is active could be cached as "never", silently dropping
    /// the capturing thread's events (see `ensure_interest_sink`). This
    /// hammers that interaction: many distinct callsites, each hit by a
    /// subscriber-less thread and a capturing thread at the same time.
    #[test]
    fn capture_never_loses_events_to_a_concurrent_subscriberless_thread() {
        macro_rules! callsites {
            ($($name:ident),*) => {
                $(fn $name() { info!(target: "test::interest", "hit"); })*
                const CALLSITES: &[fn()] = &[$($name),*];
            };
        }
        callsites!(c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15);

        let mut lost = Vec::new();
        for (i, emit) in CALLSITES.iter().copied().enumerate() {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let b2 = std::sync::Arc::clone(&barrier);
            // Subscriber-less thread: races to register this callsite.
            let racer = std::thread::spawn(move || {
                b2.wait();
                emit();
            });
            let (_, c) = capture(|| {
                barrier.wait();
                std::thread::sleep(Duration::from_micros(200));
                emit();
            });
            racer.join().unwrap();
            if c.events.len() != 1 {
                lost.push(i);
            }
        }
        assert!(lost.is_empty(), "capture lost events at callsites {lost:?}");
    }
}
