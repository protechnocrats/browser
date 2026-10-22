//! The threads background fetches run on (performance audit).
//!
//! Every `fetch_async` used to start an OS thread of its own: a page with
//! 500 images started 500 threads at once — memory, scheduler load and
//! 500 simultaneous connections to one server, all from page content. Now
//! fetches run on a bounded pool: at most [`FetchPool::max_workers`]
//! threads, at most [`FetchPool::per_host`] requests in flight to one host
//! (what browsers allow per host), and at most [`FetchPool::max_queued`]
//! waiting — past that a request fails at once (reported as
//! `Guard::FetchQueueFull`) instead of queueing without bound.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

/// Worker threads of the process-wide pool.
pub const MAX_FETCH_WORKERS: usize = 16;
/// Requests in flight to one host at a time.
pub const MAX_FETCHES_PER_HOST: usize = 6;
/// Requests waiting for a worker, across the process.
pub const MAX_QUEUED_FETCHES: usize = 4096;
/// A worker with nothing to do for this long exits (a later request
/// starts a new one).
const IDLE_EXIT: Duration = Duration::from_secs(30);

type Job = Box<dyn FnOnce() + Send>;

pub struct FetchPool {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    work: Condvar,
    max_workers: usize,
    per_host: usize,
    max_queued: usize,
}

#[derive(Default)]
struct State {
    queue: VecDeque<(String, Job)>,
    in_flight: HashMap<String, usize>,
    workers: usize,
    idle: usize,
    started: usize,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FetchPool {
    pub fn new(max_workers: usize, per_host: usize, max_queued: usize) -> FetchPool {
        FetchPool {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                work: Condvar::new(),
                max_workers: max_workers.max(1),
                per_host: per_host.max(1),
                max_queued,
            }),
        }
    }

    /// The pool every `NetworkContext` fetches on.
    pub fn global() -> &'static FetchPool {
        static POOL: OnceLock<FetchPool> = OnceLock::new();
        POOL.get_or_init(|| FetchPool::new(MAX_FETCH_WORKERS, MAX_FETCHES_PER_HOST, MAX_QUEUED_FETCHES))
    }

    pub fn max_workers(&self) -> usize {
        self.inner.max_workers
    }

    pub fn per_host(&self) -> usize {
        self.inner.per_host
    }

    pub fn max_queued(&self) -> usize {
        self.inner.max_queued
    }

    /// Worker threads started so far (tests, diagnostics).
    pub fn workers_started(&self) -> usize {
        lock(&self.inner.state).started
    }

    /// Queues `job` to run on a worker once `host` has a free slot. Gives
    /// the job back when the queue is full.
    pub fn submit(&self, host: String, job: Job) -> Result<(), Job> {
        let mut state = lock(&self.inner.state);
        if state.queue.len() >= self.inner.max_queued {
            return Err(job);
        }
        state.queue.push_back((host, job));
        if state.idle == 0 && state.workers < self.inner.max_workers {
            state.workers += 1;
            state.started += 1;
            let n = state.started;
            let inner = Arc::clone(&self.inner);
            let spawned = std::thread::Builder::new().name(format!("fetch-{n}")).spawn(move || work(&inner));
            if spawned.is_err() {
                // No thread: the queued job waits for an existing worker.
                state.workers -= 1;
            }
        }
        drop(state);
        // Every waiting worker: the new job may be runnable only by the
        // one whose host just freed a slot, which `notify_one` may miss.
        self.inner.work.notify_all();
        Ok(())
    }
}

fn work(inner: &Inner) {
    let mut state = lock(&inner.state);
    loop {
        let runnable = state.queue.iter().position(|(host, _)| state.in_flight.get(host).copied().unwrap_or(0) < inner.per_host);
        match runnable {
            Some(i) => {
                let Some((host, job)) = state.queue.remove(i) else { continue };
                *state.in_flight.entry(host.clone()).or_insert(0) += 1;
                drop(state);
                // A panicking job must not take the worker (or the host's
                // slot) with it.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                state = lock(&inner.state);
                if let Some(n) = state.in_flight.get_mut(&host) {
                    *n -= 1;
                    if *n == 0 {
                        state.in_flight.remove(&host);
                    }
                }
                // A slot for `host` is free: a job waiting for it may run.
                inner.work.notify_all();
            }
            None => {
                state.idle += 1;
                let (next, timeout) = inner.work.wait_timeout(state, IDLE_EXIT).unwrap_or_else(PoisonError::into_inner);
                state = next;
                state.idle -= 1;
                if timeout.timed_out() && state.queue.is_empty() {
                    state.workers -= 1;
                    return;
                }
            }
        }
    }
}

