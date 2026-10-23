# Milestone 40 — Script runtime interface & generated bindings

> **Status:** Done, with gaps (2026-10-20). **Still open:** interned names
> (ADR 021 §5) were split to **M40b** (done there, ADR 024) — they belong with the parser and
> selector matching, not the bindings (ADR 023, Alternatives); Boa 0.22
> still cannot be interrupted, so `BoaRuntime::interrupt_handle` is `None`
> and the renderer watchdog stays the bound (ADR 013) — the interface is
> exercised by the stub runtime; ~~a wrapper's expando properties are lost if
> the wrapper is collected and made again~~ — kept for nodes in the document
> since the gap pass (ADR 023 §4, amended); a detached node whose subtree
> script still reaches through another node can still lose them;
> ~~`window`/`self` are not exposed yet~~ — exposed in the gap pass, goldens
> unchanged (below).
> Decision record: `docs/adr/023-script-runtime-and-bindings.md`. Overall
> status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** JavaScript as the replaceable subsystem architecture doc §2 says it
is (architecture review A3, A4): a runtime behind an interface, bindings
generated from WebIDL with one wrapper per node, and GC handles traced by the
runtime instead of released by hand (ADR 015's follow-up).

## What was built

1. **The runtime interface** (ADR 023 §1), in a new core crate, `script`:
   `ScriptRuntime` — `evaluate`, `run_jobs`, `call`, `settle`, `release`,
   `interrupt_handle`, `collect_garbage` — object-safe, so a page holds a
   `Box<dyn ScriptRuntime>` inside `script::ScriptEnvironment`, which is what
   `engine`'s page now holds. `engine` re-exports the trait. The roadmap
   placed it in `engine`; `js` implements it and `engine` depends on `js`, so
   that would have been a dependency cycle.
2. **The host side, runtime-independent** (§2). Everything that is not the
   JavaScript engine moved from `js` to `script`, written against plain
   `script::Value`s: the DOM operations, the detached forest, event dispatch
   with its capture/target/bubble phases and default actions, native text
   editing, focus and tab order, form validation and GET submission,
   `requestAnimationFrame`'s queue, `fetch`/XHR (capabilities, CORS,
   credentials, revocation), storage, `console.log`. `js` is now only Boa:
   `BoaRuntime` (under 500 lines) and `js::new_environment`.
3. **Bindings generated from WebIDL** (§3). `crates/script/webidl/dom.webidl`
   and `window.webidl` declare every interface script can reach — 13
   interfaces and namespaces, 64 members. `crates/webidl` parses the subset
   used (interfaces with inheritance, `[Global]`, namespaces, dictionaries,
   enums, callbacks, attributes, operations with optional, defaulted and
   variadic arguments, constructors); `script`'s build script generates a
   trait per interface with typed methods (`NodeImpl::insert_before(&mut
   self, this, node: ObjectRef, child: Option<ObjectRef>)`), `dispatch`
   (checks `this` against the interface and its ancestors, the argument
   count and every argument's type, then converts the result), and the
   member table runtimes build their objects from. `HostState` implements
   the traits; a member the IDL declares and the host does not implement is
   a compile error.
4. **One wrapper per node** (§4). Host objects are named by `ObjectRef
   { interface, key }` — a node by its `NodeId`. Boa builds one prototype per
   interface, chained as the IDL inherits, with a native function per
   operation and an accessor per attribute; a wrapper is an object with that
   prototype and its `ObjectRef` in an internal slot. A cache of weak
   references (`WeakGc`, through a cell the wrapper and the cache share)
   gives back the same wrapper while it lives; a collected wrapper's guard
   tells the host, which frees unreachable detached nodes and host objects
   (events, responses, XHRs). `document.createElement`'s element is the same
   wrapper before and after it is attached, removed and put back — M18's two
   wrapper types and their "stale handle" gap are gone.
5. **GC handles traced** (§5). No native function captures a handle: each is
   a `Copy` closure over the member it calls. Listeners, rAF callbacks, XHR
   handlers and pending promises' resolvers live in a traced
   `Gc<GcRefCell<Tables>>`; the host holds `CallbackId`s and `PromiseId`s.
   `scripts/gc-handle-captures.txt` is empty, `JsEnvironment`'s hand-written
   `Drop` is gone, and `check-gc-handles.sh` now scans `script` too and fails
   on any untraced field that could hold a handle.
6. **A second runtime** (§6, §7). `script::stub::StubRuntime` (feature
   `test-runtime`) interprets a small subset of JavaScript — declarations,
   member access, calls, `new`, `+`, `===`, arrow functions — over the same
   generated `dispatch`, and can be interrupted. `script::cases` holds 17
   binding cases (scripts, clicks, animation frames, collections, expected
   documents); the stub and Boa both pass all of them.

Found and fixed on the way:
- **A node argument could be forged.** Through M39 a node argument was found
  by its `__node_id` data property, so `list.removeChild({__node_id: 7})`
  named node 7 whether or not the script held it. Nodes are now known only by
  the wrapper's internal slot; the hostile-arguments test and the
  `js_script` fuzz seed try the forgery.
- **Two trait methods with one name.** `Response.status` and
  `XMLHttpRequest.status` are both `status(&mut self, …)` on `HostState`; a
  helper with the same name took the trait method's place through autoref.
  Generated code calls every host method by its trait (`<H as
  ResponseImpl>::status`), and helpers have names of their own.

Changed on purpose (each is the standard behaviour; tests updated):
- `event.target` is the target's wrapper (it was the target's `id` string);
  `event.key` is `null` outside key events (it was absent).
