//! Routes and route policy (Milestone 37, ADR 011).
//!
//! Every route runs the same fetch logic (`DirectRoute`'s: redirects hop by
//! hop, cookies, cache, filter, third-party rules) over a [`Transport`] —
//! direct, through a proxy, or gated on a system VPN. A [`RoutePolicy`]
//! picks one route per request, in a fixed order, and a route never falls
//! back to another.

use crate::doh::{DohConfig, DohHook, DohResolver};
use crate::tls::{TlsTrust, TrustReport};
use crate::{DirectRoute, FetchResponse, NetworkError, NetworkRoute, NetworkStore, RequestContext};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Asks the host whether a system VPN is up (ADR 011 §4: the engine reads no
/// system state; the host grants this probe).
pub type VpnProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// How a route's connections are made.
#[derive(Clone, Default)]
pub struct Transport {
    pub(crate) proxy: Option<ureq::Proxy>,
    pub(crate) doh: Option<Arc<DohResolver>>,
    pub(crate) tls: Option<Arc<rustls::ClientConfig>>,
    /// A route that may connect only while this says so.
    pub(crate) gate: Option<VpnProbe>,
}

impl Transport {
    pub(crate) fn agent(&self) -> ureq::Agent {
        let mut builder = ureq::AgentBuilder::new()
            // Redirects are followed by `fetch` itself, hop by hop, so
            // that each hop's `Set-Cookie` is stored under THAT hop's
            // host and each hop sends only the cookies that match it.
            .redirects(0)
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            // What a proxy's `CONNECT` line carries (every request sets its
            // own): the engine's frozen name, not the HTTP library's version.
            .user_agent(&crate::NetworkPolicy::default().user_agent);
        if let Some(proxy) = &self.proxy {
            builder = builder.proxy(proxy.clone());
        } else if let Some(doh) = &self.doh {
            // A proxy resolves names itself (ADR 011); a direct connection
            // asks DoH, when one is configured.
            builder = builder.resolver(DohHook(Arc::clone(doh)));
        }
        if let Some(tls) = &self.tls {
            builder = builder.tls_config(Arc::clone(tls));
        }
        builder.build()
    }
}

/// A proxy: `http://`, `socks4://`, `socks4a://` or `socks5://` (`socks://`),
/// with optional `user:password@`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub url: String,
}

impl ProxyConfig {
    pub fn parse(&self) -> Result<ureq::Proxy, String> {
        let scheme = self.url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
        if !matches!(scheme.as_deref(), Some("http" | "socks4" | "socks4a" | "socks5" | "socks")) {
            return Err(format!("unsupported proxy {:?}: use http://, socks4://, socks4a:// or socks5://", crate::loggable_url(&self.url)));
        }
        let rest = self.url.split_once("://").map_or("", |(_, r)| r);
        let host = rest.rsplit('@').next().unwrap_or("").split(['/', ':']).next().unwrap_or("");
        if host.is_empty() {
            return Err("a proxy URL needs a host".into());
        }
        ureq::Proxy::new(&self.url).map_err(|e| format!("proxy: {e}"))
    }
}

/// Requests through a proxy. If the proxy cannot be reached the request
/// fails; it is never sent directly (ADR 011 §2).
pub struct ProxyRoute(DirectRoute);

impl ProxyRoute {
    pub fn new(store: Arc<NetworkStore>, config: &ProxyConfig, base: &Transport) -> Result<ProxyRoute, String> {
        let transport = Transport { proxy: Some(config.parse()?), ..base.clone() };
        Ok(ProxyRoute(DirectRoute::with_transport(store, transport, "proxy")))
    }
}

impl NetworkRoute for ProxyRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.0.fetch(url)
    }
    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        self.0.fetch_with(url, ctx)
    }
}

/// Direct connections made only while the host's probe says a system VPN is
/// up; while it is down every request fails.
pub struct SystemVpnRoute(DirectRoute);

impl SystemVpnRoute {
    pub fn new(store: Arc<NetworkStore>, probe: VpnProbe, base: &Transport) -> SystemVpnRoute {
        let transport = Transport { gate: Some(probe), ..base.clone() };
        SystemVpnRoute(DirectRoute::with_transport(store, transport, "system-vpn"))
    }
}

impl NetworkRoute for SystemVpnRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.0.fetch(url)
    }
    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        self.0.fetch_with(url, ctx)
    }
}

/// The name of the route every request takes when no rule applies.
pub const DIRECT: &str = "direct";
/// The name the system-VPN route has in rules.
pub const SYSTEM_VPN: &str = "system-vpn";

/// Which route a request takes — plain data, decided by
/// [`RouteRules::choose`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteRules {
    /// 1. In private mode, every request takes this route (if set).
    pub private_override: Option<String>,
    /// 2. By top-level site: `(domain, route)`; a domain matches itself and
    ///    its subdomains; the longest matching domain wins.
    pub site_rules: Vec<(String, String)>,
    /// 3. Everything else (if set); else `DIRECT`.
    pub profile_default: Option<String>,
}

