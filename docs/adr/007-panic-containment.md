# ADR 007 — A panic in a page is that page's crash, not the browser's

- **Status:** accepted (Milestone 30)
- **Decides:** what happens when code running for a page panics, and the build settings that guarantee it.
- **Constrains:** release profiles, every lock shared with a page thread, and hosts' handling of `EngineEvent::PageCrashed`.

## Context

The pipeline is written not to panic on hostile input (fuzzed parsers, depth
caps, no-panic network code), but a bug somewhere in a large engine — ours or a
dependency's (Boa has two open advisories, `deny.toml`) — can still panic. With
every page on the UI thread that took the whole browser down. With page threads
(ADR 006) it need not.

## Decision

1. **The page thread's whole body runs inside `catch_unwind`.** A panic
   unwinds the page's own state (its `Rc` DOM, Boa context, caches), records
   the panic message in the page's status, publishes
   `EngineEvent::PageCrashed { page, reason }`, wakes the host, and ends the
   thread. The host keeps running; other pages are unaffected; the crashed
   page's later commands are dropped. (A host can then close it or create a
   new page — a "reload" — as it likes.)
2. **`panic = "unwind"` is required**, in every profile that builds a host.
   `panic = "abort"` would turn any page panic into a process abort and
   defeat this entirely; the workspace sets no `panic` key (the default is
   `unwind`), and a comment in the workspace `Cargo.toml` says why it must
   stay that way.
3. **Shared state tolerates poisoning.** Every lock a page thread shares with
   the host (its mailbox) or with other pages (the network store, the profile's
   `localStorage`) recovers the data from a poisoned lock instead of
   propagating the panic (`unwrap_or_else(PoisonError::into_inner)`). The data
   behind them are plain collections with no multi-step invariant a torn
   update could break worse than losing every page would.
4. **A crash is observable** (a `PageCrashed` guard event and counter via
   `diagnostics`), never silent, and never reported off the machine (no crash
   upload — §1, vendor-independent).
5. **`PageCommand::SimulateCrash`** panics a page's thread on purpose — the
   engine's `about:crash` — so containment is tested end to end, not assumed.

## Not covered

A stack overflow or an allocation failure aborts the process regardless of
`panic = "unwind"`; only process isolation contains those. The pipeline's
recursion caps and size caps exist precisely so hostile input cannot reach
either, and page threads get a 16 MiB stack (ADR 006).
