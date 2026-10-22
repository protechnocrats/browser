#!/usr/bin/env bash
# ADR 007: page threads contain panics with catch_unwind, which only works
# with panic = "unwind". Fails if any manifest in the workspace (or the fuzz
# crate) sets panic = "abort" in any profile.
#
# Overridable for the self-test: PANIC_MANIFESTS (space-separated paths).

set -uo pipefail
cd "$(dirname "$0")/.."

manifests="${PANIC_MANIFESTS:-$(git ls-files '*Cargo.toml' 2>/dev/null || find . -name Cargo.toml -not -path './target/*')}"
hits="$(grep -nE '^[[:space:]]*panic[[:space:]]*=[[:space:]]*"abort"' $manifests 2>/dev/null || true)"
if [ -n "$hits" ]; then
    echo "FAIL: panic = \"abort\" defeats page-thread crash containment (docs/adr/007-panic-containment.md):"
    echo "$hits" | sed 's/^/  /'
    exit 1
fi
echo "Panic strategy holds: no profile sets panic = \"abort\"."
