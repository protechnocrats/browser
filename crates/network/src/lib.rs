//! Networking — Milestone 8. The first milestone that feeds the
//! pipeline from a real fetched document instead of a hardcoded HTML
//! string, and the milestone that introduces `NetworkRoute` (per the
//! architecture doc §6.1) — even though `DirectRoute` is the only
//! real implementation today, establishing the trait now means every
//! later addition (proxy, VPN, the privacy/filter engine) plugs into
//! an existing seam instead of retrofitting one.
//!
//! ## The `ring`/rustls exception to this project's memory-safety stance
//!
//! This crate's own code is `#![forbid(unsafe_code)]`, same as every
//! other core crate. But TLS is a real, narrow exception worth being
//! upfront about rather than glossing over: `ureq`'s TLS backend is
//! `rustls`, and `rustls`'s default crypto provider is `ring`, which
//! contains hand-written assembly/C for performance-critical
//! primitives (AES, SHA, elliptic-curve math) — genuinely not
//! pure-safe-Rust. As of this writing there is no mature pure-Rust
//! TLS crypto backend in wide production use; `ring` is the same
//! trust boundary `rustls` itself asks of everyone who uses it
//! (including large production deployments at companies like
//! Cloudflare and AWS). The distinction that matters: the TLS
//! *protocol state machine* — where most historical TLS
//! vulnerabilities (including Heartbleed-class bugs) actually lived —
//! stays in safe Rust via `rustls`. Only the low-level crypto
//! primitives, a much smaller and more heavily audited surface, use
//! `ring`. This is a deliberate, documented, narrow exception — not
//! an oversight, and not a precedent for casually reaching for
//! `unsafe` or C dependencies elsewhere.
//!
//! ## Scope
//!
//! See milestones/milestones-01-17.md for full scope. Fetches
//! the main document plus (once fetched and parsed) linked
//! stylesheets and images. No caching/conditional-requests (Phase 2),
//! no cookies (that's `cookies.rs`, wired in via Milestone 9 — see
//! that module), no WebSockets/streaming, no cert pinning beyond
//! `rustls`'s defaults.

pub mod cache;
pub mod context;
pub mod cookies;
pub mod encoding;
pub mod pool;
// Milestone 37: DNS over HTTPS (ADR 012), routes (ADR 011), TLS trust (ADR 018).
pub mod dns;
pub mod doh;
mod routes;
pub mod tls;

pub use context::{NetworkContext, NetworkStore};
pub use doh::{DohConfig, DohResolver};
/// Milestone 37 (ADR 019): the request filter, and what a request is for.
pub use filter::{Category as FilterCategory, FilterEngine, ListReport, RequestType as Destination, Verdict};
pub use routes::{
    ConfigReport, NetworkConfig, ProxyConfig, ProxyRoute, RoutePolicy, RouteReason, RouteRules, SystemVpnRoute, Transport, VpnProbe, DIRECT, SYSTEM_VPN,
};
pub use tls::{TlsTrust, TrustReport};
pub use origin::{Origin, Site};

use std::io::Read;
use std::sync::Arc;

/// Maximum response body size this crate will ever buffer in memory
/// (100 MiB) — see the DoS-prevention comment at the `read_to_end`
/// call site. Generous enough for any real page/stylesheet/image this
/// engine handles; just a backstop against a malicious/misbehaving
/// server, not a real-world content-size limit.
pub const MAX_RESPONSE_BODY_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct FetchResponse {
    pub status: u16,
    /// The URL actually fetched — may differ from the requested URL
    /// after following redirects.
    pub final_url: String,
    /// May contain multiple entries with the same name (e.g.
    /// multiple `Set-Cookie` headers) — deliberately a `Vec`, not a
    /// `HashMap`, so nothing gets silently dropped.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FetchResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Interprets the body as UTF-8. Real charset sniffing (from a
    /// `Content-Type` charset parameter, or a `<meta charset>` inside
    /// the document) is out of scope — see Milestone 3's (milestones/milestones-01-17.md)
    /// notes on encoding detection being explicitly deferred; this
    /// crate inherits that same simplification. Lossy conversion
    /// rather than an error, matching this project's "degrade
    /// gracefully" discipline for anything that touches untrusted
    /// external input.
    pub fn body_as_string(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkError {
    InvalidUrl(String),
    /// A real network-level failure — DNS, connection refused,
    /// timeout, TLS handshake failure, etc. An HTTP error *status*
    /// (404, 500, ...) is NOT a `NetworkError` — that's a normal,
    /// successful `FetchResponse` with that status code, matching how
    /// a real browser treats it (still renders the error page body).
    Transport(String),
    Io(String),
    /// Milestone 37: the request filter blocked the request before it was
    /// sent (ADR 019). Carries the category and rule.
    Blocked(String),
}

impl std::fmt::Display for NetworkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkError::InvalidUrl(e) => write!(f, "invalid URL: {e}"),
            NetworkError::Transport(e) => write!(f, "network error: {e}"),
            NetworkError::Io(e) => write!(f, "I/O error reading response body: {e}"),
            NetworkError::Blocked(e) => write!(f, "blocked by the content filter: {e}"),
        }
    }
}

impl std::error::Error for NetworkError {}

/// See architecture doc §6.1 — a `NetworkRoute` is how a request
/// actually reaches the network. The engine/browser code above this
/// trait never knows which implementation it's talking to.
///
/// `Send + Sync`: a route is shared behind an `Arc` by a
/// [`NetworkContext`] and driven from background fetch threads.
pub trait NetworkRoute: Send + Sync {
    /// A browser-initiated request (no initiating document).
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError>;

    /// A request made on behalf of a document, described by `ctx`. A
    /// route that ignores the context (a test double) just forwards to
    /// [`Self::fetch`]; `DirectRoute` uses it for `SameSite` cookies,
    /// `Referer`, and cache partitioning.
    fn fetch_with(&self, url: &str, _ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        self.fetch(url)
    }
}

/// The document a request is made on behalf of: its [`Origin`] (which
/// decides `SameSite` cookies, the HTTP-cache partition, and the
/// credentials mode) and, when it has one, its URL (used only for
/// `Referer`). A document with no URL still has an origin — an opaque one
/// — so its requests are cross-site to everything (ADR 004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Initiator {
    pub origin: Origin,
    pub url: Option<String>,
}

impl Initiator {
    /// The document at `url` (its origin is parsed from it).
    pub fn for_url(url: &str) -> Initiator {
        Initiator { origin: Origin::parse(url), url: Some(url.to_string()) }
    }

    /// A document whose origin is already known (possibly opaque) and whose
    /// URL, if any, is `url`.
    pub fn document(origin: Origin, url: Option<&str>) -> Initiator {
        Initiator { origin, url: url.map(str::to_string) }
    }
}

/// Whether cookies ride on a request, and whether its response may set
/// them — the Fetch standard's *credentials mode*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Credentials {
    /// Cookies are sent and stored on every hop (still subject to
    /// `SameSite`). Page loads, stylesheets and images.
    #[default]
    Include,
    /// Only on hops that are same-origin with the initiator: the default
    /// for script `fetch()` and `XMLHttpRequest`, so a cross-origin API call
    /// carries no cookies unless the script asks for them.
    SameOrigin,
    /// Never: no `Cookie` header, and `Set-Cookie` is ignored.
    Omit,
}

/// Who is making a request, and why — the facts that cookies (`SameSite`),
/// `Referer`, and HTTP-cache partitioning depend on. Built by whoever
/// knows: `app` for page loads and navigations, `js` for `fetch()`/XHR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestContext {
    /// The document that initiated the request. `None` means
    /// browser-initiated (a typed URL, the startup page): never
    /// cross-site, and no `Referer`.
    pub initiator: Option<Initiator>,
    /// A top-level navigation (typing a URL, clicking a link) rather than
    /// a subresource or script request.
    pub is_navigation: bool,
    pub credentials: Credentials,
    /// Milestone 37: what the request is for — what the filter's `$script`,
    /// `$image`, … options test. A navigation is `Document`.
    pub destination: Destination,
}

impl RequestContext {
    /// A top-level navigation the user made directly (no initiator).
    pub fn typed_navigation() -> Self {
        RequestContext { initiator: None, is_navigation: true, credentials: Credentials::Include, destination: Destination::Document }
    }

    /// A top-level navigation triggered from `page_url` (a link click or
    /// form submit).
    pub fn navigation_from(page_url: &str) -> Self {
        Self::navigation_by(Initiator::for_url(page_url))
    }

    /// A top-level navigation triggered by the document `initiator`.
    pub fn navigation_by(initiator: Initiator) -> Self {
        RequestContext { initiator: Some(initiator), is_navigation: true, credentials: Credentials::Include, destination: Destination::Document }
    }

    /// A subresource request made by the document at `page_url`
    /// (stylesheets, images): credentials included.
    pub fn subresource_of(page_url: &str) -> Self {
        Self::subresource_by(Initiator::for_url(page_url))
    }

    /// A subresource request made by the document `initiator`.
    pub fn subresource_by(initiator: Initiator) -> Self {
        RequestContext { initiator: Some(initiator), is_navigation: false, credentials: Credentials::Include, destination: Destination::Other }
    }

    /// The same request, for `destination` (Milestone 37).
    pub fn with_destination(mut self, destination: Destination) -> Self {
        self.destination = destination;
        self
    }

