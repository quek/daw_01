#!/usr/bin/env python3
"""Verification harness for the AHE guard layer (cross-platform, stdlib only).

Runs the REAL guard scripts against hand-crafted positive/negative tool calls and
asserts exit code + which guard ids fire. SANDBOXED: guard_engine.py is invoked
with USERPROFILE/HOME pointed at a throwaway tempdir that holds a COPY of the live
guards.jsonl, so this never pollutes the real guard_hits.jsonl (which would risk a
false warn->block escalation by reflect.py).

What it checks
  1. guards.jsonl is well-formed JSONL: every rule parses, has id/tool/field/all,
     every regex in all/none compiles, every `source` memory file exists.
  2. guard_engine.py: per-rule positive (should fire) + negative (should not) cases,
     plus field-selection / file_glob gating / none-suppression / malformed-input.
  3. check_destructive_delete.py: recursive-delete-on-var/root blocks; literal subpath
     and per-statement splitting do NOT block.
  4. reflect.py: a warn guard fired in >=3 distinct sessions auto-escalates to block
     (the ACTUATE step), run fully inside the sandbox.

Exit 0 = all pass; exit 1 = at least one failure.
"""
import sys
import os
import re
import json
import shutil
import tempfile
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import ahe_paths  # noqa: E402  (the module under test for slug/path derivation)

DESTRUCT = os.path.join(HERE, "check_destructive_delete.py")  # registry-independent
HOOK_SCRIPTS = ("guard_engine.py", "reflect.py", "ahe_paths.py")
REAL_GUARDS = ahe_paths.guards_file()          # <repo>/.claude/guards.jsonl (tracked)
MEM_DIR = os.path.join(ahe_paths.state_dir(), "memory")
# optional arg: validate a candidate registry (e.g. scripts/guards.proposed.jsonl) in the
# sandbox without touching the tracked one.
GUARDS_SRC = sys.argv[1] if len(sys.argv) > 1 else REAL_GUARDS

# Synthetic repo root for the fixtures below. Never write a real machine path here:
# this file ships in a public repository. Nothing touches the filesystem through these
# paths -- guard_engine.py only does string work on file_path, plus os.path.isabs.
#
# isabs IS platform-dependent, and the worktree guard only engages for absolute paths:
# posixpath.isabs("X:/proj/a") is False, so a drive-letter fixture would silently turn
# every worktree positive case into a vacuous pass on Linux. Pick a root that is
# absolute on the host instead.
SYN_ROOT = "X:/proj" if os.name == "nt" else "/proj"
# Backslash variant of the same path, to exercise the engine's "\" -> "/" normalization.
# On POSIX the drive-letter form is not absolute, so keep the leading "/" there and vary
# only the separators.
SYN_ROOT_BS = SYN_ROOT.replace("/", "\\") if os.name == "nt" else SYN_ROOT
# A user-dir path OUTSIDE the repo (memory / runtime state live there). The project
# slug is derived at runtime now, so nothing here needs a real one.
SYN_USER_PROJ = ("Y:/home/u" if os.name == "nt" else "/home/u") + \
    "/.claude/projects/proj-slug"

PASS, FAIL = [], []


def ok(name):
    PASS.append(name)


def bad(name, detail):
    FAIL.append((name, detail))


# ---------------------------------------------------------------- sandbox setup
# The hooks resolve their registry from THEIR OWN location (<repo>/scripts/x.py ->
# <repo>/.claude/guards.jsonl), so a sandbox is a throwaway REPO: scripts/ copies of
# the real hooks plus a registry. HOME points at the same dir, so guard_hits.jsonl and
# guard_state.json land inside the sandbox and the live ones are never touched (a
# stray hit could otherwise trip reflect.py's warn->block escalation for real).
def make_sandbox(prefix, guards_src=GUARDS_SRC):
    sb = tempfile.mkdtemp(prefix=prefix)
    os.makedirs(os.path.join(sb, "scripts"), exist_ok=True)
    os.makedirs(os.path.join(sb, ".claude"), exist_ok=True)
    for name in HOOK_SCRIPTS:
        shutil.copyfile(os.path.join(HERE, name), os.path.join(sb, "scripts", name))
    if guards_src is not None:
        shutil.copyfile(guards_src, os.path.join(sb, ".claude", "guards.jsonl"))
    return sb


def sb_script(sandbox, name):
    return os.path.join(sandbox, "scripts", name)


def sb_registry(sandbox):
    return os.path.join(sandbox, ".claude", "guards.jsonl")


def sb_proj(sandbox):
    """The state dir the sandboxed hooks will use (HOME == sandbox root)."""
    return os.path.join(sandbox, ".claude", "projects", ahe_paths.slug(sandbox))


def sb_state(sandbox):
    return os.path.join(sb_proj(sandbox), "guard_state.json")


_sandbox = make_sandbox("guard_test_")


def _sandbox_env(home):
    env = dict(os.environ)
    env["USERPROFILE"] = home
    env["HOME"] = home
    env["HOMEDRIVE"] = ""
    env["HOMEPATH"] = home
    return env


def run_engine(tool_name, tool_input, sandbox=None, cwd=None):
    sb = sandbox or _sandbox
    payload_obj = {"session_id": "TEST_SESSION", "tool_name": tool_name,
                   "tool_input": tool_input}
    if cwd is not None:
        payload_obj["cwd"] = cwd
    payload = json.dumps(payload_obj)
    p = subprocess.run([sys.executable, sb_script(sb, "guard_engine.py")],
                       input=payload.encode("utf-8"),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=_sandbox_env(sb))
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    return p.returncode, out


