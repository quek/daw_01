#!/usr/bin/env python3

# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later

"""PreToolUse hook (Bash | PowerShell): BLOCK a recursive/force delete whose target is a
variable/env reference or a filesystem root -- a path whose runtime value the hook cannot
verify, which is exactly how a wrong-tree wipe happens (2026-06-13: Remove-Item -Recurse
-Force on an unchecked env-derived path). Port of check_destructive_delete.ps1 to Python
(cross-platform, no PowerShell). See memory feedback_verify_env_var_before_use.

Unlike the warn-only guard_engine rules, this BLOCKS: exit 2 + stderr cancels the tool call.
This logic is intentionally a CODE hook, not a guards.jsonl regex row: it needs per-statement
splitting (so a var printed on one line does not taint a delete on another) and a multi-
condition AND, which a single whole-text regex cannot express safely.

Detection requires BOTH in the SAME statement:
  (1) a recursive/force delete  (rm -rf, Remove-Item -Recurse, rd /s, ...)
  (2) a dangerous target: a variable/env reference OR a root-ish token.
A recursive delete of a concrete literal subpath (rm -rf target/debug) is allowed.

stdin: Claude Code JSON { tool_name, tool_input: { command } }
"""
import sys
import re
import json

RECURSE = re.compile(r'(?i)(-Recurse\b|-rec\b|--recursive\b|\s-[a-z]*r[a-z]*f[a-z]*\b|\s-[a-z]*f[a-z]*r[a-z]*\b|\s-r\b|\s-R\b|/s\b)')
DELVERB = re.compile(r'(?i)(\bRemove-Item\b|\brmdir\b|\brd\b|\bdel\b|\bri\b|\brm\b)')
VAR = re.compile(r'(\$env:|\$\{?[A-Za-z_]|%[A-Za-z_][A-Za-z0-9_]*%|\$\()')
ROOT = re.compile(r'(?i)(^|\s|=|"|\')(/|\\|~|\.|\*|[A-Za-z]:[\\/]?)(\s|$|"|\')')


def main():
    raw = sys.stdin.buffer.read().decode("utf-8", "replace")
    if not raw.strip():
        return 0
    try:
        data = json.loads(raw)
    except Exception:
        return 0
    cmd = str((data.get("tool_input") or {}).get("command") or "")
    if not cmd:
        return 0

    hit = None
    for seg in re.split(r'(?:\r?\n|;|&&|\|\|)', cmd):
        if not DELVERB.search(seg):
            continue
        if not RECURSE.search(seg):
            continue
        if VAR.search(seg) or ROOT.search(seg):
            hit = seg.strip()
            break

    if not hit:
        return 0

    lines = [
        "[BLOCKED: recursive/force delete on an unverified variable or root path]",
        "",
        "statement: " + hit,
        "",
        "A recursive/force delete (rm -rf, Remove-Item -Recurse -Force, rd /s) is",
        "targeting a path that is a variable/env reference or a filesystem root.",
        "The hook cannot see the runtime value, so this is blocked to avoid wiping",
        "the wrong tree (incident 2026-06-13: unchecked env var -> near-root delete).",
        "",
        "Do this instead:",
        "  1. Print the resolved value first and confirm it is the dir you mean.",
        "  2. Use a verified LITERAL absolute path as the delete target, OR",
        "  3. For a single file: delete that one verified absolute path.",
        "  4. Guard first: refuse if the path is empty or shorter than ~5 chars.",
        "",
        "(ref: ~/.claude/projects/F--dev-daw-01/memory/feedback_verify_env_var_before_use.md)",
    ]
    sys.stderr.buffer.write(("\n".join(lines) + "\n").encode("utf-8", "replace"))
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)