    /// Whether this request, going to `hop_url`'s site, is third-party: a
    /// page made it and the page's site is another (ADR 019 §2). A
    /// navigation is first-party by definition.
    pub fn is_third_party(&self, hop_site: &Site) -> bool {
        !self.is_navigation && self.initiator.as_ref().is_some_and(|i| &i.origin.site() != hop_site)
    }

    /// The same request with a different credentials mode.
    pub fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = credentials;
        self
    }

    /// Whether this request, going to `hop_origin`, may send and store
    /// cookies under its credentials mode.
    pub fn credentials_apply_to(&self, hop_origin: &Origin) -> bool {
        match self.credentials {
            Credentials::Include => true,
            Credentials::Omit => false,
            Credentials::SameOrigin => self.initiator.as_ref().is_none_or(|i| i.origin.same_origin(hop_origin)),
        }
    }
}

/// How much of the initiating page's URL a request reveals as `Referer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferrerPolicy {
    /// Never send `Referer`.
    NoReferrer,
    /// The web default (architecture doc §6.1): the full URL (without
    /// credentials or fragment) to the same origin; only the origin to
    /// other origins; nothing when going from `https` to `http`.
    StrictOriginWhenCrossOrigin,
}

/// Request-hygiene defaults: what the engine says about itself and its
/// user on every request. Each is a *fixed policy*, not derived from the
/// machine — nothing here varies with the OS, locale, or hardware, so it
/// cannot be used to tell users apart (architecture doc §1, privacy-first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// Frozen and minimal: names the engine and nothing about the OS,
    /// architecture, or version details beyond one fixed string. (The
    /// HTTP library's own default — `ureq/2.12.1` — used to go out, which
    /// fingerprinted the dependency stack.)
    pub user_agent: String,
    /// A fixed value rather than the OS locale, which would leak it.
    pub accept_language: String,
    pub referrer: ReferrerPolicy,
    /// Milestone 37 (ADR 019 §2): cookies on third-party requests.
    pub third_party_cookies: ThirdPartyCookies,
}

/// Whether a third-party request carries and stores cookies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThirdPartyCookies {
    /// The default: no `Cookie` header, and `Set-Cookie` ignored — whatever
    /// the cookie's `SameSite` (P6).
    #[default]
    Block,
    /// Third-party cookies follow `SameSite` alone (the pre-M37 behaviour).
    Allow,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy {
            user_agent: "browser-engine/0.1".to_string(),
            accept_language: "en-US,en;q=0.5".to_string(),
            referrer: ReferrerPolicy::StrictOriginWhenCrossOrigin,
            third_party_cookies: ThirdPartyCookies::Block,
        }
    }
}

/// The `Referer` value for a request from `initiator` to `target` under
/// `policy`, if any. Never includes credentials or a fragment.
pub fn referer_for(policy: ReferrerPolicy, initiator: &str, target: &url::Url) -> Option<String> {
    if policy == ReferrerPolicy::NoReferrer {
        return None;
    }
    let mut from = url::Url::parse(initiator).ok()?;
    if from.scheme() != "http" && from.scheme() != "https" {
        return None;
    }
    if from.scheme() == "https" && target.scheme() == "http" {
        return None; // never leak an https page's address to a plain-http host
    }
    let _ = from.set_username("");
    let _ = from.set_password(None);
    from.set_fragment(None);
    if Origin::parse(from.as_str()).same_origin(&Origin::parse(target.as_str())) {
        Some(from.to_string())
    } else {
        let mut origin_only = from;
        origin_only.set_path("/");
        origin_only.set_query(None);
        Some(origin_only.to_string())
    }
}

/// HTTP-cache key. The cache is **partitioned by top-level site** so a
/// resource cached for one site cannot be probed from another (the
/// cache-timing "is this URL cached?" side channel, and `ETag` tracking):
/// a navigation is keyed by the destination's own site, a subresource by
/// the site of the page that requested it.
///
/// Milestone 28: the key also records whether the request carried
/// credentials, so a response fetched WITH the user's cookies is never
/// served from cache to a request made without them (a script's
/// same-origin-credentials `fetch()` of a URL the page earlier loaded as a
/// credentialed image, say) — that would hand out credentialed content to
/// a request the server never saw as credentialed.
fn cache_key(ctx: &RequestContext, url: &str) -> String {
    let partition = match (&ctx.initiator, ctx.is_navigation) {
        (Some(initiator), false) => initiator.origin.site(),
        _ => Site::of_url(url),
    };
    let credentials = if ctx.credentials_apply_to(&Origin::parse(url)) { "cred" } else { "anon" };
    format!("{}|{credentials}|{url}", partition.partition_key())
}

/// Most redirect hops one `fetch` will follow before giving up —
/// matches what the previous internal `ureq` handling allowed.
const MAX_REDIRECTS: usize = 10;

/// The only real `NetworkRoute` implementation right now — a plain,
/// direct connection, no proxy/VPN routing. Per the architecture
/// doc's standing principle: **we do not build or operate VPN
/// infrastructure**; if a `ProxyRoute`/`UserVPNRoute` ever gets built,
/// it's a separate `NetworkRoute` implementation the caller opts into,
/// not something bolted onto this one.
///
/// Milestone 26: cookie jar and HTTP cache no longer live in here —
/// they are the shared [`NetworkStore`] this route was built with, so
/// every fetch through one [`NetworkContext`] sees the same cookies and
/// cache. Construct through `NetworkContext::new()`, not directly.
pub struct DirectRoute {
    agent: ureq::Agent,
    store: Arc<NetworkStore>,
    /// Milestone 37: how this route connects (direct, a proxy, gated on a
    /// VPN) and its name in errors.
    transport: Transport,
    name: &'static str,
}

/// Milestone 38b (ADR 020): called from a background thread when work a
/// page waits for (a fetch's result) is ready, so the page is woken rather
/// than polled.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// A background fetch dispatched by `NetworkContext::fetch_async`. Not a
/// future/promise in the async-Rust sense — deliberately just a
/// receiving end of a channel, polled with `try_recv`, so this crate
/// doesn't need to pull in an async runtime (`tokio` et al.) to offer
/// real concurrency. `js`'s `fetch()` binding polls this once per
/// frame tick — see that crate for how a plain channel becomes a real
/// non-blocking `Promise`.
pub struct FetchHandle {
    receiver: std::sync::mpsc::Receiver<Result<FetchResponse, NetworkError>>,
}

impl FetchHandle {
    /// `None` if the background fetch hasn't completed yet (never
    /// blocks waiting); `Some(result)` the first time it's called
    /// after completion. Calling this again after it's returned
    /// `Some` will also return `None` forever after (the channel is
    /// drained) — callers should treat one `Some` as the final
    /// answer, not poll past it.
    pub fn try_recv(&self) -> Option<Result<FetchResponse, NetworkError>> {
        self.receiver.try_recv().ok()
    }

    /// Blocks until the fetch completes or `deadline` passes (`None`) —
    /// woken by the result itself, not by polling.
    pub fn wait_until(&self, deadline: std::time::Instant) -> Option<Result<FetchResponse, NetworkError>> {
        let timeout = deadline.saturating_duration_since(std::time::Instant::now());
        self.receiver.recv_timeout(timeout).ok()
    }
}

/// Real background-thread concurrency: runs `route.fetch(url)` on a
/// genuine OS thread and returns immediately with a `FetchHandle` the
/// caller polls later, rather than blocking until the request
/// completes. On a multi-core machine the fetch runs in true parallel
/// with whatever the calling thread does next; on a single-core
/// machine, the OS still time-slices the two threads preemptively, so
/// the calling thread is never stuck waiting on network I/O either
/// way. A real OS thread the kernel schedules independently — not a
/// language-level "coroutine"; reaching for a full async runtime
/// (`tokio`) just to get this one behavior would be a large dependency
/// for a small, well-understood need `std::thread` + a channel already
/// covers correctly.
///
/// Takes the route behind an `Arc` so the background thread holds its
/// own strong reference (and, through it, the shared store) for as long
/// as the request takes, independent of how long the caller keeps its
/// own reference alive.
pub(crate) fn spawn_fetch(route: Arc<dyn NetworkRoute>, url: &str, ctx: RequestContext, notify: Option<Notify>) -> FetchHandle {
    let (sender, receiver) = std::sync::mpsc::channel();
    let url = url.to_string();
    let host = pool::host_of(&url);
    let refused = sender.clone();
    let job_notify = notify.clone();
    // On the bounded fetch pool (see `pool`), not a thread of its own.
    let submitted = pool::FetchPool::global().submit(
        host,
        Box::new(move || {
            let result = fetch_instrumented(route.as_ref(), &url, &ctx);
            // A `send` failure just means the receiver (the
            // `FetchHandle`) was already dropped — the caller stopped
            // caring about this fetch's result (e.g. the page
            // navigated away). A legitimate outcome, not an error to
            // surface anywhere.
            let _ = sender.send(result);
            if let Some(notify) = &job_notify {
                notify();
            }
        }),
    );
    if submitted.is_err() {
        diagnostics::guard(diagnostics::Guard::FetchQueueFull, "too many fetches waiting; request refused");
        let _ = refused.send(Err(NetworkError::Transport("too many requests in flight".to_string())));
        if let Some(notify) = &notify {
            notify();
        }
    }
    FetchHandle { receiver }
}

/// A URL that is safe to write to a diagnostics stream: scheme, host,
/// port and path only — never userinfo, query string or fragment, which
/// routinely carry credentials and tokens (architecture doc §1,
/// privacy-first). Diagnostics are local-only and opt-in, but "don't log
/// secrets" is cheaper to guarantee at the source than to audit later.
pub fn loggable_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<unparseable url>".to_string(),
    }
}