def run_destruct(command):
    payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": command}})
    p = subprocess.run([sys.executable, DESTRUCT], input=payload.encode("utf-8"),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    out = (p.stdout + p.stderr).decode("utf-8", "replace")
    return p.returncode, out


def fired_ids(out):
    return set(re.findall(r"\[(?:warn|BLOCK)\]\s+(\S+)", out))


# ---------------------------------------------------------------- 1. static lint
def check_registry():
    seen = set()
    with open(GUARDS_SRC, "r", encoding="utf-8") as fh:
        for n, line in enumerate(fh, 1):
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            try:
                r = json.loads(s)
            except Exception as e:
                bad("registry:line%d-parses" % n, "JSON error: %s" % e)
                continue
            rid = r.get("id")
            if not rid:
                bad("registry:line%d-has-id" % n, s[:60])
                continue
            if rid in seen:
                bad("registry:dup-id", rid)
            seen.add(rid)
            if not r.get("all"):
                bad("registry:%s-has-all" % rid, "missing 'all'")
            if r.get("action") not in ("warn", "block"):
                bad("registry:%s-action" % rid, "action=%r" % r.get("action"))
            if r.get("field") not in ("command", "command_code", "text", "file_path",
                                      "ask_options", "worktree_outside", "cd_redundant",
                                      "ask_multi", None):
                bad("registry:%s-field" % rid, "field=%r" % r.get("field"))
            pats = []
            for key in ("all", "none"):
                v = r.get(key) or []
                pats += v if isinstance(v, list) else [v]
            for p in pats:
                try:
                    re.compile(str(p))
                except re.error as e:
                    bad("registry:%s-regex" % rid, "%r: %s" % (p, e))
            src = str(r.get("source") or "")
            if src.startswith("feedback_") or src.startswith("project_") or src.startswith("user_"):
                if not os.path.isfile(os.path.join(MEM_DIR, src + ".md")):
                    bad("registry:%s-source-memory" % rid, "missing %s.md" % src)
            ok("registry:%s" % rid)


# ---------------------------------------------------------------- 2. engine cases
# (label, tool, tool_input, expect_exit, must_fire[set], must_not_fire[set])
CASES = [
    # no-powershell-tool
    ("ps-tool/pos", "PowerShell", {"command": "Get-Process"}, 2, {"no-powershell-tool"}, set()),
    ("ps-tool/empty-neg", "PowerShell", {"command": ""}, 0, set(), {"no-powershell-tool"}),
    # no-powershell-invoke
    ("ps-invoke/pos", "Bash", {"command": "powershell -c ls"}, 2, {"no-powershell-invoke"}, set()),
    ("ps-invoke/pwsh", "Bash", {"command": "pwsh -File a.ps1"}, 2, {"no-powershell-invoke"}, set()),
    ("ps-invoke/neg", "Bash", {"command": "ls -la"}, 0, set(), {"no-powershell-invoke"}),
    # no-ps1-file
    ("ps1-file/pos", "Write", {"file_path": "scripts/foo.ps1", "content": "x"}, 2, {"no-ps1-file"}, set()),
    ("ps1-file/neg", "Write", {"file_path": "scripts/foo.py", "content": "x"}, 0, set(), {"no-ps1-file"}),
    # no-command-chaining
    ("chain/pos", "Bash", {"command": "cargo build && cargo test"}, 2, {"no-command-chaining"}, set()),
    ("chain/neg", "Bash", {"command": "cargo build"}, 0, set(), {"no-command-chaining"}),
    # launch-no-tail-pipe
    ("tail/pos", "Bash", {"command": "cargo run -p daw_gui | tail -f log"}, 2, {"launch-no-tail-pipe"}, set()),
    ("tail/neg-notail", "Bash", {"command": "cargo run -p daw_gui"}, 0, set(), {"launch-no-tail-pipe"}),
    ("tail/neg-noapp", "Bash", {"command": "cat foo | tail"}, 0, set(), {"launch-no-tail-pipe"}),
    # no-kill-running-app
    ("kill/pos-taskkill", "Bash", {"command": "taskkill /F /IM daw_gui.exe"}, 2, {"no-kill-running-app"}, set()),
    ("kill/pos-pkill", "Bash", {"command": "pkill daw_plugin_host"}, 2, {"no-kill-running-app"}, set()),
    ("kill/neg-noapp", "Bash", {"command": "kill %1"}, 0, set(), {"no-kill-running-app"}),
    ("kill/neg-noverb", "Bash", {"command": "cargo run -p daw_gui"}, 0, set(), {"no-kill-running-app"}),
    # no-duplicate-app-launch (+ none-suppress)
    ("dup/pos-run", "Bash", {"command": "cargo run -p daw_gui"}, 0, {"no-duplicate-app-launch"}, set()),
    ("dup/pos-exe", "Bash", {"command": "./target/debug/daw_gui.exe"}, 0, {"no-duplicate-app-launch"}, set()),
    ("dup/none-suppress", "Bash", {"command": "tasklist | grep daw_gui ; ./target/debug/daw_gui.exe"},
     2, set(), {"no-duplicate-app-launch"}),   # exit 2 は連結ガード由来
    ("dup/neg-build", "Bash", {"command": "cargo build -p daw_gui"}, 0, set(), {"no-duplicate-app-launch"}),
    # git-add-broad
    ("add/pos-A", "Bash", {"command": "git add -A"}, 0, {"git-add-broad"}, set()),
    ("add/pos-dot", "Bash", {"command": "git add ."}, 0, {"git-add-broad"}, set()),
    ("add/pos-u", "Bash", {"command": "git add -u"}, 0, {"git-add-broad"}, set()),
    ("add/neg-files", "Bash", {"command": "git add foo.rs bar.rs"}, 0, set(), {"git-add-broad"}),
    ("add/neg-pathdot", "Bash", {"command": "git add ./foo.rs"}, 0, set(), {"git-add-broad"}),
    # no-commit-settings-local
    ("local/pos", "Bash", {"command": "git add .claude/settings.local.json"}, 2,
     {"no-commit-settings-local"}, set()),
    ("local/neg-settings", "Bash", {"command": "git add .claude/settings.json"}, 0,
     set(), {"no-commit-settings-local"}),
    # compromise-smell-ja
    ("ja/pos-cost", "Edit", {"file_path": "x.md", "new_string": "実装コストが低いので"}, 0,
     {"compromise-smell-ja"}, set()),
    ("ja/pos-compromise", "Edit", {"file_path": "x.md", "new_string": "ここは妥協する"}, 0,
     {"compromise-smell-ja"}, set()),
    ("ja/neg-compromise-nashi", "Edit", {"file_path": "x.md", "new_string": "妥協なしで理想を追う"}, 0,
     set(), {"compromise-smell-ja"}),
    ("ja/neg-clean", "Edit", {"file_path": "x.md", "new_string": "これは理想的な実装です"}, 0,
     set(), {"compromise-smell-ja"}),
    # gui-conv-entry-format (file_glob gating)
    ("conv/neg-undated-bracket", "Write",
     {"file_path": f"{SYN_ROOT}/docs/gui_01_conversation.md", "content": "## random title [foo]\n"},
     0, set(), {"gui-conv-entry-format"}),
    ("conv/neg-goodheading", "Write",
     {"file_path": f"{SYN_ROOT}/docs/gui_01_conversation.md",
      "content": "## #72 [Open] 2026-06-15 [request] 件名\n"}, 0, set(), {"gui-conv-entry-format"}),
    ("conv/neg-globmiss", "Write",
     {"file_path": f"{SYN_ROOT}/docs/other.md", "content": "## random title [foo]\n"},
     0, set(), {"gui-conv-entry-format"}),
    # compromise-smell-en
    ("en/pos-lowrisk", "Edit", {"file_path": "x.rs", "new_string": "// this is low-risk"}, 0,
     {"compromise-smell-en"}, set()),
    ("en/pos-pragmatic", "Edit", {"file_path": "x.rs", "new_string": "the pragmatic choice"}, 0,
     {"compromise-smell-en"}, set()),
    ("en/neg-clean", "Edit", {"file_path": "x.rs", "new_string": "the ideal architecture"}, 0,
     set(), {"compromise-smell-en"}),
    # compromise-smell-ask (AskUserQuestion の選択肢で妥協案を user に提示する smell)
    ("ask/pos-approx", "AskUserQuestion",
     {"questions": [{"question": "どうしますか？", "header": "方針", "options": [
         {"label": "まず近似版（推奨）", "description": "確実・低リスクで手戻りが少ない。"},
         {"label": "理想形", "description": "大きめの改修。"}]}]},
     0, {"compromise-smell-ask"}, set()),
    ("ask/pos-cost", "AskUserQuestion",
     {"questions": [{"question": "どれにしますか？", "header": "X", "options": [
         {"label": "A", "description": "実装コストが低い。"},
         {"label": "B", "description": "影響範囲が広いが理想。"}]}]},
     0, {"compromise-smell-ask"}, set()),
    ("ask/neg-ideal-only", "AskUserQuestion",
     {"questions": [{"question": "声をどう保存しますか？", "header": "保存", "options": [
         {"label": "per-user global", "description": "コンテンツアドレスで跨ぎ再利用。"},
         {"label": "per-project", "description": "プロジェクトに自己完結。"}]}]},
     0, set(), {"compromise-smell-ask"}),
    # one-question-at-a-time (ask_multi: >1 question fires; neutral text so smell stays silent)
    ("askmulti/pos", "AskUserQuestion",
     {"questions": [
         {"question": "保存先は？", "header": "保存", "options": [{"label": "global", "description": "跨ぎ再利用。"}]},
         {"question": "命名規則は？", "header": "命名", "options": [{"label": "slug", "description": "安定 ID。"}]}]},
     0, {"one-question-at-a-time"}, set()),
    ("askmulti/neg-single", "AskUserQuestion",
     {"questions": [
         {"question": "保存先は？", "header": "保存", "options": [{"label": "global", "description": "跨ぎ再利用。"}]}]},
     0, set(), {"one-question-at-a-time"}),
    # confirm-before-commit
    ("commit/pos", "Bash", {"command": "git commit -m 'x'"}, 0, {"confirm-before-commit"}, set()),
    ("commit/neg", "Bash", {"command": "git status"}, 0, set(), {"confirm-before-commit"}),
    # no-force-worktree-remove
    ("wt/pos", "Bash", {"command": "git worktree remove --force ../wt"}, 0,
     {"no-force-worktree-remove"}, set()),
    ("wt/neg-noforce", "Bash", {"command": "git worktree remove ../wt"}, 0,
     set(), {"no-force-worktree-remove"}),
    ("wt/neg-list", "Bash", {"command": "git worktree list"}, 0, set(), {"no-force-worktree-remove"}),
    # ---- FIXED false positives: these must now stay SILENT (command_code masking) ----
    ("fix/ps-in-commit-msg", "Bash",
     {"command": "git commit -m 'chore: PowerShell 全廃 (.ps1 hooks を bash/python に移行)'"},
     0, set(), {"no-powershell-invoke"}),
    ("fix/ps-grep", "Bash", {"command": "git grep -in powershell -- scripts/ .githooks/"},
     0, set(), {"no-powershell-invoke"}),
    ("fix/kill-in-grep", "Bash", {"command": "rg -n \"kill\" daw_gui/src/smoke_test.rs"},
     0, set(), {"no-kill-running-app"}),
    ("fix/kill-in-gitlog", "Bash", {"command": "git log --oneline --grep=\"kill switch daw_audio\""},
     0, set(), {"no-kill-running-app"}),
    ("fix/settings-in-comment", "Bash",
     {"command": "git add .gitignore  # ignore .claude/settings.local.json"},
     0, set(), {"no-commit-settings-local"}),
    ("fix/settings-example", "Bash", {"command": "git add .claude/settings.local.json.example"},
     0, set(), {"no-commit-settings-local"}),
    ("fix/settings-doc", "Bash", {"command": "git add docs/settings.local.json.md"},
     0, set(), {"no-commit-settings-local"}),
    ("fix/tail-of-logfile", "Bash",
     {"command": "cat /c/Users/x/AppData/Local/daw_01/logs/daw_gui.2026-06-15 | tail -c 4000"},
     0, set(), {"launch-no-tail-pipe"}),
    ("fix/grep-daw_gui-tail", "Bash", {"command": "grep daw_gui Cargo.lock | tail -5"},
     0, set(), {"launch-no-tail-pipe"}),
    ("fix/chain-in-commit-msg", "Bash",
     {"command": "git commit -m \"feat: gate audio && video preview behind smoke test\""},
     0, set(), {"no-command-chaining"}),
    ("fix/chain-in-heredoc", "Bash",
     {"command": "cat > scripts/check.sh <<'EOF'\nif [ -f a ] && [ -f b ]; then echo ok; fi\nEOF"},
     0, set(), {"no-command-chaining"}),
    ("fix/chain-in-gitgrep", "Bash", {"command": "git log --grep='Acquire && Release'"},
     0, set(), {"no-command-chaining"}),
    ("fix/ps-mention-in-heredoc", "Bash",
     {"command": "cat > docs/migration.md <<'EOF'\n旧 hook は powershell -File x.ps1 で起動。bash へ移行済み。\nEOF"},
     0, set(), {"no-powershell-invoke", "no-ps1-via-bash"}),
    ("fix/smell-en-in-memory", "Write",
     {"file_path": f"{SYN_USER_PROJ}/memory/feedback_pursue_ideal_only.md",
      "content": "検出語: low-risk / pragmatic / workaround / compromise。これらが出たら違反。"},
     0, set(), {"compromise-smell-en"}),
    ("fix/smell-ja-in-claude-md", "Edit",
     {"file_path": f"{SYN_ROOT}/CLAUDE.md", "new_string": "実装コスト/許容範囲/妥協/現実的に を禁ずる"},
     0, set(), {"compromise-smell-ja"}),
    ("fix/smell-ja-nonai", "Edit",
     {"file_path": f"{SYN_ROOT}/DESIGN.md", "new_string": "ここは妥協のない理想実装を採る"},
     0, set(), {"compromise-smell-ja"}),
    ("fix/conv-section-link", "Write",
     {"file_path": f"{SYN_ROOT}/docs/gui_01_conversation.md",
      "content": "## 参考リンク [clap repo](https://github.com/free-audio/clap)\n"},
     0, set(), {"gui-conv-entry-format"}),
    ("fix/commit-dry-run", "Bash", {"command": "git commit --dry-run"}, 0,
     set(), {"confirm-before-commit"}),
    ("fix/commit-help", "Bash", {"command": "git commit --help"}, 0, set(), {"confirm-before-commit"}),
    ("fix/worktree-doc-grep", "Bash",
     {"command": "grep -rn 'git worktree remove --force' scripts/"}, 0,
     set(), {"no-force-worktree-remove"}),
    ("fix/dup-smoke-test", "Bash",
     {"command": "cargo run -p daw_gui -- --smoke-test daw_gui/tests/fixtures/smoke_test.mp4"},
     0, set(), {"no-duplicate-app-launch"}),
    # ---- CLOSED coverage gaps: these must now FIRE ----
    ("gap/ps-invoke-flag", "Bash", {"command": "powershell -NoProfile -Command Get-Date"}, 2,
     {"no-powershell-invoke"}, set()),
    ("gap/ps-ise", "Bash", {"command": "powershell_ise scripts/foo.ps1"}, 2,
     {"no-powershell-invoke"}, set()),
    ("gap/pwsh-via-cmd", "Bash", {"command": "cmd //c pwsh.exe -NoProfile -Command x"}, 2,
     {"no-powershell-invoke"}, set()),
    ("gap/ps1-via-redirect", "Bash",
     {"command": "cat > scripts/build.ps1 <<'EOF'\nWrite-Host hi\nEOF"}, 2,
     {"no-ps1-via-bash"}, set()),
    ("gap/psm1-file", "Write", {"file_path": "scripts/AheHelpers.psm1", "content": "x"}, 2,
     {"no-ps1-file"}, set()),
    ("gap/pwsh-shebang", "Write",
     {"file_path": "scripts/release.sh", "content": "#!/usr/bin/env pwsh\nWrite-Host build\n"}, 2,
     {"no-pwsh-shebang"}, set()),
    ("gap/chain-semicolon", "Bash", {"command": "git add a.rs ; git status"}, 2,
     {"no-command-chaining"}, set()),
    ("gap/chain-or", "Bash", {"command": "cargo build || echo failed"}, 2,
     {"no-command-chaining"}, set()),
    ("gap/launch-pipe-head", "Bash", {"command": "cargo run -p daw_gui 2>&1 | head -30"}, 2,
     {"launch-no-tail-pipe"}, set()),
    ("gap/kill-killall", "Bash", {"command": "killall daw_gui.exe"}, 2, {"no-kill-running-app"}, set()),
    ("gap/kill-pkill", "Bash", {"command": "pkill daw_plugin_host"}, 2, {"no-kill-running-app"}, set()),
    ("gap/dup-make-run", "Bash", {"command": "make run"}, 0, {"no-duplicate-app-launch"}, set()),
    ("gap/add-dir", "Bash", {"command": "git add ui/"}, 0, {"git-add-broad"}, set()),
    ("gap/add-forcedir", "Bash", {"command": "git add -f .claude/"}, 2,
     {"no-commit-settings-local", "git-add-broad"}, set()),
    ("gap/add-update", "Bash", {"command": "git add --update"}, 0, {"git-add-broad"}, set()),
    ("gap/add-magic", "Bash", {"command": "git add :/"}, 0, {"git-add-broad"}, set()),
    ("gap/settings-stage", "Bash", {"command": "git stage .claude/settings.local.json"}, 2,
     {"no-commit-settings-local"}, set()),
    ("gap/commit-gitC", "Bash", {"command": "git -C ../wt commit -m x"}, 0,
     {"confirm-before-commit"}, set()),
    ("gap/commit-merge-noff", "Bash", {"command": "git merge --no-ff feature/x"}, 0,
     {"confirm-before-commit"}, set()),
    ("gap/commit-cherrypick", "Bash", {"command": "git cherry-pick 1de8027"}, 0,
     {"confirm-before-commit"}, set()),
    ("gap/wt-shortf", "Bash", {"command": "git worktree remove -f ../wt"}, 0,
     {"no-force-worktree-remove"}, set()),
    ("gap/wt-cleanup-force", "Bash", {"command": "bash scripts/cleanup_worktree.sh --name x --force"},
     0, {"no-force-worktree-remove"}, set()),
    ("gap/wt-prune", "Bash", {"command": "git worktree prune --expire=now"}, 0,
     {"no-force-worktree-remove"}, set()),
    ("gap/wt-auto-hook", "Write",
     {"file_path": f"{SYN_ROOT}/.githooks/post-merge",
      "content": "#!/usr/bin/env bash\nbash scripts/cleanup_worktree.sh --merged --force\n"},
     0, {"no-auto-worktree-hook"}, set()),
    ("gap/smell-en-lowest", "Edit", {"file_path": "x.rs", "new_string": "// lowest-risk: clone here"},
     0, {"compromise-smell-en"}, set()),
    ("gap/smell-en-kludge", "Edit", {"file_path": "x.rs", "new_string": "// kludge: special-case it"},
     0, {"compromise-smell-en"}, set()),
    ("gap/smell-ja-tema", "Write", {"file_path": "x.md", "content": "実装の手間が大きいので見送る"},
     0, {"compromise-smell-ja"}, set()),
    ("gap/conv-dated-malformed", "Write",
     {"file_path": f"{SYN_ROOT}/docs/gui_01_conversation.md",
      "content": "## 要望: track 名の自動コントラスト 2026-06-15\n"},
     0, {"gui-conv-entry-format"}, set()),
    # ---- no-command-chaining の絞り込み (メモリが許容している形は発火しない) ----
    # メモリ feedback_no_command_chaining は「**独立した**コマンドの連結」を禁じ、
    # 「シェル変数の共有や順序依存がある場合のみ連結を検討」と明示的に許容している。
    # block へ上げた以上、許容形で誤爆すると正当な作業が止まるので全部固定する。
    ("chain/neg-for-loop", "Bash", {"command": "for f in a b; do echo $f; done"}, 0,
     set(), {"no-command-chaining"}),
    ("chain/neg-if-then", "Bash", {"command": "if [ -f x ]; then echo y; fi"}, 0,
     set(), {"no-command-chaining"}),
    # 制御構文はキーワード単体ではなく対で判定する。単体だと `echo done` の done に当たって
    # 「done を含む連結コマンド」が丸ごと素通りする (実装時に踏んだ)。
    ("chain/pos-done-as-word", "Bash", {"command": "echo done ; echo more"}, 2,
     {"no-command-chaining"}, set()),
    ("chain/neg-fallback-true", "Bash", {"command": "cargo build || true"}, 0,
     set(), {"no-command-chaining"}),
    ("chain/neg-pipe-only", "Bash", {"command": "ls -la | head -5"}, 0,
     set(), {"no-command-chaining"}),
    ("chain/neg-varshare-single", "Bash", {"command": 'SP="/tmp/x"; ls "$SP"'}, 0,
     set(), {"no-command-chaining"}),
    ("chain/pos-varshare-multi", "Bash", {"command": 'SP="/tmp/x"; ls "$SP"; echo done'}, 2,
     {"no-command-chaining"}, set()),
    ("chain/neg-declared", "Bash", {"command": "DAW01_CHAIN=1 rm -rf x && mkdir x"}, 0,
     set(), {"no-command-chaining"}),
    # ---- 検査系が自分自身を検査対象にしてしまう問題 (fixture の自己発火) ----
    # compromise-smell の検出語は、この harness の fixture とレジストリ本体に必ず出てくる。
    # 除外が将来外れたら気付けるよう、除外側と **肯定側 (対照)** を対にして固定する。
    # 肯定側が無いと「常に発火しない実装」でも緑になり、検査の証明にならない。
    ("selfref/neg-harness-fixture", "Write",
     {"file_path": "scripts/test_guards.py", "content": '("x", "Edit", {"new_string": "low-risk hack"})'},
     0, set(), {"compromise-smell-en"}),
    ("selfref/neg-registry", "Write",
     {"file_path": ".claude/guards.jsonl", "content": '{"id":"x","all":["妥協|実装コスト"]}'},
     0, set(), {"compromise-smell-ja"}),
    ("selfref/neg-engine", "Write",
     {"file_path": "scripts/guard_engine.py", "content": "# 検出語: low-risk / pragmatic\n"},
     0, set(), {"compromise-smell-en"}),
    ("selfref/pos-control-en", "Write",   # 対照: 同じ文字列でも通常のファイルなら発火する
     {"file_path": "daw_gui/src/view/root.rs", "content": "// low-risk hack\n"},
     0, {"compromise-smell-en"}, set()),
    ("selfref/pos-control-ja", "Write",
     {"file_path": "docs/plan_x.html", "content": "実装コストが低いのでこちらにする\n"},
     0, {"compromise-smell-ja"}, set()),
    # ---- 「起動しない」つもりの検証コマンドが実際には起動する経路 ----
    ("testrun/pos-tests", "Bash", {"command": "cargo test -p daw_gui --tests"}, 2,
     {"no-bulk-test-run"}, set()),
    ("testrun/pos-all-targets", "Bash", {"command": "cargo test --workspace --all-targets"}, 2,
     {"no-bulk-test-run"}, set()),
    ("testrun/pos-bare-daw_gui", "Bash", {"command": "cargo test -p daw_gui"}, 2,
     {"no-bulk-test-run"}, set()),
    ("testrun/pos-launching-target", "Bash",
     {"command": "cargo test -p daw_gui --features daw_gui/script --test clip_rename_smoke"}, 2,
     {"no-app-launching-test-target"}, set()),
    # 名前に smoke が付かないのに起動する = 名前判定では捕まらない 2 件
    ("testrun/pos-vst3-no-smoke-in-name", "Bash",
     {"command": "cargo test -p daw_gui --test pdc_real_vst3"}, 2,
     {"no-app-launching-test-target"}, set()),
    ("testrun/pos-sidechain-vst3", "Bash",
     {"command": "cargo test -p daw_gui --test sidechain_real_vst3"}, 2,
     {"no-app-launching-test-target"}, set()),
    ("testrun/neg-named-safe-targets", "Bash",
     {"command": "cargo test -p daw_gui --test arr_widget --test pr_widget"}, 0,
     set(), {"no-bulk-test-run", "no-app-launching-test-target"}),
    ("testrun/neg-check-all-targets", "Bash", {"command": "cargo check --workspace --all-targets"},
     0, set(), {"no-bulk-test-run"}),
    ("testrun/neg-clippy-all-targets", "Bash",
     {"command": "cargo clippy --workspace --all-targets -- -D warnings"}, 0,
     set(), {"no-bulk-test-run"}),
    ("testrun/neg-other-crate", "Bash", {"command": "cargo test -p common"}, 0,
     set(), {"no-bulk-test-run"}),
    ("testrun/neg-other-crate-named", "Bash",
     {"command": "cargo test -p common --test model_roundtrip"}, 0,
     set(), {"no-bulk-test-run", "no-app-launching-test-target"}),
    ("testrun/neg-mention-in-commit-msg", "Bash",
     {"command": "git commit -m 'chore: cargo test --all-targets をやめる'"}, 0,
     set(), {"no-bulk-test-run"}),
    # ---- architecture invariants (docs/plan_arch_refactor.md:469) ----
    # Every negative below is a REAL line taken from the current tree
    # (`git grep -n untagged/push_undo_snapshot/MainToChild -- '*.rs'`). The removal of
    # these constructs is documented in doc comments all over common/src, so a matcher
    # that counts comment mentions would fire on ~40 existing lines. arch_lint.sh has
    # the same problem and solves it with strip_comments; these cases pin the parity.
    # Since r.md #76 (2026-08-28) arch_lint.sh delegates that classification to the Rust
    # lexer in scripts/loc_budget.py (--filter-comments), so parity holds only for
    # LEADING line comments: inside a raw string or a /* … */ block the two disagree,
    # and arch-lint is the accurate one. The guard errs toward nudging more, which is the
    # safe direction for a write-time nudge (same rationale as escalate: false).
    ("arch/untagged-pos", "Write",
     {"file_path": "common/src/model/content.rs",
      "content": "#[derive(Deserialize)]\n#[serde(untagged)]\npub enum ClipContent {}\n"},
     0, {"arch-no-new-untagged"}, set()),
    ("arch/untagged-neg-doccomment", "Edit",
     {"file_path": "common/src/model/content.rs",
      "new_string": "/// `#[serde(untagged)]` lets v6 `.daw` files (which serialised\n"},
     0, set(), {"arch-no-new-untagged"}),
    ("arch/untagged-neg-linecomment", "Edit",
     {"file_path": "common/src/model/content.rs",
      "new_string": "// v30 (arch-refactor §10): 明示 `type` タグで variant を判別する (旧 `#[serde(untagged)]` は\n"},
     0, set(), {"arch-no-new-untagged"}),
    ("arch/untagged-neg-globmiss", "Write",
     {"file_path": "daw_gui/src/view/root.rs", "content": "#[serde(untagged)]\n"},
     0, set(), {"arch-no-new-untagged"}),
    ("arch/tuplekey-pos", "Edit",
     {"file_path": "daw_gui/src/state/ipc.rs",
      "new_string": "    pub slot_has_gui: std::collections::HashMap<(u32, u32), bool>,\n"},
     0, {"arch-no-positional-tuple-key"}, set()),
    ("arch/tuplekey-pos-other-crate", "Edit",   # file_glob list: ui/crates も対象
     {"file_path": "ui/crates/ui/src/widgets/x.rs",
      "new_string": "    pool: HashMap<(u32, u32), SizePool>,\n"},
     0, {"arch-no-positional-tuple-key"}, set()),
    ("arch/tuplekey-neg-comment", "Edit",
     {"file_path": "daw_gui/src/state/ipc.rs",
      "new_string": "    // 旧実装は pool: HashMap<(u32, u32), SizePool> だった\n"},
     0, set(), {"arch-no-positional-tuple-key"}),
    ("arch/tuplekey-neg-stable-id", "Edit",
     {"file_path": "daw_gui/src/state/ipc.rs",
      "new_string": "    pub slot_has_gui: std::collections::HashMap<u64, bool>,\n"},
     0, set(), {"arch-no-positional-tuple-key"}),
    ("arch/undo-pos", "Edit",
     {"file_path": "daw_gui/src/handler/tracks.rs", "new_string": "        self.push_undo_snapshot();\n"},
     0, {"arch-no-direct-undo-snapshot"}, set()),
    ("arch/undo-neg-doccomment", "Edit",
     {"file_path": "daw_gui/src/event.rs",
      "new_string": "    /// `push_undo_snapshot` を明示呼び出し (= 1 完了 = 1 Undo step)。\n"},
     0, set(), {"arch-no-direct-undo-snapshot"}),
    ("arch/undo-neg-innercomment", "Edit",
     {"file_path": "daw_gui/src/state/song_doc.rs",
      "new_string": "//! 旧 `is_undoable` whitelist (102 variants) と手動 `push_undo_snapshot`\n"},
     0, set(), {"arch-no-direct-undo-snapshot"}),
    ("arch/protocol-pos", "Edit",
     {"file_path": "common/src/protocol.rs", "new_string": "pub enum MainToChild { Ping }\n"},
     0, {"arch-no-legacy-protocol-enum"}, set()),
    ("arch/protocol-neg-doccomment", "Edit",
     {"file_path": "common/src/model/content.rs",
      "new_string": "/// `MainToChild::SetGeneratedAudio`.\n"},
     0, set(), {"arch-no-legacy-protocol-enum"}),
    ("arch/infinite-pos-audio", "Edit",
     {"file_path": "daw_audio/src/audio_worker.rs",
      "new_string": "            WaitForSingleObject(wake.0, INFINITE);\n"},
     0, {"arch-rt-no-infinite-wait"}, {"arch-rt-no-infinite-wait-ph"}),
    ("arch/infinite-pos-pluginref", "Edit",   # file_glob list: 単一ファイル指定も対象
     {"file_path": "common/src/plugin_ref.rs",
      "new_string": "        WaitForSingleObject(h, INFINITE);\n"},
     0, {"arch-rt-no-infinite-wait"}, set()),
    ("arch/infinite-pos-ph", "Edit",
     {"file_path": "daw_plugin_host/src/process_server.rs",
      "new_string": "            WaitForSingleObject(wake.0, INFINITE);\n"},
     0, {"arch-rt-no-infinite-wait-ph"}, {"arch-rt-no-infinite-wait"}),
    ("arch/infinite-neg-sanctioned", "Edit",
     {"file_path": "daw_audio/src/audio_worker.rs",
      "new_string": "            WaitForSingleObject(wake.0, INFINITE); // arch-lint: allow-infinite\n"},
     0, set(), {"arch-rt-no-infinite-wait"}),
    ("arch/infinite-neg-bounded", "Edit",
     {"file_path": "daw_audio/src/audio_worker.rs",
      "new_string": "            WaitForSingleObject(wake.0, DISPATCH_TIMEOUT_MS);\n"},
     0, set(), {"arch-rt-no-infinite-wait"}),
    ("arch/infinite-neg-comment", "Edit",
     {"file_path": "daw_audio/src/audio_worker.rs",
      "new_string": "            // 旧実装は WaitForSingleObject(wake.0, INFINITE) だった\n"},
     0, set(), {"arch-rt-no-infinite-wait"}),
    # engine robustness
    ("robust/unrelated-tool", "Read", {"file_path": "x.rs"}, 0, set(), set()),
]


def check_engine():
    for label, tool, ti, exp_exit, must, mustnt in CASES:
        rc, out = run_engine(tool, ti)
        f = fired_ids(out)
        problems = []
        if rc != exp_exit:
            problems.append("exit=%d want %d" % (rc, exp_exit))
        for g in must:
            if g not in f:
                problems.append("expected fire %s" % g)
        for g in mustnt:
            if g in f:
                problems.append("unexpected fire %s" % g)
        if problems:
            bad("engine:%s" % label, "; ".join(problems) + " | fired=%s" % sorted(f))
        else:
            ok("engine:%s" % label)


def check_engine_robustness():
    engine = sb_script(_sandbox, "guard_engine.py")
    # empty stdin
    p = subprocess.run([sys.executable, engine], input=b"", stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, env=_sandbox_env(_sandbox))
    ok("robust:empty-stdin") if p.returncode == 0 else bad("robust:empty-stdin", "exit=%d" % p.returncode)
    # malformed json
    p = subprocess.run([sys.executable, engine], input=b"{not json", stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, env=_sandbox_env(_sandbox))
    ok("robust:bad-json") if p.returncode == 0 else bad("robust:bad-json", "exit=%d" % p.returncode)
    # field selection: MultiEdit edits[].new_string
    rc, out = run_engine("MultiEdit", {"file_path": "x.rs",
                                       "edits": [{"old_string": "a", "new_string": "low-risk hack"}]})
    ok("robust:multiedit-field") if "compromise-smell-en" in fired_ids(out) else \
        bad("robust:multiedit-field", "edits[].new_string not scanned | %s" % sorted(fired_ids(out)))


# ------------------------------------------------- registry defects are NOT silent
# The 2026-08-22 outage: guards.jsonl had been gone for five days and produced no
# symptom at all, because the engine did `if not isfile: return 0`. A broken registry
# must be VISIBLE (stdout + exit 0 -- loud, but never wedging the session that is
# trying to repair it).
def check_registry_defect_is_reported():
    cases = [
        ("missing", None, "レジストリが存在しません"),
        ("empty", "", "ルールが 1 件もありません"),
        ("comments-only", "# only a comment\n\n", "ルールが 1 件もありません"),
        ("unparseable", "{not json\nalso not json\n", "JSON として読めません"),
    ]
    for label, content, expect in cases:
        sb = make_sandbox("guard_defect_", guards_src=None)
        if content is not None:
            with open(sb_registry(sb), "w", encoding="utf-8") as fh:
                fh.write(content)
        rc, out = run_engine("Bash", {"command": "git add -A"}, sandbox=sb)
        problems = []
        if rc != 0:
            problems.append("exit=%d want 0 (fail-open)" % rc)
        if expect not in out:
            problems.append("警告文に %r が無い | out=%r" % (expect, out[:160]))
        if fired_ids(out):
            problems.append("ルールが無いのに発火 %s" % sorted(fired_ids(out)))
        if problems:
            bad("defect:%s" % label, "; ".join(problems))
        else:
            ok("defect:%s" % label)
        # ...and the warning is once per session, not on every single tool call
        rc2, out2 = run_engine("Bash", {"command": "git add -A"}, sandbox=sb)
        if rc2 == 0 and expect not in out2:
            ok("defect:%s-once-per-session" % label)
        else:
            bad("defect:%s-once-per-session" % label,
                "2 回目も警告が出た (exit=%d)" % rc2)
        shutil.rmtree(sb, ignore_errors=True)


# ------------------------------------------------------------- path/slug derivation
def check_paths():
    """The slug rule must reproduce Claude Code's project-directory naming.
    Hardcoding one machine's slug is what made every other checkout fail open.

    Two layers: synthetic cases pin the RULE (and carry no real path, since this
    file ships in a public repo), then a live cross-check confirms the rule still
    matches what Claude Code actually created on this host."""
    # slug() runs abspath first, and abspath is host-dependent: on Windows a
    # POSIX-rooted path gets the current drive prepended ("/src" -> "F:\\src"), and on
    # POSIX a drive-letter path is treated as relative. So the synthetic root has to be
    # absolute FOR THIS HOST, exactly like SYN_ROOT above.
    if os.name == "nt":
        root_in, root_out, alt_in = "C:\\src\\my_app", "C--src-my-app", "C:/src/my_app"
    else:
        root_in, root_out, alt_in = "/src/my_app", "-src-my-app", "/src/my_app"
    wt_in = root_in + os.sep.join(["", ".claude", "worktrees", "feature-x"])
    cases = [
        (root_in, root_out),
        (alt_in, root_out),
        (wt_in, root_out + "--claude-worktrees-feature-x"),
    ]
    for path, expect in cases:
        got = ahe_paths.slug(path)
        ok("paths:slug(%s)" % path) if got == expect else \
            bad("paths:slug(%s)" % path, "got %r want %r" % (got, expect))
    # a worktree must resolve to the MAIN checkout, so all worktrees share one state dir
    got = ahe_paths.slug(ahe_paths.main_checkout(wt_in))
    ok("paths:worktree-shares-main-state") if got == root_out else \
        bad("paths:worktree-shares-main-state", "got %r want %r" % (got, root_out))

    # live cross-check: this harness runs from inside a Claude Code checkout, so the
    # directory our rule derives for THIS repo must be one Claude Code really made.
    projects = os.path.join(os.path.expanduser("~"), ".claude", "projects")
    if os.path.isdir(projects):
        derived = os.path.join(projects, ahe_paths.slug(ahe_paths.REPO_ROOT))
        ok("paths:slug-matches-live-dir") if os.path.isdir(derived) else \
            bad("paths:slug-matches-live-dir",
                "導出した %r が実在しない = slug 規則が Claude Code とズレている" % derived)
    else:
        # Not a hidden skip: the name says exactly what was and was not verified.
        ok("paths:slug-live-crosscheck-unavailable(~/.claude/projects なし)")

    # the registry is repo-relative and tracked
    if os.path.isfile(REAL_GUARDS):
        ok("paths:registry-tracked-in-repo")
    else:
        bad("paths:registry-tracked-in-repo", "missing %s" % REAL_GUARDS)


# ---------------------------------------------------------------- 3. destructive
DCASES = [
    ("destruct/pos-var", "rm -rf $TMPDIR/build", 2),
    ("destruct/pos-env-win", "Remove-Item -Recurse -Force %TEMP%", 2),
    ("destruct/pos-tilde", "rm -rf ~", 2),
    ("destruct/pos-root", "rm -rf /", 2),
    ("destruct/pos-star", "rm -rf *", 2),
    ("destruct/neg-literal", "rm -rf target/debug", 0),
    ("destruct/neg-norec", "rm foo.txt", 0),
    ("destruct/neg-perstmt", "echo $HOME ; rm -rf target/debug", 0),
]


def check_destruct():
    for label, cmd, exp in DCASES:
        rc, out = run_destruct(cmd)
        if rc == exp:
            ok(label)
        else:
            bad(label, "exit=%d want %d | %s" % (rc, exp, out[:120].replace("\n", " ")))


# ---------------------------------------------------------------- 4. escalation
RULE_ESCALATE = {"id": "test-escalate", "source": "feedback_no_command_chaining",
                 "tool": ["Bash"], "field": "command", "all": ["&&"],
                 "action": "warn", "msg": "test"}


def seed_escalation_sandbox(prefix, rule, sessions):
    """A sandbox repo holding one warn rule plus a hit history."""
    sb = make_sandbox(prefix, guards_src=None)
    with open(sb_registry(sb), "w", encoding="utf-8") as fh:
        fh.write(json.dumps(rule, ensure_ascii=False) + "\n")
    os.makedirs(sb_proj(sb), exist_ok=True)
    with open(os.path.join(sb_proj(sb), "guard_hits.jsonl"), "w", encoding="utf-8") as fh:
        for s in sessions:
            fh.write(json.dumps({"ts": "t", "session": s, "guard": rule["id"],
                                 "source": rule.get("source", ""), "tool": "Bash",
                                 "action": "warn"}) + "\n")
    return sb


def run_reflect(sb):
    subprocess.run([sys.executable, sb_script(sb, "reflect.py")],
                   input=json.dumps({"session_id": "sessD-stop"}).encode("utf-8"),
                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=_sandbox_env(sb))


def read_overlay(sb):
    try:
        with open(sb_state(sb), "r", encoding="utf-8") as fh:
            return (json.load(fh) or {}).get("escalated") or {}
    except Exception:
        return {}


def check_escalation():
    """warn -> block must happen in the OVERLAY, leaving the tracked registry byte-identical."""
    sb = seed_escalation_sandbox("guard_escal_", RULE_ESCALATE, ("sessA", "sessB", "sessC"))
    before = open(sb_registry(sb), "rb").read()
    run_reflect(sb)

    # (1) the tracked registry must not have been touched -- this is the invariant that
    #     made it safe to put the rules under version control in the first place.
    after = open(sb_registry(sb), "rb").read()
    ok("escalate:registry-untouched") if after == before else \
        bad("escalate:registry-untouched", "reflect.py rewrote the tracked registry")

    # (2) the overlay records the escalation
    if read_overlay(sb).get("test-escalate") == "block":
        ok("escalate:overlay-written@3sessions")
    else:
        bad("escalate:overlay-written@3sessions", "overlay=%r" % read_overlay(sb))

    # (3) round-trip: the engine reads the overlay back and now BLOCKS
    rc, out = run_engine("Bash", {"command": "a && b"}, sandbox=sb)
    if rc == 2 and "test-escalate" in fired_ids(out):
        ok("escalate:engine-applies-overlay")
    else:
        bad("escalate:engine-applies-overlay", "exit=%d fired=%s" % (rc, sorted(fired_ids(out))))
    shutil.rmtree(sb, ignore_errors=True)

    # control: 2 sessions must NOT escalate
    sb2 = seed_escalation_sandbox("guard_escal2_", RULE_ESCALATE, ("sessA", "sessB"))
    run_reflect(sb2)
    if read_overlay(sb2).get("test-escalate") is None:
        ok("escalate:no-escalate@2sessions")
    else:
        bad("escalate:no-escalate@2sessions", "overlay=%r" % read_overlay(sb2))
    rc, out = run_engine("Bash", {"command": "a && b"}, sandbox=sb2)
    ok("escalate:still-warn@2sessions") if rc == 0 else \
        bad("escalate:still-warn@2sessions", "exit=%d" % rc)
    shutil.rmtree(sb2, ignore_errors=True)

    # opt-out: a warn rule with escalate:false must NOT escalate even at 4 sessions
    rule_opt = dict(RULE_ESCALATE, id="test-escalate-optout", escalate=False)
    sb3 = seed_escalation_sandbox("guard_escal3_", rule_opt,
                                  ("sessA", "sessB", "sessC", "sessD"))
    run_reflect(sb3)
    if read_overlay(sb3).get("test-escalate-optout") is None:
        ok("escalate:optout-respected@4sessions")
    else:
        bad("escalate:optout-respected@4sessions", "overlay=%r" % read_overlay(sb3))

    # ...and even a hand-forged overlay entry cannot resurrect an opted-out rule
    os.makedirs(sb_proj(sb3), exist_ok=True)
    with open(sb_state(sb3), "w", encoding="utf-8") as fh:
        json.dump({"version": 1, "escalated": {"test-escalate-optout": "block"}}, fh)
    rc, out = run_engine("Bash", {"command": "a && b"}, sandbox=sb3)
    ok("escalate:optout-ignores-stale-overlay") if rc == 0 else \
        bad("escalate:optout-ignores-stale-overlay", "exit=%d (block した)" % rc)
    shutil.rmtree(sb3, ignore_errors=True)


# ------------------------------------------------- worktree-path-discipline (cwd)
def check_worktree_guard():
    """worktree-path-discipline is a cwd-relational guard, so cases must pass cwd."""
    WT = f"{SYN_ROOT}/.claude/worktrees/foo"
    MAIN = SYN_ROOT
    # (label, file_path, cwd, expect_exit, should_fire)
    WCASES = [
        ("wtpath/pos-main", f"{SYN_ROOT}/src/x.rs", WT, 2, True),
        ("wtpath/pos-backslash", f"{SYN_ROOT_BS}\\daw_gui\\src\\app.rs", WT, 2, True),
        ("wtpath/pos-sibling", f"{SYN_ROOT}/.claude/worktrees/bar/x.rs", WT, 2, True),
        ("wtpath/neg-own-worktree", f"{SYN_ROOT}/.claude/worktrees/foo/src/x.rs", WT, 0, False),
        ("wtpath/neg-memory-outside-repo",
         f"{SYN_USER_PROJ}/memory/feedback_x.md", WT, 0, False),
        ("wtpath/neg-relative", "src/x.rs", WT, 0, False),
        ("wtpath/neg-main-session", f"{SYN_ROOT}/src/x.rs", MAIN, 0, False),
        ("wtpath/neg-no-cwd", f"{SYN_ROOT}/src/x.rs", None, 0, False),
    ]
    for label, fp, cwd, exp_exit, should in WCASES:
        rc, out = run_engine("Write", {"file_path": fp, "content": "x"}, cwd=cwd)
        f = fired_ids(out)
        problems = []
        if rc != exp_exit:
            problems.append("exit=%d want %d" % (rc, exp_exit))
        fired = "worktree-path-discipline" in f
        if should and not fired:
            problems.append("expected fire")
        if not should and fired:
            problems.append("unexpected fire")
        if problems:
            bad("wtguard:%s" % label, "; ".join(problems) + " | fired=%s" % sorted(f))
        else:
            ok("wtguard:%s" % label)


# ------------------------------------------------------- no-cd-prefix (cwd-relational)
def check_cd_guard():
    """`cd` is only a violation RELATIVE to where the session already is: cd'ing to the
    cwd is redundant, and cd'ing from a worktree to the main checkout or a sibling is
    the cross-agent hazard. `cd /tmp` or `cd build/` is legitimate and must stay silent
    -- which is why this is a logic guard, not a regex on "^cd "."""
    WT = f"{SYN_ROOT}/.claude/worktrees/foo"
    CCASES = [
        ("cd/pos-same", f'cd {WT} && cargo build', WT, 2, True),
        ("cd/pos-same-quoted", f'cd "{WT}" && cargo build', WT, 2, True),
        ("cd/pos-same-trailing-slash", f'cd {WT}/ && cargo build', WT, 2, True),
        ("cd/pos-main-from-worktree", f'cd {SYN_ROOT} && git status', WT, 2, True),
        ("cd/pos-sibling-worktree", f'cd {SYN_ROOT}/.claude/worktrees/bar && ls', WT, 2, True),
        ("cd/pos-backslash", f'cd {SYN_ROOT_BS}\\.claude\\worktrees\\foo && ls', WT, 2, True),
        ("cd/pos-same-in-main-session", f'cd {SYN_ROOT} && git status', SYN_ROOT, 2, True),
        ("cd/neg-elsewhere", "cd /tmp", WT, 0, False),
        ("cd/neg-subdir", "cd daw_gui", WT, 0, False),
        ("cd/neg-own-subdir-abs", f"cd {WT}/daw_gui", WT, 0, False),
        ("cd/neg-not-a-cd", "cargo build", WT, 0, False),
        ("cd/neg-no-cwd", f"cd {WT}", None, 0, False),
    ]
    for label, cmd, cwd, exp_exit, should in CCASES:
        rc, out = run_engine("Bash", {"command": cmd}, cwd=cwd)
        f = fired_ids(out)
        problems = []
        if rc != exp_exit:
            problems.append("exit=%d want %d" % (rc, exp_exit))
        fired = "no-cd-prefix" in f
        if should and not fired:
            problems.append("expected fire")
        if not should and fired:
            problems.append("unexpected fire")
        if problems:
            bad("cdguard:%s" % label, "; ".join(problems) + " | fired=%s" % sorted(f))
        else:
            ok("cdguard:%s" % label)


# --------------------------------------- app-launching test targets stay in sync
def check_launching_targets_list():
    """The registry enumerates the daw_gui test targets that spawn the app. That list
    must be DERIVED from the tree, never guessed from names: pdc_real_vst3 and
    sidechain_real_vst3 launch without "smoke" in the name, and arr_widget / pr_widget /
    font_picker do not launch at all. The criterion is whether the target builds
    CARGO_BIN_EXE_daw_gui. Regenerate it here so a new target cannot silently escape the
    guard (a hand-written list is exactly how the first version of this got it wrong)."""
    tests_dir = os.path.join(ahe_paths.REPO_ROOT, "daw_gui", "tests")
    if not os.path.isdir(tests_dir):
        bad("launchtargets:tests-dir", "daw_gui/tests が無い: %s" % tests_dir)
        return
    actual = set()
    for name in os.listdir(tests_dir):
        if not name.endswith(".rs"):
            continue
        try:
            with open(os.path.join(tests_dir, name), "r", encoding="utf-8", errors="replace") as fh:
                if "CARGO_BIN_EXE_daw_gui" in fh.read():
                    actual.add(name[:-3])
        except Exception as e:
            bad("launchtargets:read(%s)" % name, str(e))
            return

    declared = set()
    with open(GUARDS_SRC, "r", encoding="utf-8") as fh:
        for line in fh:
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            try:
                r = json.loads(s)
            except Exception:
                continue
            if r.get("id") != "no-app-launching-test-target":
                continue
            pat = " ".join(r.get("all") or [])
            m = re.search(r"--test\\s\+\(\?:([^)]*)\)", pat)
            if m:
                declared = {t for t in m.group(1).split("|") if t}

    if not declared:
        bad("launchtargets:rule-present", "no-app-launching-test-target が読めない")
    elif declared == actual:
        ok("launchtargets:in-sync(%d)" % len(actual))
    else:
        bad("launchtargets:in-sync",
            "レジストリと実際の target がズレています。"
            "抜け=%s / 余分=%s (基準: grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs)"
            % (sorted(actual - declared), sorted(declared - actual)))

    # Makefile の test-nolaunch も **同じ基準から**導いていること。ガードと Makefile が
    # 別々の判定を持つと片方だけ通る状態ができる (これは実行時と書く瞬間の二重化であって、
    # 判定の二重化ではない)。
    mk = os.path.join(ahe_paths.REPO_ROOT, "Makefile")
    try:
        with open(mk, "r", encoding="utf-8", errors="replace") as fh:
            mk_text = fh.read()
    except Exception as e:
        bad("launchtargets:makefile-read", str(e))
        return
    ok("launchtargets:makefile-derives-from-criterion") if "CARGO_BIN_EXE_daw_gui" in mk_text else \
        bad("launchtargets:makefile-derives-from-criterion",
            "Makefile が基準 CARGO_BIN_EXE_daw_gui から導いていません (手書き列挙の疑い)")
    # コメント行は数えない (なぜ名前で判定してはいけないかの説明に target 名が出るため。
    # arch_lint.sh の strip_comments / arch-* ガードと同じ扱い)。ここは Makefile が対象で
    # `#` 始まりの行しか無いので行頭判定で足りる — r.md #76 以降 arch_lint.sh 側は
    # scripts/loc_budget.py の Rust 字句解析に寄せてあるが、それは .rs 専用なので
    # ここには使わない (raw string / ブロックコメントが存在しない)。
    mk_code = "\n".join(ln for ln in mk_text.splitlines() if not ln.lstrip().startswith("#"))
    handwritten = sorted(t for t in actual if t in mk_code)
    if handwritten:
        bad("launchtargets:makefile-no-handwritten-list",
            "Makefile に起動する target 名が直書きされています (基準から導くこと): %s" % handwritten)
    else:
        ok("launchtargets:makefile-no-handwritten-list")


def main():
    check_paths()
    check_registry()
    check_launching_targets_list()
    check_engine()
    check_engine_robustness()
    check_registry_defect_is_reported()
    check_worktree_guard()
    check_cd_guard()
    check_destruct()
    check_escalation()
    shutil.rmtree(_sandbox, ignore_errors=True)

    print("\n=== GUARD VERIFICATION ===")
    print("PASS: %d   FAIL: %d\n" % (len(PASS), len(FAIL)))
    if FAIL:
        for name, detail in FAIL:
            print("  FAIL  %s\n        %s" % (name, detail))
        return 1
    print("all %d checks passed" % len(PASS))
    return 0


if __name__ == "__main__":
    sys.exit(main())
