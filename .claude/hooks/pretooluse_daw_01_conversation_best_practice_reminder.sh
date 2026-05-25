#!/usr/bin/env bash
# PreToolUse hook on Edit / Write: daw_01 conversation file への投稿時に
# 「ベストプラクティス 1 案のみで書いているか」 の self-check reminder を強制発火。
#
# 経緯: 2026-05-25 daw_01 #045 §B で gui_01 Claude が「caller workspace の windows
# 0.61 bump が連鎖して大変だろう」 という推測コスト懸念で (A) isize raw 値受けの
# 妥協案を推奨 → daw_01 が「ベストプラクティス追求」 原則で (B) HANDLE newtype に
# 訂正、 daw_01 workspace を 0.62 に bump (破壊変更 1 行のみで pass)。 user 方針:
# **コスト懸念は判断材料から完全排除、 常にベストプラクティス 1 案のみを提案**。
#
# daw_01 conversation file への投稿は公式の API 設計判断発信点なので、 ここで
# self-check を強制発火させて妥協提案の混入を防ぐ。
#
# Reminder-only (= 機械検出は不可能、 自然言語意図 check は Claude 側で実行)。
#
# Input: tool call JSON via stdin (tool_input.file_path 等)
# Output: JSON to stdout merged into Claude's context for this PreToolUse
set -euo pipefail

INPUT=$(cat)

# tool_input.file_path に daw_01 conversation file path が含まれるか check
# (Windows path / Unix path 両表記を許容)
if printf '%s' "$INPUT" | grep -qiE 'daw_01[/\\]docs[/\\]gui_01_conversation\.md'; then
  cat <<'JSON'
{"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "REMINDER: daw_01 conversation への投稿は **ベストプラクティス 1 案のみ** で書いてください。\n\nself-check:\n  1. caller boilerplate を強要していないか? (= caller が newtype を .0 等で破る idiom が出来ていないか)\n  2. 複数選択肢 (A)/(B)/(C) を出していないか? → 出す時点で「ベストプラクティスを 1 つに絞れていない signal」\n  3. 「caller workspace のコスト懸念」 を判断材料にしていないか? → user 方針: コスト懸念は判断材料から完全排除\n  4. 「妥協」 「workaround」 を含めていないか? → 含めるなら根本対処の方針を明示\n\n詳細: ~/.claude/projects/F--dev-gui-01/memory/feedback_pursue_best_practice.md\n\nこのリマインダーは .claude/hooks/pretooluse_daw_01_conversation_best_practice_reminder.sh が出力しています。 self-check 済なら投稿を続行してください。"}}
JSON
fi
