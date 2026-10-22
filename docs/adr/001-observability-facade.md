# ADR 001 — Observability: the `tracing` facade, guards, and no subscriber in core

- **Status:** accepted (Milestone 27)
- **Decides:** how core crates emit diagnostics, and what they are forbidden to depend on.
- **Constrains:** every core crate; `app`'s `--trace`; M30 (page threads), M31 (incremental-pipeline counters), M38 (`about:engine`, frame budget).

## Context

Architecture doc §9 says observability is "not optional once the pipeline
is multi-threaded — that's precisely when debugging without it becomes
miserable." The pipeline already is (`rayon` style resolution, background
fetch and navigation threads), and M30 makes it more so. Today nothing is
observable: every security control that trips (a depth cap, a body-size
cap, a refused redirect) degrades silently, so neither a developer nor a
test can tell "the cap worked" from "the input was never hostile".

Constraints from the founding principles (§1):

- **Vendor-independent / privacy-first:** no mandatory telemetry, no
  phone-home. Diagnostics stay in the process unless a host explicitly
  prints them.
- **Minimal-dependency:** the engine must not accrue an output/formatting
  stack it has no use for.
- **Memory-safe:** core crates are `#![forbid(unsafe_code)]`.

## Decision

1. **Adopt the `tracing` crate as the emission facade.** Core crates
   depend on a tiny in-tree crate, **`diagnostics`**, which re-exports
   `tracing` (macros only: `default-features = false, features = ["std"]`
   — no `attributes` proc-macro, no `log` bridge). One crate owns the
   policy, so it is also the one place to swap the facade out.
2. **No subscriber in core. Ever.** No core crate depends on
   `tracing-subscriber` or on any crate that installs an output sink. The
   subscriber is the host's choice. `app --trace` installs
   `diagnostics::collector::Collector`; tests use `collector::capture`.
   Enforced mechanically: `scripts/check-dependency-direction.sh` runs
   `cargo tree -p <crate> -e normal` for every core crate and fails on
   `tracing-subscriber` (and on the `collector` feature being enabled by
   a core crate's normal dependencies).
3. **Ship a ~250-line in-process `Collector` instead of
   `tracing-subscriber`.** It only needs "record everything, hand it back"
   for tests and a per-frame terminal report. `tracing-subscriber` would
   add `sharded-slab`, `thread_local`, filtering and formatting layers for
   features nothing here uses. The collector is feature-gated
   (`collector`) so core crates cannot even see it, and is bounded
   (`MAX_RECORDS`) so a hostile page cannot grow it without limit.
4. **Security controls report through one function.** `diagnostics::guard(Guard, detail)`
   bumps a per-guard atomic counter *and* emits a WARN event on target
   `engine::guard`. `Guard` is a closed enum; a new depth cap, size cap,
   refusal or rejection adds a variant. A control with no `Guard` is a
   review-blocking gap.
5. **Counters are plain relaxed atomics** (`diagnostics::metrics`): no
   locks, no allocation, safe from `rayon` workers. Process-global for
   now (one engine per process). Because they are global and tests run in
   parallel, **tests assert on deltas, never absolute values.** Exact
   per-page counts ("a scroll-only frame does zero layout") are M31's
   requirement and will be per-page there; this is the baseline.
6. **Costly gauges are opt-in.** Anything that needs a tree walk to
   compute (layout box count) runs only when `metrics::detailed()` is set,
   which only `--trace` does. An untraced run pays for none of it.
7. **Local only.** Nothing in this crate opens a socket or writes a file.
   Crash-report upload and remote diagnostics are out of scope
   (ROADMAP M27/M38: "no mandatory telemetry").

## Dependency assessment: `tracing` 0.1

| | |
|---|---|
| Purpose | Span/event facade with near-zero cost when no subscriber is installed. |
| Size | `tracing` + `tracing-core` (+ `pin-project-lite`, `once_cell`, `log`-free with our features). Small. |
| `unsafe` | Present *inside* `tracing-core` (callsite registry, dispatch). Our core crates remain `forbid(unsafe_code)`; this is the same class of exception as any dependency, not a relaxation of the rule for our code. |
| License | MIT (allowed by `deny.toml`). |
| Maintenance | `tokio-rs`; widely used, actively maintained. |
| Replaceable? | Yes: core crates only see `diagnostics`; swapping the facade is one crate's change. |
| In core crates? | Only via `diagnostics` (facade only). `collector` feature: `app` and dev-dependencies only. |

## Alternatives rejected

- **`log` crate.** Flat records, no spans, no structured fields — cannot express a per-stage timing breakdown, and "which guard fired" would be string matching.
- **Hand-rolled only (no `tracing`).** We would re-implement span timing, callsite caching and a facade other tooling (`tokio-console`, `tracing-flame`, future tooling) already understands, and lose the zero-overhead disabled path.
- **`tracing-subscriber` as the default sink.** Pulls a dependency tree for features unused here; and its convenience makes "print from a core crate" one line away — the boundary we most want to keep hard.
- **`metrics`/`prometheus` crates.** Solve exporting, which we explicitly do not do (no telemetry). Relaxed atomics are all the baseline needs.
- **Per-page counters now.** Correct end state, but there is no page-thread model to hang them on until M30/M31. Global counters + delta assertions get the visibility now without designing that early.

## Consequences

- Every silent degradation becomes visible in `app --trace` and assertable in a test.
- Adding an instrumented crate means depending on `diagnostics`, not `tracing`.
- Thread-local `capture` cannot see events from other threads (`rayon` workers, fetch threads); tests must assert on calling-thread events or install a global collector. Documented on `capture`.
- Global counters mean parallel tests cannot assert exact values; deltas only.
- M30 must decide whether page threads get their own counter set; the `metrics` API is shaped so that change is local.
