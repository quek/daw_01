#!/usr/bin/env bash
# Stop hook: detect user-correction patterns in the latest user turn and stage
# a reflection memo for the next session to consider as feedback memory.
#
# Triggered after every assistant turn. We read only the latest user message
# from the transcript (not the whole history) to avoid duplicate logging.
#
# Patterns flagged as "correction-like" (lexical proxy for semantic correction):
#   negation:    違う / そうじゃ / 間違 / ではなく / でなく / "no, / don't / wrong / incorrect
#   challenge:   ぎませんか / 過ぎ / すぎ / ないんですか / じゃないですか / instead / actually
#   redirect:    やめて / 代わりに
# False positives are acceptable — the next session sees them as candidates.
# Missing a real correction is the worse failure mode (over-detect, under-act).
#
# Output: appends one line to .claude/.session_reflect_pending.md (gitignored).
# Silent if no correction detected. Never blocks the stop event.
set -euo pipefail

INPUT=$(cat)

TRANSCRIPT=$(printf '%s' "$INPUT" | sed -n 's/.*"transcript_path":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
CWD=$(printf '%s' "$INPUT" | sed -n 's/.*"cwd":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
SESSION_ID=$(printf '%s' "$INPUT" | sed -n 's/.*"session_id":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)

[ -f "$TRANSCRIPT" ] || exit 0
[ -d "$CWD" ] || exit 0

LATEST_USER=$(tac "$TRANSCRIPT" 2>/dev/null | awk '/"role"[[:space:]]*:[[:space:]]*"user"/ && !/tool_use_id/ && !/tool_result/ {print; exit}' || true)
[ -n "$LATEST_USER" ] || exit 0

PATTERNS='違う|そうじゃ|間違|ではなく|でなく|ぎませんか|過ぎ|すぎ|ないんですか|じゃないですか|やめて|代わりに|wrong|incorrect|"no,|don'\''t|instead|actually'
if ! printf '%s' "$LATEST_USER" | grep -qiE "$PATTERNS"; then
  exit 0
fi

PENDING="$CWD/.claude/.session_reflect_pending.md"
TIMESTAMP=$(date +%Y-%m-%dT%H:%M)
SNIPPET=$(printf '%s' "$LATEST_USER" | head -c 200 | tr -d '\n\r')

cat >> "$PENDING" <<MEMO
- [$TIMESTAMP] session=$SESSION_ID 修正パターン検出: $SNIPPET ...
MEMO
