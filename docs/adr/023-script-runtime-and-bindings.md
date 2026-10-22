# ADR 023 — The script runtime interface and generated bindings

- **Status:** accepted (Milestone 40, 2026-10-20)
- **Decides:** where the line between the JavaScript engine and the rest of
  the browser runs; how script-visible interfaces are declared and bound; how
  script objects name DOM nodes; who holds garbage-collected handles.
- **Constrains:** every script-visible API (it is declared in WebIDL first),
  the `js` crate (now one runtime among possible others), `engine`'s page
  (it holds a `script::ScriptEnvironment`), `scripts/check-gc-handles.sh`.
- **Supersedes:** ADR 015's release-on-drop rule (its "Alternatives" named
  this design as the sound one); the id-string identity scheme of the M11–M39
  bindings; M18's two element-wrapper types.

## Context

Architecture review A3 and A4 (`ROADMAP.md` §3). Through M39 the script
bindings were hand-written Boa closures in `js`:

- An element was named by its `id` attribute: `getElementById` built a new
  wrapper of about twenty function objects on every call, each closure
  capturing the id string and looking the element up again; an element
  without an `id` could not have listeners; `querySelector` returned only
  elements with ids. `createElement` returned a second wrapper type that
  stopped working once its element was attached (M18's documented gap).
- A node argument was recognised by a plain data property, `__node_id`,
  which script can forge (`list.removeChild({__node_id: 7})`).
- Four untraced captures held GC handles, kept sound only by emptying them
  in `JsEnvironment`'s `Drop` (ADR 015).
- Boa's types ran through the whole crate: event dispatch, form submission,
  native editing, `fetch` and storage were written against `JsValue` and
  `Context`, so no other runtime could be put behind them, and none of that
  logic could be tested without Boa.

## Decision

1. **A runtime is a trait.** `script::ScriptRuntime`: `evaluate`, `run_jobs`,
   `call` (a callback, with `this` and arguments), `settle` (a promise),
   `release` (callbacks the host no longer needs), `interrupt_handle`, and
   `collect_garbage`. It is object-safe; a page holds a
   `Box<dyn ScriptRuntime>` inside `script::ScriptEnvironment`. `js::BoaRuntime`
   is one implementation; `script::stub::StubRuntime` (behind the
   `test-runtime` feature) is another. The trait lives in a new core crate,
   `script`, not in `engine` as the roadmap first said: `js` implements it and
   `engine` depends on `js`, so a trait in `engine` would be a cycle. `engine`
   re-exports it.
2. **Everything that is not the JavaScript engine is on the host side**, in
   `script`, written against plain Rust values: the bindings' DOM operations,
   the detached forest, event dispatch (capture, target, bubble), default
   actions, native text editing, focus, form validation and submission,
   `requestAnimationFrame`'s queue, `fetch`/XHR (capabilities, CORS,
   credentials), storage. A runtime converts between its values and
   `script::Value` — `undefined`, `null`, booleans, numbers, strings, host
   objects (`ObjectRef`: an interface and a key), callbacks (`CallbackId`),
   promises, JSON text — and does nothing else.
3. **Interfaces are declared in WebIDL** (`crates/script/webidl/*.webidl`) and
   the glue is generated at build time by `script`'s build script, through a
   small WebIDL-subset parser (`crates/webidl`): interfaces with inheritance,
   a `[Global]` interface, namespaces, dictionaries, callback types,
   attributes (readonly or not), operations with optional, defaulted and
   variadic arguments, constructors. From each declaration it generates:
   - a Rust trait per interface, with typed methods (`DOMString` → `String`,
     `Node?` → `Option<ObjectRef>`, `EventListener?` → `Option<CallbackId>`,
     `Promise<T>` → `PromiseValue`…), which the host implements;
   - `dispatch`, which checks `this` against the interface (inheritance
     included), converts and checks each argument, calls the typed method and
     converts its result;
   - a table of interfaces and members (names, kinds, argument types,
     parents) that a runtime builds its objects from: Boa builds one
     prototype per interface, chained as the interfaces inherit, with one
     native function per operation and an accessor per attribute.
   An interface not in the IDL does not exist for script, so the curated API
   surface (architecture doc §6) is the IDL directory.
