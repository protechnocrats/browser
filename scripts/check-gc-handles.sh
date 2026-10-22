#!/usr/bin/env bash
# Soundness of untraced fields in the script runtime (ADR 015, ADR 023 §5):
# Boa does not trace a field marked `#[unsafe_ignore_trace]`, so a GC handle
# in one is outside the collector's view — kept alive as a root, never
# collected in a cycle, and (as found under AddressSanitizer with Boa 0.17)
# released after the heap it points into is freed.
#
# Since Milestone 40 every handle the runtime keeps is traced, and
# scripts/gc-handle-captures.txt is empty: this fails on ANY untraced field
# whose type can hold a handle. (The allowlist mechanism stays for the
# self-test: a listed field must be emptied in `impl Drop for JsEnvironment`.)
#
# Overridable for the self-test: GC_SOURCES (files), GC_ALLOWLIST, GC_DROP_FILE.

set -uo pipefail
cd "$(dirname "$0")/.."

sources="${GC_SOURCES:-$(ls crates/js/src/*.rs crates/script/src/*.rs)}"
allowlist="${GC_ALLOWLIST:-scripts/gc-handle-captures.txt}"
drop_file="${GC_DROP_FILE:-crates/js/src/lib.rs}"
# Types that are, or can contain, a Boa GC handle.
handle_types='JsFunction|JsObject|JsValue|JsPromise|JsString|JsArray|ResolvingFunctions|Gc<|GcRefCell|ListenerMap|PendingFetch|PendingXhr'

norm() { tr -d '[:space:]'; }
fail=0

# Every untraced field: the type after the first `:` of the line after the
# attribute (or of the same line, when written inline).
fields="$(awk '
    pending { if ($0 ~ /:/) { sub(/^[^:]*:/, ""); sub(/,[[:space:]]*$/, ""); print FILENAME ":" FNR ":" $0; pending = 0 } ; next }
    /#\[unsafe_ignore_trace\]/ {
        rest = $0; sub(/.*#\[unsafe_ignore_trace\]/, "", rest)
        if (rest ~ /:/) { sub(/^[^:]*:/, "", rest); sub(/,[[:space:]]*$/, "", rest); print FILENAME ":" FNR ":" rest } else { pending = 1 }
    }' $sources)"

drop_body="$(awk '/impl Drop for JsEnvironment/ { on = 1 } on { print } on && /^}/ { exit }' "$drop_file")"

while IFS= read -r entry; do
    [ -z "$entry" ] && continue
    location="${entry%%:*}"; rest="${entry#*:}"; line="${rest%%:*}"; type="${rest#*:}"
    if ! echo "$type" | grep -qE "$handle_types"; then
        continue
    fi
    listed="$(grep -v '^#' "$allowlist" | awk -F'|' -v t="$(echo "$type" | norm)" '{ gsub(/[[:space:]]/, "", $1); if ($1 == t) { gsub(/[[:space:]]/, "", $2); print $2 } }')"
    if [ -z "$listed" ]; then
        echo "FAIL: $location:$line: untraced capture of type \`$(echo "$type" | sed 's/^ *//')\` can hold a GC handle and is not in $allowlist"
        fail=1
        continue
    fi
    if ! echo "$drop_body" | grep -q "self\.$listed\b"; then
        echo "FAIL: $location:$line: \`$listed\` (holding \`$(echo "$type" | sed 's/^ *//')\`) is not emptied in \`impl Drop for JsEnvironment\` ($drop_file)"
        fail=1
    fi
done <<< "$fields"

if [ "$fail" -ne 0 ]; then
    echo "GC handles must be traced (ADR 023 §5): keep them in the runtime's traced tables, not in untraced fields."
    exit 1
fi
echo "No untraced field can hold a GC handle."
