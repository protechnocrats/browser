#!/usr/bin/env bash
# Enforces the frozen invariant (architecture doc §3, and the
# "freeze the dependency direction" discussion):
#
#   dom / html_parser / css_parser / style / layout / rendering
#   must NEVER depend on wgpu or winit (or, later, any other
#   platform/OS/UI-framework crate). Only gpu_backend and
#   shell_desktop (and future shell_* crates) may.
#
# This is meant to run in CI on every change, not just be read once.
# Exit code is non-zero if the invariant is broken, OR if a crate's
# manifest fails to parse at all — wire this in as a required check.

set -uo pipefail
cd "$(dirname "$0")/.."

# Single source of truth shared with check-forbid-unsafe.sh.
CORE_CRATES_FILE="${CORE_CRATES_FILE:-scripts/core-crates.txt}"
mapfile -t CORE_CRATES < <(sed -e 's/#.*$//' -e 's/[[:space:]]*$//' "$CORE_CRATES_FILE" | grep -v '^$')
FORBIDDEN_DEPS="wgpu|winit"

# Milestone 27 / docs/adr/001-observability-facade.md: core crates emit
# diagnostics through the `diagnostics` facade only. A tracing SUBSCRIBER
# (an output sink) is the host's choice and must never be a dependency of
# a core crate — otherwise "print from the engine" is one line away.
FORBIDDEN_SUBSCRIBERS="tracing-subscriber|tracing-appender|tracing-log|tracing-tree|tracing-journald|env_logger|fern|log4rs|flexi_logger"

failed=0

for crate in "${CORE_CRATES[@]}"; do
    echo "Checking ${crate}..."
    tree_output="$(cargo tree -p "$crate" -e normal --prefix none 2>&1)"
    tree_status=$?

    if [ "$tree_status" -ne 0 ]; then
        echo "  FAIL: \`cargo tree -p ${crate}\` errored (broken manifest?):"
        echo "$tree_output" | sed 's/^/    /'
        failed=1
        continue
    fi

    crate_ok=1

    leaked="$(echo "$tree_output" | grep -Ei "$FORBIDDEN_DEPS" || true)"
    if [ -n "$leaked" ]; then
        echo "  FAIL: ${crate} depends on a forbidden platform crate:"
        echo "$leaked" | sed 's/^/    /'
        failed=1
        crate_ok=0
    fi

    subscribers="$(echo "$tree_output" | grep -E "^($FORBIDDEN_SUBSCRIBERS) " || true)"
    if [ -n "$subscribers" ]; then
        echo "  FAIL: ${crate} depends on a tracing subscriber / logging sink (see docs/adr/001-observability-facade.md):"
        echo "$subscribers" | sed 's/^/    /'
        failed=1
        crate_ok=0
    fi

    # The `collector` feature (an in-process subscriber) is for `app` and
    # dev-dependencies only; `cargo tree -e normal` shows features enabled
    # through NORMAL dependency edges of this crate alone.
    features="$(cargo tree -p "$crate" -e normal --prefix none -f '{p} [{f}]' 2>/dev/null | grep -E '^diagnostics ' || true)"
    if echo "$features" | grep -q 'collector'; then
        echo "  FAIL: ${crate} enables diagnostics' \`collector\` feature in its normal dependencies (dev-dependencies only):"
        echo "$features" | sed 's/^/    /'
        failed=1
        crate_ok=0
    fi

    [ "$crate_ok" -eq 1 ] && echo "  OK"
done

if [ "$failed" -ne 0 ]; then
    echo ""
    echo "Dependency-direction invariant violated. See architecture doc §3."
    echo "A core crate must never import wgpu/winit — only gpu_backend and"
    echo "shell_desktop (or a future shell_*) are allowed to."
    exit 1
fi

echo ""
echo "Dependency-direction invariant holds: core crates are platform-free and subscriber-free."

# Milestone 29: and hosts reach the pipeline only through `engine`.
bash scripts/check-host-boundary.sh || exit 1

# Milestone 30 / ADR 007: panic containment needs panic = "unwind".
bash scripts/check-panic-strategy.sh || exit 1

# Performance audit: untraced GC handles in `js` are released while Boa's
# heap is alive (a heap use-after-free otherwise).
bash scripts/check-gc-handles.sh || exit 1

# ADR 016: starting processes and reading the environment are host
# capabilities, never a core crate's.
bash scripts/check-core-capabilities.sh || exit 1

# Milestone 36 / ADR 010: authority is a handle the browser grants — no
# global state in the script bindings, minting only in Browser Core and the
# renderer.
bash scripts/check-capabilities.sh || exit 1
# Milestone 39 (ADR 021): only `dom` walks or edits owned trees, fragment
# builders excepted.
bash scripts/check-dom-encapsulation.sh || exit 1
