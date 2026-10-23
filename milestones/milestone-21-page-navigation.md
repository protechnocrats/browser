# Milestone 21 — Page Navigation

> **Status:** Done. **Still open:** history (`back`/`forward`), POST navigation, and the rest of *Out of scope* below are in the `ROADMAP.md` backlog.
> Overall status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** give the engine a "load a new document, replacing the
current one" concept — every milestone through 17 has operated on
exactly one document, loaded once at startup or via `--url`. This is
not one milestone's single deferred item; it's the common root cause
named across Milestone 8 ("Navigation UI... browser-shell work,
later"), Milestone 16 (the classic-form-submission gap), and
implicitly Milestone 17.

## Builds on
Milestone 8 (`network`/`app::fetch_page` — the fetch mechanics this
milestone reuses for a SECOND, later load, not just the first),
Milestones 1–17 broadly (this milestone tears down and rebuilds the
whole pipeline state those milestones built).

## In scope
- Tearing down the current `js::JsEnvironment`, DOM tree, and cached
  layout/style state cleanly when navigation starts — no leaked state
  bleeding from the old document into the new one.
- Fetching and parsing a new document (reusing `app::fetch_page`'s
  existing mechanics — its own stylesheet/image-fetching logic doesn't
  need to change, just get invoked a second time).
- Rebuilding the pipeline against the new document and replacing the
  window's rendered content.
- The trigger this milestone actually needs to support first:
  `<a href="...">` link-following via a real click (the single most
  common real navigation trigger) and classic (non-JS) `<form
  method="get">` submission — the exact gap Milestone 16 named and
  left for this milestone.
- **Concurrency**: fetch and parse the new document on a background
  thread (extending the existing `fetch_async`/`FetchHandle` pattern
  from Milestone 8/12 to cover parse+layout, not just the network
  fetch) while the current page stays interactive, swapping over only
  once the new page is ready — avoiding a navigation-triggered UI
  freeze, the same way real browsers do.

## Out of scope (deferred)
- Browser UI (address bar, back/forward buttons, tab strip) — this is
  *engine* navigation capability, not *browser chrome*; wrapping it in
  an actual usable UI is separate, later, product-shaped work per the
  architecture doc.
- History/back-forward navigation state — a real, separate addition on
  top of "can load a second document at all."
- `<form method="post">`'s real semantics beyond a GET-equivalent
  request — POST body handling, multipart/form-data for file uploads
  (the latter also waits on Milestone 22's file-upload item).
- Same-document navigation (`#fragment` scrolling, `history.pushState`)
  — a different, smaller mechanism than a full document replacement;
  worth its own narrow follow-up once this milestone's real navigation
  exists to distinguish it from.

## Why this scope
This is inherently one unit of work — there's no meaningful partial
version of "can load a second page." The deferred-items plan frames it exactly this
way: best built once earlier phases have made the resulting pages
worth navigating between, a condition already met. Landing it here
(after Milestones 18–20, not before) means classic form navigation —
which depends on this milestone — has real validation (Milestone 20)
and real specialized inputs (Milestone 19) to navigate FROM correctly.

## Acceptance criteria
- [x] Clicking a real `<a href="...">` link tears down the current
      document/JS/layout state and loads the linked document,
      rendering correctly. **Verified two ways**: the click→navigate
      TRIGGER logic (`js::dispatch_click` setting
      `pending_navigation`, `preventDefault`-cancellable) is directly
      tested (`clicking_a_link_sets_pending_navigation_to_its_href`
      and 2 others, `crates/js/src/lib.rs`); the actual fetch+swap
      mechanism reuses `app::fetch_page` UNCHANGED, which Milestone
      8/17 already proved against both a real local server and (this
      session) real external sites. **One honest gap**: an actual
      click on a real on-screen link wasn't manually exercised this
      pass (no input-injection tool available, the same limitation
      noted for Milestones 18-20) — `app`'s demo now has a real link
      to `https://example.com/` for a human to try.
- [x] A classic `<form method="get">` with no JS `submit` listener (or
      one that doesn't call `preventDefault()`) navigates to the
      constructed URL (form fields as query parameters, unchecked
      checkboxes correctly excluded) on submit — closing the exact gap
      named in `milestone-16`'s (now consolidated) notes. **Verified**:
      `submitting_a_get_form_sets_pending_navigation_with_a_query_string`,
      `submitting_a_form_with_no_action_reports_an_empty_action`,
      `prevent_default_on_submit_suppresses_classic_navigation`,
      `an_invalid_form_never_sets_pending_navigation` (validation from
      Milestone 20 blocks this default action too, same rule as
      `submit` itself).
- [x] The OLD document's `JsEnvironment`/DOM/layout state is genuinely
      gone after navigation. **Correct by construction, not
      stress-tested with an automated repeated-navigation/memory test**:
      `app`'s navigation-completion code does
      `*js_env.borrow_mut() = js::JsEnvironment::new(..)` — a real
      replacement (the old `JsEnvironment`, its Boa `Context`,
      listeners, focus/cursor state are all dropped, not accumulated),
      not an incremental patch. No stale-listener test was written
      specifically for this (would need restructuring `app`'s
      navigation-swap logic out of `main.rs`, which — like every other
      `main.rs` helper in this project — isn't unit-tested).
- [x] Navigation's fetch+parse work happens on a background thread —
      the window stays responsive while a slow navigation is in
      flight. **Verified with a real timing assertion, not just
      "didn't hang"**:
      `navigate_async_loads_a_real_page_on_a_background_thread_without_blocking`
      (`app/src/lib.rs`) asserts `navigate_async` returns in under
      50ms even though the real fetch it dispatches takes real network
      round-trip time — the exact same proof pattern
      `network`/`js` already use for `fetch_async`.

## Implementation notes

- **`app::navigate_async`/`NavigationHandle`** mirror
  `network::FetchHandle`'s exact shape (an `mpsc::channel`, a
  background `std::thread::spawn`, a never-blocking `try_recv`) —
  runs the WHOLE `fetch_page` call (parse → linked stylesheets/images,
  already internally concurrent since Milestone 8) off the main
  thread, building the new page's `ImageCache` entirely on that
  thread and sending the complete, ready-to-swap result back in one
  piece — no `Arc<Mutex<..>>` needed anywhere.
- **`stylesheet`/`image_cache` had to become `Rc<RefCell<..>>`** (were
  owned outright by `build_scene`'s closure alone) — the same
  "REPLACE what's inside the RefCell, every closure holding the Rc
  sees it" mechanism `document` already used for JS mutation since
  Milestone 13, now doing the same job for a whole-page swap instead
  of an in-place edit.
- **`js`/`app` split, same boundary `dispatch_click`'s own doc comment
  already establishes for hit-testing**: `js::dispatch_click`/
  `dispatch_submit_internal` decide WHETHER to navigate and to WHAT
  (`pending_navigation: Option<(action, query)>`) — real, if partial,
  DOM semantics (respects `preventDefault()`, respects Milestone 20's
  validation) — but never resolve or fetch anything, since this crate
  doesn't track URLs at all. `app::resolve_navigation_target` (relative
  vs. absolute, and the hardcoded-demo-has-no-base-URL case) and
  triggering `navigate_async` both live in `app`, which is the only
  layer that ever knew a URL to begin with.
- **A real, narrow simplification, stated plainly**: `current_url` is
  updated to the REQUESTED URL on a successful navigation, not the
  post-redirect final URL (`fetch_page` doesn't report that back out
  today) — relative link/action resolution immediately after a
  cross-path redirect could be slightly off. Bounded, not silent; see
  `pending_nav`'s own doc comment in `main.rs`. *(Closed in M41: a
  navigation's result carries the URL it landed on, which becomes
  `current_url`, names the document's origin and is the URL
  `NavigationCommitted` reports — site isolation had made the old name
  matter, refusing a redirected page's script in process mode.)*
- **Form-field query-string building is real, not a stand-in**:
  `js::build_form_query_string` walks named `input`/`textarea`/
  `select` descendants in document order, correctly excludes an
  unchecked checkbox/radio (matching real HTML form-submission
  semantics — Milestone 19 gave these real `checked` state to exclude
  by), and percent-encodes (`application/x-www-form-urlencoded`,
  space-as-`+`) via a small self-contained encoder rather than a new
  dependency for something this bounded.
- 9 new tests (7 in `js` — the trigger/validation/preventDefault logic
  — and 2 in `app` — the real background-thread fetch, against a real
  local server, including a genuine timing assertion that it doesn't
  block). 451 tests total across the workspace, zero failures.
  `app`'s live demo gained a real `<a href="https://example.com/">`
  link — confirmed the app still starts and runs cleanly with the
  restructured `Rc<RefCell<..>>` state (both the hardcoded demo and
  `--url` mode re-verified on real hardware this session), though the
  click itself wasn't manually exercised (see acceptance criteria).
