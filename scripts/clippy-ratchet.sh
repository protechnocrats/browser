#!/usr/bin/env bash
# Clippy ratchet (Milestone 25): the warning count may go DOWN but never
# UP. The existing backlog (mostly reviewed test-code style) is not worth
# blocking every PR on yet, but it must not grow. CI flips to
# `-D warnings` after the backlog is burned down (ROADMAP.md §4).
#
#   scripts/clippy-ratchet.sh            check against scripts/clippy-baseline.txt
#   scripts/clippy-ratchet.sh --update   rewrite the baseline to the current count
#
# Counts distinct `warning:` diagnostics across the workspace,
# all targets, release profile — the same invocation CI runs.

set -uo pipefail
cd "$(dirname "$0")/.."

BASELINE_FILE="${BASELINE_FILE:-scripts/clippy-baseline.txt}"

# JSON output is the only format cargo replays faithfully from its cache
# (`--message-format=short` is ignored for cached results, which would
# make an incremental run report zero warnings).
json="$(cargo clippy --workspace --release --all-targets --message-format=json 2>&1)"

# One entry per (file, line, lint) — identical lints at different places
# count separately (deduplicating on the message alone would let a new
# instance of an existing lint slip through), while the same location
# reported by two targets (lib and lib-test) collapses.
if ! parsed="$(printf '%s\n' "$json" | python3 scripts/clippy_count.py)"; then
    echo "FAIL: could not parse clippy's JSON output (a broken parser must not read as zero warnings)"
    exit 1
fi
# Sanity: clippy's own summary must agree that there is JSON to parse.
if ! printf '%s\n' "$json" | grep -q '"reason":"build-finished"'; then
    echo "FAIL: clippy did not finish a build — refusing to report a count"
    printf '%s\n' "$json" | tail -20
    exit 1
fi

if echo "$parsed" | grep -q '^ERROR '; then
    echo "$parsed" | grep '^ERROR ' | head -20
    echo "FAIL: clippy reported a hard error (not just warnings)"
    exit 1
fi
warnings="$(echo "$parsed" | grep '^WARN ' | sed 's/^WARN //')"
current="$(echo -n "$warnings" | grep -c . || true)"
version="$(cargo clippy --version | awk '{print $2}')"

if [ "${1:-}" = "--update" ]; then
    printf 'count=%s\nclippy=%s\n' "$current" "$version" > "$BASELINE_FILE"
    echo "baseline updated: count=$current (clippy $version)"
    exit 0
fi

if [ ! -f "$BASELINE_FILE" ]; then
    echo "FAIL: no baseline at $BASELINE_FILE — run: scripts/clippy-ratchet.sh --update"
    exit 1
fi
baseline="$(sed -n 's/^count=//p' "$BASELINE_FILE")"
baseline_version="$(sed -n 's/^clippy=//p' "$BASELINE_FILE")"

if [ "$version" != "$baseline_version" ]; then
    echo "note: baseline recorded with clippy $baseline_version, running $version — new lints in a newer clippy can raise the count without a code change; if so, re-baseline deliberately with --update"
fi

if [ "$current" -gt "$baseline" ]; then
    echo "FAIL: clippy warnings increased: $baseline -> $current"
    echo "Current warnings (fix the new ones, do not raise the baseline):"
    echo "$warnings" | head -60
    exit 1
fi
if [ "$current" -lt "$baseline" ]; then
    echo "clippy warnings decreased: $baseline -> $current. Lock in the improvement: scripts/clippy-ratchet.sh --update"
else
    echo "clippy warnings unchanged at $current (baseline $baseline)"
fi
