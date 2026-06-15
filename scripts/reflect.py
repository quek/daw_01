#!/usr/bin/env python3
"""Stop hook: close the AHE loop by AUTO-ESCALATING guards (cross-platform, no PowerShell).

This replaces reflect.ps1. The old script only flagged Edit/Read/Bash *count
hotspots* (= normal heavy editing) which the backlog shows were ALWAYS dismissed
-- pure noise. The real signal is: a WARN guard that keeps firing across sessions
means the warning is not deterring the mistake, so it must become a BLOCK. That is
the missing ACTUATE step the user called out ("memory written, mistake repeated"):
here it happens AUTONOMOUSLY, by editing guards.jsonl (data, not settings.json),
with no human triage in the loop.

What it does every Stop:
  1. Read guard_hits.jsonl (all sessions, appended by guard_engine.py). Group by
     guard id -> set of distinct sessions.
  2. For each guard whose CURRENT registry action is "warn" and which has fired in
     >= ESCALATE_SESSIONS distinct sessions: flip its action to "block" in
     guards.jsonl, and record a terminal (done) row in the backlog for visibility.
  3. Detect 2+ consecutive Bash failures this session (genuine friction) -> an OPEN
     backlog row (target=skill) for the next session to triage.

Backlog format is identical to the one the SessionStart hook reads (9-col table
between the AHE-TABLE markers). Silent on any failure (exit 0): a reflection hiccup
must never block the Stop event.
"""
import sys
import os
import re
import json
import hashlib
from datetime import datetime

PROJ = os.path.join(os.path.expanduser("~"), ".claude", "projects", "F--dev-daw-01")
BACKLOG = os.path.join(PROJ, "ahe_backlog.md")
GUARDS = os.path.join(PROJ, "guards.jsonl")
HITS = os.path.join(PROJ, "guard_hits.jsonl")
METRICS_DIR = os.path.join(PROJ, "metrics")

ESCALATE_SESSIONS = 3  # a warn guard firing in this many distinct sessions -> auto-block

START = "<!-- AHE-TABLE-START -->"
END = "<!-- AHE-TABLE-END -->"
HEADER = "| id | status | sessions | target | first-seen | last-seen | last-session | pattern | notes |"
SEP = "|----|--------|----------|--------|------------|-----------|--------------|---------|-------|"

TEMPLATE = """# AHE backlog

Detected recurring friction (from session metrics) and AUTO-ESCALATED guards.
Managed by the Stop hook (scripts/reflect.py) and surfaced by the SessionStart hook
(OPEN rows become a Required Action). Triage each OPEN row with /promote-reflection.
status flows: open -> done | dismissed | needs-user.

## hook requests (awaiting your approval)

(none)

## patterns

__START__
__HEADER__
__SEP__
__END__
"""


def new_template():
    return (TEMPLATE.replace("__START__", START).replace("__HEADER__", HEADER)
            .replace("__SEP__", SEP).replace("__END__", END))


def compute_id(kind, matcher):
    h = hashlib.sha1(("%s|%s" % (kind, matcher)).encode("utf-8")).hexdigest()
    return "%s-%s" % (kind, h[:8])


def load_guard_hit_sessions():
    """guard id -> {sessions: set(), source: str} across ALL recorded hits."""
    out = {}
    if not os.path.isfile(HITS):
        return out
    try:
        with open(HITS, "r", encoding="utf-8") as fh:
            for ln in fh:
                ln = ln.strip()
                if not ln:
                    continue
                try:
                    h = json.loads(ln)
                except Exception:
                    continue
                gid = h.get("guard")
                if not gid:
                    continue
                rec = out.setdefault(gid, {"sessions": set(), "source": h.get("source") or ""})
                if h.get("session"):
                    rec["sessions"].add(h["session"])
    except Exception:
        pass
    return out


