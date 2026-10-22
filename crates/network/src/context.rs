//! Milestone 26 — the shared network context.
//!
//! Before this milestone every `fetch()`, every `XMLHttpRequest`, and
//! every page load built its OWN `DirectRoute` — its own cookie jar and
//! its own HTTP cache. A cookie set by a page load was therefore
//! invisible to a later `fetch()` from that same page (a real
//! correctness bug), and there was nowhere to hang per-profile state.
//!
//! [`NetworkContext`] is the fix, and the seam later milestones attach
//! to: origin partitioning (M28) keys the store, routing policy (M37)
//! swaps the route, capabilities (M36) hand a page a context (or not).
//! One context per profile — today, one per process — passed to every
//! caller that fetches.
//!
//! Two pieces, deliberately separate:
//!
//! - [`NetworkStore`] — the STATE: cookie jar, HTTP cache, and the
//!   optional cookie-persistence path. Route-independent: a proxy route
//!   (M37) still needs the same jar and cache, so they must not live
//!   inside any one route.
//! - a route (`Arc<dyn NetworkRoute + Send + Sync>`) — the TRANSPORT.
//!   Architecture doc §6.1: callers never learn which one they have.
//!
//! The handle is a cheap `Clone` (two `Arc` bumps) and is `Send + Sync`,
//! so it crosses thread boundaries freely — which the page-thread
//! migration (M30) requires.

