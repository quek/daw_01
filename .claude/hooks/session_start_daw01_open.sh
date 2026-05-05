#!/usr/bin/env bash
# SessionStart hook: surface daw_01 [Open] entries.
#
# gui_01 / daw_01 sibling project workflow:
# daw_01 puts requests into F:/dev/daw_01/docs/gui_01_conversation.md as
# `## #NNN [Open] [type] title` entries. This hook scans for those at session
# start so Claude can mention them without the user typing "daw_01 から依頼来てる".
#
# Stdout is appended to Claude's session context. Silent exit when no [Open]
# entries exist or the file is missing.
set -euo pipefail

CONV="F:/dev/daw_01/docs/gui_01_conversation.md"

[ -f "$CONV" ] || exit 0

OPEN_ENTRIES=$(grep -E '^## #[0-9]+ \[Open\]' "$CONV" 2>/dev/null || true)

[ -n "$OPEN_ENTRIES" ] || exit 0

cat <<EOF
=== daw_01 から [Open] エントリ ===
$OPEN_ENTRIES

返信手順 (memory: reference_daw_01_conversation):
- 該当エントリの "### gui_01 →" ブロックに返信を書く
- 見出しを [Open] → [Replied] に変更
- daw_01 への git commit は禁止 (memory: feedback_no_daw_01_commit)
EOF
