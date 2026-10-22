//! JavaScript, through Boa (pure Rust, no FFI), as one implementation of
//! the script runtime interface (Milestone 40,
//! `docs/adr/023-script-runtime-and-bindings.md`).
//!
//! Since Milestone 40 this crate is only the JavaScript engine: [`BoaRuntime`]
//! implements `script::ScriptRuntime`, building script objects from the
//! bindings `script` generates from WebIDL. The DOM operations, events,
//! forms, editing, `fetch`/XHR and storage — Milestones 11–39's bindings —
//! are runtime-independent and live in `script`. [`new_environment`] gives a
//! document's `script::ScriptEnvironment` with a Boa runtime;
//! [`JsEnvironment`] is the same, as a type this crate's tests and older
//! callers construct.
//!
//! ## GC handles
//!
//! No native function captures a GC handle: every one is a `Copy` closure
//! over the member it calls, and what the host refers to by id (listeners,
//! animation-frame callbacks, XHR handlers, pending promises) lives in a
//! traced table inside Boa's heap (ADR 023 §5). Until Milestone 40, four
//! untraced captures held handles and `JsEnvironment`'s `Drop` released them
//! by hand while the heap was alive (ADR 015, superseded);
//! `scripts/gc-handle-captures.txt` is now empty, and
//! `scripts/check-gc-handles.sh` fails any untraced field that could hold one.

#![forbid(unsafe_code)]

mod runtime;
// Convenience constructors for tests (a private network, a private store):
// minting authority, so never compiled into a production build
// (`scripts/check-capabilities.sh`; ADR 010 §2).
#[cfg(any(test, feature = "test-support"))]
mod test_support;
#[cfg(test)]
mod tests;

pub use runtime::BoaRuntime;
pub use script::{ScriptCapabilities, ScriptEnvironment};
pub use storage::MAX_STORAGE_BYTES;

use dom::Document;
use network::Initiator;
use std::cell::RefCell;
use std::rc::Rc;

/// Most iterations a single JS loop may run before the engine stops the
/// script (Boa's `RuntimeLimits`, default: unlimited — which let
/// `while(true){}` hang the UI thread forever). About 0.1 s of an empty
/// loop on Boa 0.22; no real page loops this long on the UI thread.
///
/// **What this does not bound:** Boa counts per loop *instance*, so nested
/// loops multiply (`N` × `N` inner iterations), and pure-compute work with
/// no loop (deep recursion is separately capped) is unbounded. Boa 0.22 has
/// no interrupt hook either; in process mode the browser ends a renderer
/// that stops finishing ticks (ADR 013), which is the real bound. This
/// closes the accidental and the trivial hostile case, not the adversarial
/// nested one.
pub const MAX_LOOP_ITERATIONS: u64 = 10_000_000;

/// Deepest JS call recursion (Boa's default is 400; pinned here so an
/// engine upgrade cannot silently raise it).
pub const MAX_RECURSION_DEPTH: usize = 400;

/// Boa's time hooks (Milestone 37, ADR 019 §3): every `Date` — `Date.now()`,
/// `new Date()`, every local-time getter, `toString`, `getTimezoneOffset` —
/// reads the clock and the timezone through these, so all of them see the
/// policy, not just the ones a script shim would patch. Since Boa 0.21 the
/// wall clock is a [`boa_engine::context::Clock`] and the timezone a
/// [`boa_engine::context::HostHooks`] offset (Milestone 38a).
struct PolicyHooks(privacy::FingerprintPolicy);

impl boa_engine::context::HostHooks for PolicyHooks {
    fn local_timezone_offset_seconds(&self, unix_time_seconds: i64) -> i32 {
        use chrono::{Offset, TimeZone};
        match self.0.timezone {
            privacy::TimezonePolicy::Real => chrono::DateTime::from_timestamp(unix_time_seconds, 0)
                .map_or(0, |utc| chrono::Local.offset_from_utc_datetime(&utc.naive_utc()).fix().local_minus_utc()),
            privacy::TimezonePolicy::Utc => 0,
        }
    }
}

/// The wall clock `Date` reads, at the policy's resolution. Boa's monotonic
/// clock (used only for its own job scheduling, not exposed to script) is
/// left as it is.
struct PolicyClock {
    policy: privacy::FingerprintPolicy,
    monotonic: boa_engine::context::time::StdClock,
}

impl boa_engine::context::Clock for PolicyClock {
    fn now(&self) -> boa_engine::context::time::JsInstant {
        self.monotonic.now()
    }

    fn system_time_millis(&self) -> i64 {
        self.policy.coarsen_epoch_ms(chrono::Utc::now().timestamp_millis())
    }
}

/// Collects this thread's JavaScript garbage now: what a page that has gone
/// (navigated away from, or closed) left in the heap is freed at once,
/// instead of whenever the next collection happens to run.
pub fn collect_garbage() {
    boa_gc::force_collect();
}

/// The script environment real page loads use (Milestone 36, ADR 010): the
/// document's identity `page` — its origin (possibly opaque) and URL — and
/// the capabilities its page was granted, with a Boa runtime. `document` is
/// the live tree, shared with the caller.
pub fn new_environment(document: Rc<RefCell<Document>>, page: Initiator, capabilities: ScriptCapabilities) -> ScriptEnvironment {
    let fingerprint = capabilities.fingerprint;
    ScriptEnvironment::new(document, page, capabilities, |host| Box::new(BoaRuntime::new(host, fingerprint)))
}

/// A [`ScriptEnvironment`] with a Boa runtime, built by this crate's
/// constructors; it dereferences to the environment.
pub struct JsEnvironment(ScriptEnvironment);

impl JsEnvironment {
    /// See [`new_environment`].
    pub fn new_with_capabilities(document: Rc<RefCell<Document>>, page: Initiator, capabilities: ScriptCapabilities) -> Self {
        JsEnvironment(new_environment(document, page, capabilities))
    }

    pub fn into_inner(self) -> ScriptEnvironment {
        self.0
    }
}

impl std::ops::Deref for JsEnvironment {
    type Target = ScriptEnvironment;

    fn deref(&self) -> &ScriptEnvironment {
        &self.0
    }
}

impl std::ops::DerefMut for JsEnvironment {
    fn deref_mut(&mut self) -> &mut ScriptEnvironment {
        &mut self.0
    }
}
