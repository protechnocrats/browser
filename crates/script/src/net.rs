//! `fetch()` and `XMLHttpRequest` (Milestone 12, Phase F): non-blocking —
//! each request runs on the network pool and its result is picked up by
//! [`ScriptEnvironment::poll_pending_fetches`] (woken by the page's notifier,
//! ADR 020) — and reaching the network only through the page's capability
//! (Milestone 36, ADR 010), with the same-origin policy for reads
//! (Milestone 28, ADR 004).

use crate::bindings::{HeadersImpl, Interface, RequestCredentials, RequestInit, ResponseImpl, XMLHttpRequestImpl};
use crate::environment::ScriptEnvironment;
use crate::host::{HostObject, HostState};
use crate::{CallbackId, HostError, ObjectRef, PromiseId, PromiseValue, Rejection, Value};
use network::{FetchResponse, Initiator, Origin, RequestContext};

/// The page's network side: what its requests are made with and what is in
/// flight.
pub(crate) struct NetState {
    /// The document these requests are made on behalf of (Milestone 28): its
    /// origin decides `SameSite` cookies, cache partition, credentials and
    /// what script may read; its URL is the `Referer` and the base for
    /// relative URLs.
    pub(crate) page: Initiator,
    /// Milestone 36: the page's network capability (`None`: never granted).
    pub(crate) network: Option<capability::NetworkCapability>,
    /// Milestone 38b (ADR 020): wakes the page when a response is ready.
    pub(crate) wake: Option<network::Notify>,
    fetches: Vec<PendingFetch>,
    xhrs: Vec<PendingXhr>,
}

impl NetState {
    pub(crate) fn new(page: Initiator, network: Option<capability::NetworkCapability>) -> NetState {
        NetState { page, network, wake: None, fetches: Vec::new(), xhrs: Vec::new() }
    }

    pub(crate) fn pending(&self) -> usize {
        self.fetches.len() + self.xhrs.len()
    }
}

struct PendingFetch {
    handle: network::FetchHandle,
    promise: PromiseId,
    /// The request as made: its initiator and credentials mode are what the
    /// CORS read check needs when the response arrives.
    request: RequestContext,
}

struct PendingXhr {
    handle: network::FetchHandle,
    /// The XHR object's key (pinned while in flight).
    xhr: u64,
    request: RequestContext,
}

pub(crate) struct ResponseData {
    status: u16,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
    /// The `Headers` object `headers` returned (`[SameObject]`), while it lives.
    headers_object: Option<u64>,
}

#[derive(Default)]
pub(crate) struct XhrData {
    method: String,
    url: String,
    request_headers: Vec<(String, String)>,
    response_headers: Vec<(String, String)>,
    status: u16,
    status_text: String,
    response_text: String,
    ready_state: u16,
    with_credentials: bool,
    /// `onreadystatechange`, `onload`, `onerror`.
    pub(crate) handlers: [Option<CallbackId>; 3],
}

/// The same-origin policy for script reads (the one rule in
/// `origin::cors_response_readable`, ADR 004), judged against the response's
/// final, post-redirect URL. A document with no URL has an opaque origin,
/// which reads cross-origin responses only under
/// `Access-Control-Allow-Origin: *`.
fn is_cross_origin_response_blocked(request: &RequestContext, response: &FetchResponse) -> bool {
    let Some(page) = &request.initiator else { return false };
    let response_origin = Origin::parse(&response.final_url);
    let readable = origin::cors_response_readable(
        &page.origin,
        &response_origin,
        request.credentials_apply_to(&response_origin),
        response.header("Access-Control-Allow-Origin"),
        response.header("Access-Control-Allow-Credentials"),
    );
    if !readable {
        diagnostics::guard(diagnostics::Guard::SopBlocked, "cross-origin response withheld from script (CORS does not allow this reader)");
    }
    !readable
}

/// A script request's URL, resolved against the page's URL (`fetch('/api')`
/// means the page's `/api`); a relative URL on a page with no URL is left as
/// written, and fails.
fn resolve_script_url(page: &Initiator, url: &str) -> String {
    page.url.as_deref().and_then(|base| network::resolve_url(base, url)).unwrap_or_else(|| url.to_string())
}

