#!/usr/bin/env python3
"""Stop hook: close the AHE loop by AUTO-ESCALATING guards (cross-platform, no PowerShell).

This replaces reflect.ps1. The old script only flagged Edit/Read/Bash *count
hotspots* (= normal heavy editing) which the backlog shows were ALWAYS dismissed
-- pure noise. The real signal is: a WARN guard that keeps firing across sessions
means the warning is not deterring the mistake, so it must become a BLOCK. That is
the missing ACTUATE step the user called out ("memory written, mistake repeated"):
here it happens AUTONOMOUSLY, as data, with no human triage in the loop.

Escalation writes an OVERLAY, never the registry
------------------------------------------------
The rule bodies live in <repo>/.claude/guards.jsonl, which is TRACKED. This hook
must not touch a tracked file: it would make every worktree dirty on every Stop,
which is precisely why the registry was originally kept out of git -- and why the
whole registry was then lost with no backup. So escalation state goes to
<state>/guard_state.json ({"escalated": {rule id: "block"}}), git-external and
shared by all worktrees; guard_engine.py applies it on top of the tracked rules.
If the overlay is ever lost, nothing durable is gone: it is recomputable from
guard_hits.jsonl by simply letting this hook run again.

What it does every Stop:
  1. Read guard_hits.jsonl (all sessions, appended by guard_engine.py). Group by
     guard id -> set of distinct sessions.
  2. For each guard whose registry action is "warn" and which has fired in
     >= ESCALATE_SESSIONS distinct sessions: record it as "block" in the OVERLAY
     (atomically), and add a terminal (done) backlog row for visibility.
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ahe_paths  # noqa: E402  (sibling module, resolved from this file's dir)

PROJ = ahe_paths.state_dir()
BACKLOG = os.path.join(PROJ, "ahe_backlog.md")
GUARDS = ahe_paths.guards_file()          # TRACKED: read-only from this hook
GUARD_STATE = ahe_paths.guard_state_file()  # untracked overlay: the only thing we write
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


def read_registry():
    """The TRACKED rule bodies, read-only. [] when unreadable."""
    out = []
    if not os.path.isfile(GUARDS):
        return out
    try:
        with open(GUARDS, "r", encoding="utf-8") as fh:
            for raw in fh:
                s = raw.strip()
                if not s or s.startswith("#"):
                    continue
                try:
                    rule = json.loads(s)
                except Exception:
                    continue
                if rule.get("id"):
                    out.append(rule)
    except Exception:
        return []
    return out


def read_overlay():
    try:
        with open(GUARD_STATE, "r", encoding="utf-8") as fh:
            data = json.load(fh)
    except Exception:
        return {}
    esc = data.get("escalated") if isinstance(data, dict) else None
    return dict(esc) if isinstance(esc, dict) else {}


def write_overlay(escalated):
    """Atomic: write a sibling temp file, then os.replace (same filesystem).

    The old code did open(GUARDS, "w") -- truncate first, write second. The Stop
    hook has a 10 s timeout, so being cut between those two steps left an empty
    registry. Never leave a half-written file where a hook can be killed."""
    try:
        os.makedirs(os.path.dirname(GUARD_STATE), exist_ok=True)
        tmp = GUARD_STATE + ".tmp"
        payload = {
            "version": 1,
            "note": "auto-escalation overlay written by scripts/reflect.py; "
                    "rule bodies live in .claude/guards.jsonl (tracked). "
                    "Safe to delete: recomputable from guard_hits.jsonl.",
            "escalated": escalated,
        }
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, ensure_ascii=False, indent=1, sort_keys=True)
        os.replace(tmp, GUARD_STATE)
        return True
    except Exception:
        return False


def escalate_guards(today):
    """Record warn->block in the OVERLAY for guards that recur across sessions.
    The tracked registry is never modified. Returns backlog rows to upsert."""
    detected = []
    hits = load_guard_hit_sessions()
    if not hits:
        return detected

    overlay = read_overlay()
    changed = False
    for rule in read_registry():
        gid = rule.get("id")
        if str(rule.get("action")) != "warn":
            continue
        if overlay.get(gid) == "block":
            continue  # already escalated; don't re-report every Stop
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
        overlay[gid] = "block"
        changed = True
        detected.append({
            "id": "guard-escalate-%s" % gid,
            "status": "done",
            "target": "guard",
            "desc": "auto-escalated warn->block: '%s' fired in %d distinct sessions (memory: %s)" % (gid, nsess, rule.get("source") or ""),
            "notes": "auto by reflect.py %s" % today,
        })

    if changed and not write_overlay(overlay):
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


# --- verify-handoff heuristic (chronic: feedback_launch_app_for_verification) ---
# The one failure a PreToolUse guard cannot catch: it lives in CHAT, not a tool
# call. Pattern = edited GUI code this session, never launched daw_gui myself, yet
# the final message asks the USER to launch / play / verify. Detected here at Stop
# and surfaced as an OPEN backlog row (next SessionStart Required Action). We do
# NOT block the Stop (reflect.py invariant: a fuzzy text heuristic must never
# wedge the session), so this is a strong automatic nudge, not action-time force.

# A real launch of the built exe / cargo run / smoke test (NOT `ls`/`tasklist`
# which merely reference the name). Smoke test counts as self-verification.
_LAUNCH_RE = re.compile(
    r"(target[\\/]debug[\\/]daw_gui\.exe|cargo\s+run\s+(?:-p|--bin)\s+daw_gui|--smoke-test)"
)
# An Edit/Write to daw_gui GUI source or the ui/ renderer crates.
_GUI_EDIT_RE = re.compile(r"daw_gui[\\/]src|ui[\\/]crates")
# Final message hands verification to the user: a launch/play/実機/preview context
# followed by a request particle. Requires the app context so a plain design
# question ("この設計でよいですか") does not match.
_HANDOFF_RE = re.compile(
    r"(起動|立ち上げ|再生|実機|プレビュー|プロジェクトを開)"
    r"[^。\n]{0,40}?"
    r"(ください|下さい|お願いし|もらえ|いただけ|ますか|くださ|見ていただ)"
)
# I actually launched / verified it myself (affirmative, completed) -> not a
# handoff. Note "私が起動しても...できない" (the excuse) does NOT match this, so it
# is correctly still flagged.
_SELF_DID_RE = re.compile(
    r"(起動しました|起動した(?:ので|。|、)|起動済|自分で起動(?:した|する。|するので|します)|"
    r"起動して(?:確認|検証)(?:した|しました|済)|smoke ?test.{0,20}(?:pass|PASS|完了|通))"
)


def _read_transcript(path):
    """Scan the session transcript: did I launch daw_gui / edit GUI code, and what
    was my final assistant text? Returns (launched, gui_edited, final_text)."""
    launched = False
    gui_edited = False
    final_text = ""
    if not path or not os.path.isfile(path):
        return launched, gui_edited, final_text
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for ln in fh:
                ln = ln.strip()
                if not ln:
                    continue
                try:
                    ent = json.loads(ln)
                except Exception:
                    continue
                if ent.get("type") != "assistant":
                    continue
                content = (ent.get("message") or {}).get("content")
                if not isinstance(content, list):
                    continue
                texts = []
                for item in content:
                    if not isinstance(item, dict):
                        continue
                    it = item.get("type")
                    if it == "tool_use":
                        name = item.get("name") or ""
                        inp = item.get("input") or {}
                        if name == "Bash" and _LAUNCH_RE.search(str(inp.get("command") or "")):
                            launched = True
                        elif name in ("Edit", "Write", "MultiEdit") and _GUI_EDIT_RE.search(
                            str(inp.get("file_path") or "")
                        ):
                            gui_edited = True
                    elif it == "text":
                        t = item.get("text")
                        if t:
                            texts.append(str(t))
                if texts:
                    final_text = "\n".join(texts)  # last assistant text block wins
    except Exception:
        return launched, gui_edited, final_text
    return launched, gui_edited, final_text


def detect_verify_handoff(transcript_path):
    """OPEN backlog row when I edited GUI code, never launched daw_gui, and the
    final message asks the user to launch/verify. Empty list otherwise."""
    launched, gui_edited, final_text = _read_transcript(transcript_path)
    if launched or not gui_edited or not final_text:
        return []
    if not _HANDOFF_RE.search(final_text) or _SELF_DID_RE.search(final_text):
        return []
    return [{
        "id": compute_id("verify-handoff", "launch-for-verification"),
        "status": "open",
        "target": "memory",
        "desc": "GUI 変更を検証せず user に起動/再生/確認を丸投げ (daw_gui 自己起動なし + 最終応答で依頼)",
        "notes": "feedback_launch_app_for_verification; 自分で起動して検証 (--smoke-test / session復元 / --script)",
    }]


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
    transcript_path = data.get("transcript_path")

    detected = []
    detected += escalate_guards(today)
    detected += detect_bash_failures(session, today)
    detected += detect_verify_handoff(transcript_path)
    upsert_backlog(detected, session_short, today)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:
        sys.exit(0)