use crate::{
    cache::HttpCache, cookies::CookieJar, ConfigReport, FetchHandle, FetchResponse, FilterEngine, NetworkConfig, NetworkError, NetworkPolicy, NetworkRoute,
    RequestContext,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Locks a mutex, recovering the data if a previous holder panicked.
/// A panic on one page's thread while it held the jar must not turn
/// every OTHER page's next request into a panic too (M30 panic
/// containment relies on this); the jar/cache are plain collections
/// with no multi-step invariant a mid-update panic could leave torn in
/// a way that matters more than losing every page.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Shared, route-independent network state. Held behind an `Arc` by the
/// [`NetworkContext`] and by whichever route it was built with.
pub struct NetworkStore {
    cookies: Mutex<CookieJar>,
    http_cache: Mutex<HttpCache>,
    /// Where persistent cookies are written after every change. `None`
    /// (the default) = in-memory only. Set by
    /// [`NetworkContext::load_cookies`].
    cookie_path: Mutex<Option<PathBuf>>,
    /// Request-hygiene policy (User-Agent, Accept-Language, Referer).
    policy: Mutex<NetworkPolicy>,
    /// Milestone 37 (ADR 019): the request filter every route consults on
    /// every hop. Always present; the built-in list at least.
    filter: Mutex<Arc<FilterEngine>>,
    /// Milestone 37 (ADR 011): private mode — what the route policy's
    /// private-browsing override keys on.
    private_mode: AtomicBool,
}

impl Default for NetworkStore {
    fn default() -> Self {
        NetworkStore {
            cookies: Mutex::default(),
            http_cache: Mutex::default(),
            cookie_path: Mutex::default(),
            policy: Mutex::default(),
            filter: Mutex::new(Arc::new(FilterEngine::with_built_in())),
            private_mode: AtomicBool::new(false),
        }
    }
}

impl NetworkStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The request filter in force (a cheap `Arc` clone: a request never
    /// holds the lock while matching).
    pub fn filter(&self) -> Arc<FilterEngine> {
        Arc::clone(&lock(&self.filter))
    }

    /// Replaces the request filter — for every route at once.
    pub fn set_filter(&self, filter: FilterEngine) {
        *lock(&self.filter) = Arc::new(filter);
    }

    pub fn private_mode(&self) -> bool {
        self.private_mode.load(Ordering::Acquire)
    }

    pub fn set_private_mode(&self, on: bool) {
        self.private_mode.store(on, Ordering::Release);
    }

    /// The cookie jar. Normal cookie send/store happens automatically
    /// inside `fetch`; direct access is for tests and diagnostics.
    pub fn cookie_jar(&self) -> MutexGuard<'_, CookieJar> {
        lock(&self.cookies)
    }

    /// The HTTP cache — for tests and diagnostics.
    pub fn http_cache(&self) -> MutexGuard<'_, HttpCache> {
        lock(&self.http_cache)
    }

    /// A snapshot of the current request-hygiene policy.
    pub fn policy(&self) -> NetworkPolicy {
        lock(&self.policy).clone()
    }

    pub fn set_policy(&self, policy: NetworkPolicy) {
        *lock(&self.policy) = policy;
    }

    /// Writes the jar to the configured cookie file, if one is set.
    /// Called by routes after they store a `Set-Cookie`, so persistence
    /// needs no shutdown hook: the file is current after every change.
    /// Best effort — an I/O failure is reported to stderr, never turned
    /// into a failed page load.
    pub fn persist_cookies_if_configured(&self) {
        let path = lock(&self.cookie_path).clone();
        if let Some(path) = path {
            if let Err(e) = self.write_cookie_file(&path) {
                eprintln!("could not persist cookies to {}: {e}", path.display());
            }
        }
    }

    /// Atomic (write a sibling temp file, then rename) so a crash
    /// mid-write can never leave a truncated jar, and owner-only
    /// (`0600` on Unix): the file holds session tokens.
    fn write_cookie_file(&self, path: &Path) -> std::io::Result<()> {
        let serialized = self.cookie_jar().serialize();
        let tmp = path.with_extension("tmp");
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

/// The one handle every fetching caller holds: shared state plus a
/// route. Cheap to clone; every clone is the SAME context.
#[derive(Clone)]
pub struct NetworkContext {
    store: Arc<NetworkStore>,
    route: Arc<dyn NetworkRoute>,
    /// Milestone 38b (ADR 020): called when a fetch made through this handle
    /// has its result ready. Per handle, not per context: a page's copy
    /// wakes that page.
    notify: Option<crate::Notify>,
}

impl NetworkContext {
    /// A context with a fresh store and the plain [`crate::DirectRoute`].
    /// This is the ONE place a `DirectRoute` is constructed outside
    /// tests (`grep -rn "DirectRoute::with_store" crates app`).
    pub fn new() -> Self {
        Self::with_route(|store| Arc::new(crate::DirectRoute::with_store(store)))
    }

    /// A context with a fresh store and a caller-chosen route — how a
    /// proxy/VPN route (M37) plugs in without touching any caller. The
    /// closure receives the store so the route can share the jar and
    /// cache with the context.
    pub fn with_route(make_route: impl FnOnce(Arc<NetworkStore>) -> Arc<dyn NetworkRoute>) -> Self {
        let store = NetworkStore::new();
        let route = make_route(Arc::clone(&store));
        NetworkContext { store, route, notify: None }
    }

    /// Milestone 37: a context whose routes, route policy, DNS and TLS trust
    /// come from `config` (ADR 011, 012, 018). `Err` for a configuration
    /// that cannot be honoured (a malformed proxy, a rule naming a route that
    /// does not exist, a DoH endpoint that is not https) — never a context
    /// that quietly does something else.
    pub fn with_config(config: &NetworkConfig) -> Result<(Self, ConfigReport), String> {
        let store = NetworkStore::new();
        let (route, report) = crate::routes::build(&store, config)?;
        Ok((NetworkContext { store, route, notify: None }, report))
    }

    /// The same context (store, route), whose fetches call `notify` when
    /// their result is ready (Milestone 38b, ADR 020).
    pub fn with_notify(&self, notify: crate::Notify) -> Self {
        NetworkContext { notify: Some(notify), ..self.clone() }
    }

    /// The callback this handle's fetches call on completion, if any.
    pub fn notifier(&self) -> Option<&crate::Notify> {
        self.notify.as_ref()
    }

    pub fn store(&self) -> &Arc<NetworkStore> {
        &self.store
    }

    /// Replaces the request filter (Milestone 37).
    pub fn set_filter(&self, filter: FilterEngine) {
        self.store.set_filter(filter);
    }

    /// Turns private mode on or off (Milestone 37: the route policy's
    /// private-browsing override).
    pub fn set_private_mode(&self, on: bool) {
        self.store.set_private_mode(on);
    }

    pub fn cookie_jar(&self) -> MutexGuard<'_, CookieJar> {
        self.store.cookie_jar()
    }

    pub fn http_cache(&self) -> MutexGuard<'_, HttpCache> {
        self.store.http_cache()
    }

    /// True if `other` is the same context (shares the same store), not
    /// merely an equivalent one.
    pub fn is_same_context(&self, other: &NetworkContext) -> bool {
        Arc::ptr_eq(&self.store, &other.store)
    }

    /// Real background-thread concurrency: dispatches `fetch` to an OS
    /// thread and returns immediately. Same mechanism and contract as
    /// `DirectRoute::fetch_async` always had, now available for any
    /// route.
    pub fn fetch_async(&self, url: &str) -> FetchHandle {
        crate::spawn_fetch(Arc::clone(&self.route), url, RequestContext::default(), self.notify.clone())
    }

    /// Like [`Self::fetch_async`], with the request's initiator and
    /// navigation flag — which decide cookies (`SameSite`), `Referer`,
    /// and HTTP-cache partitioning. Real callers use this.
    pub fn fetch_async_with(&self, url: &str, ctx: &RequestContext) -> FetchHandle {
        crate::spawn_fetch(Arc::clone(&self.route), url, ctx.clone(), self.notify.clone())
    }

    /// Like [`Self::fetch_async_with`], calling `notify` (as well as this
    /// handle's own notifier) when the result is ready — for a caller holding
    /// a context it did not make, such as a page's network capability.
    pub fn fetch_async_notify(&self, url: &str, ctx: &RequestContext, notify: Option<crate::Notify>) -> FetchHandle {
        let both: Option<crate::Notify> = match (self.notify.clone(), notify) {
            (Some(a), Some(b)) => Some(Arc::new(move || {
                a();
                b();
            })),
            (a, b) => a.or(b),
        };
        crate::spawn_fetch(Arc::clone(&self.route), url, ctx.clone(), both)
    }

    /// The synchronous counterpart of [`Self::fetch_async_with`].
    pub fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        crate::fetch_instrumented(self.route.as_ref(), url, ctx)
    }

    /// Replaces the request-hygiene policy for every later request
    /// through this context.
    pub fn set_policy(&self, policy: NetworkPolicy) {
        self.store.set_policy(policy);
    }

    /// Opt-in cookie persistence: loads persistent cookies from `path`
    /// (a missing file is fine — first run) and from then on rewrites
    /// the file whenever a `Set-Cookie` changes the jar. Off unless this
    /// is called — the default is in-memory only. Session cookies are
    /// never written (see `CookieJar::serialize`).
    pub fn load_cookies(&self, path: impl Into<PathBuf>) -> std::io::Result<()> {
        let path = path.into();
        self.cookie_jar().load_from_path(&path)?;
        *lock(&self.store.cookie_path) = Some(path);
        Ok(())
    }

    /// Writes the jar to the path given to [`Self::load_cookies`] now.
    /// `Ok(false)` if persistence isn't enabled. (Routine persistence
    /// is automatic; this is for an explicit flush.)
    pub fn save_cookies(&self) -> std::io::Result<bool> {
        let path = lock(&self.store.cookie_path).clone();
        match path {
            Some(path) => self.store.write_cookie_file(&path).map(|()| true),
            None => Ok(false),
        }
    }
}

impl Default for NetworkContext {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkRoute for NetworkContext {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        crate::fetch_instrumented(self.route.as_ref(), url, &RequestContext::default())
    }

    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        crate::fetch_instrumented(self.route.as_ref(), url, ctx)
    }
}
