#!/usr/bin/env bash
# Self-test for the CI guardrail scripts: proves they actually FAIL when
# they should (a guardrail that has never been seen to fail is not yet
# evidence of anything). Builds a throwaway synthetic workspace under a
# temp dir, runs check-forbid-unsafe.sh against it, and cleans up.
#
# Milestone 25 acceptance: "the script fails a synthetic core crate that
# omits [forbid(unsafe_code)]".

set -uo pipefail
cd "$(dirname "$0")/.."

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/crates/synthetic_core/src" "$tmp/crates/synthetic_platform/src"
printf 'synthetic_core\n' > "$tmp/core.txt"
printf 'synthetic_platform\n' > "$tmp/platform.txt"
printf '[package]\nname = "synthetic_platform"\nversion = "0.0.0"\n' > "$tmp/crates/synthetic_platform/Cargo.toml"

# For the core-crate cases below the synthetic PLATFORM crate is excepted, so
# it cannot fail them for its own reasons; cases 7-8 test it directly.
printf 'synthetic_platform\n' > "$tmp/exceptions.txt"

run() {
    CRATES_DIR="$tmp/crates" CORE_CRATES_FILE="$tmp/core.txt" PLATFORM_CRATES_FILE="$tmp/platform.txt" \
        UNSAFE_EXCEPTIONS_FILE="$tmp/exceptions.txt" bash scripts/check-forbid-unsafe.sh >/dev/null 2>&1
}

failures=0
expect() { # expect <pass|fail> <description>
    local want="$1" desc="$2" rc=0
    run || rc=$?
    if { [ "$want" = pass ] && [ "$rc" -eq 0 ]; } || { [ "$want" = fail ] && [ "$rc" -ne 0 ]; }; then
        echo "ok:   $desc"
    else
        echo "FAIL: $desc (expected $want, exit code $rc)"
        failures=1
    fi
}

# 1. Core crate with NO forbid must fail.
printf '[package]\nname = "synthetic_core"\nversion = "0.0.0"\n' > "$tmp/crates/synthetic_core/Cargo.toml"
echo '' > "$tmp/crates/synthetic_core/src/lib.rs"
expect fail "core crate without forbid(unsafe_code) is rejected"

# 2. `unsafe_code = "warn"` (weaker than forbid) must still fail.
printf '[package]\nname = "synthetic_core"\nversion = "0.0.0"\n[lints.rust]\nunsafe_code = "warn"\n' > "$tmp/crates/synthetic_core/Cargo.toml"
expect fail "core crate with only unsafe_code = \"warn\" is rejected"

# 3. A forbid under the WRONG table must not count.
printf '[package]\nname = "synthetic_core"\nversion = "0.0.0"\n[lints.clippy]\nunsafe_code = "forbid"\n' > "$tmp/crates/synthetic_core/Cargo.toml"
expect fail "forbid under the wrong lints table is rejected"

# 4. Manifest-level forbid passes.
printf '[package]\nname = "synthetic_core"\nversion = "0.0.0"\n[lints.rust]\nunsafe_code = "forbid"\n' > "$tmp/crates/synthetic_core/Cargo.toml"
expect pass "core crate with [lints.rust] unsafe_code = \"forbid\" passes"

# 5. Source-level forbid passes.
printf '[package]\nname = "synthetic_core"\nversion = "0.0.0"\n' > "$tmp/crates/synthetic_core/Cargo.toml"
printf '#![forbid(unsafe_code)]\n' > "$tmp/crates/synthetic_core/src/lib.rs"
expect pass "core crate with #![forbid(unsafe_code)] in lib.rs passes"

# 6. A brand-new, unclassified crate must fail (cannot skip by omission).
mkdir -p "$tmp/crates/new_unlisted/src"
printf '[package]\nname = "new_unlisted"\nversion = "0.0.0"\n' > "$tmp/crates/new_unlisted/Cargo.toml"
expect fail "a crate in crates/ listed in neither core nor platform list is rejected"

# 7. A PLATFORM crate that does not forbid unsafe must fail too (the
#    guardrail is stricter than the architecture doc requires)...
rm -rf "$tmp/crates/new_unlisted"
printf '' > "$tmp/exceptions.txt"
expect fail "a platform crate without forbid(unsafe_code) is rejected unless excepted"

