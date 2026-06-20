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
ENGINE = os.path.join(HERE, "guard_engine.py")
DESTRUCT = os.path.join(HERE, "check_destructive_delete.py")
REFLECT = os.path.join(HERE, "reflect.py")
REAL_PROJ = os.path.join(os.path.expanduser("~"), ".claude", "projects", "F--dev-daw-01")
REAL_GUARDS = os.path.join(REAL_PROJ, "guards.jsonl")
MEM_DIR = os.path.join(REAL_PROJ, "memory")
# optional arg: validate a candidate registry (e.g. scripts/guards.proposed.jsonl) in the
# sandbox without touching the live, all-worktree-shared guards.jsonl.
GUARDS_SRC = sys.argv[1] if len(sys.argv) > 1 else REAL_GUARDS

PASS, FAIL = [], []


def ok(name):
    PASS.append(name)


def bad(name, detail):
    FAIL.append((name, detail))


# ---------------------------------------------------------------- sandbox setup
_sandbox = tempfile.mkdtemp(prefix="guard_test_")
_sb_proj = os.path.join(_sandbox, ".claude", "projects", "F--dev-daw-01")
os.makedirs(_sb_proj, exist_ok=True)
shutil.copyfile(GUARDS_SRC, os.path.join(_sb_proj, "guards.jsonl"))


def _sandbox_env(home):
    env = dict(os.environ)
    env["USERPROFILE"] = home
    env["HOME"] = home
    env["HOMEDRIVE"] = ""
    env["HOMEPATH"] = home
    return env


