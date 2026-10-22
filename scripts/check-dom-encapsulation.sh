#!/usr/bin/env bash
# Milestone 39 (ADR 021): every crate but `dom` reads documents through
# `dom::ElementRef` and changes them through `dom::Document`'s methods. Owned
# `dom::Element` trees are fragments, built and edited only where a fragment is
# what exists: the HTML parser, `js`'s detached forest, generated content,
# the loader's pass over a parsed tree before it becomes a document.
#
# Fails if non-test code outside `dom` takes `&mut Element` or matches or
# builds `Node::Element` / `Node::Text`, in a file not listed (with its reason)
# in scripts/dom-fragment-builders.txt. Test code — `tests.rs`,
# `test_support.rs`, and everything after a `#[cfg(test)]` module — is free to
# build trees by hand.
#
# Overridable for the self-test: DOM_SOURCES (files), DOM_ALLOWLIST.

set -uo pipefail
cd "$(dirname "$0")/.."

sources="${DOM_SOURCES:-$(find crates app -path '*/src/*' -name '*.rs' ! -path 'crates/dom/*' ! -name tests.rs ! -name test_support.rs | sort)}"
allowlist="${DOM_ALLOWLIST:-scripts/dom-fragment-builders.txt}"
fail=0

for file in $sources; do
    hits="$(awk '
        /^#\[cfg\(test\)\]/ { if ((getline next_line) > 0 && next_line ~ /^mod /) exit; $0 = next_line }
        /&mut (dom::)?Element([^A-Za-z0-9_]|$)|(dom::)?Node::(Element|Text)\(/ { print FNR ": " $0 }
    ' "$file")"
    [ -z "$hits" ] && continue
    if grep -v '^#' "$allowlist" | awk -F'|' '{ gsub(/[[:space:]]/, "", $1); print $1 }' | grep -qxF "$file"; then
        continue
    fi
    echo "FAIL: $file walks or edits owned DOM trees outside \`dom\`:"
    echo "$hits" | head -5 | sed 's/^/    /'
    fail=1
done

if [ "$fail" -ne 0 ]; then
    echo "Read documents through dom::ElementRef and change them through dom::Document (ADR 021);"
    echo "a genuine fragment builder goes in $allowlist with its reason."
    exit 1
fi
echo "No crate outside dom walks or edits owned DOM trees, except the listed fragment builders."
