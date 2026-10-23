# ADR 025 — Site isolation and browser-process storage

- **Status:** accepted (Milestone 41, 2026-10-22)
- **Decides:** which documents a renderer process may hold; what happens to
  a page whose navigation lands on another site; where a renderer's
  `localStorage` lives; how the browser process enforces a page's network
  grant whatever the renderer says about its requests.
- **Constrains:** `engine` (`process`, `renderer`, `ipc`, the new
  `response_policy`), `storage` (`RemoteAreas`, `StorageStore::remote`),
  `diagnostics` (`Guard::RendererResponseWithheld`), and every renderer
  protocol message that names an origin.
- **Amends:** ADR 016 §1 (one renderer per page → per page *and* site) and
  §5 (the origin lock narrows to one site); ADR 010 §2 and its Consequences
  (process-mode storage and network enforcement).

## Context

ADR 016 put each page in a sandboxed renderer process, but a page kept its
process for life: a link from `news.example` to `bank.example` loaded the
bank into the process that had run the news site's script. The host's origin
lock remembered up to four origins of whatever sites the page had visited,
so a renderer compromised on one site could make requests as an origin of
another it had navigated to. Two capability gaps were left open by M36
because nothing in the browser process could enforce them: a renderer's
`localStorage` was its own, in memory (pages of one origin did not share it
in process mode, and nothing persisted it), and the network grant was
checked only inside the renderer — a compromised one could ask the browser
for any request its document loader could make, labelled however it liked.

## Decision

1. **A renderer holds documents of one site** (schemeful site, M28). Its
   `SiteLock` has no site until its first document, which sets it; from
   then on the host hands it no document of another site, and accepts
   requests and storage operations only on behalf of origins it has
   admitted — all of that site. A renderer that has held no document (the
   spare) suits any site.
2. **The host decides from the response.** Navigation requests — links,
   form submissions, typed URLs — are still made by the renderer through
   the host, which checks them as before (ADR 016 §5) and fetches them.
   When the response's *final* URL (after redirects) is of the renderer's
   site, it is delivered. When it is not, it is **not delivered**: the page
   moves. This covers a cross-site link, a typed URL, and a same-site URL
   that redirects off-site with one rule, enforced where the renderer
   cannot reach it — a compromised renderer that navigates elsewhere only
   gives the page away to a renderer of the other site. A navigation that
   fails (no response) leaves the page where it was, its document shown.
3. **Moving a page.** A renderer that has held no document is started for
   the response's site (the spare, another started behind it), told what
   the page's state needs — viewport, visibility, grant kinds — and given a
   load of the URL; its navigation request is answered with the response
   the host already has, so the document is fetched once. The page's
   mailbox and bridge stay the same, so the engine API, the order of the
   page's commands, coalescing and crash reporting are unchanged; the page
   gets a new renderer, and the old one is shut down and ended. Until the
   new renderer's first navigation commits or fails, its frames are not
   shown (the old document stays on screen) and its `NavigationStarted` for
   a navigation the page already announced is dropped, so a host sees one
   navigation. A command sent while the swap happens goes to the new
   renderer (both take the host's state lock). An HTML document a host loads
   (`LoadRequest::Html`) with an address of another site moves the page the
   same way, at once. A renderer is never reused for another site or
   another page once it has held a document; that is the reuse limit.
4. **`localStorage` is the browser's.** A renderer's store is
   `StorageStore::remote`: every operation on a shareable area is a
   `ToHost::Storage` request (get, set, remove, clear, length), synchronous
   (as `localStorage` is), waited for at most 3 s (under the unresponsive
   bound, ADR 013, as fonts are). The host answers it from the profile's
   store through **the page's own storage grant**, scoped to the origin's
   first-party key, and only for an origin the renderer has admitted; it
   refuses anything else (`Guard::RendererRequestRefused`). A revocation
   flips the grant's token in the host at once. So pages of one origin
   share their area in process mode, as in thread mode, and the profile
   persists it. Opaque origins' areas stay private and local to the
   renderer: they are shared with nothing.
5. **The network grant is enforced by the browser.** The host keeps the
   page's grants (updated as `Grant` and `Revoke` commands pass the
   bridge). A page without the network capability gets made only its
   document loader's requests — documents, stylesheets, images, fonts — and
   a request labelled script's (`xmlhttprequest`, `script`, `other`) is
   refused before the network. Because a label is the renderer's word, the
   host also gives a renderer of a **cross-origin** response only what the
   request's stated purpose can use (`response_policy`): script requests
   and fonts only what CORS lets the document read (the rule script applies
   itself, `origin::cors_response_readable`); images only a body that is an
   image the engine decodes; stylesheets nothing served as HTML, JSON or
   XML. Otherwise the renderer gets the status and URL, without headers or
   body — what an honest renderer would have shown anyway (a broken image,
   a stylesheet that applies nothing, a fetch that fails its CORS check) —
   and `Guard::RendererResponseWithheld` is reported. Same-origin responses
   and navigations are whole.

## Consequences

- `Engine::renderer_serial(page)` names a page's current renderer
  (diagnostics, tests); `renderer_pid` and `renderer_sandbox` follow the
  page to its new renderer.
- The renderer protocol gains `ToHost::Storage` and `ToRenderer::
  StorageReply` (seeds `21`–`26` of `ipc_decode`); nothing else changes on
  the wire.
- Each storage operation in process mode is a round trip to the browser
  process, on the page thread. Pages do not cache their area; one that
  calls `localStorage` in a hot loop pays for it, as in browsers that keep
  storage out of the renderer.
- A document is named by the URL it landed on, after redirects — its
  address, its origin, and the URL `NavigationCommitted` reports. (Through
  M40b the engine named it by the URL it asked for, M21's simplification;
  under §1 a page redirected to another site would then name an origin its
  renderer was never given, and its script would be refused. Fixed with
  this ADR.) The host admits only the landed-on origin, never the
  requested one.
- The host's in-flight request for the old renderer's navigation is never
  answered when the page moves: the old renderer is ended.
- **Not decided here: sandboxes beyond Linux.** macOS (`sandbox_init` or
  the App Sandbox) and Windows (an AppContainer, set at process creation)
  each need system calls through FFI, so an `unsafe` exception in
  `scripts/unsafe-exceptions.txt`, and a machine of that platform to build
  and run the self-test on. Neither exists here; `sandbox::enter` keeps
  reporting "process isolation only" there. They go to the backlog with
  that reason.

## Alternatives

- **The renderer asks to move** (it knows the target before it fetches).
  Rejected: then the decision is the renderer's, and a compromised one
  would simply not ask. The host must refuse to deliver another site's
  document regardless, and once it does, it might as well decide.
- **A speculative renderer kept beside the old one until commit.** What
  browsers do to keep the old page interactive and to recover when the new
  one fails to parse. Here the host has the response before it moves the
  page, so a failure to *fetch* already keeps the old page; only a document
  the parser rejects leaves the page blank in its new renderer. Not worth a
  second live renderer per navigating page yet.
- **Withholding by `Content-Type` alone (full ORB).** The engine draws only
  images it can decode and fonts that pass CORS, so sniffing what it can use
  is narrower and needs no MIME table; the stylesheet check is the one place
  a `Content-Type` list is used.
- **A storage cache in the renderer.** Fewer round trips, but a second copy
  to keep coherent across pages of one origin (browsers broadcast changes);
  not needed until a page is measured to suffer.