- An element needs no `id` to have listeners, and `querySelector` returns
  any element (both needed an `id`).
- A DOM operation that cannot be done throws — `HierarchyRequestError`,
  `NotFoundError` — where it did nothing or returned `null`; a missing or
  ill-typed argument throws a `TypeError` where it became `""`
  (`createElement()` made a `div`). Four `js` tests now assert the
  exceptions.
- Attached elements have `setAttribute`, `getAttribute`, `removeAttribute`,
  `id` and `tagName` (only detached ones had the first three); detached ones
  have `style`, `checked` and `addEventListener`.
- `requestAnimationFrame` returns a handle and throws on a non-function (it
  returned `undefined` and ignored one); XHR's response attributes are
  read-only; an invalid `fetch` `credentials` value is a `TypeError` (it was
  `same-origin`); `submit` bubbles from the form to its ancestors.

## Acceptance criteria

- [x] **A test runtime (a stub implementing the trait) drives the DOM
      tests.** `the_stub_runtime_passes_every_binding_case` runs the 17 cases
      in `script::cases` on `StubRuntime`; `boa_passes_every_binding_case`
      runs the same cases on Boa. They cover node identity, `getElementById`
      and `querySelector`, text, attributes, `style`, `checked`, created,
      removed, moved, replaced and cloned nodes, detached subtrees, text
      nodes, argument checking and DOM exceptions, listeners on elements
      without ids, capture/target/bubble order, `stopPropagation`,
      `preventDefault` of a default action, `requestAnimationFrame`, storage,
      and freeing unreachable detached nodes.
- [x] **`getElementById` returns the same wrapper for the same node.**
      `getElementById('a') === getElementById('a')`, across a collection and
      before and after attaching and removing (the cases);
      `get_element_by_id_makes_one_wrapper_per_node_however_often_it_is_called`:
      10,000 calls make at most two wrappers (the element and the document),
      counted by `HostState::wrappers_opened`.
- [x] **`scripts/gc-handle-captures.txt` is empty** (comments only), and
      `check-gc-handles.sh` passes over `js` and `script`.

## Verification

Standing Definition of Done: 1,156 tests, 0 failures (up from 1,144: 3 in
`webidl`, 6 in `script`, 3 in `js`; the forest's 3 unit tests moved from
`js` to `script` with it); `cargo build --workspace --release` clean; clippy
at 28 (down from 43: the removed bindings carried 15, the new code adds none;
baseline updated); every guardrail and `test-guardrails.sh` pass (`script`
and `webidl` are core crates; `check-capabilities.sh` checks `script` for
global state; `check-host-boundary.sh` keeps hosts off `script`); the fuzz suite passes —
25 targets, the new one `webidl_parse` (both IDL files, malformed input,
deep nesting; replay adds 100,000-deep `Promise<`, unclosed brackets and
100,000 members), and a `js_script` seed, `07_generated_bindings`, that calls
prototype members on the wrong `this`, tears accessors off, forges a node id
and drops a thousand wrappers. No new renderer message, so no new
`ipc_decode` seeds. The 56 corpus pages render and match their goldens,
unchanged. Their script errors are the same as M39c's but one: on
`rust-book-getting-started`, mdBook's sidebar script called `setAttribute` on
an attached element, which M39c's wrapper did not have; it now runs two lines
further, to `querySelectorAll` (not built). The attributes it sets do not
change what is drawn.

AddressSanitizer, CI's `sanitizer` command with `script` and `dom` added
(the job now runs `script` too), 4 rounds: 0 reports. One round, run while
two release builds loaded the machine (load average 6.9), had two timing
failures in `frame_budget.rs` — the page degraded a step further than the
test expects, which is ADR 020 working under load, not a memory error — and
they stopped the rest of that round; it was run again on its own and passed.

