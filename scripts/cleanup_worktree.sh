#!/usr/bin/env bash
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
#                       has its own commits and is fully merged into main.
# Safety (skipped only with --force):
#   * target must live under <repo>/.claude/worktrees/ (never the main worktree).
#   * branch must be fully merged into main (no unmerged work lost).
#   * working tree must be clean (no uncommitted tracked changes).
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

# Absolute MSYS path, trailing slash stripped, for stable comparison.
norm() { cygpath -u "$1" 2>/dev/null | sed 's:/*$::'; }

# 0 (true) if the worktree at git-reported path $1 is still registered. Avoids a
# `git ... | grep` pipe so `set -o pipefail` + SIGPIPE can't abort the script.
_still_registered() {
  local p="$1" line
  while IFS= read -r line; do
    case "$line" in "worktree $p") return 0;; esac
  done < <(git -C "$repo" worktree list --porcelain 2>/dev/null)
  return 1
}

# Kill the processes that typically hold a worktree dir open (only on --force).
# rust-analyzer is a stateless, respawnable LSP; we never touch daw_* apps.
kill_holders() {
  log "  --force: terminating rust-analyzer (respawnable LSP) to release dir handles"
  taskkill //F //IM rust-analyzer.exe >/dev/null 2>&1 || true
  taskkill //F //IM rust-analyzer-proc-macro-srv.exe >/dev/null 2>&1 || true
  sleep 1
}

# ---- worktree enumeration --------------------------------------------------
# Emit one TAB-separated row per worktree: <path>\t<head>\t<branch>\t<locked>
list_worktrees() {
  git -C "$repo" worktree list --porcelain 2>/dev/null | awk -v RS='' '
    {
      p=""; h=""; b=""; locked=0
      n=split($0, lines, "\n")
      for (i=1;i<=n;i++) {
        if (lines[i] ~ /^worktree /)    { p=substr(lines[i],10) }
        else if (lines[i] ~ /^HEAD /)   { h=substr(lines[i],6) }
        else if (lines[i] ~ /^branch /) { b=substr(lines[i],8); sub(/^refs\/heads\//,"",b) }
        else if (lines[i] ~ /^locked/)  { locked=1 }
      }
      if (p!="") printf "%s\t%s\t%s\t%s\n", p, h, b, locked
    }'
}

remove_one() {
  local wtpath="$1" branch="$3" locked="$4"
  local wt; wt="$(norm "$wtpath")"
  local root; root="$(norm "$repo")"
  local wtroot_n; wtroot_n="$(norm "$wtroot")"

  # --- guards --------------------------------------------------------------
  if [ "$wt" = "$root" ]; then log "REFUSE: target is the main worktree ($wt)"; return; fi
  case "$wt/" in
    "$wtroot_n"/*) : ;;
    *) log "REFUSE: not under .claude/worktrees: $wt"; return;;
  esac
  if [ "$locked" = "1" ] && [ "$o_force" -ne 1 ]; then log "SKIP (git-locked): $wt"; return; fi

  if [ "$o_force" -ne 1 ]; then
    if [ -z "$branch" ]; then log "SKIP (detached HEAD, use --force): $wt"; return; fi
    if ! git -C "$repo" merge-base --is-ancestor "$branch" main 2>/dev/null; then
      log "SKIP (branch '$branch' not merged into main): $wt"; return
    fi
    if [ -n "$(git -C "$wt" status --porcelain 2>/dev/null)" ]; then
      log "SKIP (uncommitted changes, use --force): $wt"; return
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

  # Delete the now-unreferenced branch.
  if [ -n "$branch" ]; then
    if [ "$o_force" -eq 1 ]; then
      git -C "$repo" branch -D "$branch" >/dev/null 2>&1 && log "  deleted branch '$branch'" || log "  NOTE: branch '$branch' not deleted; kept"
    else
      git -C "$repo" branch -d "$branch" >/dev/null 2>&1 && log "  deleted branch '$branch'" || log "  NOTE: branch '$branch' not fully merged?; kept"
    fi
  fi
  log "REMOVED: $wt"
}

# ---- select targets --------------------------------------------------------
wtroot_n="$(norm "$wtroot")"
matched=0
while IFS=$'\t' read -r p h b lk; do
  [ -n "$p" ] || continue
  pn="$(norm "$p")"
  if [ "$o_all" -eq 1 ]; then
    case "$pn/" in "$wtroot_n"/*) : ;; *) continue;; esac
    [ -n "$b" ] || continue
    mb="$(git -C "$repo" merge-base "$b" main 2>/dev/null || true)"
    tip="$(git -C "$repo" rev-parse "$b" 2>/dev/null || true)"
    [ -n "$mb" ] && [ -n "$tip" ] || continue
    [ "$mb" != "$tip" ] || continue                                   # no own work -> active/fresh, skip
    git -C "$repo" merge-base --is-ancestor "$b" main 2>/dev/null || continue
    matched=1; remove_one "$p" "$h" "$b" "$lk"
  elif [ -n "$o_tip" ]; then
    case "$pn/" in "$wtroot_n"/*) : ;; *) continue;; esac
    case "$h" in "$o_tip"*) matched=1; remove_one "$p" "$h" "$b" "$lk";; esac
  elif [ -n "$o_path" ]; then
    [ "$pn" = "$(norm "$o_path")" ] && { matched=1; remove_one "$p" "$h" "$b" "$lk"; }
  elif [ -n "$o_name" ]; then
    [ "$pn" = "$(norm "$wtroot/$o_name")" ] && { matched=1; remove_one "$p" "$h" "$b" "$lk"; }
  fi
done < <(list_worktrees)

if [ "$matched" -eq 0 ]; then
  if [ "$o_all" -eq 1 ]; then log "no fully-merged worktrees to remove"
  elif [ -n "$o_tip" ]; then log "no worktree matches merged tip $o_tip (nothing to clean)"
  elif [ -n "$o_path" ]; then log "no registered worktree at $o_path"
  elif [ -n "$o_name" ]; then log "no registered worktree named '$o_name' (at $wtroot/$o_name)"
  else log "usage: cleanup_worktree.sh (--name <n> | --path <p> | --merged-tip <sha> | --all) [--force]"; exit 2
  fi
fi

exit 0
