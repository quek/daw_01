#!/usr/bin/env bash
# PreToolUse hook on Bash: remind, around git commit, to
#   (1) run the /review skill,
#   (2) do a 同件 (sibling-occurrence) check -- when this is a bug fix, search for
#       the SAME root cause elsewhere and fix the whole class in this commit, not
#       just the reported instance. A standing discipline done by default, without
#       the user having to ask "同件チェックは?".
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
{"hookSpecificOutput": {"hookEventName": "PreToolUse", "additionalContext": "REMINDER (commit 前に未実行の項目を完了させること。この session で済んでいれば無視して続行):\n1) /review skill: RT-audio 安全性 (ホットパスのヒープ確保 / ロック / I/O 禁止)・パフォーマンス (描画ループ / 毎フレーム計算)・FFI / セキュリティ整合性 (ポインタ・整数キャスト・エラー握りつぶし) を確認。skill: F:/dev/daw_01/.claude/skills/review/SKILL.md\n2) 同件チェック (この commit が bug fix の場合・必須): 同じ root cause の同種箇所が他に無いか grep/検索で全件洗い出し、見つけたら同じ commit で class ごと修正する。1 件だけ直して報告しない。ユーザーに促される前に既定で行うこと。\n\nこのリマインダーは .claude/hooks/pretooluse_git_commit_review_reminder.sh が出力しています。"}}
JSON
fi
