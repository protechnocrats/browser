#!/usr/bin/env python3
"""Reads `cargo clippy --message-format=json` on stdin and prints one line
per distinct diagnostic: `WARN <file>:<line>: warning[<lint>]: <message>`
or `ERROR <file>:<line>: <message>`.

Distinct = (file, line, lint, message), so identical lints at different
places count separately, while the same location reported by two targets
(lib and lib-test) collapses. Used by clippy-ratchet.sh."""
import json
import sys

warnings, errors = set(), set()
saw_any_message = False
for raw in sys.stdin:
    raw = raw.strip()
    if not raw.startswith("{"):
        continue
    try:
        m = json.loads(raw)
    except ValueError:
        continue
    if m.get("reason") != "compiler-message":
        continue
    saw_any_message = True
    msg = m["message"]
    level = msg.get("level")
    spans = [s for s in msg.get("spans", []) if s.get("is_primary")]
    where = f"{spans[0]['file_name']}:{spans[0]['line_start']}" if spans else "?"
    code = (msg.get("code") or {}).get("code") or ""
    if level == "warning" and spans:
        warnings.add(f"{where}: warning[{code}]: {msg['message']}")
    elif level and level.startswith("error"):
        errors.add(f"{where}: {msg['message']}")

for e in sorted(errors):
    print("ERROR " + e)
for w in sorted(warnings):
    print("WARN " + w)