/// The network, through the page's capability, or the refusal as the
/// exception script sees (`NotAllowedError: …`). Checked on every request,
/// so a revocation takes effect at the next one.
fn network_of(network: &Option<capability::NetworkCapability>) -> Result<&network::NetworkContext, HostError> {
    let result = match network {
        Some(granted) => granted.context(),
        None => Err(capability::CapabilityError::report(capability::CapabilityKind::Network, capability::Denial::NotGranted)),
    };
    result.map_err(|e| HostError::error(e.to_string()))
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// `fetch(input, init)`: a pending promise, settled when the response
/// arrives. The default credentials mode is `same-origin`: a cross-origin
/// fetch sends and stores no cookies unless `{credentials: 'include'}`, and
/// then reading it needs an exact `Access-Control-Allow-Origin` plus
/// `Access-Control-Allow-Credentials: true`.
pub(crate) fn fetch(host: &mut HostState, input: &str, init: RequestInit) -> PromiseValue {
    // Milestone 36: no capability, no request — a capability error, and
    // nothing reaches the network.
    let route = match network_of(&host.net.network) {
        Ok(route) => route.clone(),
        Err(e) => return PromiseValue::Rejected(Box::new(Rejection::Error(e))),
    };
    let url = resolve_script_url(&host.net.page, input);
    let credentials = match init.credentials {
        Some(RequestCredentials::Include) => network::Credentials::Include,
        Some(RequestCredentials::Omit) => network::Credentials::Omit,
        Some(RequestCredentials::SameOrigin) | None => network::Credentials::SameOrigin,
    };
    let request = RequestContext::subresource_by(host.net.page.clone()).with_credentials(credentials).with_destination(network::Destination::XmlHttpRequest);
    let promise = host.new_promise();
    let handle = route.fetch_async_notify(&url, &request, host.net.wake.clone());
    host.net.fetches.push(PendingFetch { handle, promise, request });
    diagnostics::metrics::PENDING_FETCHES.set(host.net.pending() as u64);
    PromiseValue::Pending(promise)
}

impl HostState {
    fn response_data(&self, this: ObjectRef) -> Result<&ResponseData, HostError> {
        match self.object(this.key) {
            Some(HostObject::Response(r)) => Ok(r),
            _ => Err(HostError::type_error("the response is gone")),
        }
    }

    fn xhr(&self, this: ObjectRef) -> Result<&XhrData, HostError> {
        match self.object(this.key) {
            Some(HostObject::Xhr(x)) => Ok(x),
            _ => Err(HostError::type_error("the request is gone")),
        }
    }

    fn xhr_mut(&mut self, this: ObjectRef) -> Result<&mut XhrData, HostError> {
        match self.object_mut(this.key) {
            Some(HostObject::Xhr(x)) => Ok(x),
            _ => Err(HostError::type_error("the request is gone")),
        }
    }

    fn set_handler(&mut self, this: ObjectRef, index: usize, value: Option<CallbackId>) -> Result<(), HostError> {
        let old = std::mem::replace(&mut self.xhr_mut(this)?.handlers[index], value);
        self.release(old);
        Ok(())
    }
}

impl ResponseImpl for HostState {
    fn status(&mut self, this: ObjectRef) -> Result<u16, HostError> {
        Ok(self.response_data(this)?.status)
    }

    fn ok(&mut self, this: ObjectRef) -> Result<bool, HostError> {
        Ok((200..300).contains(&self.response_data(this)?.status))
    }

    fn status_text(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(status_text(self.response_data(this)?.status).to_string())
    }

    fn url(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(self.response_data(this)?.url.clone())
    }

    fn headers(&mut self, this: ObjectRef) -> Result<ObjectRef, HostError> {
        let (existing, headers) = {
            let r = self.response_data(this)?;
            (r.headers_object, r.headers.clone())
        };
        if let Some(key) = existing.filter(|k| matches!(self.object(*k), Some(HostObject::Headers(_)))) {
            return Ok(ObjectRef::new(Interface::Headers, key));
        }
        let key = self.new_object(HostObject::Headers(headers));
        if let Some(HostObject::Response(r)) = self.object_mut(this.key) {
            r.headers_object = Some(key);
        }
        Ok(ObjectRef::new(Interface::Headers, key))
    }

    fn text(&mut self, this: ObjectRef) -> Result<PromiseValue, HostError> {
        Ok(PromiseValue::Resolved(Box::new(Value::String(self.response_data(this)?.body.clone()))))
    }

    fn json(&mut self, this: ObjectRef) -> Result<PromiseValue, HostError> {
        Ok(PromiseValue::Resolved(Box::new(Value::Json(self.response_data(this)?.body.clone()))))
    }
}

impl HeadersImpl for HostState {
    fn get(&mut self, this: ObjectRef, name: String) -> Result<Option<String>, HostError> {
        match self.object(this.key) {
            Some(HostObject::Headers(h)) => Ok(h.iter().find(|(k, _)| k.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone())),
            _ => Err(HostError::type_error("the headers are gone")),
        }
    }

    fn has(&mut self, this: ObjectRef, name: String) -> Result<bool, HostError> {
        Ok(self.get(this, name)?.is_some())
    }
}

/// Phase F: `XMLHttpRequest` — `open()`, `setRequestHeader()`, `send()`, then
/// `status`/`statusText`/`responseText`/`readyState` (4 when done)/
/// `getResponseHeader()`, with `onreadystatechange` and `onload` fired on
/// completion and `onerror` on a revoked capability. Only GET is issued (the
/// `network` crate is GET-only; `method` is recorded); no progress, no
/// `abort()`. Credentials follow `withCredentials` (read at `send()`).
impl XMLHttpRequestImpl for HostState {
    fn constructor(&mut self) -> Result<ObjectRef, HostError> {
        Ok(ObjectRef::new(Interface::XMLHttpRequest, self.new_object(HostObject::Xhr(XhrData::default()))))
    }

    fn onreadystatechange(&mut self, this: ObjectRef) -> Result<Option<CallbackId>, HostError> {
        Ok(self.xhr(this)?.handlers[0])
    }

    fn set_onreadystatechange(&mut self, this: ObjectRef, value: Option<CallbackId>) -> Result<(), HostError> {
        self.set_handler(this, 0, value)
    }

    fn onload(&mut self, this: ObjectRef) -> Result<Option<CallbackId>, HostError> {
        Ok(self.xhr(this)?.handlers[1])
    }

    fn set_onload(&mut self, this: ObjectRef, value: Option<CallbackId>) -> Result<(), HostError> {
        self.set_handler(this, 1, value)
    }

    fn onerror(&mut self, this: ObjectRef) -> Result<Option<CallbackId>, HostError> {
        Ok(self.xhr(this)?.handlers[2])
    }

    fn set_onerror(&mut self, this: ObjectRef, value: Option<CallbackId>) -> Result<(), HostError> {
        self.set_handler(this, 2, value)
    }

    fn ready_state(&mut self, this: ObjectRef) -> Result<u16, HostError> {
        Ok(self.xhr(this)?.ready_state)
    }

    fn with_credentials(&mut self, this: ObjectRef) -> Result<bool, HostError> {
        Ok(self.xhr(this)?.with_credentials)
    }

    fn set_with_credentials(&mut self, this: ObjectRef, value: bool) -> Result<(), HostError> {
        self.xhr_mut(this)?.with_credentials = value;
        Ok(())
    }

    fn status(&mut self, this: ObjectRef) -> Result<u16, HostError> {
        Ok(self.xhr(this)?.status)
    }

    fn status_text(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(self.xhr(this)?.status_text.clone())
    }

    fn response_text(&mut self, this: ObjectRef) -> Result<String, HostError> {
        Ok(self.xhr(this)?.response_text.clone())
    }

    fn response(&mut self, this: ObjectRef) -> Result<String, HostError> {
        self.response_text(this)
    }

    fn open(&mut self, this: ObjectRef, method: String, url: String) -> Result<(), HostError> {
        let xhr = self.xhr_mut(this)?;
        xhr.method = method;
        xhr.url = url;
        xhr.ready_state = 1;
        Ok(())
    }

    fn set_request_header(&mut self, this: ObjectRef, name: String, value: String) -> Result<(), HostError> {
        if !name.is_empty() {
            self.xhr_mut(this)?.request_headers.push((name, value));
        }
        Ok(())
    }

    /// Dispatches on the network pool and returns at once (it used to block
    /// the page for the whole round trip).
    fn send(&mut self, this: ObjectRef) -> Result<(), HostError> {
        // Milestone 36: without the capability, `send()` throws and nothing
        // is requested.
        let route = network_of(&self.net.network)?.clone();
        let (url, with_credentials) = {
            let xhr = self.xhr(this)?;
            (resolve_script_url(&self.net.page, &xhr.url), xhr.with_credentials)
        };
        let credentials = if with_credentials { network::Credentials::Include } else { network::Credentials::SameOrigin };
        let request = RequestContext::subresource_by(self.net.page.clone()).with_credentials(credentials).with_destination(network::Destination::XmlHttpRequest);
        let handle = route.fetch_async_notify(&url, &request, self.net.wake.clone());
        self.pin(this.key);
        self.net.xhrs.push(PendingXhr { handle, xhr: this.key, request });
        Ok(())
    }

    fn get_response_header(&mut self, this: ObjectRef, name: String) -> Result<Option<String>, HostError> {
        Ok(self.xhr(this)?.response_headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone()))
    }
}