# 8. ...and passes when it is explicitly excepted, or when it forbids.
printf 'synthetic_platform\n' > "$tmp/exceptions.txt"
expect pass "a platform crate listed in the exceptions file is allowed"
printf '' > "$tmp/exceptions.txt"
printf '[package]\nname = "synthetic_platform"\nversion = "0.0.0"\n[lints.rust]\nunsafe_code = "forbid"\n' > "$tmp/crates/synthetic_platform/Cargo.toml"
expect pass "a platform crate that forbids unsafe passes"

# ---- check-host-boundary.sh (Milestone 29) ----
# A synthetic workspace: a host crate `app` and stand-ins for `dom` and
# `engine`. The host depending on `dom` directly must fail; on `engine`
# (which itself depends on `dom`) must pass.
hw="$tmp/hostws"
mkdir -p "$hw/app/src" "$hw/dom/src" "$hw/engine/src"
printf '[workspace]\nmembers = ["app", "dom", "engine"]\nresolver = "2"\n' > "$hw/Cargo.toml"
printf '[package]\nname = "dom"\nversion = "0.0.0"\nedition = "2021"\n' > "$hw/dom/Cargo.toml"
printf '[package]\nname = "engine"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\ndom = { path = "../dom" }\n' > "$hw/engine/Cargo.toml"
for c in app dom engine; do echo '' > "$hw/$c/src/lib.rs"; done
host_check() { MANIFEST="$hw/Cargo.toml" HOSTS="app" bash scripts/check-host-boundary.sh >/dev/null 2>&1; }
check_host() { # check_host <pass|fail> <description>
    local want="$1" desc="$2" rc=0
    host_check || rc=$?
    if { [ "$want" = pass ] && [ "$rc" -eq 0 ]; } || { [ "$want" = fail ] && [ "$rc" -ne 0 ]; }; then
        echo "ok:   $desc"
    else
        echo "FAIL: $desc (expected $want, exit code $rc)"
        failures=1
    fi
}
printf '[package]\nname = "app"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\ndom = { path = "../dom" }\n' > "$hw/app/Cargo.toml"
check_host fail "a host depending directly on an engine-internal crate is rejected"
printf '[package]\nname = "app"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nengine = { path = "../engine" }\n' > "$hw/app/Cargo.toml"
check_host pass "a host depending only on the engine facade passes"
printf '[package]\nname = "app"\nversion = "0.0.0"\nedition = "2021"\n[dependencies]\nengine = { path = "../engine" }\n[dev-dependencies]\ndom = { path = "../dom" }\n' > "$hw/app/Cargo.toml"
check_host pass "an internal crate as a dev-dependency (tests only) is allowed"

# ---- check-panic-strategy.sh (Milestone 30, ADR 007) ----
pm="$tmp/panic"
mkdir -p "$pm"
printf '[package]\nname = "x"\n[profile.release]\npanic = "abort"\n' > "$pm/Cargo.toml"
if PANIC_MANIFESTS="$pm/Cargo.toml" bash scripts/check-panic-strategy.sh >/dev/null 2>&1; then
    echo "FAIL: a profile with panic = \"abort\" was accepted"; failures=1
else
    echo "ok:   a profile with panic = \"abort\" is rejected"
fi
printf '[package]\nname = "x"\n[profile.release]\npanic = "unwind"\n' > "$pm/Cargo.toml"
if PANIC_MANIFESTS="$pm/Cargo.toml" bash scripts/check-panic-strategy.sh >/dev/null 2>&1; then
    echo "ok:   panic = \"unwind\" passes"
else
    echo "FAIL: panic = \"unwind\" was rejected"; failures=1
fi

# 9. The PSL freshness check fails on a stale snapshot and passes on a fresh one.
printf '// VERSION: 2026-01-01_00-00-00_UTC\ncom\n' > "$tmp/psl.dat"
if PSL_FILE="$tmp/psl.dat" TODAY=2026-09-20 bash scripts/check-psl-freshness.sh >/dev/null 2>&1; then
    echo "FAIL: a 262-day-old PSL snapshot must be rejected"; failures=1
else
    echo "ok:   a stale PSL snapshot is rejected"
fi
if PSL_FILE="$tmp/psl.dat" TODAY=2026-02-01 bash scripts/check-psl-freshness.sh >/dev/null 2>&1; then
    echo "ok:   a fresh PSL snapshot passes"
else
    echo "FAIL: a 31-day-old PSL snapshot must pass"; failures=1
fi
printf 'com\n' > "$tmp/noversion.dat"
if PSL_FILE="$tmp/noversion.dat" bash scripts/check-psl-freshness.sh >/dev/null 2>&1; then
    echo "FAIL: a PSL file with no VERSION must be rejected"; failures=1