4. **One wrapper per node, cached, held weakly.** A host object is named by
   `ObjectRef { interface, key }` — for a node, its `NodeId` (M39). The Boa
   runtime keeps one wrapper per `ObjectRef` in a cache of weak references,
   so `getElementById('x') === getElementById('x')` and a node's wrapper is
   the same before and after it is attached, removed and inserted again. The
   wrapper object carries its `ObjectRef` in an internal slot (Boa native
   data), never in a property script can forge. A wrapper that is collected
   tells the host (a guard dropped with it), which is how the detached
   forest still frees subtrees no script can reach, and how host objects
   (events, responses, XHRs) are freed. Expando properties on a wrapper are
   lost if the wrapper itself is collected and later recreated — browsers
   keep such wrappers alive; this engine does not yet (stated gap).
   *(Amended in the gap pass, 2026-10-21: the Boa runtime keeps the wrapper
   of every node in the document, strongly, in its traced tables — the
   document can always reach the node again, so its identity and expandos
   must last (Blink does the same). An operation that inserts a node keeps
   its wrapper and its descendants' live ones; one that removes it lets
   them go; any other departure (a parent's `textContent` replaced) is
   found by a check amortised over the document's link updates. A detached
   node's wrapper is still weak: if its whole subtree is unreachable it is
   freed, expandos and all, which no script can observe; if script still
   holds another node of the subtree, a collected wrapper's expandos are
   lost — the remaining, narrow gap. Boa 0.22 offers no hook to see an
   expando being added, which would close it.)*
5. **GC handles are traced by the runtime.** No native function captures a
   GC handle. Callbacks the host keeps (listeners, rAF callbacks, XHR event
   handlers) and the resolvers of pending promises live in the runtime's own
   tables, inside the GC heap (`Gc<GcRefCell<…>>`, traced), and the host holds
   only `CallbackId`s and `PromiseId`s. `scripts/gc-handle-captures.txt` is
   empty, `JsEnvironment`'s `Drop` is gone, and `check-gc-handles.sh` keeps
   failing any untraced field that could hold a handle — with nothing it may
   allow.
6. **Interrupt where the runtime can.** `interrupt_handle` returns a `Send`
   handle that stops the running script at the runtime's next check, or
   `None` when the runtime cannot be interrupted. Boa 0.22 cannot (ADR 013):
   it returns `None`, and the renderer watchdog stays the bound. The stub
   runtime can, which tests the contract.
7. **The DOM binding tests run on two runtimes.** A set of cases — scripts in
   a small subset of JavaScript (declarations, member access, calls,
   assignment, literals, `===`) with their expected documents — runs on the
   Boa runtime and on the stub, which interprets that subset over the same
   generated dispatch. Both must give the same documents and results.

## Consequences

- A second JavaScript engine is a crate that implements `ScriptRuntime` and
  builds objects from the generated table; none of the host side changes.
- Adding a DOM API is an IDL line and a typed method; a method the host does
  not implement is a compile error, not a silent gap.
- Listeners, focus and events name nodes by `NodeId`: an element without an
  `id` can have listeners, `event.target` is the target's wrapper, and
  `querySelector` returns any element.
- Wrappers are prototype objects, not twenty closures each; a lookup is one
  hash probe and, for a live wrapper, no allocation.
- `Document` is an `EventTarget` here, not a `Node` (there is no document
  node in the arena); `checked` is on `Element`, not `HTMLInputElement`;
  `Event.key` is on `Event`, not `KeyboardEvent`. Each is one IDL line to
  correct when the interface it belongs to is built.

## Alternatives

- **Hand-written bindings behind a trait, no IDL.** Removes Boa from the host
  side but leaves every API's argument conversion written by hand, the part
  that does not scale to the thousands of APIs real sites use (A3).
- **A full WebIDL parser crate (`weedle2`).** Covers the whole grammar; we
  use a small subset, the input is our own files read at build time, and a
  hundred-line-scale grammar is cheaper to audit than a dependency
  (minimal-dependency principle). Moving to it later changes only the build
  script.
- **Strong wrapper cache.** Simplest identity, but every wrapper — and every
  detached subtree a wrapper names — would live as long as the page, which
  is the leak the performance audit fixed in the detached forest.
- **Interned names in this milestone** (ADR 021 §5). They pay only when the
  parser interns and selector matching compares atoms — style and parser
  work with its own measurements, not binding work. Split to **M40b**.
