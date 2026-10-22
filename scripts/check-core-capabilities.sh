#!/usr/bin/env bash
# Capability-based (architecture doc §1), ADR 016: a core crate never starts
# a process or reads the environment. Starting a renderer process is a
# capability the host grants the engine (`engine::RendererLauncher`); the
# environment belongs to the host too. Scans each core crate's non-test
# library source (everything before a top-level `#[cfg(test)] mod tests`;
# not `main.rs` or `src/bin/` — a binary is a host, like `conformance`'s CLI).
#
# Overridable for the self-test: CORE_CRATES_FILE, CRATES_DIR.

set -uo pipefail
cd "$(dirname "$0")/.."

CORE_CRATES_FILE="${CORE_CRATES_FILE:-scripts/core-crates.txt}"
CRATES_DIR="${CRATES_DIR:-crates}"
FORBIDDEN='process::Command|Command::new|env::var|env::vars|env::args'

failed=0
for crate in $(sed -e 's/#.*$//' "$CORE_CRATES_FILE"); do
    [ -d "$CRATES_DIR/$crate/src" ] || continue
    while IFS= read -r file; do
        # Test modules, and a crate's binaries (a binary is a host).
        case "$file" in */tests.rs | */main.rs | */src/bin/*) continue ;; esac
        hits="$(awk '/^#\[cfg\(test\)\]/ { pending = 1; next } pending && /^mod tests/ { exit } { pending = 0; print FILENAME ":" FNR ": " $0 }' "$file" | grep -E "$FORBIDDEN" || true)"
        if [ -n "$hits" ]; then
            echo "FAIL: core crate \`$crate\` starts processes or reads the environment (a host capability; ADR 016):"
            echo "$hits" | sed 's/^/  /'
            failed=1
        fi
    done < <(find "$CRATES_DIR/$crate/src" -name '*.rs')
done
if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "Core capabilities hold: no core crate starts a process or reads the environment."