def run_engine(tool_name, tool_input, home=None):
    payload = json.dumps({"session_id": "TEST_SESSION", "tool_name": tool_name,
                          "tool_input": tool_input})
    p = subprocess.run([sys.executable, ENGINE], input=payload.encode("utf-8"),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       env=_sandbox_env(home or _sandbox))
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
            if r.get("field") not in ("command", "command_code", "text", "file_path", "ask_options", None):
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
    # no-cd-prefix (+ none-suppression)
    ("cd/pos-simple", "Bash", {"command": "cd /foo"}, 2, {"no-cd-prefix"}, set()),
    ("cd/pos-chain", "Bash", {"command": "cd F:/dev/daw_01 && cargo build"}, 2,
     {"no-cd-prefix", "no-command-chaining"}, set()),
    ("cd/neg-gitC", "Bash", {"command": "git -C F:/dev/other status"}, 0, set(), {"no-cd-prefix"}),
    ("cd/neg-cdr", "Bash", {"command": "cdr something"}, 0, set(), {"no-cd-prefix"}),
    ("cd/neg-cdprogram", "Bash", {"command": "cd-discid /dev/sr0"}, 0, set(), {"no-cd-prefix"}),
    ("cd/amp-fires", "Bash", {"command": "cd && ls"}, 2, {"no-cd-prefix"}, set()),
    # no-command-chaining
    ("chain/pos", "Bash", {"command": "cargo build && cargo test"}, 0, {"no-command-chaining"}, set()),
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
     0, set(), {"no-duplicate-app-launch"}),
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
     {"file_path": "F:/dev/daw_01/docs/gui_01_conversation.md", "content": "## random title [foo]\n"},
     0, set(), {"gui-conv-entry-format"}),
    ("conv/neg-goodheading", "Write",
     {"file_path": "F:/dev/daw_01/docs/gui_01_conversation.md",
      "content": "## #72 [Open] 2026-06-15 [request] 件名\n"}, 0, set(), {"gui-conv-entry-format"}),
    ("conv/neg-globmiss", "Write",
     {"file_path": "F:/dev/daw_01/docs/other.md", "content": "## random title [foo]\n"},
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
     {"file_path": "C:/Users/x/.claude/projects/F--dev-daw-01/memory/feedback_pursue_ideal_only.md",
      "content": "検出語: low-risk / pragmatic / workaround / compromise。これらが出たら違反。"},
     0, set(), {"compromise-smell-en"}),
    ("fix/smell-ja-in-claude-md", "Edit",
     {"file_path": "F:/dev/daw_01/CLAUDE.md", "new_string": "実装コスト/許容範囲/妥協/現実的に を禁ずる"},
     0, set(), {"compromise-smell-ja"}),
    ("fix/smell-ja-nonai", "Edit",
     {"file_path": "F:/dev/daw_01/DESIGN.md", "new_string": "ここは妥協のない理想実装を採る"},
     0, set(), {"compromise-smell-ja"}),
    ("fix/conv-section-link", "Write",
     {"file_path": "F:/dev/daw_01/docs/gui_01_conversation.md",
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
    ("gap/cd-manifest-path", "Bash",
     {"command": "cargo build --manifest-path F:/dev/daw_01/Cargo.toml -p daw_gui"}, 2,
     {"no-cd-prefix"}, set()),
    ("gap/cd-pushd", "Bash", {"command": "pushd F:/dev/daw_01"}, 2, {"no-cd-prefix"}, set()),
    ("gap/chain-semicolon", "Bash", {"command": "git add a.rs ; git status"}, 0,
     {"no-command-chaining"}, set()),
    ("gap/chain-or", "Bash", {"command": "cargo build || echo failed"}, 0,
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
     {"file_path": "F:/dev/daw_01/.githooks/post-merge",
      "content": "#!/usr/bin/env bash\nbash scripts/cleanup_worktree.sh --merged --force\n"},
     0, {"no-auto-worktree-hook"}, set()),
    ("gap/smell-en-lowest", "Edit", {"file_path": "x.rs", "new_string": "// lowest-risk: clone here"},
     0, {"compromise-smell-en"}, set()),
    ("gap/smell-en-kludge", "Edit", {"file_path": "x.rs", "new_string": "// kludge: special-case it"},
     0, {"compromise-smell-en"}, set()),
    ("gap/smell-ja-tema", "Write", {"file_path": "x.md", "content": "実装の手間が大きいので見送る"},
     0, {"compromise-smell-ja"}, set()),
    ("gap/conv-dated-malformed", "Write",
     {"file_path": "F:/dev/daw_01/docs/gui_01_conversation.md",
      "content": "## 要望: track 名の自動コントラスト 2026-06-15\n"},
     0, {"gui-conv-entry-format"}, set()),
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
    # empty stdin
    p = subprocess.run([sys.executable, ENGINE], input=b"", stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, env=_sandbox_env(_sandbox))
    ok("robust:empty-stdin") if p.returncode == 0 else bad("robust:empty-stdin", "exit=%d" % p.returncode)
    # malformed json
    p = subprocess.run([sys.executable, ENGINE], input=b"{not json", stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, env=_sandbox_env(_sandbox))
    ok("robust:bad-json") if p.returncode == 0 else bad("robust:bad-json", "exit=%d" % p.returncode)
    # missing guards file -> must not crash (point at empty sandbox)
    empty = tempfile.mkdtemp(prefix="guard_empty_")
    rc, out = run_engine("Bash", {"command": "cd /foo"}, home=empty)
    ok("robust:no-guards-file") if rc == 0 else bad("robust:no-guards-file", "exit=%d" % rc)
    shutil.rmtree(empty, ignore_errors=True)
    # field selection: MultiEdit edits[].new_string
    rc, out = run_engine("MultiEdit", {"file_path": "x.rs",
                                       "edits": [{"old_string": "a", "new_string": "low-risk hack"}]})
    ok("robust:multiedit-field") if "compromise-smell-en" in fired_ids(out) else \
        bad("robust:multiedit-field", "edits[].new_string not scanned | %s" % sorted(fired_ids(out)))


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
def check_escalation():
    """Seed a sandbox where a warn guard fired in 3 sessions; reflect.py must flip it to block."""
    sb = tempfile.mkdtemp(prefix="guard_escal_")
    proj = os.path.join(sb, ".claude", "projects", "F--dev-daw-01")
    os.makedirs(proj, exist_ok=True)
    rule = {"id": "test-escalate", "source": "feedback_no_command_chaining", "tool": ["Bash"],
            "field": "command", "all": ["&&"], "action": "warn", "msg": "test"}
    with open(os.path.join(proj, "guards.jsonl"), "w", encoding="utf-8") as fh:
        fh.write(json.dumps(rule, ensure_ascii=False) + "\n")
    with open(os.path.join(proj, "guard_hits.jsonl"), "w", encoding="utf-8") as fh:
        for s in ("sessA", "sessB", "sessC"):
            fh.write(json.dumps({"ts": "t", "session": s, "guard": "test-escalate",
                                 "source": "feedback_no_command_chaining", "tool": "Bash",
                                 "action": "warn"}) + "\n")
    payload = json.dumps({"session_id": "sessD-stop"})
    subprocess.run([sys.executable, REFLECT], input=payload.encode("utf-8"),
                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=_sandbox_env(sb))
    with open(os.path.join(proj, "guards.jsonl"), "r", encoding="utf-8") as fh:
        after = json.loads(fh.readline())
    if after.get("action") == "block":
        ok("escalate:warn->block@3sessions")
    else:
        bad("escalate:warn->block@3sessions", "action stayed %r" % after.get("action"))

    # control: 2 sessions must NOT escalate
    sb2 = tempfile.mkdtemp(prefix="guard_escal2_")
    proj2 = os.path.join(sb2, ".claude", "projects", "F--dev-daw-01")
    os.makedirs(proj2, exist_ok=True)
    with open(os.path.join(proj2, "guards.jsonl"), "w", encoding="utf-8") as fh:
        fh.write(json.dumps(rule, ensure_ascii=False) + "\n")
    with open(os.path.join(proj2, "guard_hits.jsonl"), "w", encoding="utf-8") as fh:
        for s in ("sessA", "sessB"):
            fh.write(json.dumps({"ts": "t", "session": s, "guard": "test-escalate",
                                 "action": "warn"}) + "\n")
    subprocess.run([sys.executable, REFLECT], input=payload.encode("utf-8"),
                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=_sandbox_env(sb2))
    with open(os.path.join(proj2, "guards.jsonl"), "r", encoding="utf-8") as fh:
        after2 = json.loads(fh.readline())
    if after2.get("action") == "warn":
        ok("escalate:no-escalate@2sessions")
    else:
        bad("escalate:no-escalate@2sessions", "action became %r" % after2.get("action"))

    # opt-out: a warn rule with escalate:false must NOT escalate even at 3+ sessions
    sb3 = tempfile.mkdtemp(prefix="guard_escal3_")
    proj3 = os.path.join(sb3, ".claude", "projects", "F--dev-daw-01")
    os.makedirs(proj3, exist_ok=True)
    rule_opt = dict(rule, id="test-escalate-optout", escalate=False)
    with open(os.path.join(proj3, "guards.jsonl"), "w", encoding="utf-8") as fh:
        fh.write(json.dumps(rule_opt, ensure_ascii=False) + "\n")
    with open(os.path.join(proj3, "guard_hits.jsonl"), "w", encoding="utf-8") as fh:
        for s in ("sessA", "sessB", "sessC", "sessD"):
            fh.write(json.dumps({"ts": "t", "session": s, "guard": "test-escalate-optout",
                                 "action": "warn"}) + "\n")
    subprocess.run([sys.executable, REFLECT], input=payload.encode("utf-8"),
                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=_sandbox_env(sb3))
    with open(os.path.join(proj3, "guards.jsonl"), "r", encoding="utf-8") as fh:
        after3 = json.loads(fh.readline())
    if after3.get("action") == "warn":
        ok("escalate:optout-respected@4sessions")
    else:
        bad("escalate:optout-respected@4sessions", "escalate:false rule became %r" % after3.get("action"))

    shutil.rmtree(sb, ignore_errors=True)
    shutil.rmtree(sb2, ignore_errors=True)
    shutil.rmtree(sb3, ignore_errors=True)


def main():
    check_registry()
    check_engine()
    check_engine_robustness()
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