else
    echo "ok:   a PSL file with no VERSION line is rejected"
fi

# ---- check-core-capabilities.sh (ADR 016) ----
cc="$tmp/capabilities"
mkdir -p "$cc/crates/corex/src"
printf 'corex\n' > "$cc/core.txt"
printf 'pub fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = std::env::var("X"); }\n}\n' > "$cc/crates/corex/src/lib.rs"
if CORE_CRATES_FILE="$cc/core.txt" CRATES_DIR="$cc/crates" bash scripts/check-core-capabilities.sh >/dev/null 2>&1; then
    echo "ok:   environment reads in a core crate's tests pass"
else
    echo "FAIL: a core crate's test module was flagged"; failures=1
fi
printf 'pub fn f() { let _ = std::process::Command::new("sh"); }\n' > "$cc/crates/corex/src/lib.rs"
if CORE_CRATES_FILE="$cc/core.txt" CRATES_DIR="$cc/crates" bash scripts/check-core-capabilities.sh >/dev/null 2>&1; then
    echo "FAIL: a core crate starting a process was accepted"; failures=1
else
    echo "ok:   a core crate starting a process is rejected"
fi
printf 'pub fn f() -> Option<String> { std::env::var("HOME").ok() }\n' > "$cc/crates/corex/src/lib.rs"
if CORE_CRATES_FILE="$cc/core.txt" CRATES_DIR="$cc/crates" bash scripts/check-core-capabilities.sh >/dev/null 2>&1; then
    echo "FAIL: a core crate reading the environment was accepted"; failures=1
else
    echo "ok:   a core crate reading the environment is rejected"
fi

# ---- check-gc-handles.sh (performance audit: Boa heap use-after-free) ----
gc="$tmp/gc"
mkdir -p "$gc"
printf 'Rc<RefCell<Vec<JsFunction>>> | raf_callbacks\n' > "$gc/allow.txt"
cat > "$gc/lib.rs" <<'RS'
impl Drop for JsEnvironment {
    fn drop(&mut self) {
        let raf = std::mem::take(&mut *self.raf_callbacks.borrow_mut());
        drop(raf);
    }
}
RS
cat > "$gc/ok.rs" <<'RS'
struct C {
    #[unsafe_ignore_trace]
    callbacks: Rc<RefCell<Vec<JsFunction>>>,
    #[unsafe_ignore_trace]
    dirty: Rc<Cell<bool>>,
}
RS
gc_check() { GC_SOURCES="$1" GC_ALLOWLIST="$gc/allow.txt" GC_DROP_FILE="$2" bash scripts/check-gc-handles.sh >/dev/null 2>&1; }
if gc_check "$gc/ok.rs" "$gc/lib.rs"; then
    echo "ok:   untraced handles that Drop releases pass"
else
    echo "FAIL: untraced handles that Drop releases were rejected"; failures=1
fi
printf 'struct D {\n    #[unsafe_ignore_trace]\n    held: Rc<RefCell<Vec<JsObject>>>,\n}\n' > "$gc/new.rs"
if gc_check "$gc/new.rs" "$gc/lib.rs"; then
    echo "FAIL: a new untraced capture holding a GC handle was accepted"; failures=1
else
    echo "ok:   a new untraced capture holding a GC handle is rejected"
fi
printf 'impl Drop for JsEnvironment {\n    fn drop(&mut self) {}\n}\n' > "$gc/lib_empty.rs"
if gc_check "$gc/ok.rs" "$gc/lib_empty.rs"; then
    echo "FAIL: a listed handle Drop does not release was accepted"; failures=1
else
    echo "ok:   a listed handle that Drop does not release is rejected"
fi

