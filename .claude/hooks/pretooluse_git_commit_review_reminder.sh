#!/usr/bin/env bash
# PreToolUse hook on Bash: remind to run /review skill before git commit.
#
# review/SKILL.md states "コミット前に自動で実行される想定" but no automation
# existed in daw_01 until this hook (gui_01 already had the equivalent:
# pretooluse_git_commit_review_reminder.sh). We inject a reminder via
# additionalContext so Claude runs /review before commit when it has not done so
# already in this session.
#
# Reminder-only, not an enforced block: /review is heavy (RT-safety / perf /
# FFI scan + cargo build) and irrelevant for trivial doc-only commits. Claude
# judges per commit whether it applies.
#
# Input: tool call JSON via stdin.
# Output: JSON to stdout merged into Claude's context for this PreToolUse.
set -euo pipefail

INPUT=$(cat)

# Match `git commit`, `git -C <path> commit`, etc. in the tool_input.command
# substring. False-positive on the substring is acceptable — an extra reminder
# is cheap, a missed one is not.
if printf '%s' "$INPUT" | grep -qiE 'git( +-C +[^ "]+)? +commit'; then
  cat <<'JSON'
{"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "REMINDER: commit を実行する前に、未実行なら /review skill を呼び出して RT-audio 安全性 (ホットパスのヒープ確保 / ロック / I/O 禁止) ・ パフォーマンス (描画ループ / 毎フレーム計算) ・ FFI / セキュリティ整合性 (ポインタ・整数キャスト・エラー握りつぶし) のチェックを完了させてください。skill: F:/dev/daw_01/.claude/skills/review/SKILL.md\n\nこのリマインダーは .claude/hooks/pretooluse_git_commit_review_reminder.sh が出力しています。/review を既にこの session で走らせ済みなら無視して commit を続行してください。"}}
JSON
fi
