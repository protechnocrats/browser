#!/usr/bin/env bash
# Fails when the vendored Public Suffix List snapshot is too old
# (docs/adr/002-public-suffix-list.md). A stale list means new public
# suffixes are missing — cookies scoped to them would not be recognised as
# supercookies — so staleness is a security property, not housekeeping.
#
# Overridable for testing:  PSL_FILE, TODAY (YYYY-MM-DD), MAX_AGE_DAYS.

set -uo pipefail
cd "$(dirname "$0")/.."

PSL_FILE="${PSL_FILE:-crates/origin/data/public_suffix_list.dat}"
MAX_AGE_DAYS="${MAX_AGE_DAYS:-180}"
TODAY="${TODAY:-$(date +%F)}"

version="$(sed -n 's|^// VERSION: ||p' "$PSL_FILE" | head -1)"
if [ -z "$version" ]; then
    echo "FAIL: $PSL_FILE has no '// VERSION:' line"
    exit 1
fi
snapshot_date="${version%%_*}"   # 2026-09-18_18-42-54_UTC -> 2026-09-18

age_days="$(python3 - "$snapshot_date" "$TODAY" <<'PY'
import sys, datetime
a = datetime.date.fromisoformat(sys.argv[1]); b = datetime.date.fromisoformat(sys.argv[2])
print((b - a).days)
PY
)" || { echo "FAIL: could not parse dates ($snapshot_date, $TODAY)"; exit 1; }

if [ "$age_days" -gt "$MAX_AGE_DAYS" ]; then
    echo "FAIL: the Public Suffix List snapshot is $age_days days old (limit $MAX_AGE_DAYS; snapshot $snapshot_date)."
    echo "      Run scripts/update-public-suffix-list.sh, test, and commit the new snapshot."
    exit 1
fi
echo "OK:   Public Suffix List snapshot is $age_days days old (limit $MAX_AGE_DAYS; snapshot $snapshot_date)"