# ---- check-capabilities.sh (Milestone 36, ADR 010) ----
cap="$tmp/capcheck"
mkdir -p "$cap/js/src" "$cap/crates/core_x/src" "$cap/crates/browser_core/src"
printf '#[cfg(any(test, feature = "test-support"))]\nmod test_support;\n' > "$cap/js/src/lib.rs"
printf 'pub fn f() {}\n' > "$cap/js/src/test_support.rs"
printf 'pub fn g() {}\n' > "$cap/crates/core_x/src/lib.rs"
printf 'pub fn mint() { let _ = Authority::new(); }\n' > "$cap/crates/browser_core/src/lib.rs"
printf '[package]\nname = "x"\n[dependencies]\njs = { path = "../js" }\n[dev-dependencies]\njs = { path = "../js", features = ["test-support"] }\n' > "$cap/Cargo.toml"
cap_check() { JS_SRC="$cap/js/src" SOURCE_ROOTS="$cap/crates" MINT_ALLOWED="$cap/crates/browser_core/src/" MANIFESTS="$cap/Cargo.toml" bash scripts/check-capabilities.sh >/dev/null 2>&1; }
cap_expect() { # cap_expect <pass|fail> <description>
    local want="$1" desc="$2" rc=0
    cap_check || rc=$?
    if { [ "$want" = pass ] && [ "$rc" -eq 0 ]; } || { [ "$want" = fail ] && [ "$rc" -ne 0 ]; }; then
        echo "ok:   $desc"
    else
        echo "FAIL: $desc (expected $want, exit code $rc)"
        failures=1
    fi
}
cap_expect pass "minting in Browser Core and test-support in dev-dependencies pass"
printf 'static STATE: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());\n' > "$cap/js/src/state.rs"
cap_expect fail "a global Mutex in the js bindings is rejected"
printf 'thread_local! {\n    static T: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };\n}\n' > "$cap/js/src/state.rs"
cap_expect fail "a thread_local in the js bindings is rejected"
printf '#[cfg(test)]\nthread_local! {\n    static T: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };\n}\npub fn after() {}\n' > "$cap/js/src/state.rs"
cap_expect pass "a test-only thread_local passes"
rm "$cap/js/src/state.rs"
printf 'pub fn g() { let _ = capability::Authority::new(); }\n' > "$cap/crates/core_x/src/lib.rs"
cap_expect fail "minting outside Browser Core and the renderer is rejected"
printf '#[cfg(test)]\npub fn g() {\n    let _ = capability::Authority::new();\n}\n' > "$cap/crates/core_x/src/lib.rs"
cap_expect pass "minting in a test-only function passes"
printf 'pub fn g() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = Authority::new(); }\n}\npub fn h() { let _ = Authority::new(); }\n' > "$cap/crates/core_x/src/lib.rs"
cap_expect fail "minting after a test module ends is still caught"
printf 'pub fn g() {}\n' > "$cap/crates/core_x/src/lib.rs"
printf 'mod test_support;\n' > "$cap/js/src/lib.rs"
cap_expect fail "an ungated test_support module is rejected"
printf '#[cfg(any(test, feature = "test-support"))]\nmod test_support;\n' > "$cap/js/src/lib.rs"
printf '[package]\nname = "x"\n# test-support is for tests\n[dependencies]\njs = { path = "../js", features = ["test-support"] }\n' > "$cap/Cargo.toml"
cap_expect fail "test-support enabled in a normal dependency is rejected"

# ---- check-dom-encapsulation.sh (Milestone 39, ADR 021) ----
domc="$tmp/domcheck"
mkdir -p "$domc"
printf '# file | reason\n%s/builder.rs | builds fragments\n' "$domc" > "$domc/allow.txt"
printf 'fn read(e: ElementRef<'"'"'_>) -> usize { e.element_children().count() }\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = Node::Element(Element::new("p")); }\n}\n' > "$domc/reader.rs"
printf 'fn build(e: &mut Element) { e.children.push(Node::Text(String::new())); }\n' > "$domc/builder.rs"
dom_check() { DOM_SOURCES="$1" DOM_ALLOWLIST="$domc/allow.txt" bash scripts/check-dom-encapsulation.sh >/dev/null 2>&1; }
if dom_check "$domc/reader.rs $domc/builder.rs"; then
    echo "ok:   readers through ElementRef, test trees and listed fragment builders pass"
else
    echo "FAIL: readers, test trees or a listed fragment builder were rejected"; failures=1
fi
printf 'fn walk(e: &mut dom::Element) {}\n' > "$domc/walker.rs"
if dom_check "$domc/walker.rs"; then
    echo "FAIL: an unlisted &mut Element walk was accepted"; failures=1
else
    echo "ok:   an unlisted &mut Element walk is rejected"
fi
printf 'fn f(n: &Node) -> bool { matches!(n, dom::Node::Element(_)) }\n' > "$domc/matcher.rs"
if dom_check "$domc/matcher.rs"; then
    echo "FAIL: an unlisted match on Node::Element was accepted"; failures=1
else
    echo "ok:   an unlisted match on Node::Element is rejected"
fi

if [ "$failures" -ne 0 ]; then
    echo ""
    echo "guardrail self-test FAILED"
    exit 1
fi
echo ""
echo "guardrail self-test passed: the guardrails fail when they should."
