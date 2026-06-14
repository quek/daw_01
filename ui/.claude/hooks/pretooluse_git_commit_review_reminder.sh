#!/usr/bin/env bash
# PreToolUse hook on Bash: remind to run /review skill before git commit.
#
# review/SKILL.md states "コミット前に自動で実行される想定" but no automation
# existed until this hook. We inject a reminder via additionalContext so
# Claude runs /review before commit when it has not done so already.
#
# Reminder-only, not an enforced block: /review is heavy (30s+) and irrelevant
# for trivial doc-only commits. Claude judges per commit.
#
# Input: tool call JSON via stdin.
# Output: JSON to stdout merged into Claude's context for this PreToolUse.
set -euo pipefail

INPUT=$(cat)

# Match `git commit`, `git -C <path> commit`, etc. in the tool_input.command
# substring. False-positive on the substring is acceptable — extra reminder is
# cheap, missing one isn't.
if printf '%s' "$INPUT" | grep -qiE 'git( +-C +[^ "]+)? +commit'; then
  cat <<'JSON'
{"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "REMINDER: commit を実行する前に、未実行なら /review skill を呼び出して設計不変条件 (no-Clone / no-Message / no-derive) ・ パフォーマンス ・ 整合性チェックを完了させてください。skill: F:/dev/gui_01/.claude/skills/review/SKILL.md\n\nこのリマインダーは .claude/hooks/pretooluse_git_commit_review_reminder.sh が出力しています。/review を既にこの session で走らせ済みなら無視して commit を続行してください。"}}
JSON
fi
