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

着手前チェック (memory: feedback_verify_request_premise / feedback_pursue_best_practice):
- 実装着手前に、 まず gui_01 側の理想 (理想の API 形 / そもそも公開すべきか) を自分で検討する。
  daw_01 が提案した API 形・解法をそのまま実装に移さない。
- 要望の根拠が「caller 側で取得できない / 内部レイアウト値で再現不可 / SSoT 違反」型のときは、
  着手前に daw_01 source (additional working dir、 read-only で読める) を該当キーワードで grep して
  その前提が本当か検証する。 前提が偽なら要望ごと不要になる
  (例 #098: hover_beat / header_w を grep するだけで daw_01 が既に mirror 済みと判明し全 revert)。
- 前提が偽 / gui_01 の理想と異なるなら、 実装せず conversation file で「既に X で取れるのでは?」
  または理想案を 1 つ返す。 妥協案や複数選択肢 (A)(B)(C) は出さない。

返信手順 (memory: reference_daw_01_conversation):
- 該当エントリの "### gui_01 →" ブロックに返信を書く
- 見出しを [Open] → [Replied] に変更
- daw_01 への git commit は禁止 (memory: feedback_no_daw_01_commit)
EOF
