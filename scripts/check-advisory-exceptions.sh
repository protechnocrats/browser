#!/usr/bin/env bash
# Keeps the two advisory-exception lists honest (Milestone 25):
#
#   1. deny.toml [advisories].ignore and .cargo/audit.toml
#      [advisories].ignore must name exactly the same RUSTSEC ids —
#      otherwise `cargo deny` and `cargo audit` silently disagree.
#   2. Every exception whose reason says "EXPIRES YYYY-MM-DD" must not be
#      past that date. An exception that outlives its expiry FAILS CI,
#      which is what makes "time-boxed" a property instead of a promise.
#
# Overridable for testing:  DENY_TOML, AUDIT_TOML, TODAY (YYYY-MM-DD).

set -uo pipefail
cd "$(dirname "$0")/.."

python3 - "${DENY_TOML:-deny.toml}" "${AUDIT_TOML:-.cargo/audit.toml}" "${TODAY:-$(date +%F)}" <<'PY'
import re, sys

deny_path, audit_path, today = sys.argv[1:4]
deny = open(deny_path).read()
audit = open(audit_path).read()

def ignore_block(text):
    m = re.search(r'^ignore\s*=\s*\[(.*?)^\]', text, re.S | re.M)
    return m.group(1) if m else ""

deny_block = ignore_block(deny)
deny_ids = set(re.findall(r'id\s*=\s*"(RUSTSEC-\d+-\d+)"', deny_block))
audit_ids = set(re.findall(r'"(RUSTSEC-\d+-\d+)"', ignore_block(audit)))

failed = False
if deny_ids != audit_ids:
    failed = True
    print(f"FAIL: advisory ignore lists differ")
    for i in sorted(deny_ids - audit_ids):
        print(f"  {i}: in {deny_path} but not {audit_path}")
    for i in sorted(audit_ids - deny_ids):
        print(f"  {i}: in {audit_path} but not {deny_path}")
else:
    print(f"OK:   {len(deny_ids)} advisory exceptions agree between deny.toml and audit.toml")

for m in re.finditer(r'id\s*=\s*"(RUSTSEC-\d+-\d+)"[^\n]*?EXPIRES\s+(\d{4}-\d{2}-\d{2})', deny_block):
    rid, expiry = m.groups()
    if expiry < today:
        failed = True
        print(f"FAIL: exception {rid} expired on {expiry} (today is {today}) — resolve it or renew it with a new reasoned date")
    else:
        print(f"OK:   {rid} expires {expiry}")

sys.exit(1 if failed else 0)
PY