/// Runs `route.fetch(url)` and reports it: `engine::network` events for
/// start/finish/fail (URL sanitized by [`loggable_url`]) plus the
/// `fetches_*` counters. Every fetch, sync or background, goes through
/// here, so no path can be silent.
pub(crate) fn fetch_instrumented(route: &dyn NetworkRoute, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
    use diagnostics::{metrics, tracing};
    metrics::FETCHES_STARTED.inc();
    tracing::info!(target: "engine::network", url = %loggable_url(url), "fetch started");
    let started = std::time::Instant::now();
    let result = route.fetch_with(url, ctx);
    match &result {
        Ok(r) => {
            metrics::FETCHES_FINISHED.inc();
            tracing::info!(
                target: "engine::network",
                url = %loggable_url(&r.final_url),
                status = r.status,
                bytes = r.body.len() as u64,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "fetch finished"
            );
        }
        Err(e) => {
            metrics::FETCHES_FAILED.inc();
            tracing::warn!(target: "engine::network", url = %loggable_url(url), error = %e, "fetch failed");
        }
    }
    result
}

fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

impl DirectRoute {
    /// The ONLY constructor. A route is always built around a store so
    /// that a cookie jar / cache can never be accidentally private to
    /// one call site again (the Milestone 26 bug). Production code goes
    /// through `NetworkContext::new()`.
    pub fn with_store(store: Arc<NetworkStore>) -> Self {
        Self::with_transport(store, Transport::default(), DIRECT)
    }

    /// Milestone 37: the same fetch logic over another transport — what a
    /// proxy or VPN route is (ADR 011 §3).
    pub fn with_transport(store: Arc<NetworkStore>, transport: Transport, name: &'static str) -> Self {
        DirectRoute { agent: transport.agent(), store, transport, name }
    }

    /// A transport failure, said as this route's: a proxy or VPN route that
    /// cannot connect has failed closed (ADR 011 §2).
    fn transport_error(&self, message: String) -> NetworkError {
        if self.transport.proxy.is_some() {
            diagnostics::guard(diagnostics::Guard::RouteFailedClosed, "the proxy could not carry a request; it was not sent directly");
            NetworkError::Transport(format!("via {} route: {message} (no fallback to a direct connection)", self.name))
        } else {
            NetworkError::Transport(message)
        }
    }

    /// The shared store this route reads and writes.
    pub fn store(&self) -> &Arc<NetworkStore> {
        &self.store
    }
}

/// Whether a hop may carry and store cookies, and the `Cookie` header it
/// sends: the credentials mode (M28), the third-party policy (Milestone 37,
/// ADR 019 §2 — a third-party hop carries no cookie and stores none,
/// whatever its `SameSite`, P6), then `SameSite` and the cookie's own scope.
pub(crate) fn hop_cookies(store: &NetworkStore, policy: &NetworkPolicy, ctx: &RequestContext, hop: &url::Url) -> (bool, Option<String>) {
    let hop_origin = Origin::parse(hop.as_str());
    let request_site = hop_origin.site();
    let third_party_blocked = ctx.is_third_party(&request_site) && policy.third_party_cookies == ThirdPartyCookies::Block;
    let credentials = ctx.credentials_apply_to(&hop_origin) && !third_party_blocked;
    if !credentials {
        return (false, None);
    }
    let initiator_site = ctx.initiator.as_ref().map(|i| i.origin.site());
    let header = store.cookie_jar().header_for_request(&cookies::SendContext {
        host: hop.host_str().unwrap_or(""),
        path: hop.path(),
        is_secure: hop.scheme() == "https",
        request_site: &request_site,
        initiator_site: initiator_site.as_ref(),
        is_navigation: ctx.is_navigation,
    });
    (true, header)
}

impl NetworkRoute for DirectRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.fetch_with(url, &RequestContext::default())
    }

    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        let mut current = url::Url::parse(url).map_err(|e| NetworkError::InvalidUrl(e.to_string()))?;
        let policy = self.store.policy();
        let cache_key = cache_key(ctx, url);

        // Phase F: consult the HTTP cache. A fresh entry short-circuits
        // the network entirely; a stale-but-validatable one adds
        // conditional headers so the server can answer 304. Keyed by the
        // URL originally requested; conditional headers ride only the
        // first hop (they describe the requested resource, not whatever
        // a redirect leads to).
        let conditional_headers = match self.store.http_cache().before_request(&cache_key) {
            cache::CacheDecision::Fresh(cached) => return Ok(cached),
            cache::CacheDecision::Revalidate(headers) => headers,
            cache::CacheDecision::Miss => Vec::new(),
        };

        for hop in 0..=MAX_REDIRECTS {
            // Milestone 37: a VPN-gated route connects only while the VPN is
            // up (ADR 011 §2) — checked per hop.
            if let Some(gate) = &self.transport.gate {
                if !gate() {
                    diagnostics::guard(diagnostics::Guard::RouteFailedClosed, "the system VPN is down; request refused");
                    return Err(NetworkError::Transport(format!("via {} route: the system VPN is down (no fallback to a direct connection)", self.name)));
                }
            }
            let domain = current.host_str().unwrap_or("").to_string();
            let is_secure = current.scheme() == "https";
            let hop_url = current.to_string();

            let mut request = self
                .agent
                .get(&hop_url)
                .set("User-Agent", &policy.user_agent)
                .set("Accept-Language", &policy.accept_language)
                // What `encoding::decode_body` can undo.
                .set("Accept-Encoding", "gzip, deflate");
            if let Some(initiator_url) = ctx.initiator.as_ref().and_then(|i| i.url.as_deref()) {
                if let Some(referer) = referer_for(policy.referrer, initiator_url, &current) {
                    request = request.set("Referer", &referer);
                }
            }
            // Cookies matching THIS hop's host/path, permitted by the
            // credentials mode for THIS hop's origin (a same-origin `fetch()`
            // redirected cross-origin stops carrying cookies), and by
            // `SameSite` given who initiated the request.
            let hop_origin = Origin::parse(&hop_url);
            let request_site = hop_origin.site();
            // Milestone 37 (ADR 019): the filter sees every hop before it is
            // sent — a redirect to a tracker is blocked like a request to it.
            let third_party = ctx.is_third_party(&request_site);
            let page_host = ctx.initiator.as_ref().filter(|_| !ctx.is_navigation).and_then(|i| i.url.as_deref()).and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string));
            let verdict = self.store.filter().check(&filter::FilterRequest {
                url: &hop_url,
                kind: if ctx.is_navigation { Destination::Document } else { ctx.destination },
                page_host: page_host.as_deref(),
                third_party,
            });
            if let Verdict::Block { category, rule } = verdict {
                diagnostics::guard(diagnostics::Guard::RequestBlocked, category.name());
                return Err(NetworkError::Blocked(format!("{} ({rule})", category.name())));
            }
            let (credentials, cookie_header) = hop_cookies(&self.store, &policy, ctx, &current);
            if let Some(cookie_header) = cookie_header {
                request = request.set("Cookie", &cookie_header);
            }
            if hop == 0 {
                for (name, value) in &conditional_headers {
                    request = request.set(name, value);
                }
            }

            let response = match request.call() {
                Ok(r) => r,
                // ureq treats 4xx/5xx as an Err by default — we want a
                // normal FetchResponse with that status instead, matching
                // real browser behavior (an error page still renders).
                Err(ureq::Error::Status(_, r)) => r,
                Err(ureq::Error::Transport(t)) => return Err(self.transport_error(t.to_string())),
            };

            let status = response.status();

            let mut headers = Vec::new();
            for name in response.headers_names() {
                for value in response.all(&name) {
                    headers.push((name.clone(), value.to_string()));
                }
            }

            // Store any Set-Cookie headers under THIS hop's host —
            // including on a redirect response, which is exactly where
            // login flows set their session cookie. (Scoping to the
            // ORIGINAL request host, as before, would attribute a
            // redirect target's cookies to the wrong site.)
            if credentials {
                let mut stored_any = false;
                {
                    let mut jar = self.store.cookie_jar();
                    for (name, value) in &headers {
                        if name.eq_ignore_ascii_case("set-cookie") {
                            jar.store(value, &domain, is_secure);
                            stored_any = true;
                        }
                    }
                }
                if stored_any {
                    self.store.persist_cookies_if_configured();
                }
            }

            if is_redirect_status(status) {
                let location = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone());
                if let Some(location) = location {
                    let next = current
                        .join(&location)
                        .map_err(|e| NetworkError::Transport(format!("invalid redirect Location {location:?}: {e}")))?;
                    // Zero-trust: a server chooses where it redirects us.
                    // Only web schemes are followable — never `file:`,
                    // `data:`, `ftp:`, or anything else.
                    if next.scheme() != "http" && next.scheme() != "https" {
                        diagnostics::guard(diagnostics::Guard::RedirectSchemeRefused, "redirect to a non-http(s) scheme refused");
                        return Err(NetworkError::Transport(format!(
                            "redirect to unsupported scheme {:?} refused",
                            next.scheme()
                        )));
                    }
                    current = next;
                    continue; // drop this response unread; fetch the next hop
                }
                // A 3xx with no Location is just a response — fall through.
            }

            // Security: cap how much of a response body we'll ever buffer in
            // memory. Without this, a malicious/compromised server can just
            // keep streaming bytes (chunked transfer has no declared length
            // at all) for as long as the connection's timeout allows,
            // exhausting the process's memory — a DoS against the whole
            // browser, not just the page that triggered it. `+ 1` lets us
            // detect truncation (read hit the cap) instead of silently
            // accepting a partial body as if it were complete.
            let mut body = Vec::new();
            let read = response
                .into_reader()
                .take(MAX_RESPONSE_BODY_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|e| NetworkError::Io(e.to_string()))?;
            if read as u64 > MAX_RESPONSE_BODY_BYTES {
                diagnostics::guard(diagnostics::Guard::ResponseBodySize, "response body exceeded the byte cap; refused");
                return Err(NetworkError::Io(format!("response body exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit")));
            }

            // Undo any `Content-Encoding` (M32), bounded like the raw body.
            let content_encoding = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-encoding")).map(|(_, v)| v.clone());
            let (body, headers) = if content_encoding.is_some() {
                let decoded = encoding::decode_body(content_encoding.as_deref(), body, MAX_RESPONSE_BODY_BYTES as usize).map_err(|e| {
                    if e == encoding::DecodeError::TooLarge {
                        diagnostics::guard(diagnostics::Guard::ResponseBodySize, "decoded response body exceeded the byte cap; refused");
                    }
                    NetworkError::Io(e.to_string())
                })?;
                // The headers now describe the decoded body.
                let headers = headers
                    .into_iter()
                    .filter(|(k, _)| !k.eq_ignore_ascii_case("content-encoding") && !k.eq_ignore_ascii_case("content-length"))
                    .collect();
                (decoded, headers)
            } else {
                (body, headers)
            };
            let fetched = FetchResponse { status, final_url: hop_url, headers, body };

            // Phase F: let the cache store a cacheable 200, or — on a 304 —
            // hand back the stored body it just revalidated.
            if let Some(from_cache) = self.store.http_cache().after_response(&cache_key, &fetched) {
                return Ok(from_cache);
            }
            return Ok(fetched);
        }

        diagnostics::guard(diagnostics::Guard::TooManyRedirects, "redirect chain exceeded the hop limit");
        Err(NetworkError::Transport(format!("too many redirects (more than {MAX_REDIRECTS})")))
    }
}

