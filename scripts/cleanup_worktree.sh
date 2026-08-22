#!/usr/bin/env bash

# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later

# PowerShell-free removal of a merged daw_01 git worktree.
#
# Why bash (not PowerShell): `make rm-worktree` used to shell out to a .ps1, but
# make spawns powershell from Git Bash in a context where powershell cannot
# reliably locate/spawn git, so the script saw zero worktrees and aborted. Bash
# recipes (like `make fetch-ffmpeg`) run fine here.
#
# `make fetch-ffmpeg` vendors ffmpeg as a REAL COPY (cp -r), so worktrees contain
# no junction; `git worktree remove --force` deletes only the worktree's own
# files (never the main repo's gitignored, unrecoverable vendored ffmpeg).
#
# Invocation is MANUAL/EXPLICIT (Makefile: `make rm-worktree NAME=...`). It is
# deliberately NOT wired into a git hook: an earlier auto-on-merge hook removed a
# sibling agent's active worktree (2026-06-15). Removal stays explicit/targeted.
#
# Modes (pick one):
#   --name <name>       remove .claude/worktrees/<name>.
#   --path <path>       remove the worktree at <path>.
#   --merged-tip <sha>  remove the worktree whose HEAD == <sha>. No-op if none.
#   --all               remove EVERY worktree under .claude/worktrees whose branch
#                       is fully merged into main. "Merged" (see branch_merged_into_main)
#                       means the branch has NO unique non-merge commit AND adds no net
#                       content over main -- so it catches the merge-flow leftover
#                       (tip is a "merge main into feature" commit, unreachable from
#                       main yet carrying no unique work) WITHOUT deleting a branch that
#                       holds real committed work. This INCLUDES a worktree whose tip ==
#                       main HEAD: landing a feature via `git push . <branch>:main`
#                       leaves the feature tip AT main HEAD, and that is precisely the
#                       "I merged it but worktree-rm-merged won't delete it" case. A
#                       harness lock left behind by an EXITED "claude session (pid N)" is
#                       auto-cleared (see remove_one lock handling) so --all sweeps it
#                       unattended; a lock whose pid is STILL RUNNING is kept (SKIP). The
#                       remove_one guards (clean tree + merged, plus live-lock SKIP unless
#                       --force) keep an active or dirty worktree safe; a clean one at main
#                       HEAD has nothing to lose and is recreated with one command. --all ALSO
#                       prunes leftover EMPTY .claude/worktrees/<dir> directories that
#                       git already deregistered (prune_orphan_dirs) -- the
#                       "空ディレクトリが残る" symptom.
# Safety (skipped only with --force):
#   * target must live under <repo>/.claude/worktrees/ (never the main worktree).
#   * branch must be fully merged into main (no unmerged work lost).
#   * working tree must be clean: no uncommitted tracked changes, and no unsaved
#     gitignored/untracked deliverables except the regenerable target/ & third_party/.
#   * a harness lock is respected while its owning "claude session (pid N)" is alive
#     (SKIP); once that pid is gone the lock is stale and auto-cleared. --force clears
#     the lock unconditionally.
set -uo pipefail

# ---- args ------------------------------------------------------------------
o_name='' o_path='' o_tip='' o_all=0 o_force=0
while [ $# -gt 0 ]; do
  case "$1" in
    --name)        o_name="${2:-}"; shift 2;;
    --path)        o_path="${2:-}"; shift 2;;
    --merged-tip)  o_tip="${2:-}"; shift 2;;
    --all)         o_all=1; shift;;
    --force)       o_force=1; shift;;
    -h|--help)     echo "usage: cleanup_worktree.sh (--name <n> | --path <p> | --merged-tip <sha> | --all) [--force]"; exit 0;;
    *) echo "cleanup_worktree: unknown arg: $1" >&2; exit 2;;
  esac
done

repo="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$repo" ] || repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
wtroot="$repo/.claude/worktrees"
logfile="$repo/target/worktree-cleanup.log"
mkdir -p "$repo/target"

log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" | tee -a "$logfile"; }

