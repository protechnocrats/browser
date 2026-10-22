#!/usr/bin/env bash
# Milestone 29 (docs/adr/005-engine-api-shape.md): a HOST — `app`, `renderer`
# (ADR 016), `browser_core` (ADR 010), or any `shell_*` crate — drives the pipeline only through the `engine` facade.
# It must not depend DIRECTLY on the engine's internal crates, or "the
# engine is completely separate" is one `use` statement from being false.
#
# Checks direct, normal (non-dev) dependencies only: a host of course
# reaches these crates transitively, through `engine`.
#
# Overridable for the self-test (scripts/test-guardrails.sh): MANIFEST, HOSTS.

set -uo pipefail
cd "$(dirname "$0")/.."

MANIFEST="${MANIFEST:-Cargo.toml}"
PLATFORM_CRATES_FILE="${PLATFORM_CRATES_FILE:-scripts/platform-crates.txt}"
if [ -z "${HOSTS:-}" ]; then
    HOSTS="app browser_core conformance renderer $(sed -e 's/#.*$//' "$PLATFORM_CRATES_FILE" | grep -E '^shell_' | tr '\n' ' ')"
fi
INTERNAL="dom html_parser css_parser style layout script js"

failed=0
for host in $HOSTS; do
    direct="$(cargo tree --manifest-path "$MANIFEST" -p "$host" -e normal --depth 1 --prefix none -f '{p}' 2>&1)"
    if [ $? -ne 0 ]; then
        echo "  FAIL: \`cargo tree -p ${host}\` errored:"
        echo "$direct" | sed 's/^/    /'
        failed=1
        continue
    fi
    # First line is the host itself; the rest are its direct dependencies.
    names="$(echo "$direct" | tail -n +2 | awk '{print $1}' | sort -u)"
    for internal in $INTERNAL; do
        if echo "$names" | grep -qx "$internal"; then
            echo "  FAIL: host \`${host}\` depends directly on engine-internal crate \`${internal}\` (use the \`engine\` API; ADR 005)"
            failed=1
        fi
    done
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "Host boundary holds: hosts ($HOSTS) use only the engine facade."
