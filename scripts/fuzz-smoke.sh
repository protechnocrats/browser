#!/usr/bin/env bash
# Runs every libFuzzer target for a fixed budget against its committed
# corpus (Milestone 25). Used by the `fuzz-smoke` PR job (short budget)
# and the scheduled nightly workflow (long budget).
#
#   scripts/fuzz-smoke.sh [SECONDS_PER_TARGET]     default 60
#
# Requires: nightly toolchain, `cargo install cargo-fuzz`, and a C++
# compiler (libFuzzer is C++). On a machine without those, the same
# targets' seeds/mutations/stress inputs run on stable via
#   cd fuzz && cargo test --release --no-default-features
#
# Targets are discovered from `cargo fuzz list`, so adding a [[bin]] to
# fuzz/Cargo.toml is all CI needs. Exits non-zero if ANY target crashes,
# times out, or runs out of memory; crash artifacts land in
# fuzz/artifacts/<target>/ (reproduce with the `replay` example — see
# fuzz/examples/replay.rs).

set -uo pipefail
cd "$(dirname "$0")/../fuzz"

SECS="${1:-60}"
# max_len: 64 KiB so inputs can grow past libFuzzer's 4 KiB default
# (long attribute values, deep nesting) without making every execution
# slow. timeout: any single input taking >10 s is a hang bug.
# rss_limit_mb: an allocation blow-up must FAIL the run, not OOM the runner.
LIBFUZZER_ARGS=(-max_total_time="$SECS" -max_len=65536 -timeout=10 -rss_limit_mb=2048)
# cargo-fuzz defaults --target to the triple IT was built for; prebuilt
# binaries (taiki-e/install-action) are static musl, and ASan can't link
# against static libc. Pin the build to the nightly toolchain's host.
TARGET="$(rustc +nightly -vV | sed -n 's/^host: //p')"

mapfile -t targets < <(cargo +nightly fuzz list 2>/dev/null)
if [ "${#targets[@]}" -eq 0 ]; then
    echo "FAIL: cargo fuzz list found no targets (is cargo-fuzz installed, and nightly available?)"
    exit 1
fi

echo "fuzz-smoke: ${#targets[@]} targets, ${SECS}s each: ${targets[*]}"
failed=()
for t in "${targets[@]}"; do
    echo "::group::fuzz $t (${SECS}s)"
    if cargo +nightly fuzz run --target "$TARGET" "$t" -- "${LIBFUZZER_ARGS[@]}"; then
        echo "OK: $t"
    else
        echo "FAIL: $t crashed, hung, or exceeded the memory limit (see fuzz/artifacts/$t/)"
        failed+=("$t")
    fi
    echo "::endgroup::"
done

if [ "${#failed[@]}" -ne 0 ]; then
    echo ""
    echo "fuzz-smoke FAILED: ${failed[*]}"
    echo "Reproduce:  cd fuzz && cargo run --release --no-default-features --example replay -- <target> artifacts/<target>/<crash-file>"
    exit 1
fi
echo ""
echo "fuzz-smoke passed: ${#targets[@]} targets clean for ${SECS}s each."
