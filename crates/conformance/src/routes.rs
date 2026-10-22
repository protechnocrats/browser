//! The two network routes the harness plugs into the engine's
//! `NetworkContext` (architecture doc §6.1: a route is an implementation
//! detail the engine never sees): one that records what a live load
//! fetched, and one that serves a recording back.

use crate::snapshot::Recorded;
use network::{FetchResponse, NetworkError, NetworkRoute, NetworkStore, RequestContext};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Fetches for real (through the engine's own `DirectRoute`, so the same
/// requests, headers and cookies as a normal load) and keeps a copy of every
/// response.
pub struct RecordingRoute {
    inner: network::DirectRoute,
    pub log: Mutex<Vec<Recorded>>,
}

impl RecordingRoute {
    pub fn new(store: Arc<NetworkStore>) -> RecordingRoute {
        RecordingRoute { inner: network::DirectRoute::with_store(store), log: Mutex::new(Vec::new()) }
    }

    pub fn take(&self) -> Vec<Recorded> {
        std::mem::take(&mut *self.log.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl NetworkRoute for RecordingRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.fetch_with(url, &RequestContext::default())
    }

    fn fetch_with(&self, url: &str, ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        let result = self.inner.fetch_with(url, ctx);
        if let Ok(r) = &result {
            let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
            if !log.iter().any(|x| x.requested_url == url) {
                log.push(Recorded { requested_url: url.to_string(), status: r.status, final_url: r.final_url.clone(), headers: r.headers.clone(), body: r.body.clone() });
            }
        }
        result
    }
}

/// Serves a snapshot's responses by requested URL; anything not recorded is a
/// 404 (and remembered, for the report). Never touches the network.
pub struct ReplayRoute {
    by_url: HashMap<String, Recorded>,
    pub misses: Mutex<Vec<String>>,
}

impl ReplayRoute {
    pub fn new(responses: &[Recorded]) -> ReplayRoute {
        ReplayRoute { by_url: responses.iter().map(|r| (r.requested_url.clone(), r.clone())).collect(), misses: Mutex::new(Vec::new()) }
    }

    pub fn misses(&self) -> Vec<String> {
        self.misses.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl NetworkRoute for ReplayRoute {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        match self.by_url.get(url) {
            Some(r) => Ok(FetchResponse { status: r.status, final_url: r.final_url.clone(), headers: r.headers.clone(), body: r.body.clone() }),
            None => {
                self.misses.lock().unwrap_or_else(PoisonError::into_inner).push(url.to_string());
                Ok(FetchResponse { status: 404, final_url: url.to_string(), headers: Vec::new(), body: Vec::new() })
            }
        }
    }
}
