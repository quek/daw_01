# PostToolUse hook (.claude/settings.json, matcher "Bash" / if "Bash(git *)").
# Runs a release build after a successful `git commit` so every commit keeps the
# release profile compiling. release is a separate profile from debug, so
# debug-clean code can still break release; this catches it per commit
# (CLAUDE.md "build vs verify" distinction).
#
# Commit detection is done HERE in this script (not via the hook `if`):
# Claude Code's matcher/if does literal prefix matching and does NOT normalize
# flags like `-C`, so `Bash(git commit*)` does NOT match `git -C <path> commit`
# (that is why the old hook never fired under `git -C` / worktree sessions).
# settings.json `if` is broadened to "Bash(git *)" as a cheap pre-filter; the
# precise commit decision is made here by reading the command from stdin
# (tool_input.command). Invocation-form agnostic. Non-commit git commands exit 0.
#
# IMPORTANT: keep this script ASCII-only. PowerShell 5.1 misparses BOM-less UTF-8
# files that contain non-ASCII (e.g. Japanese) text; that previously corrupted the
# $commitRx literal below into an empty string, and an empty regex matches every
# command, so a release build ran on EVERY git command. ASCII-only removes any
# encoding/BOM dependency (Edit/Write tools emit BOM-less UTF-8).
#
# On build failure, exit 2 to block the PostToolUse and surface cargo's error to
# Claude. Build runs in cwd (= the repo/worktree where the commit happened), so
# worktree sessions build the correct tree.

$raw = [Console]::In.ReadToEnd()
$cmd = ''
try { $cmd = ($raw | ConvertFrom-Json).tool_input.command } catch { }

# Match a git commit invocation: plain `git commit`, `git -C <path> commit`,
# `git -c k=v commit`, and leading VAR=val assignments. Only "-flag value" pairs
# are absorbed between `git` and `commit`, so `git log ... commit` and
# `git commit-tree` do NOT match.
$commitRx = '(?:^|[&|;]\s*)(?:\w+=\S*\s+)*git\s+(?:-\S+\s+\S+\s+)*commit(?:\s|$)'
if ($cmd -notmatch $commitRx) {
    exit 0
}

cargo build --workspace --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "release build FAILED after commit (cargo exit code $LASTEXITCODE). Fix the release build before continuing."
    exit 2
}
