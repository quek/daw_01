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
<repo>/.claude/guards.jsonl -- TRACKED, one JSON object per line.

It used to live in the per-project user dir, outside git. That was wrong and it
cost the project the whole registry: on 2026-08-22 the file was found missing,
with the last recorded fire on 08-17, i.e. every pattern guard had been a silent
no-op for five days. The reason it was kept out of git was that reflect.py wrote
the auto warn->block escalation back into the same file, which would have made
every worktree permanently dirty. So two things of DIFFERENT KIND were sharing
one file, and the mutable one dragged the durable one out of version control.

They are now separated:

    <repo>/.claude/guards.jsonl        rule bodies. Hand-written project
                                       knowledge, same class as CLAUDE.md and
                                       .claude/hooks/ -- tracked, reviewed,
                                       branch-aware, restorable.
    <state>/guard_state.json           escalation overlay {rule id: "block"}.
                                       Runtime state, git-external, shared by all
                                       worktrees, and fully recomputable from
                                       guard_hits.jsonl if it is ever lost.

This engine reads the tracked rules and applies the overlay on top; reflect.py
writes ONLY the overlay and never touches a tracked file. Paths come from
scripts/ahe_paths.py (repo root from __file__, state dir from the main
checkout's slug) -- nothing is hardcoded to one machine. Each rule:

    id        unique slug
    source    feedback memory slug it enforces (link back = single source of truth)
    tool      tool name or list ("Bash","Edit","Write","MultiEdit","PowerShell");
              "*" = any tool
    field     which tool_input to scan: "command" | "command_code" (shell literals
              masked) | "text" (Edit/Write/MultiEdit new content) | "file_path" |
              "ask_options" (AskUserQuestion question+option text) | "worktree_outside"
              / "cd_redundant" / "ask_multi" (LOGIC fields computed in code, because
              they are RELATIONS -- target vs session cwd, question count -- that no
              single-field regex can express; the rule row supplies only action/msg.
              See the cwd-aware block in main())
    file_glob optional fnmatch glob (or list of globs) on tool_input.file_path
              (forward-slash normalized); rule skipped unless one of them matches
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ahe_paths  # noqa: E402  (sibling module, resolved from this file's dir)


def _eprint(text):
    sys.stderr.buffer.write(text.encode("utf-8", "replace"))


def _oprint(text):
    sys.stdout.buffer.write(text.encode("utf-8", "replace"))


def _mask_shell_literals(cmd):
    """Return cmd with shell *literal* text blanked to spaces (newlines kept):
    heredoc bodies, single/double-quoted spans, and # comments. This lets a guard
    regex match shell-significant CODE -- the command verbs, operators and real
    paths -- instead of DATA: commit messages, grep/--grep patterns, comments and
    file bodies. That is what separates "INVOKE powershell" (a real violation)
    from "MENTION powershell" (an innocent `git commit -m '...powershell...'` or
    `git grep powershell`). Fail-open: any error returns the raw command, and
    over-masking can only make a guard MISS (never wrongly cancel). Never raises.
    """
    try:
        # pass 1: blank heredoc bodies (line-oriented)
        lines = cmd.split("\n")
        body = [False] * len(lines)
        i = 0
        while i < len(lines):
            m = re.search(r"<<-?\s*[\"']?([A-Za-z_][A-Za-z0-9_]*)[\"']?", lines[i])
            if m:
                delim = m.group(1)
                j = i + 1
                while j < len(lines) and lines[j].strip() != delim:
                    body[j] = True
                    j += 1
                i = j + 1
                continue
            i += 1
        text = "\n".join((" " * len(ln)) if body[k] else ln for k, ln in enumerate(lines))

        # pass 2: blank quoted spans and comments (char scan)
        res = list(text)
        n = len(text)
        st = 0  # 0 normal, 1 single-quote, 2 double-quote
        p = 0
        while p < n:
            c = text[p]
            if st == 0:
                if c == "'":
                    st = 1
                    res[p] = " "
                elif c == '"':
                    st = 2
                    res[p] = " "
                elif c == "#" and (p == 0 or text[p - 1] in " \t\n;&|("):
                    while p < n and text[p] != "\n":
                        res[p] = " "
                        p += 1
                    continue
            elif st == 1:
                if c != "\n":
                    res[p] = " "
                    if c == "'":
                        st = 0
            else:  # st == 2
                if c == "\\" and p + 1 < n:
                    res[p] = " "
                    if text[p + 1] != "\n":
                        res[p + 1] = " "
                    p += 2
                    continue
                if c != "\n":
                    res[p] = " "
                    if c == '"':
                        st = 0
            p += 1
        return "".join(res)
    except Exception:
        return cmd


def _load_registry(guard_file):
    """(rules, defect). defect is a human-readable string when the registry is
    unusable -- missing, unreadable, empty, or entirely unparseable. Returning a
    defect instead of an empty list is the whole point: the previous version did
    `if not os.path.isfile(...): return 0`, which made "no registry" and "no rule
    matched" indistinguishable, so a five-day outage produced zero symptoms."""
    if not os.path.isfile(guard_file):
        return [], "レジストリが存在しません"
    try:
        with open(guard_file, "r", encoding="utf-8") as fh:
            lines = fh.readlines()
    except Exception as e:
        return [], "レジストリを読めません (%s)" % e

    rules = []
    bad_lines = 0
    for line in lines:
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        try:
            rule = json.loads(s)
        except Exception:
            bad_lines += 1
            continue
        if not rule.get("id") or not rule.get("all"):
            bad_lines += 1
            continue
        rules.append(rule)

    if not rules:
        if bad_lines:
            return [], "全 %d 行が JSON として読めません" % bad_lines
        return [], "レジストリにルールが 1 件もありません"
    return rules, None


def _report_registry_defect(state_dir, guard_file, defect, session):
    """Say it out loud, once per session (stdout + exit 0).

    Not a block: a broken registry must not wedge the session that is trying to
    repair it. Not silence either -- silence is what let the outage run for five
    days. Once per session keeps it visible without spamming every tool call; if
    the marker cannot be written we warn EVERY time rather than risk going quiet.
    """
    marker = os.path.join(state_dir, "guard_bootstrap_warned.json")
    seen = {}
    try:
        with open(marker, "r", encoding="utf-8") as fh:
            seen = json.load(fh) or {}
    except Exception:
        seen = {}
    if session and seen.get(session):
        return
    if session:
        try:
            seen[session] = datetime.now().strftime("%Y-%m-%dT%H:%M:%S")
            # keep the marker small; only recent sessions matter
            if len(seen) > 50:
                seen = dict(sorted(seen.items(), key=lambda kv: kv[1])[-50:])
            os.makedirs(state_dir, exist_ok=True)
            tmp = marker + ".tmp"
            with open(tmp, "w", encoding="utf-8") as fh:
                json.dump(seen, fh, ensure_ascii=False)
            os.replace(tmp, marker)
        except Exception:
            pass  # unwritable marker => warn again next time (never go silent)
    _oprint(
        "[guard: レジストリが機能していません — ガードは 1 件も効いていません]\n"
        "\n"
        "  %s\n"
        "  %s\n"
        "\n"
        "  これは fail-open です。ツール呼び出しは通しますが、フィードバックメモリ由来の\n"
        "  ガードは全部無効です。復旧するまで、過去に指摘された同じミスを自分で見張って\n"
        "  ください。レジストリはリポジトリ追跡下なので、git から復元できます。\n"
        % (defect, guard_file)
    )


def _with_overlay(rule, overlay):
    """warn -> block for a rule reflect.py has auto-escalated. escalate:false rules
    are never escalated (reflect.py already skips them; re-checked here so a stale
    overlay entry can never resurrect an opted-out rule as a block)."""
    if (overlay.get(str(rule.get("id"))) == "block"
            and str(rule.get("action")) == "warn"
            and rule.get("escalate") is not False):
        escalated = dict(rule)
        escalated["action"] = "block"
        return escalated
    return rule


def _load_overlay(path):
    """Escalation overlay {rule id: action}. Missing/corrupt => no overlay."""
    try:
        with open(path, "r", encoding="utf-8") as fh:
            data = json.load(fh)
    except Exception:
        return {}
    esc = data.get("escalated") if isinstance(data, dict) else None
    return esc if isinstance(esc, dict) else {}


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

    proj_dir = ahe_paths.state_dir()
    guard_file = ahe_paths.guards_file()
    rules, defect = _load_registry(guard_file)
    if defect:
        _report_registry_defect(proj_dir, guard_file, defect, session)
        return 0
    overlay = _load_overlay(ahe_paths.guard_state_file())

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

    # --- cwd-aware LOGIC fields (relation between session CWD and the target) ---
    # The hook payload carries `cwd` (verified present in Claude Code). These fields
    # encode a relational check that a single-field regex cannot: the rule row in
    # guards.jsonl only supplies action/msg/source, the logic lives here (CLAUDE.md:
    # "pattern guard は data、logic guard は code").
    cwd_norm = str(data.get("cwd") or "").replace("\\", "/")
    # worktree path discipline: when CWD is inside a worktree, an ABSOLUTE-path file
    # op under the repo root but OUTSIDE this worktree (= the main checkout or a
    # sibling worktree) is the cross-agent contention hazard. Paths outside the repo
    # (user-dir memory / guards.jsonl etc.) and relative paths (resolve under the
    # worktree) are NOT flagged. Comparison is case-insensitive (Windows paths).
    worktree_outside = ""
    mwt = re.match(r"^(.*?)/\.claude/worktrees/[^/]+", cwd_norm)
    if mwt and file_path and os.path.isabs(file_path):
        repo_l = mwt.group(1).lower()
        wt_l = mwt.group(0).lower()
        fpl = file_path_norm.lower()
        if (fpl == repo_l or fpl.startswith(repo_l + "/")) and \
           not (fpl == wt_l or fpl.startswith(wt_l + "/")):
            worktree_outside = file_path_norm
    # cd-prefix discipline: `cd <dir> && ...` where <dir> IS the session cwd is pure
    # noise -- the Bash tool already runs there -- and from a worktree, cd'ing to the
    # main checkout or a sibling worktree is the cross-agent contention hazard.
    # A plain `cd /tmp` or `cd build/` is legitimate and must NOT fire, which is why
    # this is a relational check in code and not a regex on "^cd ".
    cd_redundant = ""
    mcd = re.match(r"^\s*cd\s+(?:\"([^\"]+)\"|'([^']+)'|(\S+))", command)
    if mcd and cwd_norm:
        target = (mcd.group(1) or mcd.group(2) or mcd.group(3) or "")
        target = target.replace("\\", "/").rstrip("/").lower()
        cwd_l = cwd_norm.rstrip("/").lower()
        if target and target == cwd_l:
            cd_redundant = target
        elif target:
            m2 = re.match(r"^(.*?)/\.claude/worktrees/[^/]+", cwd_l)
            if m2:
                repo_l, wt_l = m2.group(1), m2.group(0)
                if (target == repo_l or target.startswith(repo_l + "/")) and \
                   not (target == wt_l or target.startswith(wt_l + "/")):
                    cd_redundant = target
    # AskUserQuestion batching: more than one question asked at once.
    _qs = tool_input.get("questions")
    ask_multi = "multi" if isinstance(_qs, list) and len([q for q in _qs if isinstance(q, dict)]) > 1 else ""

    fired = []
    for rule in rules:
        tools = rule.get("tool")
        tools = tools if isinstance(tools, list) else [tools]
        if "*" not in tools and tool not in tools:
            continue

        field = str(rule.get("field") or "text")
        if field == "command":
            text = command
        elif field == "command_code":
            text = _mask_shell_literals(command)
        elif field == "file_path":
            text = file_path
        elif field == "worktree_outside":
            text = worktree_outside
        elif field == "cd_redundant":
            text = cd_redundant
        elif field == "ask_multi":
            text = ask_multi
        elif field == "ask_options":
            # AskUserQuestion の質問文 + 全選択肢の label / description を 1 文字列に
            # 連結して scan する。 「妥協案を user の選択肢として出す」 瞬間を
            # compromise-smell で捕まえるための field (PreToolUse hook は assistant の
            # 地の文は見られないが、 AskUserQuestion は tool 呼び出しなので捕捉できる)。
            parts = []
            for q in tool_input.get("questions") or []:
                if not isinstance(q, dict):
                    continue
                parts.append(str(q.get("question") or ""))
                for o in q.get("options") or []:
                    if isinstance(o, dict):
                        parts.append(str(o.get("label") or ""))
                        parts.append(str(o.get("description") or ""))
            text = "\n".join(parts)
        else:
            text = edit_text
        if not text:
            continue

        glob = rule.get("file_glob")
        if glob:
            gs = glob if isinstance(glob, list) else [glob]
            if not any(fnmatch.fnmatch(file_path_norm, str(g)) for g in gs if g):
                continue

        glob_not = rule.get("file_glob_not")
        if glob_not:
            gn = glob_not if isinstance(glob_not, list) else [glob_not]
            if any(fnmatch.fnmatch(file_path_norm, str(g)) for g in gn if g):
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

        fired.append(_with_overlay(rule, overlay))

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