# Absolute path, trailing slash stripped, for stable comparison. cygpath ships
# only with Cygwin/MSYS2/Git-for-Windows; on stock Linux/macOS/CI it is absent, so
# fall back to the raw path (git already reports POSIX-style absolute paths there).
# Without this fallback every norm() returns "" off-Windows, collapsing every path
# comparison and making the whole script non-functional (fails safe, but broken).
norm() { local p; p="$(cygpath -u "$1" 2>/dev/null)"; [ -n "$p" ] || p="$1"; printf '%s' "$p" | sed 's:/*$::'; }

# 0 (true) if the worktree at git-reported path $1 is still registered. Avoids a
# `git ... | grep` pipe so `set -o pipefail` + SIGPIPE can't abort the script.
_still_registered() {
  local p="$1" line
  while IFS= read -r line; do
    case "$line" in "worktree $p") return 0;; esac
  done < <(git -C "$repo" worktree list --porcelain 2>/dev/null)
  return 1
}

# 0 (true) if removing branch $1 loses no committed work that is not already in main.
# Test: `git cherry main $1` emits NO '+' line. git cherry walks every NON-MERGE commit
# unique to $1 (i.e. in main..$1) and compares it to main BY PATCH-ID, marking it '-'
# when an equivalent patch is already in main and '+' when it is not.
#   * No '+' line  => every unique commit's CONTENT is in main, so deleting the branch
#     orphans nothing. This is what lets squash / rebase / cherry-pick landings (a new
#     SHA on main) count as merged. The old reachability test
#     (`rev-list --no-merges main..$1` empty) reported those as UNMERGED -- which is
#     why recover-fixme-79 (landed on main as 54186f5, patch-identical to the branch's
#     2f9fb0c) was wrongly KEPT. Patch-id catches it.
#   * Any '+' line => a unique commit has no patch twin in main -> NOT merged -> kept.
#     This is the load-bearing safety guard and it is EXACT for the add-then-revert /
#     spike-then-undo / WIP-undo flow: that branch's unique ADD commit has no patch in
#     main, so it shows '+' and the branch is KEPT (a bare net-zero TREE test would
#     wrongly delete it and gc-orphan the commits).
# git cherry IGNORES merge commits, so a "merge main into feature" tip is a no-op --
# exactly the merge-flow leftover the previous three-dot content guard mis-handled.
# Caveat: content introduced ONLY by an evil merge (hand-edited into a merge commit,
# never separately committed, never on main) is invisible to patch-id. This repo lands
# work via squash / ff / ordinary merges and never hand-edits merges, so the test is
# exact here; --force stays the escape hatch regardless.
# Any git error (bad ref / no merge-base) -> non-zero substitution -> treated as NOT merged.
branch_merged_into_main() {
  local b="$1" cherry
  [ -n "$b" ] || return 1
  cherry="$(git -C "$repo" cherry main "$b" 2>/dev/null)" || return 1
  case "$cherry" in
    *'+ '*) return 1 ;;   # a unique non-merge commit has no patch-id twin in main
    *)      return 0 ;;   # no '+' line: all unique work already in main (or none)
  esac
}

# Kill the processes that typically hold a worktree dir open (only on --force).
# rust-analyzer is a stateless, respawnable LSP; we never touch daw_* apps.
kill_holders() {
  log "  --force: terminating rust-analyzer (respawnable LSP) to release dir handles"
  taskkill //F //IM rust-analyzer.exe >/dev/null 2>&1 || true
  taskkill //F //IM rust-analyzer-proc-macro-srv.exe >/dev/null 2>&1 || true
  sleep 1
}

