#!/usr/bin/env bash
# SessionStart hook: surface pending reflection candidates written by the Stop
# hook (stop_session_reflect.sh) in previous sessions, and clean up per-session
# event logs. Ported from gui_01's AHE — this is the loop-closer: without it the
# reflection just sits in a file and the assistant forgets to read it (daw_01's
# previous setup had no SessionStart hook, so the loop stayed half-open).
#
# Outputs the pending candidates as a Required Action so the next session MUST
# triage them (save as feedback memory / discard) before doing the user's task.
# Then rotates the file to .last so it isn't shown twice.
#
# Silent if the pending file is missing or empty.
set -euo pipefail

INPUT=$(cat || true)

CWD=$(printf '%s' "$INPUT" | sed -n 's/.*"cwd":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
[ -z "$CWD" ] && CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

# AHE pending file は **main worktree の .claude/** に集約 (stop_session_reflect.sh と一致)。
# 各 worktree session の Stop hook が main worktree path に書き、SessionStart hook も同 path から
# 読む → worktree 跨ぎでも learning が共有される。
MAIN_WORKTREE=$(git -C "$CWD" worktree list --porcelain 2>/dev/null | awk '/^worktree / {print substr($0, 10); exit}')
[ -z "$MAIN_WORKTREE" ] && MAIN_WORKTREE="$CWD"

# clean up previous sessions' per-session event logs (dead weight once the
# session is done). Best-effort.
find "$MAIN_WORKTREE/.claude" -maxdepth 1 -name '.session_events.*.log' -mmin +60 -delete 2>/dev/null || true

PENDING="$MAIN_WORKTREE/.claude/.session_reflect_pending.md"
[ -s "$PENDING" ] || exit 0

cat <<EOF
=== Required Action: 前 session の reflection 候補を処理 ===

直前 session で **user 修正発言** または **assistant rework 操作** (git rebase / amend /
reset --hard / force-push / cherry-pick) が検出されました。user の最初の依頼に応答する
**前に**、各候補を以下のいずれかで処理してください (skip 禁止):

1. **save**: 一般化できる learning なら ~/.claude/projects/F--dev-daw-01/memory/feedback_*.md に
   書き、MEMORY.md index に 1 行追加
2. **discard**: 単発のノイズなら無視 (理由を 1 文だけ user に報告)

候補:
$(cat "$PENDING")

判断基準: 同じ pain point が **再発しそう** (= 別 worktree でも踏みそう、別 phase でも繰り返しそう、
別 crate/機能でも同根) なら save。1 度きりの偶発事象なら discard。

EOF

mv "$PENDING" "$MAIN_WORKTREE/.claude/.session_reflect_pending.md.last" 2>/dev/null || true
