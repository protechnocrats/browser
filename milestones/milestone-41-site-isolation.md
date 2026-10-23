# Milestone 41 — Site isolation & browser-process storage

> **Status:** Done, with gaps (2026-10-22). **Still open:** sandboxes beyond
> Linux (macOS, Windows) were not built — they need an `unsafe` FFI
> exception and a machine of each platform to build and run the self-test
> on, and neither is available here → backlog (process isolation); the
> live app was not driven across sites by a person (🚫). Decision record: `docs/adr/025-site-isolation.md`. Overall
> status: `AUDIT.md` · the plan: `ROADMAP.md`.

**Goal:** finish what process isolation (ADR 016) started (architecture
review A1): a renderer per site, storage and the network grant enforced by
the browser process.

## What was built

1. **A renderer holds one site's documents** (ADR 025 §1–2). The host's
   `SiteLock` gains a site, set by a renderer's first document; the host
   delivers a navigation response only if its final URL is of that site,
   and accepts requests and storage operations only for origins it admitted
   — all of that site. Before M41 the lock kept the last four origins the
   page had visited, whatever their site.
2. **Pages move between renderers** (§3). A navigation — link, form, typed
   URL, or a same-site URL redirecting off-site — whose response is of
   another site is not delivered: a renderer that has held no document (the
   spare; another started behind it) is set up with the page's viewport,
   visibility and grant kinds, given the load, and answered with the
   response the host already has (fetched once). The page's mailbox and
   bridge stay, so the engine API is unchanged; the old renderer is shut
   down and ended. The new renderer's frames are held back until its
   document commits, so the old document stays on screen, and the page
   announces the navigation once. A host-made HTML document of another site
   moves the page too. A failed navigation moves nothing.