# 0 (true) if OS process $1 is currently running -- used to tell a STALE harness lock
# (owning "claude session (pid N)" already exited) from a live one. The harness stores a
# NATIVE Windows pid in the lock reason, which MSYS2 `kill -0` cannot see, so on Windows
# we query the Win32 process table via tasklist; on Linux/macOS (no tasklist) `kill -0`
# is exact. A non-numeric arg (unparseable reason) never reaches here -- the caller only
# calls this once it has extracted a bare pid -- but guard anyway and fail SAFE (alive),
# so an unclear lock is kept, never auto-cleared.
pid_alive() {
  local pid="$1"
  case "$pid" in ''|*[!0-9]*) return 0;; esac
  if command -v tasklist >/dev/null 2>&1; then
    tasklist //FI "PID eq $pid" //NH 2>/dev/null | grep -qw "$pid"
  else
    kill -0 "$pid" 2>/dev/null
  fi
}

# ---- worktree enumeration --------------------------------------------------
# Emit one row per worktree, fields separated by US (0x1f, \037): a NON-whitespace
# delimiter is REQUIRED -- with a tab, `read`'s IFS-whitespace splitting collapses
# adjacent tabs and drops empty fields, so a DETACHED worktree (empty branch field)
# would shift `locked` into `branch`, defeating both the detached-HEAD and
# locked-skip guards in remove_one. US preserves empty fields exactly.
# Row layout: <path>\037<head>\037<branch>\037<locked>\037<lockreason>
# lockreason is the free text git stores from `worktree lock --reason` (the harness
# writes "claude session <name> (pid <N>)"); remove_one parses the pid out of it to
# tell a stale lock (owning session gone) from a live one.
list_worktrees() {
  git -C "$repo" worktree list --porcelain 2>/dev/null | awk -v RS='' '
    {
      p=""; h=""; b=""; locked=0; lr=""
      n=split($0, lines, "\n")
      for (i=1;i<=n;i++) {
        if (lines[i] ~ /^worktree /)    { p=substr(lines[i],10) }
        else if (lines[i] ~ /^HEAD /)   { h=substr(lines[i],6) }
        else if (lines[i] ~ /^branch /) { b=substr(lines[i],8); sub(/^refs\/heads\//,"",b) }
        else if (lines[i] ~ /^locked/)  { locked=1; lr=substr(lines[i],8) }
      }
      if (p!="") printf "%s\037%s\037%s\037%s\037%s\n", p, h, b, locked, lr
    }'
}

# Remove leftover .claude/worktrees/<dir> entries that git no longer tracks as a
# worktree. They appear when an earlier `worktree remove`/`prune` deregistered the
# worktree but its rmdir lost a race to a holder process, leaving only the now-empty
# dir. Every git-driven path above (list_worktrees -> remove_one) is blind to these,
# so they pile up forever -- the "空ディレクトリがいっぱい残る" symptom. We rmdir ONLY
# empty orphans: a non-empty one might hold unsaved work, so we keep it and warn.
# rmdir (never rm -rf) does NOT descend a reparse point, so a stray junction is
# unlinked, never followed (no vendored-ffmpeg hazard).
prune_orphan_dirs() {
  [ -d "$wtroot" ] || return 0
  local registered=$'\n' p _h _b _lk _lr dir dn
  while IFS=$'\037' read -r p _h _b _lk _lr; do
    [ -n "$p" ] || continue
    registered="$registered$(norm "$p")"$'\n'
  done < <(list_worktrees)

  for dir in "$wtroot"/*/; do
    [ -d "$dir" ] || continue          # no glob match -> literal, skipped here
    dir="${dir%/}"
    dn="$(norm "$dir")"
    case "$registered" in *$'\n'"$dn"$'\n'*) continue;; esac   # still a live worktree
    if rmdir "$dir" 2>/dev/null; then
      log "pruned deregistered empty worktree dir: $dn"
    elif [ -n "$(find "$dir" -mindepth 1 -maxdepth 1 2>/dev/null)" ]; then
      log "SKIP (deregistered dir not empty -- may hold unsaved work; inspect): $dn"
    else
      log "NOTE (deregistered empty dir held open; clears when the holder exits): $dn"
    fi
  done
}

remove_one() {
  local wtpath="$1" branch="$3" locked="$4" lockreason="${5:-}"
  local wt; wt="$(norm "$wtpath")"
  local root; root="$(norm "$repo")"
  local wtroot_n; wtroot_n="$(norm "$wtroot")"

  # --- guards --------------------------------------------------------------
  if [ "$wt" = "$root" ]; then log "REFUSE: target is the main worktree ($wt)"; return; fi
  case "$wt/" in
    "$wtroot_n"/*) : ;;
    *) log "REFUSE: not under .claude/worktrees: $wt"; return;;
  esac

  # --- lock handling -------------------------------------------------------
  # The harness locks each fresh worktree to its owning session, storing the reason
  # "claude session <name> (pid <N>)". That lock only means something while the session
  # lives; once it exits the lock is STALE and blocks nothing real -- yet the merged
  # worktree lingers, which is exactly the "make worktree-rm-merged したのに消えない"
  # symptom. Removing a locked worktree REQUIRES clearing the lock first (`git worktree
  # remove --force` -- single -- still refuses a locked tree), so we ALWAYS unlock before
  # proceeding, but ONLY when entitled to:
  #   * --force        -> caller overrides; clear the lock and proceed.
  #   * owner pid dead -> stale lock; clear it and proceed (session ended, worktree
  #                       merged -> precisely what --all should sweep unattended).
  #   * pid alive/none -> a live session (or a lock whose reason we can't parse) still
  #                       owns it; SKIP untouched. We never auto-clear an unclear lock.
  if [ "$locked" = "1" ]; then
    local lpid; lpid="$(printf '%s' "$lockreason" | sed -n 's/.*[Pp][Ii][Dd][ =]\([0-9][0-9]*\).*/\1/p')"
    if [ "$o_force" -eq 1 ]; then
      git -C "$repo" worktree unlock "$wtpath" >/dev/null 2>&1 && log "  --force: cleared lock: $wt"
    elif [ -n "$lpid" ] && ! pid_alive "$lpid"; then
      if git -C "$repo" worktree unlock "$wtpath" >/dev/null 2>&1; then
        log "  auto-cleared stale lock (owner session pid $lpid is gone): $wt"
      else
        log "SKIP (git-locked; stale-unlock failed -- retry with FORCE=1): $wt"; return
      fi
    else
      log "SKIP (git-locked${lpid:+, owner session pid $lpid still running}; close it or FORCE=1): $wt"; return
    fi
  fi

  if [ "$o_force" -ne 1 ]; then
    if [ -z "$branch" ]; then log "SKIP (detached HEAD, use --force): $wt"; return; fi
    if ! branch_merged_into_main "$branch"; then
      log "SKIP (branch '$branch' not merged into main): $wt"; return
    fi
    # `--ignored` so unsaved gitignored deliverables (e.g. the project's untracked
    # r.md backlog, scratch design notes) also block a non-force removal --
    # plain `git status --porcelain` hides them and the dir would be deleted with
    # the notes inside. Exclude the known-regenerable / machine-local ignored entries
    # that are ALWAYS present in every worktree (so they must never block a removal):
    #   * target/ build caches -- including NESTED ones such as ui/target/, hence the
    #     (.*/)? prefix (a bare `target/` anchor missed ui/target/ and skipped every wt).
    #   * third_party/ (vendored ffmpeg, a real copy restorable via `make fetch-ffmpeg`).
    #   * .claude/settings.local.json -- the gitignored, harness-synced per-machine
    #     permissions allowlist. It exists in EVERY worktree, so without this exclusion
    #     `--all` always finds a "leftover" and skips every worktree (the bug behind
    #     "make worktree-rm-merged したけど消えなかった"). It is config, not a deliverable.
    if [ -n "$(git -C "$wt" status --porcelain --ignored 2>/dev/null | grep -vE '^!! ((.*/)?target/|third_party/|\.claude/settings\.local\.json$)')" ]; then
      log "SKIP (uncommitted or unsaved gitignored changes, use --force): $wt"; return
    fi
  fi

  log "removing worktree: $wt (branch '$branch')"

  git -C "$repo" worktree remove --force "$wt" 2>/dev/null || true
  if [ "$o_force" -eq 1 ] && _still_registered "$wtpath"; then
    kill_holders
    git -C "$repo" worktree remove --force "$wt" 2>/dev/null || true
  fi

  # A holder process can make git leave the dir (and the registration) behind.
  [ -e "$wt" ] && rmdir "$wt" 2>/dev/null || true
  git -C "$repo" worktree prune --expire=now 2>/dev/null || true

  if _still_registered "$wtpath"; then
    log "  LOCKED: held by another process; still registered. Close the editor / Claude session for this worktree, then re-run (or FORCE=1). ($wt)"
    return
  fi
  [ -e "$wt" ] && log "  NOTE: deregistered, but an empty dir remains (a process holds it open); it will clear on its own. ($wt)"

  # Delete the now-unreferenced branch. We only reach here after either --force or
  # a confirmed branch_merged_into_main (no unique non-merge commit), so `branch -D`
  # orphans nothing real. Plain `branch -d` would WRONGLY refuse the merge-flow
  # leftover (tip is a "merge main into feature" commit, unreachable from main), so
  # use -D unconditionally -- the predicate, not git's -d check, is the safety net.
  if [ -n "$branch" ]; then
    git -C "$repo" branch -D "$branch" >/dev/null 2>&1 && log "  deleted branch '$branch'" || log "  NOTE: branch '$branch' not deleted; kept"
  fi
  log "REMOVED: $wt"
}

# ---- select targets --------------------------------------------------------
wtroot_n="$(norm "$wtroot")"
matched=0
while IFS=$'\037' read -r p h b lk lr; do
  [ -n "$p" ] || continue
  pn="$(norm "$p")"
  if [ "$o_all" -eq 1 ]; then
    case "$pn/" in "$wtroot_n"/*) : ;; *) continue;; esac
    [ -n "$b" ] || continue
    # content-merged: subsumes is-ancestor AND catches the merge-flow leftover whose
    # tip sits AT main HEAD (the `git push . <branch>:main` landing -- the exact
    # "merged but worktree-rm-merged won't delete it" case). We no longer exclude
    # tip == main HEAD; remove_one's clean guard + live-lock SKIP (a stale harness lock
    # from an exited session is auto-cleared, a running one is kept) keep an active or
    # dirty worktree safe, and a clean fresh worktree at main HEAD has nothing to lose
    # and is one command to recreate.
    branch_merged_into_main "$b" || continue
    matched=1; remove_one "$p" "$h" "$b" "$lk" "$lr"
  elif [ -n "$o_tip" ]; then
    case "$pn/" in "$wtroot_n"/*) : ;; *) continue;; esac
    case "$h" in "$o_tip"*) matched=1; remove_one "$p" "$h" "$b" "$lk" "$lr";; esac
  elif [ -n "$o_path" ]; then
    [ "$pn" = "$(norm "$o_path")" ] && { matched=1; remove_one "$p" "$h" "$b" "$lk" "$lr"; }
  elif [ -n "$o_name" ]; then
    [ "$pn" = "$(norm "$wtroot/$o_name")" ] && { matched=1; remove_one "$p" "$h" "$b" "$lk" "$lr"; }
  fi
done < <(list_worktrees)

# Sweep deregistered leftover dirs (the git-driven loop above never sees them, so they
# accumulate as the "空ディレクトリが残る" symptom). --all only: targeted modes name one dir.
[ "$o_all" -eq 1 ] && prune_orphan_dirs

if [ "$matched" -eq 0 ]; then
  if [ "$o_all" -eq 1 ]; then log "no fully-merged worktrees to remove"
  elif [ -n "$o_tip" ]; then log "no worktree matches merged tip $o_tip (nothing to clean)"
  elif [ -n "$o_path" ]; then log "no registered worktree at $o_path"
  elif [ -n "$o_name" ]; then log "no registered worktree named '$o_name' (at $wtroot/$o_name)"
  else log "usage: cleanup_worktree.sh (--name <n> | --path <p> | --merged-tip <sha> | --all) [--force]"; exit 2
  fi
fi

exit 0
