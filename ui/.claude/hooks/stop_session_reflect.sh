#!/usr/bin/env bash
# Stop hook: detect learnable signals from the latest assistant turn and stage
# them as reflection candidates for the next session.
#
# Triggered after every assistant turn. We read the latest user message + the
# latest assistant turn from the transcript (not the whole history) to avoid
# duplicate logging across turns. Per-session dedup uses .session_events.<id>.log
# (auto-discarded by SessionStart's rotation).
#
# Two classes of signals are captured:
#
# (A) User correction patterns (lexical proxy for semantic correction)
#   negation:    違う / そうじゃ / 間違 / ではなく / でなく / "no, / don't / wrong / incorrect
#   challenge:   ぎませんか / 過ぎ / すぎ / ないんですか / じゃないですか / instead / actually
#   redirect:    やめて / 代わりに
#
# (B) Assistant rework signals (process pain points the user may not articulate)
#   git rebase / git commit --amend / git reset --hard / git push --force / git cherry-pick
#   These often mean a recoverable mistake or a missed pre-commit check that
#   could become a reusable feedback memory.
#
# False positives are acceptable — the next session sees them as candidates and
# the LLM judges save vs discard. Missing a real signal is the worse failure mode.
#
# Output: appends one line to .claude/.session_reflect_pending.md (gitignored).
# Silent if no signal detected. Never blocks the stop event.
set -euo pipefail

INPUT=$(cat)

TRANSCRIPT=$(printf '%s' "$INPUT" | sed -n 's/.*"transcript_path":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
CWD=$(printf '%s' "$INPUT" | sed -n 's/.*"cwd":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
SESSION_ID=$(printf '%s' "$INPUT" | sed -n 's/.*"session_id":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)

[ -f "$TRANSCRIPT" ] || exit 0
[ -d "$CWD" ] || exit 0

# AHE pending file は **main worktree の .claude/** に集約する。 各 worktree CWD に書くと、
# 別 worktree (= main や別 branch worktree) で起動した次 session が pending file を見つけられず
# 学びが silo 化する (2026-05-08 に判明、 cranky-wescoff worktree の pending が main session 起動時に
# surface されなかった)。 main worktree path は `git worktree list --porcelain` の最初のエントリ。
MAIN_WORKTREE=$(git -C "$CWD" worktree list --porcelain 2>/dev/null | awk '/^worktree / {print substr($0, 10); exit}')
[ -z "$MAIN_WORKTREE" ] && MAIN_WORKTREE="$CWD"
[ -d "$MAIN_WORKTREE/.claude" ] || mkdir -p "$MAIN_WORKTREE/.claude" 2>/dev/null

PENDING="$MAIN_WORKTREE/.claude/.session_reflect_pending.md"
SESSION_LOG="$MAIN_WORKTREE/.claude/.session_events.${SESSION_ID}.log"
TIMESTAMP=$(date +%Y-%m-%dT%H:%M)

# (A) user correction pattern detection — single line per turn
LATEST_USER=$(tac "$TRANSCRIPT" 2>/dev/null | awk '/"role"[[:space:]]*:[[:space:]]*"user"/ && !/tool_use_id/ && !/tool_result/ {print; exit}' || true)
if [ -n "$LATEST_USER" ]; then
  PATTERNS='違う|そうじゃ|間違|ではなく|でなく|ぎませんか|過ぎ|すぎ|ないんですか|じゃないですか|やめて|代わりに|wrong|incorrect|"no,|don'\''t|instead|actually'
  if printf '%s' "$LATEST_USER" | grep -qiE "$PATTERNS"; then
    SNIPPET=$(printf '%s' "$LATEST_USER" | head -c 200 | tr -d '\n\r')
    # dedupe per session (same user message can't trigger twice — but Stop fires
    # once per turn so the latest user message is fixed for the current Stop event;
    # dedup is mostly defensive against transcript re-reads).
    KEY="user-correction:$(printf '%s' "$LATEST_USER" | sha1sum | cut -c1-16)"
    if ! grep -qF "$KEY" "$SESSION_LOG" 2>/dev/null; then
      echo "$KEY" >> "$SESSION_LOG"
      printf -- '- [%s] session=%s 修正パターン検出: %s ...\n' "$TIMESTAMP" "$SESSION_ID" "$SNIPPET" >> "$PENDING"
    fi
  fi
fi

# (B) assistant rework signal detection — looks at the latest assistant turn's
# bash tool calls. Dedupe per session so each unique rework command is logged
# only once even if it appears across multiple turns.
LATEST_ASSISTANT=$(tac "$TRANSCRIPT" 2>/dev/null | awk '/"role"[[:space:]]*:[[:space:]]*"assistant"/ {print; exit}' || true)
if [ -n "$LATEST_ASSISTANT" ]; then
  # extract distinct rework commands (whitespace-collapsed, sort -u for stability)
  REWORK=$(printf '%s' "$LATEST_ASSISTANT" \
    | grep -oE 'git rebase|git commit --amend|git reset --hard|git push --force|git cherry-pick' \
    | sort -u || true)
  if [ -n "$REWORK" ]; then
    while IFS= read -r cmd; do
      [ -z "$cmd" ] && continue
      KEY="rework:$cmd"
      if ! grep -qF "$KEY" "$SESSION_LOG" 2>/dev/null; then
        echo "$KEY" >> "$SESSION_LOG"
        printf -- '- [%s] session=%s rework signal: %s (この session 内で発生 — 事前 check で回避できなかったか検討)\n' "$TIMESTAMP" "$SESSION_ID" "$cmd" >> "$PENDING"
      fi
    done <<< "$REWORK"
  fi
fi