impl ScriptEnvironment {
    /// Settles every script request whose result has arrived: a `fetch`
    /// promise resolves with a `Response` or rejects; an XHR gets its status
    /// and text and its handlers run. What is still in flight stays.
    pub fn poll_pending_fetches(&mut self) {
        self.begin();
        // Milestone 36: a revoked capability also refuses what was already in
        // flight — script never sees those responses.
        let revoked = {
            let host = self.host.borrow();
            host.net.network.as_ref().is_some_and(|n| n.is_revoked()) && host.net.pending() > 0
        };
        if revoked {
            let error = capability::CapabilityError::report(capability::CapabilityKind::Network, capability::Denial::Revoked);
            let (fetches, xhrs) = {
                let mut host = self.host.borrow_mut();
                (std::mem::take(&mut host.net.fetches), std::mem::take(&mut host.net.xhrs))
            };
            for pending in fetches {
                self.runtime.settle(pending.promise, Err(Rejection::Error(HostError::error(error.to_string()))));
            }
            for pending in xhrs {
                self.finish_xhr(pending.xhr, 0, String::new(), error.to_string(), Vec::new(), &[2]);
            }
            self.run_jobs();
        }

        let ready: Vec<(PendingFetch, Result<FetchResponse, network::NetworkError>)> = {
            let mut host = self.host.borrow_mut();
            let mut ready = Vec::new();
            let mut still = Vec::new();
            for pending in std::mem::take(&mut host.net.fetches) {
                match pending.handle.try_recv() {
                    Some(result) => ready.push((pending, result)),
                    None => still.push(pending),
                }
            }
            host.net.fetches = still;
            ready
        };
        for (pending, result) in ready {
            let settlement = match result {
                // A blocked cross-origin response is a rejected promise (a
                // `TypeError` in browsers), never a response with an error
                // status.
                Ok(response) if is_cross_origin_response_blocked(&pending.request, &response) => Err(Rejection::Value(Value::String(
                    "TypeError: Failed to fetch: cross-origin response blocked by same-origin policy (no matching Access-Control-Allow-Origin)".into(),
                ))),
                Ok(response) => {
                    let data = ResponseData { status: response.status, url: response.final_url.clone(), headers: response.headers.clone(), body: response.body_as_string(), headers_object: None };
                    let key = self.host.borrow_mut().new_object(HostObject::Response(data));
                    Ok(Value::Object(ObjectRef::new(Interface::Response, key)))
                }
                Err(e) => Err(Rejection::Value(Value::String(e.to_string()))),
            };
            // Held while the runtime wraps it; freed at once if it does not.
            let response = match &settlement {
                Ok(Value::Object(o)) => Some(o.key),
                _ => None,
            };
            if let Some(key) = response {
                self.host.borrow_mut().pin(key);
            }
            self.runtime.settle(pending.promise, settlement);
            if let Some(key) = response {
                self.host.borrow_mut().unpin(key);
            }
        }

        let ready_xhrs: Vec<(PendingXhr, Result<FetchResponse, network::NetworkError>)> = {
            let mut host = self.host.borrow_mut();
            let mut ready = Vec::new();
            let mut still = Vec::new();
            for pending in std::mem::take(&mut host.net.xhrs) {
                match pending.handle.try_recv() {
                    Some(result) => ready.push((pending, result)),
                    None => still.push(pending),
                }
            }
            host.net.xhrs = still;
            ready
        };
        for (pending, result) in ready_xhrs {
            let (status, text, headers) = match result {
                Ok(r) if is_cross_origin_response_blocked(&pending.request, &r) => (0, "network error: blocked by same-origin policy".to_string(), Vec::new()),
                Ok(r) => (r.status, r.body_as_string(), r.headers.clone()),
                Err(e) => (0, format!("network error: {e}"), Vec::new()),
            };
            self.finish_xhr(pending.xhr, status, status_text(status).to_string(), text, headers, &[0, 1]);
        }

        let pending = self.host.borrow().net.pending();
        diagnostics::metrics::PENDING_FETCHES.set(pending as u64);
        self.run_jobs();
    }

    /// Completes an XHR (readyState 4) and runs the handlers `handlers`
    /// (indices into `XhrData::handlers`), with the XHR as `this`.
    fn finish_xhr(&mut self, key: u64, status: u16, status_text: String, text: String, headers: Vec<(String, String)>, handlers: &[usize]) {
        let callbacks: Vec<CallbackId> = {
            let mut host = self.host.borrow_mut();
            match host.object_mut(key) {
                Some(HostObject::Xhr(xhr)) => {
                    xhr.status = status;
                    xhr.status_text = status_text;
                    xhr.response_text = text;
                    xhr.response_headers = headers;
                    xhr.ready_state = 4;
                    handlers.iter().filter_map(|&i| xhr.handlers[i]).collect()
                }
                _ => Vec::new(),
            }
        };
        let this = Value::Object(ObjectRef::new(Interface::XMLHttpRequest, key));
        for callback in callbacks {
            if let Err(e) = self.runtime.call(callback, &this, &[]) {
                crate::report_script_error("xhr_callback", &e);
            }
        }
        self.host.borrow_mut().unpin(key);
        self.sync();
    }

    pub fn has_pending_fetches(&self) -> bool {
        self.host.borrow().net.pending() > 0
    }
}
