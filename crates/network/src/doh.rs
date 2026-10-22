//! DNS over HTTPS (Milestone 37, ADR 012): a resolver the user names, used
//! strictly — no fallback to system DNS — by every route that connects
//! directly.

use crate::dns::{self, QueryType};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Longest an answer is kept, whatever its TTL.
pub const MAX_CACHE_TTL: Duration = Duration::from_secs(300);
/// Most names kept.
pub const MAX_CACHE_ENTRIES: usize = 1024;
/// Largest DoH response read.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// Which resolver to ask. There is no default (ADR 012 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DohConfig {
    /// The RFC 8484 endpoint, e.g. `https://dns.example/dns-query`.
    pub resolver_url: String,
    /// Addresses for the resolver's own host, so that name is not looked up
    /// in system DNS (not needed when the URL's host is an IP address).
    pub bootstrap: Vec<IpAddr>,
}

impl DohConfig {
    /// Checks the endpoint: `https`, or plain `http` on a loopback address
    /// only (a local resolver, tests).
    pub fn validate(&self) -> Result<url::Url, String> {
        let url = url::Url::parse(&self.resolver_url).map_err(|e| format!("DoH resolver URL: {e}"))?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d == "localhost",
            None => return Err("DoH resolver URL has no host".into()),
        };
        match url.scheme() {
            "https" => Ok(url),
            "http" if loopback => Ok(url),
            _ => Err("a DoH resolver must use https (plain http only on a loopback address)".into()),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Resolves names through one DoH endpoint, caching answers for their TTL.
pub struct DohResolver {
    endpoint: url::Url,
    agent: ureq::Agent,
    cache: Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>,
}

impl std::fmt::Debug for DohResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DohResolver").field("endpoint", &self.endpoint.as_str()).finish()
    }
}

/// `host:port` split, with brackets taken off an IPv6 literal.
fn split_netloc(netloc: &str) -> Option<(&str, u16)> {
    let (host, port) = netloc.rsplit_once(':')?;
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    Some((host, port.parse().ok()?))
}

impl DohResolver {
    /// A resolver for `config`, whose TLS trusts `tls` (the profile's trust
    /// store; `None` for `ureq`'s default).
    pub fn new(config: &DohConfig, tls: Option<Arc<rustls::ClientConfig>>) -> Result<DohResolver, String> {
        let endpoint = config.validate()?;
        let resolver_host = endpoint.host_str().unwrap_or("").to_string();
        let bootstrap = config.bootstrap.clone();
        let mut builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirects(0)
            // The resolver's own name: its bootstrap addresses if given;
            // else the system resolves it (the one name that leaks).
            .resolver(move |netloc: &str| -> std::io::Result<Vec<SocketAddr>> {
                match split_netloc(netloc) {
                    Some((host, port)) if host == resolver_host && !bootstrap.is_empty() => {
                        Ok(bootstrap.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
                    }
                    _ => netloc.to_socket_addrs().map(Iterator::collect),
                }
            });
        if let Some(tls) = tls {
            builder = builder.tls_config(tls);
        }
        Ok(DohResolver { endpoint, agent: builder.build(), cache: Mutex::default() })
    }

    /// One query: the addresses and their TTL.
    fn query(&self, host: &str, qtype: QueryType) -> Result<dns::Answer, String> {
        let message = dns::encode_query(host, qtype).map_err(|e| e.to_string())?;
        let mut url = self.endpoint.clone();
        url.query_pairs_mut().append_pair("dns", &dns::base64url(&message));
        let response = self
            .agent
            .get(url.as_str())
            .set("Accept", "application/dns-message")
            .call()
            .map_err(|e| format!("the resolver did not answer: {e}"))?;
        let mut body = Vec::new();
        response.into_reader().take(MAX_RESPONSE_BYTES).read_to_end(&mut body).map_err(|e| e.to_string())?;
        dns::decode_answers(&body, host, qtype).map_err(|e| e.to_string())
    }

    /// The addresses of `host`, from the cache or the resolver.
    pub fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        if host == "localhost" {
            return Ok(vec![IpAddr::from([127, 0, 0, 1]), IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)]);
        }
        let now = Instant::now();
        if let Some((addresses, expires)) = lock(&self.cache).get(&host) {
            if *expires > now {
                return Ok(addresses.clone());
            }
        }
        let mut addresses = Vec::new();
        let mut ttl = u32::MAX;
        let mut errors = Vec::new();
        for qtype in [QueryType::A, QueryType::Aaaa] {
            match self.query(&host, qtype) {
                Ok(answer) => {
                    if !answer.addresses.is_empty() {
                        ttl = ttl.min(answer.ttl);
                    }
                    addresses.extend(answer.addresses);
                }
                Err(e) => errors.push(e),
            }
        }
        if addresses.is_empty() {
            diagnostics::guard(diagnostics::Guard::DohFailed, "DNS over HTTPS could not resolve a name; the request failed (no fallback)");
            let why = errors.into_iter().next().unwrap_or_else(|| "no such name".to_string());
            return Err(format!("DoH: could not resolve {host}: {why}"));
        }
        let keep = Duration::from_secs(u64::from(ttl)).min(MAX_CACHE_TTL);
        let mut cache = lock(&self.cache);
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, (_, expires)| *expires > now);
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.clear();
            }
        }
        if !keep.is_zero() {
            cache.insert(host, (addresses.clone(), now + keep));
        }
        Ok(addresses)
    }

    /// Names in the cache (tests).
    pub fn cached(&self) -> usize {
        lock(&self.cache).len()
    }
}

/// The transport's resolver hook (ADR 012 §6).
#[derive(Clone)]
pub(crate) struct DohHook(pub Arc<DohResolver>);

impl ureq::Resolver for DohHook {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let (host, port) = split_netloc(netloc).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad host:port"))?;
        self.0
            .lookup(host)
            .map(|ips| ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_must_be_https_except_on_loopback() {
        let config = |url: &str| DohConfig { resolver_url: url.into(), bootstrap: Vec::new() };
        assert!(config("https://dns.example/dns-query").validate().is_ok());
        assert!(config("http://127.0.0.1:8053/dns-query").validate().is_ok());
        assert!(config("http://[::1]/dns-query").validate().is_ok());
        assert!(config("http://localhost/dns-query").validate().is_ok());
        assert!(config("http://dns.example/dns-query").validate().is_err());
        assert!(config("ftp://dns.example/").validate().is_err());
        assert!(config("not a url").validate().is_err());
    }

    #[test]
    fn netlocs_split_including_ipv6() {
        assert_eq!(split_netloc("example.com:443"), Some(("example.com", 443)));
        assert_eq!(split_netloc("[::1]:8080"), Some(("::1", 8080)));
        assert_eq!(split_netloc("example.com"), None);
    }
}
