#!/usr/bin/env bash
# Enforces architecture doc §2, "Rules for `unsafe`":
#
#   `#![forbid(unsafe_code)]` in every core crate. `unsafe` permitted
#   only in platform-shell crates, at the smallest possible boundary.
#
# This goes one step further than the doc: NO platform crate needs
# `unsafe` today either, so every crate must forbid it unless it is
# listed (with a reason) in scripts/unsafe-exceptions.txt, which is empty.
#
# Two checks:
#   1. Every crate in scripts/core-crates.txt AND scripts/platform-crates.txt
#      forbids unsafe — either `[lints.rust] unsafe_code = "forbid"` in its
#      Cargo.toml or `#![forbid(unsafe_code)]` in its src/lib.rs — unless it
#      is listed in scripts/unsafe-exceptions.txt.
#   2. Every directory under crates/ is classified as core
#      (core-crates.txt) or platform (platform-crates.txt). An
#      unclassified crate FAILS, so a new core crate cannot silently
#      skip check 1 by never being added to the list.
#
# Overridable for scripts/test-guardrails.sh, which runs this against a
# synthetic workspace:  CRATES_DIR, CORE_CRATES_FILE, PLATFORM_CRATES_FILE,
# UNSAFE_EXCEPTIONS_FILE.

set -uo pipefail
cd "$(dirname "$0")/.."

CRATES_DIR="${CRATES_DIR:-crates}"
CORE_CRATES_FILE="${CORE_CRATES_FILE:-scripts/core-crates.txt}"
PLATFORM_CRATES_FILE="${PLATFORM_CRATES_FILE:-scripts/platform-crates.txt}"
UNSAFE_EXCEPTIONS_FILE="${UNSAFE_EXCEPTIONS_FILE:-scripts/unsafe-exceptions.txt}"

read_list() {
    # Strip comments and blank lines.
    sed -e 's/#.*$//' -e 's/[[:space:]]*$//' "$1" | grep -v '^$' || true
}

# True if Cargo.toml has `unsafe_code = "forbid"` (or the inline-table
# form `{ level = "forbid" ... }`) inside a [lints.rust] table.
manifest_forbids_unsafe() {
    awk '
        /^\[/ { in_lints = ($0 ~ /^\[lints\.rust\][[:space:]]*$/) ; next }
        in_lints && /^[[:space:]]*unsafe_code[[:space:]]*=/ {
            if ($0 ~ /"forbid"/) found = 1
        }
        END { exit found ? 0 : 1 }
    ' "$1"
}

source_forbids_unsafe() {
    [ -f "$1" ] && grep -Eq '^[[:space:]]*#!\[forbid\(unsafe_code\)\]' "$1"
}

failed=0
mapfile -t core < <(read_list "$CORE_CRATES_FILE")
mapfile -t platform < <(read_list "$PLATFORM_CRATES_FILE")
mapfile -t exceptions < <(read_list "$UNSAFE_EXCEPTIONS_FILE")

for crate in "${core[@]}" "${platform[@]}"; do
    dir="$CRATES_DIR/$crate"
    if [ ! -f "$dir/Cargo.toml" ]; then
        echo "FAIL: ${crate} is listed as a crate but ${dir}/Cargo.toml does not exist"
        failed=1
        continue
    fi
    excepted=0
    for e in "${exceptions[@]}"; do
        [ "$e" = "$crate" ] && excepted=1
    done
    if [ "$excepted" -eq 1 ]; then
        echo "note: ${crate} is listed in $(basename "$UNSAFE_EXCEPTIONS_FILE") (allowed to use unsafe)"
        continue
    fi
    if manifest_forbids_unsafe "$dir/Cargo.toml" \
        || source_forbids_unsafe "$dir/src/lib.rs" \
        || source_forbids_unsafe "$dir/src/main.rs"; then
        echo "OK:   ${crate} forbids unsafe_code"
    else
        echo "FAIL: crate ${crate} does not forbid unsafe_code (and is not in $(basename "$UNSAFE_EXCEPTIONS_FILE"))"
        echo "      add to ${dir}/Cargo.toml:  [lints.rust]  unsafe_code = \"forbid\""
        failed=1
    fi
done

# Every crate directory must be classified.
for dir in "$CRATES_DIR"/*/; do
    [ -f "${dir}Cargo.toml" ] || continue
    name="$(basename "$dir")"
    classified=0
    for c in "${core[@]}" "${platform[@]}"; do
        [ "$c" = "$name" ] && classified=1
    done
    if [ "$classified" -eq 0 ]; then
        echo "FAIL: ${name} is in ${CRATES_DIR}/ but listed in neither $(basename "$CORE_CRATES_FILE") nor $(basename "$PLATFORM_CRATES_FILE")"
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    echo ""
    echo "unsafe-code guardrail violated. See architecture doc §2."
    exit 1
fi

echo ""
echo "unsafe-code guardrail holds: every crate forbids unsafe_code."
