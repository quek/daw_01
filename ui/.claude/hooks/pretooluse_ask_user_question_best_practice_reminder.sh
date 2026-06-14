#!/usr/bin/env bash
# PreToolUse hook on AskUserQuestion: ユーザに選択肢を提示する時に
# 「コスト懸念由来の妥協 option を含めていないか」 の self-check reminder を強制発火。
#
# 経緯: pretooluse_daw_01_conversation_best_practice_reminder.sh と同じ。
# AskUserQuestion 自体は問題ないが、 options に「caller workspace の bump コスト回避」
# 「型安全性を犠牲にする workaround」 等を含めると user の判断を妥協方向に誘導しがち。
# user 方針 (2026-05-25): コスト懸念は判断材料から完全排除、 常にベストプラクティス
# 1 案のみで進める。 「ベストプラクティス 1 案で進められるなら AskUserQuestion 自体不要」。
#
# Reminder-only (= 機械検出は不可能、 options 中身の意図 check は Claude 側で実行)。
#
# Input: tool call JSON via stdin
# Output: JSON to stdout merged into Claude's context for this PreToolUse
set -euo pipefail

# stdin は使わないが既存 hook と pattern 一致のため drain
cat >/dev/null

cat <<'JSON'
{"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "REMINDER: AskUserQuestion の options に **コスト懸念由来の妥協案** を入れていないか確認してください。\n\nself-check:\n  1. caller への影響 / dependency bump 等を理由に「型安全性 / SSoT / caller boilerplate ゼロ」 のベストプラクティスを犠牲にする option を入れていないか?\n  2. ベストプラクティス 1 案で進められるなら AskUserQuestion 自体不要 (= 「複数案を出す」 自体が「ベストプラクティスを 1 つに絞れていない signal」)\n  3. user 方針: コスト懸念は判断材料から完全排除\n\n詳細: ~/.claude/projects/F--dev-gui-01/memory/feedback_pursue_best_practice.md\n\nこのリマインダーは .claude/hooks/pretooluse_ask_user_question_best_practice_reminder.sh が出力しています。 self-check 済なら呼び出しを続行してください。"}}
JSON
