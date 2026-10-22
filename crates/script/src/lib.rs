//! The script runtime interface, and the host side of it (Milestone 40,
//! `docs/adr/023-script-runtime-and-bindings.md`).
//!
//! JavaScript is a replaceable subsystem (architecture doc §2): a page's
//! script runs in a [`ScriptRuntime`] — Boa, through the `js` crate, or the
//! stub in [`stub`] for tests — and everything else lives here, written
//! against plain [`Value`]s:
//!
//! - [`bindings`]: generated from `webidl/*.webidl` by `build.rs`. One trait
//!   per interface, a checked [`bindings::dispatch`], and the member table a
//!   runtime builds its objects from. An API not declared there does not
//!   exist for script.
//! - [`HostState`] implements those traits: the DOM operations (nodes named
//!   by `dom::NodeId`, ADR 021), the detached forest, storage, `fetch` and
//!   `XMLHttpRequest` (through the page's capabilities, ADR 010), events.
//! - [`ScriptEnvironment`] is what the engine holds per document: the
//!   runtime, the host state, and the engine-facing operations — dispatching
//!   clicks, keys and submissions with their default actions, native text
//!   editing, focus, animation frames, settling network requests.
//!
//! A runtime names host objects by [`ObjectRef`], callbacks by
//! [`CallbackId`] and pending promises by [`PromiseId`]; it keeps the script
//! objects those name in its own garbage-collected, traced tables (ADR 023
//! §5). The host holds no handle into any runtime's heap.

#![forbid(unsafe_code)]

pub mod bindings;
mod detached;
mod environment;
mod events;
mod host;
mod net;
#[cfg(any(test, feature = "test-runtime"))]
pub mod cases;
#[cfg(any(test, feature = "test-runtime"))]
pub mod stub;
#[cfg(test)]
mod tests;

pub use bindings::{Call, Interface};
pub use environment::{ScriptCapabilities, ScriptEnvironment};
pub use host::{HostRef, HostState, WrapperGuard, DOCUMENT, WINDOW};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A host object as script sees it: which interface, and which instance —
/// for a node its `dom::NodeId`, for other objects a key the host chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectRef {
    pub interface: Interface,
    pub key: u64,
}

impl ObjectRef {
    pub fn new(interface: Interface, key: u64) -> ObjectRef {
        ObjectRef { interface, key }
    }
}

/// A script function the host keeps (a listener, a rAF callback, an XHR
/// handler), named by the runtime that holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallbackId(pub u32);

/// A pending promise the host will settle, named by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PromiseId(pub u64);

/// A value crossing between a runtime and the host.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Object(ObjectRef),
    Callback(CallbackId),
    Promise(PromiseValue),
    /// The value `JSON.parse` gives for this text (`Response.json()`). A
    /// runtime that cannot parse it rejects (in a promise) or throws.
    Json(String),
    /// A dictionary argument: its members that were present.
    Dictionary(Vec<(String, Value)>),
    /// A script value of no kind the bindings know (an object that is not a
    /// host object, where one was expected). Checked arguments refuse it.
    Other,
}

/// A promise a host method returns.
#[derive(Clone, Debug, PartialEq)]
pub enum PromiseValue {
    /// Settled later, with [`ScriptRuntime::settle`].
    Pending(PromiseId),
    Resolved(Box<Value>),
    Rejected(Box<Rejection>),
}

/// Why a promise is rejected: a value, or an error the runtime builds.
#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    Value(Value),
    Error(HostError),
}

/// How a pending promise ends.
pub type Settlement = Result<Value, Rejection>;

/// An exception a host method throws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// `Error` (capability refusals, `QuotaExceededError`, DOM exceptions —
    /// whose name leads the message, as `NotFoundError: …`).
    Error,
    TypeError,
}

impl HostError {
    pub fn error(message: impl Into<String>) -> HostError {
        HostError { kind: ErrorKind::Error, message: message.into() }
    }

    pub fn type_error(message: impl Into<String>) -> HostError {
        HostError { kind: ErrorKind::TypeError, message: message.into() }
    }

    /// A DOM exception: `name` (`NotFoundError`, `HierarchyRequestError`)
    /// leads the message.
    pub fn dom(name: &str, message: &str) -> HostError {
        HostError::error(format!("{name}: {message}"))
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ErrorKind::Error => write!(f, "Error: {}", self.message),
            ErrorKind::TypeError => write!(f, "TypeError: {}", self.message),
        }
    }
}

/// A script that did not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    pub kind: ScriptErrorKind,
    /// As the runtime reports it (`Uncaught …`, `SyntaxError: …`).
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptErrorKind {
    /// The script threw, or did not parse.
    Thrown,
    /// The runtime stopped it at an execution limit (loop iterations,
    /// recursion).
    RuntimeLimit,
    /// Stopped through an [`InterruptHandle`].
    Interrupted,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Stops the script a runtime is running, from any thread, at the runtime's
/// next check (ADR 013, ADR 023 §6). The runtime clears it when it stops.
#[derive(Clone, Debug, Default)]
pub struct InterruptHandle(Arc<AtomicBool>);

impl InterruptHandle {
    pub fn new() -> InterruptHandle {
        InterruptHandle::default()
    }

    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether an interrupt is pending; clears it.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

/// A JavaScript engine, as a page uses it (ADR 023 §1). A runtime is built
/// over a [`HostRef`] and calls [`bindings::dispatch`] on it for every
/// binding script invokes; the host never calls a runtime while a binding is
/// running.
pub trait ScriptRuntime {
    /// Runs `source` as a classic script. `Ok` is its completion value, as
    /// script's `String(value)` would give it.
    fn evaluate(&mut self, source: &str) -> Result<String, ScriptError>;

    /// Runs queued promise jobs until none are left.
    fn run_jobs(&mut self) -> Result<(), ScriptError>;

    /// Calls `callback` with `this` and `args`.
    fn call(&mut self, callback: CallbackId, this: &Value, args: &[Value]) -> Result<Value, ScriptError>;

    /// Ends the pending promise `promise` (a no-op for an unknown one).
    fn settle(&mut self, promise: PromiseId, settlement: Settlement);

    /// Forgets callbacks the host no longer holds.
    fn release(&mut self, callbacks: &[CallbackId]);

    /// A handle that interrupts this runtime's scripts, if it can be
    /// interrupted at all.
    fn interrupt_handle(&self) -> Option<InterruptHandle>;

    /// Collects this runtime's garbage now.
    fn collect_garbage(&mut self);
}

/// Reports a script error that would otherwise vanish: an `engine::script`
/// event, plus the `ScriptRuntimeLimit` guard when the runtime stopped the
/// script at an execution limit. Never panics; no page content is included
/// beyond the error text.
pub fn report_script_error(source: &'static str, error: &ScriptError) {
    // For the conformance report: most script errors on real pages are a
    // Web API this engine does not have ("X is not defined").
    diagnostics::unsupported::record("js-error", error.message.lines().next().unwrap_or(""));
    if error.kind == ScriptErrorKind::RuntimeLimit {
        diagnostics::guard(diagnostics::Guard::ScriptRuntimeLimit, "a script exceeded an execution limit and was stopped");
    }
    diagnostics::tracing::warn!(target: "engine::script", source = source, error = %error.message, "script threw");
}

/// A number as script's `String(n)` writes it, for the common cases
/// (integers without a fraction, `NaN`, infinities); a runtime formats its
/// own values itself.
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}
