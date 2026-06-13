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

# --- Stream 1: user-correction / rework candidates (terminal verbs: save / discard) ---
PENDING="$MAIN_WORKTREE/.claude/.session_reflect_pending.md"
if [ -s "$PENDING" ]; then
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
fi

# --- Stream 2: AHE backlog (detected friction patterns; terminal verbs: promote / hook-request / dismiss) ---
# Lives in the per-project user dir (shared across all worktrees, like metrics).
# This is the stream that actuates into skills/hooks/commands -- the previous loop
# wrote it to a file no hook read, so it dead-ended at memory. Surface OPEN rows
# here so each session is OBLIGATED to drive them to a terminal status.
USERHOME=$(printf '%s' "${USERPROFILE:-$HOME}" | sed 's#\\#/#g')
BACKLOG="$USERHOME/.claude/projects/F--dev-daw-01/ahe_backlog.md"
if [ -f "$BACKLOG" ]; then
  OPEN_ROWS=$(awk -F'|' '/^\| / { st=$3; gsub(/^[ \t]+|[ \t]+$/,"",st); if (st=="open" || st=="needs-user") print }' "$BACKLOG" || true)
  if [ -n "$OPEN_ROWS" ]; then
    HOOK_COUNT=$(awk -F'|' '/^\| / { st=$3; tg=$5; gsub(/^[ \t]+|[ \t]+$/,"",st); gsub(/^[ \t]+|[ \t]+$/,"",tg); if (tg=="hook" && (st=="open"||st=="needs-user")) c++ } END { print c+0 }' "$BACKLOG" || echo 0)
    ESC_COUNT=$(awk -F'|' '/^\| / { st=$3; ss=$4; gsub(/^[ \t]+|[ \t]+$/,"",st); gsub(/^[ \t]+|[ \t]+$/,"",ss); if (st=="open" && ss+0>=3) c++ } END { print c+0 }' "$BACKLOG" || echo 0)
    cat <<EOF
=== Required Action: AHE backlog の未処理パターンを triage ===

session metrics から検出された再発パターンが未処理です。user の依頼に応答する前に、
各 OPEN 行を以下のいずれかで **終端** させてください (skip 禁止):

1. **promote**: /promote-reflection で target の artifact (skill / command / memory) を作り、行を done に
2. **hook 承認依頼**: target=hook は hook 登録の編集が classifier にブロックされる (user のみ可)。
   ready-to-paste spec を backlog の "hook requests" 節に書いて user に承認依頼 (行は needs-user)
3. **dismiss**: 不要なら status を dismissed にし notes に理由を 1 文 (以後 reflect.ps1 は再浮上させない)

列: id | status | sessions | target | first-seen | last-seen | last-session | pattern | notes
sessions>=3 = escalated (繰り返し踏んでいる。優先 promote)。
file: $BACKLOG

未処理 (open / needs-user):
$OPEN_ROWS

[hook 承認待ち: ${HOOK_COUNT} 件] [escalated: ${ESC_COUNT} 件]

EOF
  fi
fi

exit 0
