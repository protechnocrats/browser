# ADR 015 — Script GC handles held by Rust are released while Boa's heap is alive

- **Status:** superseded by ADR 023 §5 (Milestone 40, 2026-10-20): every
  handle the runtime keeps is now traced, the host holds only ids, and
  `scripts/gc-handle-captures.txt` is empty. Accepted in the
  performance-audit fixes, 2026-09-26.
- **Decides:** how the `js` crate may hold Boa garbage-collected handles
  (`JsFunction`, `JsObject`, promise resolving functions) in Rust state, and
  when it must let go of them.
- **Constrains:** every native-function capture in `js`, `JsEnvironment`'s
  `Drop`, and when a page thread collects script garbage.

## Context

Native functions bound into script capture Rust state through
`#[derive(Trace, Finalize)]` structs whose fields are marked
`#[unsafe_ignore_trace]` (the `js` crate docs, "The safe-closure-capture
pattern"). The derive writes an `unsafe impl Trace` inside `boa_gc`, so the
crate's `forbid(unsafe_code)` does not see it — and its soundness rests on a
rule the code had broken: the pattern's own docs said captures hold "no
`JsValue` inside them", but four hold GC handles — the
`requestAnimationFrame` callbacks, the event listeners, and the pending
`fetch` and `XMLHttpRequest` requests.

Boa keeps its heap in a thread-local. When a thread exits it frees every
object in that heap, in no particular order. A closure object freed there
drops its capture; if the capture still holds a handle to an object freed
earlier in the same teardown, dropping the handle writes to freed memory.

The performance audit found it: the engine's unit tests aborted with
`malloc(): corrupted double-linked list` in about 5% of runs (16 of 300;
0 of 120 with one test thread, where no thread exits), and AddressSanitizer
reported a heap use-after-free on its first run, freed and used inside
`boa_gc::BoaGc::drop` via `js::raf::RafCaptures`. Any page that registers a
callback reaches it when its thread ends — a page closed, the engine shut
down.

## Decision

1. **`JsEnvironment`'s `Drop` empties every container of GC handles** that
   an untraced capture shares, while the heap is alive. After it, the
   closures still in the heap hold only empty containers, so the order the
   heap is torn down in no longer matters.
2. **A page's script garbage is collected when the page goes** —
   `js::collect_garbage()` after a navigation replaces the environment and
   after the page thread's loop ends — so an old page's objects are freed
   while the thread runs, not left to its exit.
3. **The rule is enforced mechanically.** `scripts/check-gc-handles.sh`
   (run by `check-dependency-direction.sh`, self-tested in
   `test-guardrails.sh`) fails when an `#[unsafe_ignore_trace]` field's type
   can hold a handle and is not listed in `scripts/gc-handle-captures.txt`,
   or is listed but not emptied in that `Drop`.
4. **CI runs the script-heavy suites under AddressSanitizer**, three times
   per change (the `sanitizer` job): this class of bug shows up as a rare
   abort in plain runs and at once under the sanitizer.

## Alternatives

- **Trace the containers for real** (`Gc<GcRefCell<…>>` shared between Rust
  and the closures): the sound design, and the one to move to with the Boa
  upgrade (M38, ADR 013), when the bindings are reworked anyway. Until then
  the release-on-drop rule is small and checked.
- **Never let a page thread exit** — leaks every closed page's heap and only
  moves the teardown to process exit.
