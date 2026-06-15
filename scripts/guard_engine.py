#!/usr/bin/env python3
"""PreToolUse hook: GENERIC, DATA-DRIVEN guard engine (cross-platform, no PowerShell).

Why this exists (the AHE gap it closes)
----------------------------------------
Feedback memories are recalled only as PASSIVE <system-reminder> background
context -- they do NOT fire at the moment a mistake is being made, so the same
mistake recurs ("memory written, mistake repeated"). The only forcing function
that fires at action-time is a PreToolUse hook. Previously, turning a feedback
memory into such a hook required (a) hand-writing a bespoke check_*.ps1 AND
(b) editing .claude/settings.json -- which the harness classifier BLOCKS (needs
the human). That cost meant promotion almost never happened, so violations were
"dismissed" in the backlog and recurred (see ahe_backlog bash-repeat-c52bab59:
a cd-prefix was DETECTED as a recurrence of feedback_no_cd_prefix, but there was
no path to make it an active guard, so it was dismissed -- and recurs).

This engine is registered ONCE in settings.json. After that, adding/refining a
guard is just APPENDING one JSON line to the registry -- no script, no settings
edit, no human approval. That makes observe -> reflect -> ACTUATE -> close fully
autonomous: a recurring friction becomes an active forcing function.

Registry
--------
~/.claude/projects/F--dev-daw-01/guards.jsonl  (one JSON object per line).
Per-project user dir => shared across the main repo and ALL worktrees, like
metrics/backlog (no git churn, no merge conflict). Each rule:

    id        unique slug
    source    feedback memory slug it enforces (link back = single source of truth)
    tool      tool name or list ("Bash","Edit","Write","MultiEdit","PowerShell");
              "*" = any tool
    field     which tool_input to scan: "command" | "text" (Edit/Write/MultiEdit
              new content) | "file_path"
    file_glob optional fnmatch glob on tool_input.file_path (forward-slash
              normalized); rule skipped if it does not match
    all       list of regex; ALL must match the field text for the rule to fire
    none      optional list of regex; if ANY matches, the rule is suppressed
              (sanctioned-exception escape hatch, e.g. "git -C" for the cd guard)
    action    "warn" (stdout, exit 0; injected as a system reminder) |
              "block" (stderr, exit 2; cancels the tool call)
    msg       the corrective message shown to Claude

Fires are appended to guard_hits.jsonl (same dir) so the Stop hook (reflect.py)
can see which guards keep firing and auto-escalate warn -> block.

stdin: Claude Code JSON { session_id, tool_name, tool_input{...} }
Pure standard library (json, re, fnmatch). UTF-8 forced on all I/O so Japanese
messages round-trip on Windows consoles too. Never raises to the caller: any
internal error exits 0 (a guard must never break a tool call by crashing).
"""
import sys
import os
import re
import json
import fnmatch
from datetime import datetime


def _eprint(text):
    sys.stderr.buffer.write(text.encode("utf-8", "replace"))


def _oprint(text):
    sys.stdout.buffer.write(text.encode("utf-8", "replace"))


def main():
    raw = sys.stdin.buffer.read().decode("utf-8", "replace")
    if not raw.strip():
        return 0
    try:
        data = json.loads(raw)
    except Exception:
        return 0

    tool = str(data.get("tool_name") or "")
    if not tool:
        return 0
    tool_input = data.get("tool_input") or {}
    session = str(data.get("session_id") or "")

    proj_dir = os.path.join(os.path.expanduser("~"), ".claude", "projects", "F--dev-daw-01")
    guard_file = os.path.join(proj_dir, "guards.jsonl")
    if not os.path.isfile(guard_file):
        return 0

    # --- candidate field texts from this tool call ---
    command = str(tool_input.get("command") or "")
    file_path = str(tool_input.get("file_path") or "")
    file_path_norm = file_path.replace("\\", "/")
    if tool_input.get("new_string"):
        edit_text = str(tool_input.get("new_string"))
    elif tool_input.get("content"):
        edit_text = str(tool_input.get("content"))
    elif tool_input.get("edits"):
        parts = []
        for e in tool_input.get("edits") or []:
            if isinstance(e, dict) and e.get("new_string"):
                parts.append(str(e.get("new_string")))
        edit_text = "\n".join(parts)
    else:
        edit_text = ""

    fired = []
    try:
        with open(guard_file, "r", encoding="utf-8") as fh:
            rule_lines = fh.readlines()
    except Exception:
        return 0

    for line in rule_lines:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            rule = json.loads(line)
        except Exception:
            continue
        if not rule.get("id") or not rule.get("all"):
            continue

        tools = rule.get("tool")
        tools = tools if isinstance(tools, list) else [tools]
        if "*" not in tools and tool not in tools:
            continue

        field = str(rule.get("field") or "text")
        if field == "command":
            text = command
        elif field == "file_path":
            text = file_path
        else:
            text = edit_text
        if not text:
            continue

        glob = rule.get("file_glob")
        if glob and not fnmatch.fnmatch(file_path_norm, str(glob)):
            continue

        all_pats = rule.get("all") or []
        all_pats = all_pats if isinstance(all_pats, list) else [all_pats]
        try:
            if not all(re.search(str(p), text) for p in all_pats if p):
                continue
        except re.error:
            continue

        none_pats = rule.get("none") or []
        none_pats = none_pats if isinstance(none_pats, list) else [none_pats]
        try:
            if any(re.search(str(p), text) for p in none_pats if p):
                continue
        except re.error:
            pass

        fired.append(rule)

    if not fired:
        return 0

    # --- log fires so reflect.py can auto-escalate repeat offenders ---
    ts = datetime.now().strftime("%Y-%m-%dT%H:%M:%S")
    try:
        with open(os.path.join(proj_dir, "guard_hits.jsonl"), "a", encoding="utf-8") as fh:
            for r in fired:
                fh.write(json.dumps({
                    "ts": ts,
                    "session": session,
                    "guard": str(r.get("id")),
                    "source": str(r.get("source") or ""),
                    "tool": tool,
                    "action": str(r.get("action") or "warn"),
                }, ensure_ascii=False) + "\n")
    except Exception:
        pass

    has_block = any(str(r.get("action")) == "block" for r in fired)
    lines = []
    if has_block:
        lines.append("[guard: BLOCKED -- a known recurring mistake was about to be repeated]")
    else:
        lines.append("[guard: warning -- this matches a feedback memory you keep violating]")
    lines.append("")
    for r in fired:
        tag = "BLOCK" if str(r.get("action")) == "block" else "warn"
        lines.append("  [%s] %s  (memory: %s)" % (tag, r.get("id"), r.get("source") or ""))
        lines.append("    " + str(r.get("msg") or ""))
        lines.append("")
    lines.append("These guards are data rows in guards.jsonl, derived from your feedback")
    lines.append("memories. If a guard is wrong, fix/remove its row (no settings edit needed).")
    msg = "\n".join(lines)

    if has_block:
        _eprint(msg + "\n")
        return 2
    else:
        _oprint(msg + "\n")
        return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)
