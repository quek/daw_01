#!/usr/bin/env bash
# Stop hook: surface daw_01 [Open] entries that appeared mid-session.
#
# session_start_daw01_open.sh fires only at session start. If daw_01 commits a
# new [Open] entry while a Claude session is already active, the SessionStart
# hook never re-runs and the request goes unnoticed until the next session.
# This Stop hook closes that gap: every time Claude finishes a response, we
# scan the conversation file. If any [Open] exists, output is appended to
# Claude's context for the next user turn so Claude proactively addresses it.
#
# Combined with the SessionStart hook, this means: ANY user activity (typing
# any prompt) after daw_01 commits a new [Open] will surface it to Claude.
#
# Truly off-session detection (no user activity at all) requires an external
# poller (scheduled-tasks MCP or OS cron). This hook handles the common case
# where the user is interacting with Claude.
#
# Stdout is appended to Claude's context. Silent exit when no [Open] or file
# missing.
set -euo pipefail

CONV="F:/dev/daw_01/docs/gui_01_conversation.md"

[ -f "$CONV" ] || exit 0

OPEN_ENTRIES=$(grep -E '^## #[0-9]+ \[Open\]' "$CONV" 2>/dev/null || true)

[ -n "$OPEN_ENTRIES" ] || exit 0

cat <<EOF
=== daw_01 から [Open] エントリ (session 中検出) ===
$OPEN_ENTRIES

返信手順 (memory: reference_daw_01_conversation):
- 該当エントリの "### gui_01 →" ブロックに返信を書く
- 見出しを [Open] → [Replied] に変更
- daw_01 への git commit は禁止 (memory: feedback_no_daw_01_commit)

session 中に daw_01 から新規 [Open] が commit されたか、 前 session で未処理のまま残った
entry の可能性。 次の user 入力時に処理してください。
EOF