/// A `Content-Type` header's media type and `charset` parameter, both
/// lowercased (Milestone 32). Never panics on hostile input (fuzzed:
/// `fuzz_targets/http_headers.rs`); a value with no media type gives an
/// empty `mime`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContentType {
    pub mime: String,
    pub charset: Option<String>,
}

pub fn parse_content_type(value: &str) -> ContentType {
    let mut parts = value.split(';');
    let mime = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let charset = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase())
    });
    ContentType { mime, charset }
}

/// Resolves a possibly-relative URL (`href`/`src` attribute value)
/// against the page's own URL — what `<link>`/`<img>`/`background-image`
/// need to turn a relative path into something `fetch`-able. `None`
/// for genuinely unparseable input, never a panic.
pub fn resolve_url(base: &str, relative: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    base.join(relative).ok().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// A minimal hand-rolled HTTP/1.1 mock server for tests — no
    /// extra dependency, full control, one OS-assigned port per test
    /// so parallel `cargo test` runs never collide. GET-request
    /// parsing only (this crate never issues anything else yet); good
    /// enough to exercise real request/response wiring without any
    /// network access outside this machine.
    fn spawn_mock_server<F>(handler: F) -> String
    where
        F: Fn(&str, &[(String, String)]) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind mock server");
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = handle_connection(stream, handler.as_ref());
                });
            }
        });

        format!("http://127.0.0.1:{port}")
    }

    fn handle_connection(
        stream: TcpStream,
        handler: &(dyn Fn(&str, &[(String, String)]) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync),
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        // "GET /path HTTP/1.1"
        let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }

        let (status, resp_headers, body) = handler(&path, &headers);

        let mut stream = stream;
        let status_text = match status {
            200 => "OK",
            301 => "Moved Permanently",
            302 => "Found",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        write!(stream, "HTTP/1.1 {status} {status_text}\r\n")?;
        write!(stream, "Content-Length: {}\r\n", body.len())?;
        for (name, value) in &resp_headers {
            write!(stream, "{name}: {value}\r\n")?;
        }
        write!(stream, "Connection: close\r\n\r\n")?;
        stream.write_all(&body)?;
        stream.flush()?;
        Ok(())
    }

    #[test]
    fn fetches_a_simple_ok_response() {
        let base = spawn_mock_server(|_path, _headers| {
            (200, vec![("Content-Type".to_string(), "text/html".to_string())], b"<html></html>".to_vec())
        });
        let route = NetworkContext::new();
        let response = route.fetch(&base).expect("fetch should succeed");
        assert_eq!(response.status, 200);
        assert_eq!(response.body_as_string(), "<html></html>");
        assert_eq!(response.header("Content-Type"), Some("text/html"));
    }

    #[test]
    fn non_2xx_status_is_a_response_not_an_error() {
        let base = spawn_mock_server(|_path, _headers| (404, vec![], b"not found".to_vec()));
        let route = NetworkContext::new();
        let response = route.fetch(&base).expect("a 404 should still be Ok(..), not Err(..)");
        assert_eq!(response.status, 404);
        assert_eq!(response.body_as_string(), "not found");
    }

    #[test]
    fn server_error_status_is_also_a_response_not_an_error() {
        let base = spawn_mock_server(|_path, _headers| (500, vec![], b"server error".to_vec()));
        let route = NetworkContext::new();
        let response = route.fetch(&base).expect("a 500 should still be Ok(..)");
        assert_eq!(response.status, 500);
    }

    #[test]
    fn follows_redirects_and_reports_the_final_url() {
        let base = spawn_mock_server(|path, _headers| {
            if path == "/start" {
                (302, vec![("Location".to_string(), "/end".to_string())], vec![])
            } else {
                (200, vec![], b"final destination".to_vec())
            }
        });
        let route = NetworkContext::new();
        let response = route.fetch(&format!("{base}/start")).expect("should follow the redirect");
        assert_eq!(response.status, 200);
        assert_eq!(response.body_as_string(), "final destination");
        assert!(response.final_url.ends_with("/end"));
    }

    #[test]
    fn invalid_url_is_a_network_error_not_a_panic() {
        let route = NetworkContext::new();
        let result = route.fetch("not a valid url at all");
        assert!(matches!(result, Err(NetworkError::InvalidUrl(_))));
    }

    #[test]
    fn connection_refused_is_a_transport_error_not_a_panic() {
        // Nothing is listening on this port — a real, immediate
        // connection failure, exercising the Transport error path
        // without needing external network access.
        let route = NetworkContext::new();
        let result = route.fetch("http://127.0.0.1:1");
        assert!(matches!(result, Err(NetworkError::Transport(_))));
    }

    #[test]
    fn set_cookie_is_sent_back_on_the_next_request_to_the_same_server() {
        let base = spawn_mock_server(|path, headers| {
            if path == "/login" {
                (200, vec![("Set-Cookie".to_string(), "session=abc123; Path=/".to_string())], vec![])
            } else {
                // Echo back whether a Cookie header was received, so
                // the test can check it via the response body.
                let cookie_seen = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("cookie"));
                (200, vec![], if cookie_seen { b"cookie-received".to_vec() } else { b"no-cookie".to_vec() })
            }
        });
        let route = NetworkContext::new();

        route.fetch(&format!("{base}/login")).expect("login request should succeed");
        assert_eq!(route.cookie_jar().header_for("127.0.0.1", "/", false), Some("session=abc123".to_string()));

        let second = route.fetch(&format!("{base}/account")).expect("second request should succeed");
        assert_eq!(second.body_as_string(), "cookie-received");
    }

    #[test]
    fn http_cache_serves_a_fresh_response_without_a_second_network_hit() {
        let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits_srv = Arc::clone(&hits);
        let base = spawn_mock_server(move |_path, _headers| {
            hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (200, vec![("Cache-Control".to_string(), "max-age=60".to_string())], b"cached payload".to_vec())
        });
        let route = NetworkContext::new();

        let first = route.fetch(&format!("{base}/asset.css")).unwrap();
        assert_eq!(first.body_as_string(), "cached payload");
        let second = route.fetch(&format!("{base}/asset.css")).unwrap();
        assert_eq!(second.body_as_string(), "cached payload");

        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "second fetch should be served from cache");
        assert_eq!(route.http_cache().hits, 1);
    }

    #[test]
    fn http_cache_revalidates_with_etag_and_handles_304() {
        let base = spawn_mock_server(move |_path, headers| {
            let inm = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("if-none-match")).map(|(_, v)| v.clone());
            if inm.as_deref() == Some("\"v1\"") {
                (304, vec![("ETag".to_string(), "\"v1\"".to_string())], vec![])
            } else {
                (
                    200,
                    vec![("ETag".to_string(), "\"v1\"".to_string()), ("Cache-Control".to_string(), "max-age=0".to_string())],
                    b"real body".to_vec(),
                )
            }
        });
        let route = NetworkContext::new();

        let first = route.fetch(&format!("{base}/x")).unwrap();
        assert_eq!(first.body_as_string(), "real body");
        // Stale (max-age=0) => conditional request => server 304 =>
        // cache serves the stored body.
        let second = route.fetch(&format!("{base}/x")).unwrap();
        assert_eq!(second.status, 200);
        assert_eq!(second.body_as_string(), "real body");
        assert_eq!(route.http_cache().revalidations, 1);
    }

    #[test]
    fn resolve_url_handles_relative_and_absolute_paths() {
        assert_eq!(
            resolve_url("https://example.com/page/index.html", "style.css"),
            Some("https://example.com/page/style.css".to_string())
        );
        assert_eq!(
            resolve_url("https://example.com/page/index.html", "/root.css"),
            Some("https://example.com/root.css".to_string())
        );
        assert_eq!(
            resolve_url("https://example.com/page/index.html", "https://other.com/x.css"),
            Some("https://other.com/x.css".to_string())
        );
    }

    #[test]
    fn content_types_parse() {
        assert_eq!(parse_content_type("text/html; charset=UTF-8"), ContentType { mime: "text/html".into(), charset: Some("utf-8".into()) });
        assert_eq!(parse_content_type("Text/CSS"), ContentType { mime: "text/css".into(), charset: None });
        assert_eq!(parse_content_type("text/html; foo=bar; charset=\"Shift_JIS\"").charset.as_deref(), Some("shift_jis"));
        assert_eq!(parse_content_type(";;;=;").mime, "");
    }

    #[test]
    fn resolve_url_on_garbage_input_does_not_panic() {
        assert_eq!(resolve_url("not a url", "style.css"), None);
        assert_eq!(resolve_url("https://example.com/", ""), Some("https://example.com/".to_string()));
    }

    // ---- Real concurrency: fetch_async ----

    #[test]
    fn fetch_async_eventually_delivers_the_real_response() {
        let base = spawn_mock_server(|_path, _headers| (200, vec![], b"async body".to_vec()));
        let route = NetworkContext::new();
        let handle = route.fetch_async(&base);

        // Poll with a generous but bounded timeout — this is
        // genuinely asynchronous, so the result isn't necessarily
        // ready on the very first check.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            if let Some(result) = handle.try_recv() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline, "fetch_async never completed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        let response = result.expect("fetch should succeed");
        assert_eq!(response.body_as_string(), "async body");
    }

    #[test]
    fn fetch_async_does_not_block_the_calling_thread() {
        // The real proof of concurrency, not just a non-blocking-
        // looking API: the mock server deliberately sleeps for 200ms
        // before responding. If `fetch_async` blocked the calling
        // thread the way `fetch` does, this test's `try_recv()` call
        // immediately afterward could only ever see the request
        // already resolved (since 200ms would have already elapsed
        // synchronously). Instead, immediately after dispatching, the
        // result must NOT be ready yet — proving the calling thread
        // kept running while a separate OS thread did the waiting.
        let base = spawn_mock_server(|_path, _headers| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            (200, vec![], b"slow".to_vec())
        });
        let route = NetworkContext::new();

        let handle = route.fetch_async(&base);
        // Checked essentially immediately — the calling thread was
        // never made to wait for the server's 200ms sleep.
        assert!(handle.try_recv().is_none(), "result should not be ready this fast if fetch_async were blocking");

        // Eventually it does complete correctly (same polling pattern
        // as the test above), confirming this isn't just "never
        // finishes" — it genuinely completes later, off-thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            if let Some(result) = handle.try_recv() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline, "fetch_async never completed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(result.unwrap().body_as_string(), "slow");
    }

    #[test]
    fn multiple_concurrent_fetches_all_complete_correctly() {
        // Real parallelism proof: dispatch several fetches to
        // DIFFERENT mock servers at once (all from one DirectRoute,
        // sharing its Mutex-protected cookie jar across threads —
        // this also exercises that the shared state is genuinely
        // thread-safe, not just "usually fine"), and confirm every
        // one resolves with ITS OWN correct, distinct body — no
        // cross-talk between concurrently in-flight requests.
        let bases: Vec<String> = (0..5)
            .map(|i| spawn_mock_server(move |_path, _headers| (200, vec![], format!("response-{i}").into_bytes())))
            .collect();

        let route = NetworkContext::new();
        let handles: Vec<FetchHandle> = bases.iter().map(|base| route.fetch_async(base)).collect();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut results: Vec<Option<Result<FetchResponse, NetworkError>>> = (0..handles.len()).map(|_| None).collect();
        while results.iter().any(Option::is_none) {
            for (i, handle) in handles.iter().enumerate() {
                if results[i].is_none() {
                    if let Some(r) = handle.try_recv() {
                        results[i] = Some(r);
                    }
                }
            }
            assert!(std::time::Instant::now() < deadline, "not all concurrent fetches completed in time");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        for (i, result) in results.into_iter().enumerate() {
            assert_eq!(result.unwrap().unwrap().body_as_string(), format!("response-{i}"));
        }
    }

    #[test]
    fn fetch_async_against_an_unroutable_url_reports_the_error_not_a_panic() {
        let route = NetworkContext::new();
        let handle = route.fetch_async("http://127.0.0.1:1");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = loop {
            if let Some(result) = handle.try_recv() {
                break result;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(matches!(result, Err(NetworkError::Transport(_))));
    }

    // ---- Milestone 26: shared network context ----

    /// Base URL of a mock server, but addressed as `localhost` instead
    /// of `127.0.0.1` — the same listener, a DIFFERENT host as far as
    /// cookies are concerned (the jar is keyed by host, not port).
    fn as_localhost(base: &str) -> String {
        base.replace("127.0.0.1", "localhost")
    }

    fn cookie_setting_server(cookie: &'static str) -> String {
        spawn_mock_server(move |_path, _headers| (200, vec![("Set-Cookie".to_string(), cookie.to_string())], b"ok".to_vec()))
    }

    /// Echoes the request's `Cookie` header (or `none`) as the body.
    fn cookie_echo_server() -> String {
        spawn_mock_server(|_path, headers| {
            let cookie = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")).map(|(_, v)| v.clone());
            (200, vec![], cookie.unwrap_or_else(|| "none".to_string()).into_bytes())
        })
    }

    #[test]
    fn a_cookie_set_by_one_fetch_is_sent_by_a_later_fetch_through_the_same_context() {
        // The Milestone 26 bug: a page load's cookies were invisible to
        // a later `fetch()` because each built its own jar.
        let setter = cookie_setting_server("sid=abc; Path=/");
        let echo = cookie_echo_server(); // same host (127.0.0.1), different port
        let ctx = NetworkContext::new();

        ctx.fetch(&setter).unwrap();
        let sync_seen = ctx.fetch(&echo).unwrap().body_as_string();
        assert_eq!(sync_seen, "sid=abc");

        // ...and from a background fetch dispatched off a CLONE of the context.
        let clone = ctx.clone();
        let handle = clone.fetch_async(&echo);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let async_seen = loop {
            if let Some(r) = handle.try_recv() {
                break r.unwrap().body_as_string();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(async_seen, "sid=abc");
        assert!(ctx.is_same_context(&clone));
    }

    #[test]
    fn two_contexts_do_not_share_cookies() {
        let setter = cookie_setting_server("sid=abc; Path=/");
        let echo = cookie_echo_server();
        let a = NetworkContext::new();
        let b = NetworkContext::new();

        a.fetch(&setter).unwrap();
        assert_eq!(a.fetch(&echo).unwrap().body_as_string(), "sid=abc");
        assert_eq!(b.fetch(&echo).unwrap().body_as_string(), "none", "context B must not see context A's cookie");
        assert!(!a.is_same_context(&b));
    }

    #[test]
    fn two_contexts_do_not_share_the_http_cache() {
        let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits_srv = Arc::clone(&hits);
        let base = spawn_mock_server(move |_p, _h| {
            hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (200, vec![("Cache-Control".to_string(), "max-age=60".to_string())], b"x".to_vec())
        });
        let (a, b) = (NetworkContext::new(), NetworkContext::new());
        a.fetch(&base).unwrap();
        a.fetch(&base).unwrap(); // served from A's cache
        b.fetch(&base).unwrap(); // B has its own, empty cache
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn redirect_hop_cookies_are_scoped_to_each_hops_own_host() {
        // A (127.0.0.1) sets a cookie ON ITS 302, then redirects to B
        // (localhost), which sets its own. Each must land under its own
        // host — and A's must not ride along to B.
        let b_base = spawn_mock_server(|_path, headers| {
            let saw_a_cookie = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("cookie") && v.contains("hop_a"));
            (
                200,
                vec![("Set-Cookie".to_string(), "hop_b=2; Path=/".to_string())],
                if saw_a_cookie { b"LEAKED".to_vec() } else { b"clean".to_vec() },
            )
        });
        let b_url = as_localhost(&b_base);
        let a_base = {
            let b_url = b_url.clone();
            spawn_mock_server(move |_path, _headers| {
                (302, vec![("Set-Cookie".to_string(), "hop_a=1; Path=/".to_string()), ("Location".to_string(), format!("{b_url}/landing"))], vec![])
            })
        };

        let ctx = NetworkContext::new();
        let response = ctx.fetch(&format!("{a_base}/start")).unwrap();
        assert_eq!(response.body_as_string(), "clean", "hop A's cookie must not be sent to hop B's host");
        assert!(response.final_url.starts_with("http://localhost:"), "final_url is the last hop: {}", response.final_url);

        let jar = ctx.cookie_jar();
        assert_eq!(jar.header_for("127.0.0.1", "/", false), Some("hop_a=1".to_string()), "A's cookie is scoped to A's host");
        assert_eq!(jar.header_for("localhost", "/", false), Some("hop_b=2".to_string()), "B's cookie is scoped to B's host");
    }

    #[test]
    fn relative_and_absolute_redirect_locations_and_all_redirect_statuses_are_followed() {
        for status in [301u16, 302, 303, 307, 308] {
            let base = spawn_mock_server(move |path, _h| match path {
                "/a" => (status, vec![("Location".to_string(), "b".to_string())], vec![]), // relative
                "/b" => (status, vec![("Location".to_string(), "/c?x=1".to_string())], vec![]), // root-relative
                _ => (200, vec![], path.as_bytes().to_vec()),
            });
            let response = NetworkContext::new().fetch(&format!("{base}/a")).unwrap();
            assert_eq!(response.status, 200, "status {status}");
            assert_eq!(response.body_as_string(), "/c?x=1", "status {status}");
        }
    }

    #[test]
    fn a_redirect_loop_is_bounded_not_infinite() {
        let base = spawn_mock_server(|_p, _h| (302, vec![("Location".to_string(), "/again".to_string())], vec![]));
        let started = std::time::Instant::now();
        let result = NetworkContext::new().fetch(&base);
        assert!(matches!(&result, Err(NetworkError::Transport(m)) if m.contains("too many redirects")), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_redirect_to_a_non_web_scheme_is_refused() {
        // A hostile server must not be able to bounce us to `file:` (or
        // `data:`, `ftp:`, ...).
        for target in ["file:///etc/passwd", "data:text/html,<b>x", "ftp://example.com/x", "javascript:alert(1)"] {
            let target = target.to_string();
            let base = spawn_mock_server(move |_p, _h| (302, vec![("Location".to_string(), target.clone())], vec![]));
            let result = NetworkContext::new().fetch(&base);
            assert!(matches!(&result, Err(NetworkError::Transport(m)) if m.contains("unsupported scheme")), "{result:?}");
        }
    }

    #[test]
    fn a_3xx_without_a_location_is_returned_as_a_plain_response() {
        let base = spawn_mock_server(|_p, _h| (302, vec![], b"no location".to_vec()));
        let response = NetworkContext::new().fetch(&base).unwrap();
        assert_eq!(response.status, 302);
        assert_eq!(response.body_as_string(), "no location");
    }

    #[test]
    fn a_malformed_redirect_location_is_an_error_not_a_panic() {
        let base = spawn_mock_server(|_p, _h| (302, vec![("Location".to_string(), "http://[::1".to_string())], vec![]));
        assert!(matches!(NetworkContext::new().fetch(&base), Err(NetworkError::Transport(_))));
    }

    fn scratch_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("network-m26-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cookies.txt")
    }

    #[test]
    fn persistent_cookies_survive_a_new_context_when_a_path_is_configured() {
        let path = scratch_path("roundtrip");
        let setter = cookie_setting_server("keep=1; Path=/; Max-Age=3600");
        let echo = cookie_echo_server();

        let first = NetworkContext::new();
        first.load_cookies(&path).unwrap(); // missing file is fine — first run
        first.fetch(&setter).unwrap();
        assert!(path.exists(), "the file is written as soon as a Set-Cookie changes the jar — no shutdown hook needed");

        let second = NetworkContext::new(); // a fresh process, conceptually
        second.load_cookies(&path).unwrap();
        assert_eq!(second.fetch(&echo).unwrap().body_as_string(), "keep=1");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn persistence_is_off_by_default_and_session_cookies_are_never_written() {
        let path = scratch_path("session");
        let setter = cookie_setting_server("s=1; Path=/"); // no Max-Age/Expires: a session cookie

        let unconfigured = NetworkContext::new();
        unconfigured.fetch(&setter).unwrap();
        assert!(!path.exists(), "nothing is written unless a path was explicitly configured");
        assert!(!unconfigured.save_cookies().unwrap(), "nothing to save when persistence is off");

        let configured = NetworkContext::new();
        configured.load_cookies(&path).unwrap();
        configured.fetch(&setter).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("s\t1"), "session cookies must not be persisted: {written:?}");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_cookie_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch_path("perms");
        let ctx = NetworkContext::new();
        ctx.load_cookies(&path).unwrap();
        ctx.fetch(&cookie_setting_server("k=v; Path=/; Max-Age=60")).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the jar holds session tokens");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_cookie_file_loads_without_panicking_and_is_ignored() {
        let path = scratch_path("corrupt");
        std::fs::write(&path, b"\xff\xfe not utf8 \x00\x00").unwrap();
        // Invalid UTF-8 is an I/O error from read_to_string — reported, not a panic.
        let ctx = NetworkContext::new();
        let _ = ctx.load_cookies(&path);

        std::fs::write(&path, "garbage\nonly\tthree\tfields\n").unwrap();
        NetworkContext::new().load_cookies(&path).expect("malformed lines are skipped, not fatal");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A route that records what it was asked for — proves a context
    /// can be built around ANY `NetworkRoute` without callers changing.
    struct RecordingRoute {
        seen: std::sync::Mutex<Vec<String>>,
    }
    impl NetworkRoute for Arc<RecordingRoute> {
        fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
            self.seen.lock().unwrap().push(url.to_string());
            Ok(FetchResponse { status: 200, final_url: url.to_string(), headers: vec![], body: b"from custom route".to_vec() })
        }
    }

    #[test]
    fn a_context_can_be_built_around_any_route() {
        let recorder = Arc::new(RecordingRoute { seen: std::sync::Mutex::new(Vec::new()) });
        let ctx = NetworkContext::with_route({
            let recorder = Arc::clone(&recorder);
            move |_store| Arc::new(recorder)
        });

        assert_eq!(ctx.fetch("http://example.invalid/sync").unwrap().body_as_string(), "from custom route");

        let handle = ctx.fetch_async("http://example.invalid/async");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.try_recv().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = recorder.seen.lock().unwrap().clone();
        assert!(seen.contains(&"http://example.invalid/sync".to_string()) && seen.contains(&"http://example.invalid/async".to_string()), "{seen:?}");
    }

    #[test]
    fn a_panic_while_holding_the_jar_does_not_poison_every_later_request() {
        let ctx = NetworkContext::new();
        let clone = ctx.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.cookie_jar();
            panic!("simulated page-thread panic while holding the cookie jar");
        })
        .join();
        // Another page's request must still work.
        assert!(ctx.fetch(&cookie_echo_server()).is_ok());
    }

    #[test]
    fn network_context_and_route_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NetworkContext>();
        assert_send_sync::<NetworkStore>();
        assert_send_sync::<DirectRoute>();
    }

    // ---- Milestone 27: every silent control is observable ----

    #[test]
    fn refused_redirects_and_redirect_loops_report_their_guards() {
        use diagnostics::{collector::capture, Guard};
        let to_file = spawn_mock_server(|_p, _h| (302, vec![("Location".to_string(), "file:///etc/passwd".to_string())], vec![]));
        let (result, c) = capture(|| NetworkContext::new().fetch(&to_file));
        assert!(result.is_err());
        assert!(c.saw_guard(Guard::RedirectSchemeRefused));
        assert!(!c.saw_guard(Guard::TooManyRedirects));

        let looping = spawn_mock_server(|_p, _h| (302, vec![("Location".to_string(), "/again".to_string())], vec![]));
        let (result, c) = capture(|| NetworkContext::new().fetch(&looping));
        assert!(result.is_err());
        assert!(c.saw_guard(Guard::TooManyRedirects));
    }

    #[test]
    fn fetch_start_finish_and_failure_are_events_and_counters() {
        use diagnostics::collector::capture;
        let before = diagnostics::metrics::snapshot();
        let ok = spawn_mock_server(|_p, _h| (200, vec![], b"hello".to_vec()));
        let (_, c) = capture(|| {
            NetworkContext::new().fetch(&format!("{ok}/page")).unwrap();
            let _ = NetworkContext::new().fetch("http://127.0.0.1:1/");
        });
        let messages: Vec<&str> = c.events.iter().map(|e| e.message.as_str()).collect();
        assert!(messages.contains(&"fetch started"), "{messages:?}");
        assert!(messages.contains(&"fetch finished"), "{messages:?}");
        assert!(messages.contains(&"fetch failed"), "{messages:?}");
        let finished = c.events.iter().find(|e| e.message == "fetch finished").unwrap();
        assert_eq!(finished.field("status"), Some("200"));
        assert_eq!(finished.field("bytes"), Some("5"));

        let after = diagnostics::metrics::snapshot();
        assert!(after.delta(&before, "fetches_started") >= 2);
        assert!(after.delta(&before, "fetches_finished") >= 1);
        assert!(after.delta(&before, "fetches_failed") >= 1);
    }

    #[test]
    fn diagnostics_never_contain_url_credentials_query_strings_or_fragments() {
        // Privacy-first: a trace must not become a place session tokens
        // and passwords leak into. Local-only and opt-in, but cheaper to
        // guarantee at the source than to audit later.
        use diagnostics::collector::capture;
        let base = spawn_mock_server(|_p, _h| (200, vec![], b"ok".to_vec()));
        let authority = base.trim_start_matches("http://");
        let secret_url = format!("http://alice:hunter2@{authority}/account?token=SECRET-TOKEN#frag-secret");
        let (_, c) = capture(|| NetworkContext::new().fetch(&secret_url));
        assert!(!c.events.is_empty());
        for e in &c.events {
            let all = format!("{} {:?}", e.message, e.fields);
            for leaked in ["alice", "hunter2", "SECRET-TOKEN", "frag-secret", "token="] {
                assert!(!all.contains(leaked), "diagnostics leaked {leaked:?}: {all}");
            }
        }
        assert!(c.events.iter().any(|e| e.field("url").is_some_and(|u| u.ends_with("/account"))), "the path is kept: {:?}", c.events);
    }

    #[test]
    fn loggable_url_strips_userinfo_query_and_fragment_and_survives_garbage() {
        assert_eq!(loggable_url("https://u:p@example.com:8443/a/b?x=1#f"), "https://example.com:8443/a/b");
        assert_eq!(loggable_url("http://example.com/"), "http://example.com/");
        assert_eq!(loggable_url("not a url"), "<unparseable url>");
        assert_eq!(loggable_url(""), "<unparseable url>");
    }

    // ---- Hardening pass: request hygiene, SameSite over the wire, cache partitioning ----

    fn header_echo_server() -> String {
        // Replies with every request header it received, one per line.
        spawn_mock_server(|_path, headers| {
            let text = headers.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
            (200, vec![], text.into_bytes())
        })
    }

    fn header_of(echoed: &str, name: &str) -> Option<String> {
        echoed.lines().find_map(|l| {
            let (k, v) = l.split_once(": ")?;
            k.eq_ignore_ascii_case(name).then(|| v.to_string())
        })
    }

    #[test]
    fn requests_carry_a_frozen_user_agent_and_language_and_never_the_http_librarys_default() {
        let base = header_echo_server();
        let echoed = NetworkContext::new().fetch(&base).unwrap().body_as_string();
        assert_eq!(header_of(&echoed, "user-agent").as_deref(), Some("browser-engine/0.1"));
        assert_eq!(header_of(&echoed, "accept-language").as_deref(), Some("en-US,en;q=0.5"));
        assert!(!echoed.to_ascii_lowercase().contains("ureq"), "the HTTP library must not fingerprint us: {echoed}");
        assert_eq!(header_of(&echoed, "referer"), None, "a browser-initiated request has no Referer");
    }

    #[test]
    fn the_hygiene_policy_is_configurable_per_context() {
        let base = header_echo_server();
        let ctx = NetworkContext::new();
        ctx.set_policy(NetworkPolicy { user_agent: "custom/9".into(), accept_language: "de".into(), referrer: ReferrerPolicy::NoReferrer, ..NetworkPolicy::default() });
        let echoed = ctx.fetch_with(&base, &RequestContext::subresource_of("https://page.test/secret?token=1")).unwrap().body_as_string();
        assert_eq!(header_of(&echoed, "user-agent").as_deref(), Some("custom/9"));
        assert_eq!(header_of(&echoed, "accept-language").as_deref(), Some("de"));
        assert_eq!(header_of(&echoed, "referer"), None, "NoReferrer sends none");
    }

    #[test]
    fn referer_follows_strict_origin_when_cross_origin() {
        let p = ReferrerPolicy::StrictOriginWhenCrossOrigin;
        let t = |u: &str| url::Url::parse(u).unwrap();
        // Same origin: the full URL — but never credentials or a fragment.
        assert_eq!(referer_for(p, "https://a.test/x/y?q=1#frag", &t("https://a.test/z")).as_deref(), Some("https://a.test/x/y?q=1"));
        assert_eq!(referer_for(p, "https://u:pw@a.test/x", &t("https://a.test/z")).as_deref(), Some("https://a.test/x"));
        // Cross-origin: the origin only (no path, no query).
        assert_eq!(referer_for(p, "https://a.test/x/y?token=SECRET", &t("https://b.test/")).as_deref(), Some("https://a.test/"));
        assert_eq!(referer_for(p, "https://a.test/x", &t("https://a.test:8443/")).as_deref(), Some("https://a.test/"), "a different port is a different origin");
        // Downgrade https -> http: nothing at all.
        assert_eq!(referer_for(p, "https://a.test/x", &t("http://b.test/")), None);
        // http -> https and http -> http are fine.
        assert_eq!(referer_for(p, "http://a.test/x?y=1", &t("https://b.test/")).as_deref(), Some("http://a.test/"));
        // Non-web initiators and garbage send nothing.
        assert_eq!(referer_for(p, "file:///etc/passwd", &t("https://b.test/")), None);
        assert_eq!(referer_for(p, "data:text/html,x", &t("https://b.test/")), None);
        assert_eq!(referer_for(p, "not a url", &t("https://b.test/")), None);
        assert_eq!(referer_for(ReferrerPolicy::NoReferrer, "https://a.test/x", &t("https://a.test/")), None);
    }

    #[test]
    fn a_page_initiated_request_sends_a_policy_shaped_referer_on_the_wire() {
        let base = header_echo_server(); // http://127.0.0.1:PORT
        let ctx = NetworkContext::new();
        // Same origin as the target: the full path (no fragment).
        let same = ctx.fetch_with(&format!("{base}/api"), &RequestContext::subresource_of(&format!("{base}/page?x=1#f"))).unwrap();
        assert_eq!(header_of(&same.body_as_string(), "referer"), Some(format!("{base}/page?x=1")));
        // Cross-origin: origin only.
        let cross = ctx.fetch_with(&format!("{base}/api"), &RequestContext::subresource_of("http://other.test/private/path?token=SECRET")).unwrap();
        let referer = header_of(&cross.body_as_string(), "referer").unwrap();
        assert_eq!(referer, "http://other.test/");
        assert!(!referer.contains("SECRET"));
    }

    #[test]
    fn samesite_cookies_are_withheld_from_cross_site_requests_over_real_http() {
        // Set cookies on host 127.0.0.1 (its own site), then request it as
        // a subresource of a page on another site and as a navigation.
        let setter = spawn_mock_server(|_p, _h| {
            (
                200,
                vec![
                    ("Set-Cookie".to_string(), "strict=1; SameSite=Strict".to_string()),
                    ("Set-Cookie".to_string(), "lax=1; SameSite=Lax".to_string()),
                    ("Set-Cookie".to_string(), "plain=1".to_string()),
                ],
                b"ok".to_vec(),
            )
        });
        let echo = header_echo_server();
        let ctx = NetworkContext::new();
        ctx.fetch(&setter).unwrap();

        let cookie_seen = |ctx_: RequestContext| {
            let echoed = ctx.fetch_with(&echo, &ctx_).unwrap().body_as_string();
            header_of(&echoed, "cookie")
        };
        // Same-site page (initiator on the same host): everything rides.
        let same = cookie_seen(RequestContext::subresource_of(&echo)).unwrap();
        assert!(same.contains("strict=1") && same.contains("lax=1") && same.contains("plain=1"), "{same}");
        // Cross-site subresource (an <img>/fetch from another site): nothing.
        assert_eq!(cookie_seen(RequestContext::subresource_of("https://evil.test/page")), None);
        // Cross-site top-level navigation (a link click): Lax and unspecified only.
        let nav = cookie_seen(RequestContext::navigation_from("https://evil.test/page")).unwrap();
        assert!(nav.contains("lax=1") && nav.contains("plain=1") && !nav.contains("strict=1"), "{nav}");
        // Typed URL: browser-initiated, everything.
        let typed = cookie_seen(RequestContext::typed_navigation()).unwrap();
        assert!(typed.contains("strict=1"));
    }

    #[test]
    fn samesite_is_evaluated_on_every_redirect_hop() {
        // page on evil.test -> subresource on A (127.0.0.1) -> redirect to B (localhost).
        // Each hop is its own request site; both are cross-site to evil.test.
        let b_base = as_localhost(&header_echo_server());
        let a_base = {
            let b = b_base.clone();
            spawn_mock_server(move |_p, _h| (302, vec![("Location".to_string(), format!("{b}/x"))], vec![]))
        };
        let ctx = NetworkContext::new();
        ctx.cookie_jar().store_from_header("b=1; SameSite=Strict", "localhost");
        let response = ctx.fetch_with(&a_base, &RequestContext::subresource_of("https://evil.test/")).unwrap();
        assert_eq!(header_of(&response.body_as_string(), "cookie"), None, "B's Strict cookie must not ride a cross-site redirect");
    }

    #[test]
    fn the_http_cache_is_partitioned_by_top_level_site() {
        // Cache-timing / ETag tracking: a resource fetched for site A must
        // not be served as a cache hit to site B.
        let hits = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hits_srv = Arc::clone(&hits);
        let base = spawn_mock_server(move |_p, _h| {
            hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (200, vec![("Cache-Control".to_string(), "max-age=60".to_string())], b"x".to_vec())
        });
        let ctx = NetworkContext::new();
        let url = format!("{base}/lib.js");
        let count = || hits.load(std::sync::atomic::Ordering::SeqCst);

        ctx.fetch_with(&url, &RequestContext::subresource_of("https://a.test/")).unwrap();
        assert_eq!(count(), 1);
        ctx.fetch_with(&url, &RequestContext::subresource_of("https://a.test/other")).unwrap();
        assert_eq!(count(), 1, "same top-level site: a cache hit");
        ctx.fetch_with(&url, &RequestContext::subresource_of("https://b.test/")).unwrap();
        assert_eq!(count(), 2, "a different top-level site must MISS, not learn that a.test fetched it");
        ctx.fetch_with(&url, &RequestContext::subresource_of("https://sub.a.test/")).unwrap();
        assert_eq!(count(), 2, "subdomains of one site share a partition");
    }

    // ---- Milestone 32: Content-Encoding ----

    #[test]
    fn a_gzip_body_is_decoded_even_when_unrequested() {
        // Regression (conformance corpus: python.org): a CDN sent gzip, and
        // the engine used to hand the compressed bytes to the HTML parser.
        let html = b"<html><body><p>hello from gzip</p></body></html>".to_vec();
        let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
        gz.extend(miniz_oxide::deflate::compress_to_vec(&html, 6));
        gz.extend_from_slice(&[0; 8]);
        let seen_accept = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = std::sync::Arc::clone(&seen_accept);
        let base = spawn_mock_server(move |_p, h| {
            *seen.lock().unwrap() = h.iter().find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding")).map(|(_, v)| v.clone());
            (200, vec![("Content-Encoding".to_string(), "gzip".to_string()), ("Content-Type".to_string(), "text/html".to_string())], gz.clone())
        });
        let r = NetworkContext::new().fetch_with(&base, &RequestContext::typed_navigation()).unwrap();
        assert_eq!(r.body, html);
        assert!(r.header("content-encoding").is_none() && r.header("content-length").is_none(), "headers describe the decoded body");
        assert_eq!(seen_accept.lock().unwrap().as_deref(), Some("gzip, deflate"));
    }

    #[test]
    fn an_unsupported_or_corrupt_encoding_fails_the_fetch_cleanly() {
        let base = spawn_mock_server(|p, _h| {
            let enc = if p == "/br" { "br" } else { "gzip" };
            (200, vec![("Content-Encoding".to_string(), enc.to_string())], b"definitely not compressed".to_vec())
        });
        let ctx = NetworkContext::new();
        assert!(matches!(ctx.fetch_with(&format!("{base}/br"), &RequestContext::default()), Err(NetworkError::Io(e)) if e.contains("br")));
        assert!(matches!(ctx.fetch_with(&format!("{base}/gz"), &RequestContext::default()), Err(NetworkError::Io(_))));
    }

    // ---- Milestone 28: origins, opaque initiators, credentials mode ----

    #[test]
    fn an_opaque_initiator_is_cross_site_to_everything() {
        // A document with no URL (the built-in page, a `data:` document) used
        // to make browser-initiated-looking requests that carried every
        // cookie. Its opaque origin now makes them cross-site.
        let echo = header_echo_server();
        let ctx = NetworkContext::new();
        ctx.cookie_jar().store_from_header("strict=1; SameSite=Strict", "127.0.0.1");
        ctx.cookie_jar().store_from_header("plain=1", "127.0.0.1");
        let opaque = Initiator::document(Origin::new_opaque(), None);
        let sent = ctx.fetch_with(&echo, &RequestContext::subresource_by(opaque.clone())).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie"), None, "no SameSite-restricted cookie rides an opaque document's request");
        assert_eq!(header_of(&sent, "referer"), None, "and there is no URL to leak as Referer");
        // Its top-level navigations still carry Lax/unspecified cookies (a link click).
        let nav = ctx.fetch_with(&echo, &RequestContext::navigation_by(opaque)).unwrap().body_as_string();
        assert_eq!(header_of(&nav, "cookie").as_deref(), Some("plain=1"));
    }

    #[test]
    fn sites_are_schemeful_for_samesite() {
        // An http page is not the same party as the https site with the same
        // name: a network attacker controls the first.
        let echo = header_echo_server();
        let ctx = NetworkContext::new();
        ctx.cookie_jar().store_from_header("lax=1; SameSite=Lax", "127.0.0.1");
        let host_port = echo.trim_start_matches("http://").to_string();
        let from_https = RequestContext::subresource_of(&format!("https://{host_port}/page"));
        let sent = ctx.fetch_with(&echo, &from_https).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie"), None, "https page -> http resource is cross-site");
        let from_http = RequestContext::subresource_of(&format!("http://{host_port}/page"));
        let sent = ctx.fetch_with(&echo, &from_http).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie").as_deref(), Some("lax=1"));
    }

    #[test]
    fn same_origin_credentials_mode_sends_and_stores_cookies_only_same_origin() {
        let setter = spawn_mock_server(|_p, _h| (200, vec![("Set-Cookie".to_string(), "fromapi=1".to_string())], b"ok".to_vec()));
        let echo = header_echo_server();
        let ctx = NetworkContext::new();
        ctx.cookie_jar().store_from_header("session=1", "127.0.0.1");

        // A script on another origin (different port = different origin,
        // same site) calls the API without asking for credentials.
        let script = RequestContext::subresource_of("http://127.0.0.1:1/app").with_credentials(Credentials::SameOrigin);
        let sent = ctx.fetch_with(&echo, &script).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie"), None, "a cross-origin fetch() carries no cookies by default");
        ctx.fetch_with(&setter, &script).unwrap();
        assert!(
            !ctx.cookie_jar().header_for("127.0.0.1", "/", false).unwrap_or_default().contains("fromapi"),
            "and its response cannot set any"
        );

        // Same-origin with the page: cookies go.
        let same = RequestContext::subresource_of(&format!("{echo}/app")).with_credentials(Credentials::SameOrigin);
        let sent = ctx.fetch_with(&echo, &same).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie").as_deref(), Some("session=1"));

        // `include` opts back in; `omit` opts out even same-origin.
        let sent = ctx.fetch_with(&echo, &script.clone().with_credentials(Credentials::Include)).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie").as_deref(), Some("session=1"));
        let sent = ctx.fetch_with(&echo, &same.with_credentials(Credentials::Omit)).unwrap().body_as_string();
        assert_eq!(header_of(&sent, "cookie"), None);
    }

    #[test]
    fn a_same_origin_fetch_redirected_cross_origin_stops_carrying_cookies() {
        let b_base = as_localhost(&header_echo_server());
        let a_base = {
            let b = b_base.clone();
            spawn_mock_server(move |_p, _h| (302, vec![("Location".to_string(), format!("{b}/x"))], vec![]))
        };
        let ctx = NetworkContext::new();
        ctx.cookie_jar().store_from_header("b=1", "localhost");
        let script = RequestContext::subresource_of(&format!("{a_base}/page")).with_credentials(Credentials::SameOrigin);
        let response = ctx.fetch_with(&a_base, &script).unwrap();
        assert_eq!(header_of(&response.body_as_string(), "cookie"), None, "the cross-origin hop gets no cookies");
    }

    #[test]
    fn cache_keys_differ_by_partition_and_navigations_use_the_destination_site() {
        let sub_a = cache_key(&RequestContext::subresource_of("https://a.test/"), "https://cdn.test/x.js");
        let sub_b = cache_key(&RequestContext::subresource_of("https://b.test/"), "https://cdn.test/x.js");
        assert_ne!(sub_a, sub_b);
        let credentialed = cache_key(&RequestContext::subresource_of("https://a.test/"), "https://cdn.test/x.js");
        let anonymous = cache_key(&RequestContext::subresource_of("https://a.test/").with_credentials(Credentials::SameOrigin), "https://cdn.test/x.js");
        assert_ne!(credentialed, anonymous, "a credentialed response is never served to an uncredentialed request");
        let nav = cache_key(&RequestContext::navigation_from("https://a.test/"), "https://cdn.test/x.js");
        assert!(nav.starts_with("https://cdn.test|"), "a navigation is keyed by its destination's (schemeful) site: {nav}");
    }

    // ---- Milestone 37: third-party cookies (ADR 019 §2, P6) ----

    /// A store holding a `SameSite=None; Secure` cookie for the tracker, set
    /// while the user visited it directly (first-party).
    fn store_with_tracker_cookie() -> Arc<NetworkStore> {
        let store = NetworkStore::new();
        store.cookie_jar().store("id=42; SameSite=None; Secure; Path=/; Max-Age=3600", "tracker.example", true);
        store
    }

    #[test]
    fn a_samesite_none_secure_cookie_is_not_sent_on_a_third_party_request_by_default() {
        let store = store_with_tracker_cookie();
        let hop = url::Url::parse("https://tracker.example/pixel.gif").unwrap();
        let policy = NetworkPolicy::default();
        let embedded = RequestContext::subresource_by(Initiator::for_url("https://news.example/article"));
        assert_eq!(hop_cookies(&store, &policy, &embedded, &hop), (false, None), "third-party: neither sent nor stored");
        // First-party — the user on the tracker's own site — still works.
        let own = RequestContext::subresource_by(Initiator::for_url("https://tracker.example/home"));
        assert_eq!(hop_cookies(&store, &policy, &own, &hop), (true, Some("id=42".into())));
        assert_eq!(hop_cookies(&store, &policy, &RequestContext::navigation_from("https://news.example/"), &hop).1.as_deref(), Some("id=42"), "a navigation is first-party");
        // The old behaviour, only when the policy asks for it.
        let allow = NetworkPolicy { third_party_cookies: ThirdPartyCookies::Allow, ..NetworkPolicy::default() };
        assert_eq!(hop_cookies(&store, &allow, &embedded, &hop).1.as_deref(), Some("id=42"));
    }

    #[test]
    fn two_sites_embedding_the_same_third_party_cannot_link_through_it() {
        let store = store_with_tracker_cookie();
        let hop = url::Url::parse("https://tracker.example/sync").unwrap();
        let policy = NetworkPolicy::default();
        for site in ["https://a.example/", "https://b.example/"] {
            let ctx = RequestContext::subresource_by(Initiator::for_url(site));
            assert_eq!(hop_cookies(&store, &policy, &ctx, &hop), (false, None), "{site}");
        }
        // And the HTTP cache keeps their copies apart (M28's partition).
        let a = cache_key(&RequestContext::subresource_by(Initiator::for_url("https://a.example/")), hop.as_str());
        let b = cache_key(&RequestContext::subresource_by(Initiator::for_url("https://b.example/")), hop.as_str());
        assert_ne!(a, b);
    }
}
