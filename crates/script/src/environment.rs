//! [`ScriptEnvironment`]: one document's script, as the engine drives it —
//! a [`ScriptRuntime`] over the document's [`HostState`] (ADR 023 §1–2).

use crate::host::{HostState, StorageSource};
use crate::{net, report_script_error, HostRef, InterruptHandle, ScriptRuntime};
use dom::Document;
use network::{Initiator, Origin};
use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// What one document's script may reach (Milestone 36, ADR 010): the
/// handles its page was granted, already narrowed to this document — the
/// storage handle reaches this document's own storage key and no other.
/// `ScriptCapabilities::default()` grants nothing.
#[derive(Debug, Clone, Default)]
pub struct ScriptCapabilities {
    pub network: Option<capability::NetworkCapability>,
    pub local_storage: Option<capability::OriginStorage>,
    /// Milestone 37 — not authority but the limit on what script observes:
    /// the clock's resolution and the timezone `Date` reports (the
    /// runtime's), and the rAF timestamps' resolution (ADR 019 §3).
    pub fingerprint: privacy::FingerprintPolicy,
}

pub struct ScriptEnvironment {
    pub(crate) runtime: Box<dyn ScriptRuntime>,
    pub(crate) host: HostRef,
    /// Milestone 28: this document's origin and URL. A document with no URL
    /// has a fresh opaque origin, never "no origin".
    page: Initiator,
    /// The live document, shared with the engine (which lays it out) —
    /// Milestone 13's mutation story rests on this being the same tree
    /// script changes, not a copy.
    pub(crate) document: Rc<RefCell<Document>>,
    /// The focused element's `id` (Milestone 16).
    pub(crate) focused_id: Option<String>,
    /// Milestone 16: the caret per text field (a character index), by `id`.
    pub(crate) cursor_positions: HashMap<String, usize>,
    /// Milestone 21: a navigation a click or submission requested and no
    /// listener prevented — `(action, query string)`, taken once per tick.
    pub(crate) pending_navigation: Option<(String, String)>,
    fingerprint: privacy::FingerprintPolicy,
    interrupt: Option<InterruptHandle>,
}

impl ScriptEnvironment {
    /// The environment of the document `page` identifies, holding what
    /// `capabilities` grant (Milestone 36, ADR 010), with the runtime
    /// `runtime` builds over its host state.
    ///
    /// Without a network capability `fetch()` rejects and
    /// `XMLHttpRequest.send()` throws before anything reaches the network;
    /// without storage, `localStorage` throws `SecurityError`. A storage
    /// handle scoped to another storage key than this document's is refused
    /// the same way (the engine never passes one; defence in depth).
    /// `sessionStorage` is always this page's own memory.
    pub fn new(
        document: Rc<RefCell<Document>>,
        page: Initiator,
        capabilities: ScriptCapabilities,
        runtime: impl FnOnce(HostRef) -> Box<dyn ScriptRuntime>,
    ) -> ScriptEnvironment {
        let ScriptCapabilities { network, local_storage, fingerprint } = capabilities;
        let storage_key = origin::StorageKey::first_party(&page.origin);
        let local_storage = match local_storage {
            Some(granted) if granted.key() == &storage_key => StorageSource::Granted(granted),
            Some(_) => {
                capability::CapabilityError::report(capability::CapabilityKind::Storage, capability::Denial::OutOfScope);
                StorageSource::Denied(capability::Denial::OutOfScope)
            }
            None => StorageSource::Denied(capability::Denial::NotGranted),
        };
        let host = Rc::new(RefCell::new(HostState::new(Rc::clone(&document), local_storage, net::NetState::new(page.clone(), network))));
        let runtime = runtime(Rc::clone(&host));
        let interrupt = runtime.interrupt_handle();
        ScriptEnvironment {
            runtime,
            host,
            page,
            document,
            focused_id: None,
            cursor_positions: HashMap::new(),
            pending_navigation: None,
            fingerprint,
            interrupt,
        }
    }

    /// M36's gap: grants this document's script the network, `localStorage`
    /// or both again (ADR 010 §4). A storage handle scoped to another key
    /// than this document's is refused, as at creation. Kinds not given are
    /// left as they are.
    pub fn grant(&mut self, network: Option<capability::NetworkCapability>, local_storage: Option<capability::OriginStorage>) {
        let mut host = self.host.borrow_mut();
        if let Some(network) = network {
            host.net.network = Some(network);
        }
        if let Some(granted) = local_storage {
            if granted.key() == &origin::StorageKey::first_party(&self.page.origin) {
                host.set_local_storage(granted);
            } else {
                capability::CapabilityError::report(capability::CapabilityKind::Storage, capability::Denial::OutOfScope);
            }
        }
    }

