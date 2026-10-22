//! Plain relaxed atomics — the counters and gauges behind the
//! observability baseline. No allocation, no locking, no dependency on a
//! subscriber; safe to bump from any thread, including `rayon` workers.
//!
//! Process-global on purpose for now (one engine per process). Because
//! they are global and Rust runs tests in parallel, **tests must assert
//! on deltas** (`after.delta(&before, name) >= n`), never on absolute
//! values. Per-page counters belong to the page-thread model (M30) and
//! the incremental pipeline's exact "zero layout on a scroll-only
//! frame" assertion (M31); this is the baseline they build on.

use crate::Guard;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// A monotonically increasing count.
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Self {
        Counter(AtomicU64::new(0))
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

/// A value that goes up and down (a size, a queue length): the most
/// recently `set` value.
pub struct Gauge(AtomicU64);

impl Gauge {
    pub const fn new() -> Self {
        Gauge(AtomicU64::new(0))
    }
    pub fn set(&self, v: u64) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! declare_metrics {
    (
        counters { $($c:ident => $cn:literal),* $(,)? }
        gauges { $($g:ident => $gn:literal),* $(,)? }
    ) => {
        $(pub static $c: Counter = Counter::new();)*
        $(pub static $g: Gauge = Gauge::new();)*

        /// Every counter and gauge, by stable name.
        fn named_values() -> Vec<(&'static str, u64)> {
            vec![$(($cn, $c.get()),)* $(($gn, $g.get()),)*]
        }
    };
}

declare_metrics! {
    counters {
        FRAMES_BUILT => "frames_built",
        IMAGE_CACHE_HITS => "image_cache_hits",
        IMAGE_CACHE_MISSES => "image_cache_misses",
        GLYPH_ATLAS_GENERATION_BUMPS => "glyph_atlas_generation_bumps",
        FETCHES_STARTED => "fetches_started",
        FETCHES_FINISHED => "fetches_finished",
        FETCHES_FAILED => "fetches_failed",
        NAVIGATIONS_STARTED => "navigations_started",
        NAVIGATIONS_FINISHED => "navigations_finished",
        NAVIGATIONS_FAILED => "navigations_failed",
    }
    gauges {
        STYLE_CACHE_ENTRIES => "style_cache_entries",
        LAYOUT_BOX_COUNT => "layout_box_count",
        // Milestone 39b: boxes the last page display list build or update
        // built (the rest were moved over from the previous list).
        DISPLAY_BOXES_BUILT => "display_boxes_built",
        PENDING_FETCHES => "pending_fetches",
    }
}

const GUARD_COUNT: usize = Guard::ALL.len();
#[allow(clippy::declare_interior_mutable_const)]
const ZERO: Counter = Counter::new();
static GUARD_HITS: [Counter; GUARD_COUNT] = [ZERO; GUARD_COUNT];

fn guard_index(g: Guard) -> usize {
    // `Guard::ALL` is the single source of truth for ordering.
    Guard::ALL.iter().position(|x| *x == g).expect("every Guard variant is listed in Guard::ALL")
}

pub(crate) fn record_guard(g: Guard) {
    GUARD_HITS[guard_index(g)].inc();
}

/// How many times `g` has fired in this process.
pub fn guard_hits(g: Guard) -> u64 {
    GUARD_HITS[guard_index(g)].get()
}

/// Frame-time histogram bucket upper bounds, in milliseconds. The last
/// bucket is "everything slower". 16.6 ms is the 60 Hz frame budget
/// (architecture doc §5).
pub const FRAME_BUCKET_UPPER_MS: [f64; 5] = [4.0, 8.0, 16.6, 33.3, 66.6];
const FRAME_BUCKET_LABELS: [&str; 6] = ["<4ms", "<8ms", "<16.6ms", "<33.3ms", "<66.6ms", ">=66.6ms"];
static FRAME_BUCKETS: [Counter; 6] = [ZERO; 6];

/// Records one frame's total build time into the histogram.
pub fn record_frame_time(d: Duration) {
    let ms = d.as_secs_f64() * 1000.0;
    let bucket = FRAME_BUCKET_UPPER_MS.iter().position(|&upper| ms < upper).unwrap_or(FRAME_BUCKET_UPPER_MS.len());
    FRAME_BUCKETS[bucket].inc();
}

// Whether expensive-to-compute gauges (e.g. walking the whole layout
// tree to count boxes) should be computed at all. Off by default so an
// untraced run pays nothing; `app --trace` turns it on.
static DETAILED: AtomicBool = AtomicBool::new(false);

pub fn set_detailed(on: bool) {
    DETAILED.store(on, Ordering::Relaxed);
}

thread_local! {
    static DETAILED_ON_THIS_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// True if a host asked for detailed (costlier) metrics — for the whole
/// process, or for work on this thread ([`detailed_on_this_thread`]).
pub fn detailed() -> bool {
    DETAILED.load(Ordering::Relaxed) || DETAILED_ON_THIS_THREAD.with(std::cell::Cell::get)
}

/// Runs `f` with detailed metrics on for this thread only: work other
/// threads do meanwhile is not measured (tests run in parallel, and one
/// test turning the process-wide switch on made every other test's frames
/// write the shared gauges it was about to read).
pub fn detailed_on_this_thread<R>(f: impl FnOnce() -> R) -> R {
    let was = DETAILED_ON_THIS_THREAD.with(|d| d.replace(true));
    let result = f();
    DETAILED_ON_THIS_THREAD.with(|d| d.set(was));
    result
}

/// A point-in-time copy of every metric, by name. Guard counters appear
/// as `guard.<name>`, histogram buckets as `frame_time.<label>`.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub values: Vec<(String, u64)>,
}

impl Snapshot {
    pub fn get(&self, name: &str) -> u64 {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| *v).unwrap_or(0)
    }

    /// `self[name] - before[name]` — how much `name` grew between two
    /// snapshots (saturating; gauges can shrink).
    pub fn delta(&self, before: &Snapshot, name: &str) -> u64 {
        self.get(name).saturating_sub(before.get(name))
    }
}

pub fn snapshot() -> Snapshot {
    let mut values: Vec<(String, u64)> = named_values().into_iter().map(|(n, v)| (n.to_string(), v)).collect();
    for g in Guard::ALL {
        values.push((format!("guard.{}", g.name()), guard_hits(g)));
    }
    for (label, bucket) in FRAME_BUCKET_LABELS.iter().zip(FRAME_BUCKETS.iter()) {
        values.push((format!("frame_time.{label}"), bucket.get()));
    }
    Snapshot { values }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_gauges_behave() {
        let c = Counter::new();
        c.inc();
        c.add(4);
        assert_eq!(c.get(), 5);
        let g = Gauge::new();
        g.set(9);
        g.set(3);
        assert_eq!(g.get(), 3);
    }

    #[test]
    fn frame_time_lands_in_the_right_bucket() {
        let before = snapshot();
        record_frame_time(Duration::from_micros(500)); // <4ms
        record_frame_time(Duration::from_millis(12)); // <16.6ms
        record_frame_time(Duration::from_millis(500)); // >=66.6ms
        let after = snapshot();
        assert!(after.delta(&before, "frame_time.<4ms") >= 1);
        assert!(after.delta(&before, "frame_time.<16.6ms") >= 1);
        assert!(after.delta(&before, "frame_time.>=66.6ms") >= 1);
    }

    #[test]
    fn snapshot_lists_every_guard_and_bucket() {
        let s = snapshot();
        for g in Guard::ALL {
            assert!(s.values.iter().any(|(n, _)| *n == format!("guard.{}", g.name())));
        }
        assert_eq!(s.values.iter().filter(|(n, _)| n.starts_with("frame_time.")).count(), 6);
    }

    #[test]
    fn counters_are_safe_to_bump_from_many_threads() {
        static C: Counter = Counter::new();
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(|| (0..1000).for_each(|_| C.inc()))).collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(C.get(), 8000);
    }
}
