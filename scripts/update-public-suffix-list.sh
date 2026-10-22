#!/usr/bin/env bash
# Refreshes the vendored Public Suffix List snapshot
# (crates/origin/data/public_suffix_list.dat). Run by a human, reviewed as a
# normal diff — the engine never fetches the list at runtime
# (docs/adr/002-public-suffix-list.md).
#
#   scripts/update-public-suffix-list.sh
#
# The download is validated before it replaces the old snapshot: it must
# carry a VERSION line, both list sections, and a plausible rule count, so a
# truncated or error-page response cannot silently become the list.

set -euo pipefail
cd "$(dirname "$0")/.."

URL="https://publicsuffix.org/list/public_suffix_list.dat"
DEST="crates/origin/data/public_suffix_list.dat"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

version_of() { sed -n 's|^// VERSION: ||p' "$1" | head -1; }

curl --fail --silent --show-error --location --max-time 60 -o "$TMP" "$URL"

grep -q '^// VERSION: ' "$TMP"                     || { echo "FAIL: no VERSION line — not the PSL?"; exit 1; }
grep -q '===BEGIN ICANN DOMAINS==='  "$TMP"        || { echo "FAIL: ICANN section missing"; exit 1; }
grep -q '===BEGIN PRIVATE DOMAINS===' "$TMP"       || { echo "FAIL: PRIVATE section missing"; exit 1; }
rules="$(grep -cv -e '^//' -e '^[[:space:]]*$' "$TMP")"
[ "$rules" -gt 5000 ]                              || { echo "FAIL: only $rules rules — truncated download?"; exit 1; }

old="$(version_of "$DEST" || true)"
new="$(version_of "$TMP")"
if cmp -s "$TMP" "$DEST"; then
    echo "already up to date ($new, $rules rules)"
    exit 0
fi
cp "$TMP" "$DEST"
echo "updated: ${old:-none} -> $new ($rules rules)"
echo "Now: cargo test -p origin psl::  and review the diff before committing."