## Measurements

`frame_bench` (median frame and best frame), interleaved with M39c
(the M39c build), 24 rounds; load average 4–6 throughout (not from this work):

| Page | Frame | M39c median | M40 median | M39c best | M40 best |
|---|---|---|---|---|---|
| demo | first | 4.06 ms | 4.03 ms | 3.58 ms | 3.44 ms |
| | scroll / DOM change / caret | 0.132 / 0.089 / 0.082 | 0.126 / 0.071 / 0.066 | 0.114 / 0.080 / 0.075 | 0.100 / 0.063 / 0.063 |
| 1k rows | first | 7.95 ms | 8.01 ms | 7.38 ms | 7.33 ms |
| | scroll / DOM change / caret | 0.036 / 0.164 / 0.023 | 0.041 / 0.153 / 0.019 | 0.022 / 0.102 / 0.019 | 0.025 / 0.136 / 0.017 |
| 10k rows | first | 68.9 ms | 70.6 ms | 63.6 ms | 62.5 ms |
| | scroll / DOM change / caret | 0.052 / 0.400 / 0.028 | 0.064 / 0.386 / 0.024 | 0.037 / 0.337 / 0.023 | 0.040 / 0.337 / 0.020 |
| 10k + flex/vars | first | 77.4 ms | 77.8 ms | 63.7 ms | 67.2 ms |
| | scroll / DOM change / caret | 0.067 / 0.408 / 0.030 | 0.060 / 0.420 / 0.026 | 0.035 / 0.365 / 0.025 | 0.037 / 0.348 / 0.019 |

Frames whose script work changed — a click through a listener, a caret move
— are level or faster (the demo's DOM change −21%, caret frames −11% to
−24%): a lookup no longer builds a wrapper of twenty closures. First frames
are within noise (medians +1–2%, best −4% to +6%, with spreads of ±7 ms on
the 10k pages). Scroll frames on the 1k and 10k pages measured 3–12 µs slower
(not the flex page): the script work a scroll frame does — `dispatch_scroll`,
settling fetches, the rAF check, `take_dirty` — was timed alone at 0.360 µs
(M39c) and 0.351 µs (M40), best of 30 × 10,000, so the difference is not in
what M40 changed; as with M39b's 2 µs, not attributed.

## Not done / open

- ~~**Interned names → M40b** (ADR 021 §5): in the
  parser, with selectors matching by atom; measured in M39 as a loss when
  done after parsing.~~ — done in M40b (ADR 024).
- **No interrupt for Boa** (ADR 013): `interrupt_handle` is `None`; pages on
  threads are still bounded only by Boa's loop and recursion limits. A
  runtime that can be interrupted plugs in without engine changes.
- ~~**Expandos on collected wrappers** are lost (the document and storage
  objects are kept for the page's life; node wrappers are not) → backlog.~~
  Closed for nodes in the document in the gap pass (2026-10-21): their
  wrappers are kept while they are in it (ADR 023 §4, amended). What
  remains: a collected wrapper of a detached node whose subtree script
  still reaches through another node.
- ~~**`window` and `self`**, `Window : EventTarget` → backlog~~ — closed in
  the gap pass (2026-10-21): both are the global object, which is an
  `EventTarget` (its own and its ancestors' members on the global; the
  runtime maps the host's `WINDOW` to it). The corpus's 110 `window`/`self`
  reference errors are gone; those scripts now stop at missing APIs further
  on (12 more "not a callable function", 2 null conversions, 2 `location is
  not defined`), and all 56 goldens match unchanged. The engine fires no
  window events yet (`load`, `resize`: backlog). (Was: §4 ranks
  `window is not defined` at 26 pages): one IDL line and a runtime mapping,
  but pages that now fail early would run further, so the corpus goldens
  need a review of their own.
- ~~**Promise-returning operations throw** a conversion error instead of
  returning a rejected promise (`fetch(url, {credentials: 'bogus'})`).~~
  Closed in the gap pass (2026-10-21): the generated `dispatch` turns any
  error of an operation declared `Promise<…>` — `this`, arguments, the host
  — into a rejected promise (WebIDL §3.2.1).
- **IDL deviations**, each marked in the IDL: `Document` is an `EventTarget`
  (there is no document node), `checked` is on `Element`, `key` on `Event`,
  XHR's `response` is text only.
- The stub interprets a subset made for the cases; it is a test runtime,
  not a JavaScript engine.
