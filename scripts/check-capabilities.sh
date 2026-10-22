#!/usr/bin/env bash
# Milestone 36 (docs/adr/010-capabilities.md): authority is a handle, granted
# by the browser — never ambient. Four checks:
#
# 1. The script bindings (`js`, and since Milestone 40 the host side in
#    `script`) reach no global mutable state: no `static mut`, no
#    `static` with interior mutability (locks, atomics, cells, lazies), no
#    `thread_local!`. What script can affect is what its capability handles
#    and its own document reach.
# 2. `capability::Authority` — the one way to mint a capability — is created
#    only by Browser Core (the profile) and by a renderer re-creating the
#    grants its host listed (`engine/src/renderer.rs`). Test code is exempt:
#    test modules, `tests/`, `examples/`, and `js`'s `test_support.rs`.
# 3. `js`'s `test_support` module (constructors that mint) is compiled only
#    under `cfg(any(test, feature = "test-support"))`.
# 4. No manifest enables `js`'s `test-support` feature in a normal
#    dependency (dev-dependencies, and the separate fuzz workspace, may).
#
# Overridable for the self-test (scripts/test-guardrails.sh): JS_SRC,
# SOURCE_ROOTS, MINT_ALLOWED, MANIFESTS.

set -uo pipefail
cd "$(dirname "$0")/.."

JS_SRC="${JS_SRC:-crates/js/src}"
# Every directory checked for global state (1); `JS_SRC` also holds the
# test-support module (3).
BINDING_SRCS="${BINDING_SRCS:-$JS_SRC crates/script/src}"
SOURCE_ROOTS="${SOURCE_ROOTS:-crates app}"
# Path prefixes (relative, as `find` prints them) allowed to mint.
MINT_ALLOWED="${MINT_ALLOWED:-crates/browser_core/src/ crates/engine/src/renderer.rs crates/capability/src/}"
MANIFESTS="${MANIFESTS:-$(ls crates/*/Cargo.toml app/Cargo.toml 2>/dev/null)}"

failed=0

# Prints a file's lines (as FILE:LINE: text) except the items marked
# `#[cfg(test)]` — a test module, a test-only function or static. An item
# ends where the braces it opened close, or at its `;` if it opens none.
non_test() {
    awk '
        /^[[:space:]]*#\[cfg\(test\)\]/ { skip = 1; depth = 0; opened = 0; next }
        skip {
            o = gsub(/\{/, "{"); c = gsub(/\}/, "}")
            depth += o - c
            if (o > 0) opened = 1
            if ((opened && depth <= 0) || (!opened && $0 ~ /;[[:space:]]*$/)) skip = 0
            next
        }
        { print FILENAME ":" FNR ": " $0 }
    ' "$1"
}

# 1. Global mutable state in the bindings.
GLOBAL='static[[:space:]]+mut[[:space:]]|static[[:space:]]+[A-Za-z_0-9]+[[:space:]]*:[^=]*(Mutex|RwLock|Atomic|Cell|OnceLock|OnceCell|LazyLock|LazyCell|Lazy<)|thread_local!|lazy_static!'
while IFS= read -r file; do
    case "$file" in */tests.rs | */test_support.rs) continue ;; esac
    hits="$(non_test "$file" | grep -E "$GLOBAL" || true)"
    if [ -n "$hits" ]; then
        echo "FAIL: global mutable state reachable from the js bindings (ADR 010):"
        echo "$hits" | sed 's/^/  /'
        failed=1
    fi
done < <(find $BINDING_SRCS -name '*.rs' | sort)

# 2. Minting outside the allowed places.
while IFS= read -r file; do
    case "$file" in */tests/* | */examples/* | */benches/* | */tests.rs | */test_support.rs) continue ;; esac
    allowed=0
    for prefix in $MINT_ALLOWED; do
        case "$file" in "$prefix"*) allowed=1 ;; esac
    done
    [ "$allowed" -eq 1 ] && continue
    hits="$(non_test "$file" | grep -E 'Authority::new\(' || true)"
    if [ -n "$hits" ]; then
        echo "FAIL: a capability Authority is created outside Browser Core and the renderer (ADR 010 §2):"
        echo "$hits" | sed 's/^/  /'
        failed=1
    fi
done < <(find $SOURCE_ROOTS -name '*.rs' -not -path '*/target/*' 2>/dev/null | sort)

# 3. The test-support module is gated.
if [ -f "$JS_SRC/test_support.rs" ]; then
    gate="$(grep -B1 -E '^mod test_support;' "$JS_SRC/lib.rs" | head -1)"
    if ! echo "$gate" | grep -qE '^#\[cfg\(any\(test, feature = "test-support"\)\)\]$'; then
        echo "FAIL: \`mod test_support;\` in $JS_SRC/lib.rs must be preceded by #[cfg(any(test, feature = \"test-support\"))]"
        failed=1
    fi
fi

# 4. `test-support` only in dev-dependencies.
for manifest in $MANIFESTS; do
    hits="$(awk '
        /^[[:space:]]*#/ { next }
        /^\[/ { section = $0; next }
        section ~ /^\[(target\..*\.)?dependencies\]$/ && /test-support/ { print FILENAME ":" FNR ": " $0 }
        section ~ /^\[(target\..*\.)?dependencies\.js\]$/ && /test-support/ { print FILENAME ":" FNR ": " $0 }
    ' "$manifest")"
    if [ -n "$hits" ]; then
        echo "FAIL: a normal dependency enables js's \`test-support\` feature (it mints capabilities; dev-dependencies only):"
        echo "$hits" | sed 's/^/  /'
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo "Capabilities hold: no global state in the js bindings; authority minted only by Browser Core and the renderer."
