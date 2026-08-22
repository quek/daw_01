#!/usr/bin/env python3

# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later

"""PostToolUse hook: append one jsonl line per tool call (cross-platform, no PowerShell).

Port of the retired log_metric.ps1. stdin = Claude Code JSON
{ session_id, tool_name, tool_input, tool_response }. Output:
~/.claude/projects/F--dev-daw-01/metrics/YYYY-MM.jsonl. Side-effect = append only;
any failure exits 0 so a logging hiccup never blocks a tool call.
"""
import sys
import os
import json
from datetime import datetime


def main():
    raw = sys.stdin.buffer.read().decode("utf-8", "replace")
    if not raw.strip():
        return 0
    try:
        data = json.loads(raw)
    except Exception:
        return 0

    tool = str(data.get("tool_name") or "")
    ti = data.get("tool_input") or {}
    tr = data.get("tool_response") or {}

    # matcher: a one-string summary of "what this call targeted"
    matcher = ""
    if tool == "Bash" and ti.get("command"):
        first_line = str(ti["command"]).splitlines()[0].strip() if str(ti["command"]).strip() else ""
        tokens = first_line.split()
        matcher = " ".join(tokens[:2])
    elif ti.get("file_path"):
        matcher = str(ti["file_path"])
    elif ti.get("pattern"):
        matcher = str(ti["pattern"])[:40]

    status = "error" if (isinstance(tr, dict) and tr.get("is_error")) else "ok"

    entry = {
        "ts": datetime.now().strftime("%Y-%m-%dT%H:%M:%S"),
        "session": str(data.get("session_id") or ""),
        "tool": tool,
        "matcher": matcher,
        "status": status,
    }

    logdir = os.path.join(os.path.expanduser("~"), ".claude", "projects", "F--dev-daw-01", "metrics")
    try:
        os.makedirs(logdir, exist_ok=True)
        logfile = os.path.join(logdir, datetime.now().strftime("%Y-%m") + ".jsonl")
        with open(logfile, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry, ensure_ascii=False) + "\n")
    except Exception:
        pass
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)