/// Why a route was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteReason {
    PrivateOverride,
    SiteRule,
    ProfileDefault,
    Direct,
}

/// The host a request's top-level site is judged by: the page's, for a
/// request a page made; the destination's, for a navigation or a request no
/// page made. `None` for an opaque page (no site rule can match it).
fn top_level_host(url: &str, ctx: &RequestContext) -> Option<String> {
    let of = |u: &str| url::Url::parse(u).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()));
    match (&ctx.initiator, ctx.is_navigation) {
        (Some(initiator), false) => initiator.url.as_deref().filter(|_| !initiator.origin.is_opaque()).and_then(of),
        _ => of(url),
    }
}

impl RouteRules {
    /// The route for a request (ADR 011 §1). A pure function: tested for
    /// every combination of the three overrides.
    pub fn choose(&self, private_mode: bool, url: &str, ctx: &RequestContext) -> (String, RouteReason) {
        if private_mode {
            if let Some(route) = &self.private_override {
                return (route.clone(), RouteReason::PrivateOverride);
            }
        }
        if let Some(host) = top_level_host(url, ctx) {
            let best = self
                .site_rules
                .iter()
                .filter(|(domain, _)| {
                    let domain = domain.trim_end_matches('.');
                    host == domain || host.ends_with(&format!(".{domain}"))
                })
                .max_by_key(|(domain, _)| domain.len());
            if let Some((_, route)) = best {
                return (route.clone(), RouteReason::SiteRule);
            }
        }
        match &self.profile_default {
            Some(route) => (route.clone(), RouteReason::ProfileDefault),
            None => (DIRECT.to_string(), RouteReason::Direct),
        }
    }
}

/// A route that picks, per request, one of its named routes by its rules.
pub struct RoutePolicy {
    routes: BTreeMap<String, Arc<dyn NetworkRoute>>,
    rules: RouteRules,
    store: Arc<NetworkStore>,
}

impl RoutePolicy {
    pub fn new(store: Arc<NetworkStore>, routes: BTreeMap<String, Arc<dyn NetworkRoute>>, rules: RouteRules) -> RoutePolicy {
        RoutePolicy { routes, rules, store }
    }

    fn route_for(&self, url: &str, ctx: &RequestContext) -> Result<&Arc<dyn NetworkRoute>, NetworkError> {
        let (name, reason) = self.rules.choose(self.store.private_mode(), url, ctx);
        diagnostics::tracing::debug!(target: "engine::network", route = %name, reason = ?reason, "route chosen");
        // A rule naming a route that does not exist fails closed: it never
        // means "direct".
        self.routes.get(&name).ok_or_else(|| {
            diagnostics::guard(diagnostics::Guard::RouteFailedClosed, "a route rule names a route that does not exist; request refused");
            NetworkError::Transport(format!("route {name:?} is not configured: request refused (no fallback)"))
        })
    }
}

impl NetworkRoute for RoutePolicy {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.fetch_with(url, &RequestContext::default())
    }
    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        self.route_for(url, ctx)?.fetch_with(url, ctx)
    }
}

/// Everything about how a profile reaches the network (ADR 011, 012, 018).
#[derive(Clone, Default)]
pub struct NetworkConfig {
    /// Named proxies, usable in [`RouteRules`].
    pub proxies: Vec<(String, ProxyConfig)>,
    /// When set, the `system-vpn` route exists, gated on this probe.
    pub system_vpn: Option<VpnProbe>,
    pub rules: RouteRules,
    /// DNS over HTTPS for direct connections; `None` = system DNS.
    pub doh: Option<DohConfig>,
    pub tls: TlsTrust,
    /// Private mode at start (it can change: `NetworkContext::set_private_mode`).
    pub private_mode: bool,
}

/// What building a context from a [`NetworkConfig`] found worth saying.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigReport {
    pub trust: TrustReport,
    pub issues: Vec<String>,
}

