//! Phase F: a minimal HTTP response cache — freshness (via
//! `Cache-Control: max-age` / `Expires`) plus conditional
//! revalidation (`ETag` -> `If-None-Match`, `Last-Modified` ->
//! `If-Modified-Since`, `304 Not Modified` handling).
//!
//! Deliberately small and in-memory: no disk persistence, no
//! `Vary`, no `stale-while-revalidate`, no heuristic freshness, no
//! `private`/`public`/`s-maxage` distinctions, GET only. It covers the
//! case that actually matters for repeated same-session fetches of the
//! same asset (a stylesheet, an image, a script) — see Phase F in
//! `milestones/milestones-01-17.md` for what's deferred.

use crate::FetchResponse;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Entry {
    response: FetchResponse,
    stored_at: Instant,
    /// From `Cache-Control: max-age` (preferred) or `Expires`.
    fresh_for: Option<Duration>,
    etag: Option<String>,
    last_modified: Option<String>,
    /// `Cache-Control: no-cache` — store it, but always revalidate
    /// before use.
    must_revalidate: bool,
}

impl Entry {
    fn is_fresh(&self) -> bool {
        if self.must_revalidate {
            return false;
        }
        match self.fresh_for {
            Some(d) => self.stored_at.elapsed() < d,
            None => false,
        }
    }
}

#[derive(Default)]
pub struct HttpCache {
    entries: HashMap<String, Entry>,
    /// Test/diagnostic counters.
    pub hits: u64,
    pub revalidations: u64,
    pub stores: u64,
}

/// What `HttpCache::before_request` tells the caller to do.
pub enum CacheDecision {
    /// Serve this straight from cache — don't hit the network at all.
    Fresh(FetchResponse),
    /// Go to the network; add these conditional headers if non-empty.
    Revalidate(Vec<(String, String)>),
    /// Nothing usable cached — plain request.
    Miss,
}

impl HttpCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consulted before issuing a request for `url`.
    pub fn before_request(&mut self, url: &str) -> CacheDecision {
        let Some(entry) = self.entries.get(url) else {
            return CacheDecision::Miss;
        };
        if entry.is_fresh() {
            self.hits += 1;
            return CacheDecision::Fresh(entry.response.clone());
        }
        let mut conditional = Vec::new();
        if let Some(etag) = &entry.etag {
            conditional.push(("If-None-Match".to_string(), etag.clone()));
        }
        if let Some(lm) = &entry.last_modified {
            conditional.push(("If-Modified-Since".to_string(), lm.clone()));
        }
        if conditional.is_empty() {
            CacheDecision::Miss
        } else {
            self.revalidations += 1;
            CacheDecision::Revalidate(conditional)
        }
    }

    /// Called with the network's response. On `304`, returns the
    /// refreshed cached response to serve instead. On a cacheable
    /// `200`, stores it. Returns `None` when the caller should just use
    /// `response` as-is.
    pub fn after_response(&mut self, url: &str, response: &FetchResponse) -> Option<FetchResponse> {
        if response.status == 304 {
            if let Some(entry) = self.entries.get_mut(url) {
                entry.stored_at = Instant::now();
                // Merge any fresher caching headers from the 304.
                apply_caching_headers(entry, response);
                return Some(entry.response.clone());
            }
            return None;
        }

        if response.status == 200 && is_cacheable(response) {
            let mut entry = Entry {
                response: response.clone(),
                stored_at: Instant::now(),
                fresh_for: None,
                etag: None,
                last_modified: None,
                must_revalidate: false,
            };
            apply_caching_headers(&mut entry, response);
            if entry.fresh_for.is_some()
                || entry.etag.is_some()
                || entry.last_modified.is_some()
                || entry.must_revalidate
            {
                self.entries.insert(url.to_string(), entry);
                self.stores += 1;
            }
        } else if response.status == 200 {
            // A fresh 200 for something we had cached but that is now
            // explicitly uncacheable — drop the stale entry.
            self.entries.remove(url);
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn header<'a>(resp: &'a FetchResponse, name: &str) -> Option<&'a str> {
    resp.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn is_cacheable(resp: &FetchResponse) -> bool {
    match header(resp, "cache-control") {
        Some(cc) => {
            let cc = cc.to_ascii_lowercase();
            !cc.contains("no-store") && !cc.contains("private")
        }
        None => true,
    }
}

fn apply_caching_headers(entry: &mut Entry, resp: &FetchResponse) {
    if let Some(cc) = header(resp, "cache-control") {
        let cc = cc.to_ascii_lowercase();
        if cc.contains("no-cache") {
            entry.must_revalidate = true;
        }
        if let Some(rest) = cc.split("max-age=").nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(secs) = digits.parse::<u64>() {
                entry.fresh_for = Some(Duration::from_secs(secs));
                entry.must_revalidate = entry.must_revalidate || secs == 0;
            }
        }
    }
    if let Some(etag) = header(resp, "etag") {
        entry.etag = Some(etag.to_string());
    }
    if let Some(lm) = header(resp, "last-modified") {
        entry.last_modified = Some(lm.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, headers: &[(&str, &str)]) -> FetchResponse {
        FetchResponse {
            status,
            final_url: "http://x/".into(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: b"body".to_vec(),
        }
    }

    #[test]
    fn fresh_entry_is_served_without_a_network_request() {
        let mut c = HttpCache::new();
        c.after_response("u", &resp(200, &[("Cache-Control", "max-age=300")]));
        assert!(matches!(c.before_request("u"), CacheDecision::Fresh(_)));
        assert_eq!(c.hits, 1);
    }

    #[test]
    fn stale_entry_with_etag_triggers_revalidation() {
        let mut c = HttpCache::new();
        c.after_response("u", &resp(200, &[("Cache-Control", "max-age=0"), ("ETag", "\"abc\"")]));
        match c.before_request("u") {
            CacheDecision::Revalidate(h) => {
                assert_eq!(h, vec![("If-None-Match".to_string(), "\"abc\"".to_string())]);
            }
            _ => panic!("expected revalidation"),
        }
    }

    #[test]
    fn a_304_serves_the_stored_body() {
        let mut c = HttpCache::new();
        let mut original = resp(200, &[("Cache-Control", "max-age=0"), ("ETag", "\"abc\"")]);
        original.body = b"cached body".to_vec();
        c.after_response("u", &original);
        let _ = c.before_request("u");
        let served = c.after_response("u", &resp(304, &[])).expect("304 should yield the cached response");
        assert_eq!(served.body, b"cached body");
        assert_eq!(served.status, 200);
    }

    #[test]
    fn no_store_is_never_cached() {
        let mut c = HttpCache::new();
        c.after_response("u", &resp(200, &[("Cache-Control", "no-store")]));
        assert!(c.is_empty());
        assert!(matches!(c.before_request("u"), CacheDecision::Miss));
    }

    #[test]
    fn last_modified_drives_if_modified_since() {
        let mut c = HttpCache::new();
        c.after_response("u", &resp(200, &[("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT")]));
        match c.before_request("u") {
            CacheDecision::Revalidate(h) => assert_eq!(h[0].0, "If-Modified-Since"),
            _ => panic!("expected revalidation via Last-Modified"),
        }
    }
}