/// The host a request is limited under (`""` when the URL has none).
pub fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    /// Runs `jobs` jobs for `host`es on `pool`, each holding its slot for a
    /// moment; returns the most that were ever in flight per host and in
    /// total.
    fn run(pool: &FetchPool, hosts: &[&str], jobs: usize) -> (usize, usize) {
        let per_host: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let max_host = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let max_total = Arc::new(AtomicUsize::new(0));
        let (done, finished) = mpsc::channel();
        for i in 0..jobs {
            let host = hosts[i % hosts.len()].to_string();
            let (per_host, max_host, total, max_total, done) =
                (Arc::clone(&per_host), Arc::clone(&max_host), Arc::clone(&total), Arc::clone(&max_total), done.clone());
            let h = host.clone();
            pool.submit(
                host,
                Box::new(move || {
                    let now = {
                        let mut m = per_host.lock().unwrap();
                        let n = m.entry(h.clone()).or_insert(0);
                        *n += 1;
                        *n
                    };
                    max_host.fetch_max(now, Ordering::SeqCst);
                    let t = total.fetch_add(1, Ordering::SeqCst) + 1;
                    max_total.fetch_max(t, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2));
                    total.fetch_sub(1, Ordering::SeqCst);
                    *per_host.lock().unwrap().get_mut(&h).unwrap() -= 1;
                    done.send(()).unwrap();
                }),
            )
            .unwrap_or_else(|_| panic!("queue has room"));
        }
        for _ in 0..jobs {
            finished.recv_timeout(Duration::from_secs(30)).expect("every job runs");
        }
        (max_host.load(Ordering::SeqCst), max_total.load(Ordering::SeqCst))
    }

    #[test]
    fn a_burst_of_requests_runs_on_a_bounded_pool_within_per_host_limits() {
        let pool = FetchPool::new(8, 3, 1000);
        let (max_host, max_total) = run(&pool, &["a.example", "b.example", "c.example"], 300);
        assert!(max_host <= 3, "at most 3 per host at once, saw {max_host}");
        assert!(max_total <= 8, "at most 8 at once, saw {max_total}");
        assert!(pool.workers_started() <= 8 * 2, "300 requests did not start 300 threads: {}", pool.workers_started());
    }

    #[test]
    fn one_slow_host_does_not_hold_back_the_others() {
        let pool = FetchPool::new(4, 1, 100);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        // Occupy the slow host's single slot.
        let g = Arc::clone(&gate);
        pool.submit("slow.example".into(), Box::new(move || { let _ = g.lock().unwrap().recv_timeout(Duration::from_secs(30)); })).ok().unwrap();
        pool.submit("slow.example".into(), Box::new(|| {})).ok().unwrap();
        let (done, finished) = mpsc::channel();
        pool.submit("fast.example".into(), Box::new(move || done.send(()).unwrap())).ok().unwrap();
        finished.recv_timeout(Duration::from_secs(10)).expect("another host's request runs while the slow host is busy");
        release.send(()).unwrap();
    }

    #[test]
    fn a_full_queue_refuses_instead_of_growing() {
        let pool = FetchPool::new(1, 1, 3);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let g = Arc::clone(&gate);
        pool.submit("h".into(), Box::new(move || { let _ = g.lock().unwrap().recv_timeout(Duration::from_secs(30)); })).ok().unwrap();
        // Wait until the worker has taken the blocking job off the queue.
        while !lock(&pool.inner.state).queue.is_empty() {
            std::thread::yield_now();
        }
        for _ in 0..3 {
            assert!(pool.submit("h".into(), Box::new(|| {})).is_ok());
        }
        assert!(pool.submit("h".into(), Box::new(|| {})).is_err(), "the fourth waiting request is refused");
        release.send(()).unwrap();
    }

    #[test]
    fn a_panicking_job_keeps_its_worker_and_frees_its_host_slot() {
        let pool = FetchPool::new(1, 1, 10);
        pool.submit("h".into(), Box::new(|| panic!("job panicked"))).ok().unwrap();
        let (done, finished) = mpsc::channel();
        pool.submit("h".into(), Box::new(move || done.send(()).unwrap())).ok().unwrap();
        finished.recv_timeout(Duration::from_secs(10)).expect("the next job for the host runs");
    }
}