3. **Spares shared by new pages and moving ones** (`process::Spares`); a
   renderer is never reused for another site or page once it has held a
   document (the reuse limit). One spare serves every site: it has held
   nothing, so it suits any (the roadmap's "per site class" reads as this).
4. **`localStorage` served by the browser** (§4). `storage::RemoteAreas`
   and `StorageStore::remote`: a renderer's shareable areas are the
   browser's. `ToHost::Storage` (get, set, remove, clear, length) is
   answered from the profile's store through the page's own grant, for
   origins the renderer admitted; a revocation applies at once in the
   browser. Opaque origins stay private and local.
5. **The network grant enforced by the browser** (§5). The host keeps the
   page's grants; without the network one, a request labelled script's
   (`xmlhttprequest`, `script`, `other`) is refused before the network.
   `response_policy`: of a cross-origin response, a renderer gets only what
   its request's stated purpose can use — script requests and fonts what
   CORS allows the document, images an image, stylesheets nothing served as
   HTML/JSON/XML — else status and URL only (`Guard::RendererResponseWithheld`,
   40 guards).
6. `Engine::renderer_serial(page)`; `renderer_pid`/`renderer_sandbox`
   follow the page.
7. **A document is named by the URL it landed on** (closing M21's
   simplification, which site isolation made matter). The loader's result
   carries the post-redirect URL; it becomes the page's `current_url`,
   names the document's origin, and is what `NavigationCommitted`
   reports. Before, after a cross-site redirect the new renderer's script
   named the origin it asked for, which the renderer was never given, so
   its requests and storage were refused; the host now admits only the
   landed-on origin, never the requested one.

Found and fixed on the way:
- **A page moving to another site could lose its `NavigationStarted`.**
  The old renderer announces the navigation in the tick of the click, but
  the host can fetch the new document and end that renderer before reading
  the announcement — and the new renderer's own announcement was being
  dropped as a duplicate. Found by running the engine suite five times in a
  row (1 failure in 5); the host now remembers what it announced, and
  publishes under the same lock that swaps renderers. 0 failures in 10 runs
  of the whole engine suite afterwards.
- **A redirected page's script was refused.** Site isolation made M21's
  naming matter: after a cross-site redirect the document was named by the
  URL it asked for, of the old site. Fixed (item 7);
  `after_a_cross_site_redirect_the_document_is_its_landed_on_origins`, in
  thread and process mode, was checked to fail with the old naming.
  Changed on purpose: `NavigationCommitted` reports the landed-on URL.
- **A watchdog kill was reported as "the renderer process ended".** The
  rewrite ended the process before marking the page crashed, so the reader
  saw the pipe close first (`a_runaway_script_has_its_renderer_ended_by_the_watchdog`
  caught it). Marked first again.

## Acceptance criteria

- [x] **A cross-site link lands in a different process; same-site does
      not.** `site_isolation.rs`:
      `a_cross_site_link_lands_in_another_renderer_and_a_same_site_one_does_not`
      (in-process renderers: another port of `127.0.0.1` keeps the renderer,
      `localhost` gets a new one; one `NavigationStarted`; no blank frame
      presented while moving; the new document fetched once);
      `a_typed_url_and_a_redirect_to_another_site_move_the_page_and_a_failure_does_not`.
      With real sandboxed processes: `real_processes.rs`,
      `a_cross_site_navigation_moves_the_page_to_another_sandboxed_process`
      (another pid; the old process gone; the new one sandboxed). Unit:
      `a_renderer_is_locked_to_the_site_of_its_first_document`,
      `a_prefetched_document_is_handed_over_once`.
- [x] **A renderer whose page has no network grant cannot get a script-style
      request (credentialed cross-origin) made by the browser.**
      `process.rs`: `a_renderer_without_the_network_grant_cannot_get_a_script_request_made`
      — the test plays a compromised renderer: a credentialed cross-origin
      request labelled `xmlhttprequest`, `script` or `other` is refused and
      the server sees nothing; labelled `image`, it is an image request and
      its JSON is withheld; with the grant it is made, and CORS still keeps
      the body. `response_policy.rs`: three unit tests of the rules.
- [x] **Two pages of one origin share `localStorage` in process mode.**
      `two_pages_of_one_origin_share_local_storage_in_process_mode`: one
      page's write is in the profile's store and read by the other page (in
      another process); a page of another site reads `null`.
      `storage_is_served_by_the_browser_only_for_origins_the_renderer_loaded`:
      refused before a document of the origin is loaded, for another site,
      for a malformed origin, and after a revocation.
- [~] **The sandbox self-test passes on each supported platform.** Linux:
      `the_sandbox_refuses_files_sockets_and_programs` passes (as before).
      macOS and Windows: no sandbox built (see the status line); the
      renderer reports "process isolation only" there.

## Verification

Standing Definition of Done: 1,209 tests, 0 failures (14 new; the same
suite at the commit before M41 counts 1,195 — `AUDIT.md`'s 1,164 predates
the gap pass); `cargo build --workspace --release` clean; clippy unchanged
at 28; every guardrail and `test-guardrails.sh` pass; the fuzz suite passes,
with six new `ipc_decode` seeds (`21`–`26`: storage requests and answers, an
unknown operation, a value claiming 2 GiB) pinned by
`the_committed_seeds_of_changed_messages_decode`. New untrusted input — the
storage messages — is bounded by the codec (strings at most the 5 MiB area
quota) and checked by the host; no new parser.
The 56-page corpus was not replayed: its recordings are local-only and not
on the machine this was built on (its test skips). It replays in thread
mode, which M41 does not change; the thread-versus-process scene test
(`process_isolation.rs`) passes unchanged.
AddressSanitizer: CI's `sanitizer` command with `storage` and `capability`
added (the new site-isolation, storage and fake-renderer tests run inside
`engine`'s suites), 3 rounds: 0 reports, 0 failures.

## Measurements

The frame pipeline did not change. Per frame, a process-mode page's host now
takes one more lock when it publishes (to order it against a renderer swap);
`frame_bench` was not re-run. Each `localStorage` operation in process mode
is now a round trip to the browser process.

## Not done / open

- **Sandboxes beyond Linux** → backlog (process isolation). They need FFI
  (`sandbox_init`; an AppContainer set at process creation), an exception in
  `scripts/unsafe-exceptions.txt`, and a macOS and a Windows machine to
  build and run `--sandbox-self-test` on; this machine has only the Linux
  target.
- A document the parser rejects after the page moved leaves the page blank
  in its new renderer (a failed *fetch* keeps the old page).
- A command in flight to a renderer the page is leaving is not lost (both
  take the state lock), but input a page's old renderer was handling when
  it was ended is.
- Not exercised in the live app across sites by a person 🚫.
- Frames (`<iframe>`) do not exist, so out-of-process frames are not a
  question yet; a GPU process and a network process stay in the backlog.
