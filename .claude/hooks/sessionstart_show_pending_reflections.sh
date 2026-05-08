#!/usr/bin/env bash
# SessionStart hook: surface pending reflection candidates written by the Stop
# hook in previous sessions, and clean up per-session event logs.
#
# Reads .claude/.session_reflect_pending.md (produced by stop_session_reflect.sh)
# and outputs its content as additional session context with a Required Action
# header. Then rotates the file to .last so it isn't shown again — and so the
# previous one is recoverable if the agent doesn't act on it.
#
# Also rotates .session_events.*.log (per-session dedup logs) — these belong to
# the just-ended session(s) and should not leak into the next.
#
# Silent if the pending file is missing or empty.
set -euo pipefail

INPUT=$(cat || true)

CWD=$(printf '%s' "$INPUT" | sed -n 's/.*"cwd":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
[ -z "$CWD" ] && CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

# clean up previous sessions' per-session event logs (they're scoped per session id,
# so once that session is done, the logs are dead weight). Best-effort, ignore errors.
find "$CWD/.claude" -maxdepth 1 -name '.session_events.*.log' -mmin +60 -delete 2>/dev/null || true

PENDING="$CWD/.claude/.session_reflect_pending.md"
[ -s "$PENDING" ] || exit 0

cat <<EOF
=== Required Action: 前 session の reflection 候補を処理 ===

直前 session で **user 修正発言** または **assistant rework 操作** (git rebase / amend /
reset --hard / force-push / cherry-pick) が検出されました。 user の最初の依頼に応答する
**前に**、 各候補を以下のいずれかで処理してください (skip 禁止):

1. **save**: 一般化できる learning なら \`~/.claude/projects/F--dev-gui-01/memory/feedback_*.md\` に書き、 MEMORY.md index に 1 行追加
2. **discard**: 単発のノイズなら無視 (理由を 1 文だけ user に報告)

候補:
$(cat "$PENDING")

判断基準: 同じ pain point が **再発しそう** (= 別 worktree でも踏みそう、 別 phase でも繰り返しそう、 別 widget でも同根) なら save。 1 度きりの偶発事象なら discard。

EOF

mv "$PENDING" "$CWD/.claude/.session_reflect_pending.md.last" 2>/dev/null || true