def escalate_guards(today):
    """Flip warn->block for guards that recur across sessions. Returns list of detected rows."""
    detected = []
    hits = load_guard_hit_sessions()
    if not hits or not os.path.isfile(GUARDS):
        return detected

    try:
        with open(GUARDS, "r", encoding="utf-8") as fh:
            lines = fh.readlines()
    except Exception:
        return detected

    changed = False
    for i, raw in enumerate(lines):
        s = raw.strip()
        if not s or s.startswith("#"):
            continue
        try:
            rule = json.loads(s)
        except Exception:
            continue
        gid = rule.get("id")
        if not gid or str(rule.get("action")) != "warn":
            continue
        if rule.get("escalate") is False:
            # advisory / precision-limited guard: a substring-level matcher that is
            # fine as a nudge but would over-cancel as a block (e.g. command-chaining
            # firing on `&&` in a commit message, or confirm-before-commit cancelling
            # every commit). Opt these out of auto warn->block escalation.
            continue
        rec = hits.get(gid)
        if not rec:
            continue
        nsess = len(rec["sessions"])
        if nsess < ESCALATE_SESSIONS:
            continue
        # escalate
        rule["action"] = "block"
        lines[i] = json.dumps(rule, ensure_ascii=False, separators=(",", ":")) + "\n"
        changed = True
        detected.append({
            "id": "guard-escalate-%s" % gid,
            "status": "done",
            "target": "guard",
            "desc": "auto-escalated warn->block: '%s' fired in %d distinct sessions (memory: %s)" % (gid, nsess, rule.get("source") or ""),
            "notes": "auto by reflect.py %s" % today,
        })

    if changed:
        try:
            with open(GUARDS, "w", encoding="utf-8") as fh:
                fh.writelines(lines)
        except Exception:
            return []
    return detected


def detect_bash_failures(session, today):
    """2+ consecutive Bash failures this session = genuine friction -> open skill row."""
    detected = []
    logfile = os.path.join(METRICS_DIR, datetime.now().strftime("%Y-%m") + ".jsonl")
    if not os.path.isfile(logfile):
        return detected
    bash = []
    try:
        with open(logfile, "r", encoding="utf-8") as fh:
            for ln in fh:
                ln = ln.strip()
                if not ln:
                    continue
                try:
                    e = json.loads(ln)
                except Exception:
                    continue
                if e.get("session") == session and e.get("tool") == "Bash" and e.get("matcher"):
                    bash.append(e)
    except Exception:
        return detected

    streak = 0
    for b in bash:
        if b.get("status") == "error":
            streak += 1
            if streak >= 2:
                m = b["matcher"]
                detected.append({
                    "id": compute_id("bash-failure", m),
                    "status": "open",
                    "target": "skill",
                    "desc": "Bash repeated failure: `%s`" % m,
                    "notes": "",
                })
                break
        else:
            streak = 0
    return detected


def upsert_backlog(detected, session_short, today):
    if not detected:
        return
    if os.path.isfile(BACKLOG):
        try:
            raw = open(BACKLOG, "r", encoding="utf-8").read()
        except Exception:
            raw = new_template()
        if START not in raw:
            raw = new_template()
    else:
        try:
            os.makedirs(os.path.dirname(BACKLOG), exist_ok=True)
        except Exception:
            pass
        raw = new_template()

    si = raw.find(START)
    ei = raw.find(END)
    if si < 0:
        raw = new_template()
        si = raw.find(START)
        ei = raw.find(END)
    if ei < 0 or ei < si:
        before = raw[:si]
        after = "\n"
        region = ""
    else:
        before = raw[:si]
        after = raw[ei + len(END):]
        region = raw[si:ei]

    rows = []
    for line in region.split("\n"):
        t = line.strip()
        if t.startswith("|") and t != HEADER and not re.match(r"^\|-", t):
            cells = [c.strip() for c in t.strip("|").split("|")]
            if len(cells) >= 9 and cells[0] != "id":
                rows.append(cells[:9])

    for d in detected:
        rid = d["id"]
        desc = d["desc"].replace("|", "/")
        notes = d.get("notes", "").replace("|", "/")
        status = d.get("status", "open")
        target = d.get("target", "skill")
        existing = next((r for r in rows if r[0] == rid), None)
        if existing:
            if existing[1] in ("done", "dismissed"):
                continue
            if existing[6] != session_short:
                try:
                    existing[2] = str(int(existing[2]) + 1)
                except Exception:
                    existing[2] = existing[2]
                existing[6] = session_short
            existing[5] = today
        else:
            rows.append([rid, status, "1", target, today, today, session_short, desc, notes])

    out = [START, HEADER, SEP]
    for r in rows:
        out.append("| " + " | ".join(r) + " |")
    new_region = "\n".join(out) + "\n" + END
    try:
        with open(BACKLOG, "w", encoding="utf-8") as fh:
            fh.write(before + new_region + after)
    except Exception:
        pass


def main():
    raw = sys.stdin.buffer.read().decode("utf-8", "replace")
    if not raw.strip():
        return 0
    try:
        data = json.loads(raw)
    except Exception:
        return 0
    session = data.get("session_id")
    if not session:
        return 0
    session_short = session[:8]
    today = datetime.now().strftime("%Y-%m-%d")

    detected = []
    detected += escalate_guards(today)
    detected += detect_bash_failures(session, today)
    upsert_backlog(detected, session_short, today)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)