/// The routes and policy for `config`, over `store`.
pub(crate) fn build(store: &Arc<NetworkStore>, config: &NetworkConfig) -> Result<(Arc<dyn NetworkRoute>, ConfigReport), String> {
    let mut report = ConfigReport::default();
    let tls = if config.tls.is_default() {
        None
    } else {
        let (tls, trust) = config.tls.client_config();
        if trust.rejected > 0 {
            report.issues.push(format!("{} extra CA certificate(s) or file(s) could not be used", trust.rejected));
        }
        report.trust = trust;
        Some(tls)
    };
    let doh = match &config.doh {
        Some(doh) => Some(Arc::new(DohResolver::new(doh, tls.clone())?)),
        None => None,
    };
    let base = Transport { proxy: None, doh, tls, gate: None };
    let mut routes: BTreeMap<String, Arc<dyn NetworkRoute>> = BTreeMap::new();
    routes.insert(DIRECT.into(), Arc::new(DirectRoute::with_transport(Arc::clone(store), base.clone(), DIRECT)));
    for (name, proxy) in &config.proxies {
        if name == DIRECT || name == SYSTEM_VPN || name.is_empty() {
            return Err(format!("a proxy cannot be named {name:?}"));
        }
        routes.insert(name.clone(), Arc::new(ProxyRoute::new(Arc::clone(store), proxy, &base)?));
    }
    if let Some(probe) = &config.system_vpn {
        routes.insert(SYSTEM_VPN.into(), Arc::new(SystemVpnRoute::new(Arc::clone(store), Arc::clone(probe), &base)));
    }
    // Every route a rule names must exist — checked now, and again (failing
    // closed) per request.
    let named = config.rules.private_override.iter().chain(config.rules.profile_default.iter()).chain(config.rules.site_rules.iter().map(|(_, r)| r));
    for name in named {
        if !routes.contains_key(name) {
            return Err(format!("a route rule names {name:?}, which is not configured"));
        }
    }
    store.set_private_mode(config.private_mode);
    Ok((Arc::new(RoutePolicy::new(Arc::clone(store), routes, config.rules.clone())), report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Initiator;

    fn page(url: &str) -> RequestContext {
        RequestContext::subresource_by(Initiator::for_url(url))
    }

    /// Every combination of the three overrides (ADR 011 §1).
    #[test]
    fn precedence_holds_for_every_combination() {
        let url = "https://cdn.other.example/x.js";
        let ctx = page("https://news.example/article");
        for private_mode in [false, true] {
            for has_override in [false, true] {
                for has_rule in [false, true] {
                    for has_default in [false, true] {
                        let rules = RouteRules {
                            private_override: has_override.then(|| "tor".to_string()),
                            site_rules: if has_rule { vec![("news.example".into(), "work".into())] } else { Vec::new() },
                            profile_default: has_default.then(|| "home".to_string()),
                        };
                        let expected = if private_mode && has_override {
                            ("tor", RouteReason::PrivateOverride)
                        } else if has_rule {
                            ("work", RouteReason::SiteRule)
                        } else if has_default {
                            ("home", RouteReason::ProfileDefault)
                        } else {
                            (DIRECT, RouteReason::Direct)
                        };
                        let (route, reason) = rules.choose(private_mode, url, &ctx);
                        assert_eq!((route.as_str(), reason), expected, "private={private_mode} override={has_override} rule={has_rule} default={has_default}");
                    }
                }
            }
        }
    }

    #[test]
    fn site_rules_follow_the_top_level_site_and_the_most_specific_wins() {
        let rules = RouteRules {
            site_rules: vec![("example.com".into(), "wide".into()), ("shop.example.com".into(), "narrow".into())],
            ..RouteRules::default()
        };
        let choose = |url: &str, ctx: &RequestContext| rules.choose(false, url, ctx).0;
        // A subresource goes the way its page does, wherever it points.
        assert_eq!(choose("https://tracker.example.net/p.gif", &page("https://www.example.com/")), "wide");
        assert_eq!(choose("https://tracker.example.net/p.gif", &page("https://shop.example.com/cart")), "narrow");
        // A navigation by where it goes.
        assert_eq!(choose("https://shop.example.com/", &RequestContext::navigation_from("https://elsewhere.org/")), "narrow");
        assert_eq!(choose("https://example.com/", &RequestContext::typed_navigation()), "wide");
        // Not a suffix match on a label boundary: no rule.
        assert_eq!(choose("https://notexample.com/", &RequestContext::typed_navigation()), DIRECT);
        // An opaque page matches no site rule.
        let opaque = RequestContext::subresource_by(Initiator::document(origin::Origin::new_opaque(), None));
        assert_eq!(choose("https://example.com/x", &opaque), DIRECT);
    }

    #[test]
    fn proxies_are_checked_when_configured() {
        for good in ["http://127.0.0.1:3128", "socks5://127.0.0.1:9050", "socks4a://proxy.example:1080", "http://user:pw@proxy.example:8080"] {
            assert!(ProxyConfig { url: good.into() }.parse().is_ok(), "{good}");
        }
        for bad in ["https://proxy.example", "ftp://x", "proxy.example:8080", "", "socks5://"] {
            assert!(ProxyConfig { url: bad.into() }.parse().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_rule_naming_a_missing_route_is_refused_up_front() {
        let store = NetworkStore::new();
        let config = NetworkConfig { rules: RouteRules { profile_default: Some("nowhere".into()), ..RouteRules::default() }, ..NetworkConfig::default() };
        assert!(build(&store, &config).is_err());
        let config = NetworkConfig { proxies: vec![(DIRECT.into(), ProxyConfig { url: "http://127.0.0.1:1".into() })], ..NetworkConfig::default() };
        assert!(build(&store, &config).is_err(), "a proxy may not take a reserved name");
    }
}