    /// Milestone 38b (ADR 020): what script requests call when their
    /// response is ready — the page's wake-up. Without one, the host must
    /// poll [`Self::has_pending_fetches`].
    pub fn set_wake(&self, notify: network::Notify) {
        self.host.borrow_mut().net.wake = Some(notify);
    }

    /// This document's origin (opaque when it has no URL).
    pub fn origin(&self) -> &Origin {
        &self.page.origin
    }

    /// This document as a request initiator: its origin and URL. What a
    /// navigation it triggers carries.
    pub fn page(&self) -> &Initiator {
        &self.page
    }

    /// The host state (tests and diagnostics read its counters).
    pub fn host(&self) -> Ref<'_, HostState> {
        self.host.borrow()
    }

    /// Stops this environment's running script from another thread, if its
    /// runtime can be interrupted (ADR 023 §6); `None` for Boa 0.22.
    pub fn interrupt_handle(&self) -> Option<InterruptHandle> {
        self.interrupt.clone()
    }

    /// Before any work at an entry point: frees what collected wrappers left.
    pub(crate) fn begin(&mut self) {
        self.host.borrow_mut().collect_closed();
    }

    /// After running script: the runtime forgets callbacks the host dropped.
    pub(crate) fn sync(&mut self) {
        let released = self.host.borrow_mut().take_released();
        if !released.is_empty() {
            self.runtime.release(&released);
        }
    }

    /// Runs the queued promise jobs; a job that fails is reported like any
    /// script error, never dropped.
    pub(crate) fn run_jobs(&mut self) {
        if let Err(e) = self.runtime.run_jobs() {
            report_script_error("promise job", &e);
        }
        self.sync();
    }

    /// Runs one `<script>`'s source (synchronous, in the order the caller
    /// chooses — Milestone 11), then the promise jobs it queued, so a
    /// `fetch(...).then(...)` chain's ready steps run within the call.
    /// `Ok` is the script's completion value as a string.
    pub fn run_script(&mut self, source: &str) -> Result<String, String> {
        self.begin();
        let result = self.runtime.evaluate(source);
        self.sync();
        self.run_jobs();
        result.map_err(|e| {
            report_script_error("run_script", &e);
            e.message
        })
    }

    /// Milestone 13: has script changed the document since this was last
    /// called? Consuming: the engine asks once per tick.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.host.borrow_mut().dirty)
    }

    /// The focused element's `id`, if any (focus rings, the caret).
    pub fn focused_element_id(&self) -> Option<String> {
        self.focused_id.clone()
    }

    /// Sets focus directly (tab order, click-to-focus on a form control).
    pub fn set_focus(&mut self, id: Option<String>) {
        self.focused_id = id;
    }

    /// The live document script changes, shared with the engine.
    pub fn document(&self) -> Rc<RefCell<Document>> {
        Rc::clone(&self.document)
    }

    /// Milestone 21: takes the navigation a click or submission requested
    /// — `(action, query string)`; the engine resolves `action` against the
    /// page's URL.
    pub fn take_pending_navigation(&mut self) -> Option<(String, String)> {
        self.pending_navigation.take()
    }

    /// Milestone 14: runs every pending `requestAnimationFrame` callback
    /// once, in registration order. A callback registered during this runs
    /// on the next frame: the list is taken before any runs.
    pub fn run_animation_frame_callbacks(&mut self, timestamp_ms: f64) {
        self.begin();
        // Milestone 37: at the fingerprint policy's resolution (ADR 019 §3).
        let timestamp_ms = self.fingerprint.coarsen_ms(timestamp_ms);
        let callbacks = std::mem::take(&mut self.host.borrow_mut().animation_frames);
        for &callback in &callbacks {
            if let Err(e) = self.runtime.call(callback, &crate::Value::Undefined, &[crate::Value::Number(timestamp_ms)]) {
                report_script_error("animation_frame", &e);
            }
        }
        self.runtime.release(&callbacks);
        self.sync();
        self.run_jobs();
    }

    pub fn has_pending_animation_frame_callbacks(&self) -> bool {
        !self.host.borrow().animation_frames.is_empty()
    }

    /// Collects the runtime's garbage now.
    pub fn collect_garbage(&mut self) {
        self.runtime.collect_garbage();
        self.begin();
    }
}
