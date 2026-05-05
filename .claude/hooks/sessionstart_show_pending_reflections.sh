#!/usr/bin/env bash
# SessionStart hook: surface pending session-reflection memos written by the
# Stop hook in previous sessions.
#
# Reads .claude/.session_reflect_pending.md (produced by stop_session_reflect.sh)
# and outputs its content as additional session context. Then rotates the file
# to .last so it isn't shown again — and so the previous one is recoverable
# if the agent doesn't act on it.
#
# Silent if the pending file is missing or empty.
set -euo pipefail

INPUT=$(cat || true)

CWD=$(printf '%s' "$INPUT" | sed -n 's/.*"cwd":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
[ -z "$CWD" ] && CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

PENDING="$CWD/.claude/.session_reflect_pending.md"
[ -s "$PENDING" ] || exit 0

cat <<EOF
=== 前 session の reflection 候補 ===
$(cat "$PENDING")

直前 session で user 修正と思われるパターンが検出されました。該当を読み返し、保存に値する learning があれば feedback memory (~/.claude/projects/F--dev-gui-01/memory/feedback_*.md) に保存してください。
EOF

mv "$PENDING" "$CWD/.claude/.session_reflect_pending.md.last" 2>/dev/null || true
