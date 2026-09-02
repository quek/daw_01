#!/usr/bin/env bash
# Integration test for scripts/cleanup_worktree.sh.
#
# Builds a throwaway git repo with one worktree per scenario, runs the REAL
# cleanup script, and asserts which worktrees (and branches) get removed / kept.
# Everything happens under a mktemp scratch dir; the real repo is never touched.
# No --force is used for the headline cases, so they never run taskkill -> safe &
# cross-platform. A dedicated --force case at the end exercises the force path on
# a worktree that no process holds open, so taskkill is still never reached.
#
# Coverage:
#   merged detection : A ancestor-merged, B merge-flow leftover (the motivating
#                      fix), C unmerged, G add-then-revert net-zero (MUST be kept:
#                      net tree is empty but the commits live only on the branch),
#                      H squash-merged (now detected via patch-id -> removed, with a
#                      data-loss guard that the squashed content survives in main).
#   --all gates       : D tip==main HEAD merged REMOVED (clean; 2026-06-28 policy --
#                      indistinguishable from a fresh worktree, user chose to remove),
#                      E dirty (tracked) skipped, I_notes dirty gitignored deliverable
#                      skipped, I_target dirty regenerable-ignored (target/) NOT a
#                      blocker -> removed, I_pycache dirty nested regenerable-ignored
#                      (scripts/__pycache__/, left by arch-lint) NOT a blocker -> removed,
#                      I_local machine-local config
#                      (.claude/settings.local.json, present in EVERY real worktree)
#                      NOT a blocker -> removed, F orphan (no merge-base) kept,
#                      L locked-without-pid conservatively skipped, L_STALE locked by a
#                      DEAD "claude session (pid N)" auto-cleared -> removed, L_LIVE
#                      locked by a LIVE pid skipped.
#   orphan dirs       : ORPH_EMPTY deregistered empty dir pruned by --all,
#                      ORPH_FULL deregistered non-empty dir kept (may hold work).
#   remove_one guards : --name on unmerged (merge guard, the most-used path),
#                      --path outside .claude/worktrees (location guard),
#                      --path the main worktree (main-worktree guard),
#                      --path a detached worktree (detached-HEAD guard; also pins
#                      the US-delimiter field parse that the guard depends on).
#   force path        : --force removes an otherwise-skipped worktree (no taskkill).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cleanup="$here/cleanup_worktree.sh"
[ -f "$cleanup" ] || { echo "FATAL: cannot find $cleanup" >&2; exit 2; }

# scratch は **checkout 内 (target/)** に取る。`mktemp -d` の /tmp は使わない:
# make 配下では bash が MSYS2 runtime、PATH の coreutils (mkdir / find / mktemp) が
# Git for Windows runtime になり、両者の `/tmp` マウント先が別 (MSYS2 = C:\msys64\tmp、
# Git = %TEMP% 由来) なので、`mkdir -p /tmp/x` は成功するのに bash のリダイレクト
# `> /tmp/x/f` が "No such file" になる。実パス (F:/...) なら runtime が違っても一致する。
# 直接 bash から回すと両方 Git runtime で揃うため再現しない (2026-09-02 に make 経由でだけ
# 「非 empty な orphan dir が消された」と誤検出して発覚)。
scratch_root="$here/../target"
mkdir -p "$scratch_root"
scratch="$(mktemp -d "$scratch_root/wt-test.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT

fail=0
pass() { printf '  PASS: %s\n' "$1"; }
die()  { printf '  FAIL: %s\n' "$1"; fail=1; }

# wt_gone <path>: true if neither the dir nor a git registration remains.
wt_gone() {
  local p="$1"
  [ -e "$p" ] && return 1
  git -C "$scratch" worktree list --porcelain 2>/dev/null | grep -qx "worktree $p" && return 1
  return 0
}
wt_present()      { ! wt_gone "$1"; }
branch_exists()   { git -C "$scratch" show-ref --quiet "refs/heads/$1"; }
# commit_reachable <sha> <ref>: true if <sha> is an ancestor of (reachable from) <ref>.
commit_reachable() { git -C "$scratch" merge-base --is-ancestor "$1" "$2" 2>/dev/null; }

run_cleanup() { ( cd "$scratch" && bash "$cleanup" "$@" >/dev/null 2>&1 ); }

# ---- build scratch repo ----------------------------------------------------
git init -q -b main "$scratch"
git -C "$scratch" config user.email t@t.t
git -C "$scratch" config user.name t
wt() { echo "$scratch/.claude/worktrees/$1"; }

cm() { # cm <file> <content> <msg>  (commit on current branch of scratch main repo)
  printf '%s\n' "$2" > "$scratch/$1"
  git -C "$scratch" add "$1"
  git -C "$scratch" commit -q -m "$3"
}

cm base "0" "C0 base"
C0="$(git -C "$scratch" rev-parse HEAD)"

# .gitignore on main (so checked-out branches inherit it) for the gitignored-clean
# scenarios. notes.md = a precious untracked deliverable; target/ + third_party/ =
# regenerable trees the clean guard must NOT treat as blocking; .claude/settings.local.json
# = harness-synced per-machine config that must not block either.
# We ALSO track a real file under .claude/ (mirroring the repo's .claude/settings.json):
# without a tracked sibling, git collapses an all-ignored dir to "!! .claude/" instead
# of reporting "!! .claude/settings.local.json", and the I_local scenario would not
# exercise the actual path the exclusion regex matches. Same for scripts/: the real
# repo tracks scripts/loc_budget.py next to the __pycache__/ it leaves behind, so git
# reports "!! scripts/__pycache__/" (not a collapsed "!! scripts/") -- I_pycache must
# hit that shape.
printf 'notes.md\ntarget/\n__pycache__/\nthird_party/\n.claude/settings.local.json\n' > "$scratch/.gitignore"
mkdir -p "$scratch/.claude" "$scratch/scripts"
printf '{}\n' > "$scratch/.claude/settings.json"
printf '# stub\n' > "$scratch/scripts/loc_budget.py"
git -C "$scratch" add .gitignore .claude/settings.json scripts/loc_budget.py
git -C "$scratch" commit -q -m "add .gitignore"
GI="$(git -C "$scratch" rev-parse HEAD)"

# Scenario A "ancestor-merged": feature work merged into main via --no-ff; the
# branch tip stays reachable from main, and main has since advanced. Expect: --all removes.
git -C "$scratch" checkout -q -b featA "$C0"
cm a "A1" "A1 feature work"
git -C "$scratch" checkout -q main
git -C "$scratch" merge -q --no-ff featA -m "merge featA"
cm c2 "C2" "C2 later main work"   # advance main so featA tip != main HEAD

# Scenario B "mergeflow-merged": THE FIX, replicating lucky/quizzical exactly.
# featB's real work (B1) lands in main via --no-ff, main advances (C3), then featB
# pulls main back in with --no-ff. featB's tip is thus a "merge main into featB"
# commit that is NOT reachable from main (so the old --is-ancestor test skipped
# it) yet has no UNIQUE non-merge commit (B1 is already in main) and adds no net
# content. --no-ff is essential: a plain `git merge main` would fast-forward featB
# onto main's tip, collapsing the scenario. Expect: --all removes.
git -C "$scratch" checkout -q -b featB "$C0"
cm b "B1" "B1 feature work"
git -C "$scratch" checkout -q main
git -C "$scratch" merge -q --no-ff featB -m "merge featB"
cm c3 "C3" "C3 later main work"
git -C "$scratch" checkout -q featB
git -C "$scratch" merge -q --no-ff main -m "Merge branch 'main' into featB"  # tip = non-ff merge-of-main
git -C "$scratch" checkout -q main

# Scenario C "unmerged": featC has real new work never put into main. Expect: --all keeps.
git -C "$scratch" checkout -q -b featC main
cm u "U1" "U1 unmerged work"
git -C "$scratch" checkout -q main

# Scenario E "dirty-merged": content-merged (branch at an ancestor) but worktree
# has an uncommitted TRACKED change. Expect: --all keeps (clean guard).
git -C "$scratch" branch dirtyE "$GI"   # ancestor of main -> content-merged, tip != main HEAD

# Scenario F "orphan": no common ancestor with main. Expect: --all keeps (no merge-base).
git -C "$scratch" checkout -q --orphan orphanF
git -C "$scratch" rm -q -rf . >/dev/null 2>&1 || true
printf 'x\n' > "$scratch/orphan.txt"
git -C "$scratch" add orphan.txt
git -C "$scratch" commit -q -m "orphan root"
git -C "$scratch" checkout -q main

# Scenario G "netzero-revert": REGRESSION GUARD for the three-dot false-positive.
# netG commits real work (big.rs) then reverts it; its tip TREE equals its
# merge-base tree (net-zero) so a bare three-dot test would call it "merged" and
# `branch -D` would orphan the work. But it has UNIQUE non-merge commits, so the
# rev-list guard must keep it. Expect: --all KEEPS + the add-commit stays reachable.
git -C "$scratch" checkout -q -b netG "$C0"
cm big "BIG" "G add big.rs (real work)"
G_ADD="$(git -C "$scratch" rev-parse HEAD)"
git -C "$scratch" rm -q big
git -C "$scratch" commit -q -m "G remove big.rs (shelve/revert)"
git -C "$scratch" checkout -q main

# Scenario H "squash-merged": single-commit feature squashed into main (distinct
# SHA on main). featH's commit is unique by SHA but PATCH-IDENTICAL to main's squash
# commit, so `git cherry main featH` marks it '-' (already in main) -> branch_merged
# is true -> --all REMOVES it. This is the patch-id upgrade: the old reachability test
# conservatively kept it. We additionally assert the squashed content survives in main
# so the removal proves it is non-destructive, not just permissive.
git -C "$scratch" checkout -q -b featH "$GI"
cm s "S1" "S1 squashed work"
git -C "$scratch" checkout -q main
git -C "$scratch" merge -q --squash featH >/dev/null 2>&1
git -C "$scratch" commit -q -m "squash featH into main"
git -C "$scratch" checkout -q main

# Scenarios I_notes / I_target / I_local: content-merged (ancestor) clean-of-tracked-
# changes branches; difference is purely the gitignored content placed in the worktree.
# I_local pins the .claude/settings.local.json exclusion: that harness-synced per-machine
# permissions file exists in EVERY real worktree, so before the fix it made --all skip
# every worktree ("make worktree-rm-merged したけど消えなかった").
git -C "$scratch" branch ignNotes "$GI"
git -C "$scratch" branch ignTarget "$GI"
git -C "$scratch" branch ignPycache "$GI"
git -C "$scratch" branch ignLocal "$GI"

# Scenario L "locked": content-merged + clean, but git-locked with NO reason. A lock
# whose reason carries no parseable pid is conservatively kept (never auto-cleared).
git -C "$scratch" branch lockL "$GI"

# Scenarios L_stale / L_live: content-merged + clean, git-locked WITH a harness-style
# "claude session (pid N)" reason. A DEAD owner pid => stale lock -> --all auto-clears and
# REMOVES it (the "session ended but the merged worktree lingers" case). A LIVE owner pid
# => still owned -> --all keeps it untouched.
git -C "$scratch" branch lockStale "$GI"
git -C "$scratch" branch lockLive  "$GI"

# Branch literally named "0", content-merged: makes the OLD tab-collapse bug
# actually destructive for a detached worktree (it would read branch="0", find it
# merged, and remove the detached tree). With the US delimiter the detached row's
# branch field stays empty and the detached-HEAD guard fires. Presence alone arms
# the regression; no worktree is materialized for it.
git -C "$scratch" branch 0 main

# Outside worktree (NOT under .claude/worktrees): only the location guard keeps it.
git -C "$scratch" branch outsideBr "$GI"

# Scenario D "fresh-at-head": branch == main HEAD. Created LAST, after every commit
# to main, so its tip really is main HEAD. Indistinguishable by git content from a
# done-but-merged worktree at HEAD (resource-monitor's situation); per the 2026-06-28
# policy choice ("clean なら一括でも消す") Expect: --all REMOVES it (clean + merged).
git -C "$scratch" branch freshD main

# ---- materialize worktrees -------------------------------------------------
git -C "$scratch" worktree add -q "$(wt A)" featA
git -C "$scratch" worktree add -q "$(wt B)" featB
git -C "$scratch" worktree add -q "$(wt C)" featC
git -C "$scratch" worktree add -q "$(wt D)" freshD
git -C "$scratch" worktree add -q "$(wt E)" dirtyE
git -C "$scratch" worktree add -q "$(wt F)" orphanF
git -C "$scratch" worktree add -q "$(wt G)" netG
git -C "$scratch" worktree add -q "$(wt H)" featH
git -C "$scratch" worktree add -q "$(wt I_notes)" ignNotes
git -C "$scratch" worktree add -q "$(wt I_target)" ignTarget
git -C "$scratch" worktree add -q "$(wt I_pycache)" ignPycache
git -C "$scratch" worktree add -q "$(wt I_local)" ignLocal
git -C "$scratch" worktree add -q "$(wt L)" lockL
git -C "$scratch" worktree add -q "$(wt L_STALE)" lockStale
git -C "$scratch" worktree add -q "$(wt L_LIVE)"  lockLive
git -C "$scratch" worktree add -q "$scratch/outside_dir" outsideBr
git -C "$scratch" worktree add -q --detach "$(wt DET)" main

printf 'dirty\n' >> "$(wt E)/base"                 # E: uncommitted TRACKED change
printf 'precious notes\n' > "$(wt I_notes)/notes.md"   # I_notes: gitignored deliverable
mkdir -p "$(wt I_target)/target"
printf 'cache\n' > "$(wt I_target)/target/out.bin"     # I_target: regenerable ignored only
mkdir -p "$(wt I_pycache)/scripts/__pycache__"
printf 'pyc\n' > "$(wt I_pycache)/scripts/__pycache__/loc_budget.cpython-312.pyc"  # I_pycache: nested regenerable ignored only
mkdir -p "$(wt I_local)/.claude"
printf '{}\n' > "$(wt I_local)/.claude/settings.local.json"  # I_local: machine-local config only
printf 'det work\n' > "$(wt DET)/det.txt"              # DET: commit unique to detached HEAD
git -C "$(wt DET)" add det.txt
git -C "$(wt DET)" commit -q -m "detached unique work"
git -C "$scratch" worktree lock "$(wt L)"

# Resolve a LIVE and a DEAD pid in the exact form pid_alive() checks on THIS platform: on
# Windows pid_alive() queries tasklist, which needs the NATIVE Windows pid (MSYS exposes it
# at /proc/<pid>/winpid); on Linux/macOS it uses kill -0 on the native pid directly. The
# LIVE pid is this test shell ($$, alive for the whole run); the DEAD pid is a throwaway
# process we spawn and immediately reap.
# /proc は **bash の組み込み** (`[ -r ]` / `read`) で開く。PATH 上の `cat` は別 runtime
# (make 配下では Git の coreutils、シェルは MSYS2 の bash) になりうるので、pid 空間が
# 合わず「/proc/<pid>/winpid: No such file」になる。組み込みなら常にシェル自身の runtime。
native_pid() {
  local w
  if [ -r "/proc/$1/winpid" ] && read -r w < "/proc/$1/winpid" 2>/dev/null; then
    printf '%s' "$w"
  else
    printf '%s' "$1"
  fi
}
live_pid="$(native_pid "$$")"
# 捨てプロセスは **このシェルと同じ bash** で起こす。素の `sleep &` は PATH 上の (Git
# runtime の) sleep になり、MSYS2 bash の /proc にはそのプロセスが載らないので
# `/proc/<pid>/winpid` が読めず、dead pid が取れない (make 配下でだけ起きる runtime 混在)。
"$BASH" -c 'exec sleep 60' & dead_bg=$!
dead_pid="$(native_pid "$dead_bg")"
kill "$dead_bg" 2>/dev/null; wait "$dead_bg" 2>/dev/null
git -C "$scratch" worktree lock --reason "claude session L_STALE (pid $dead_pid start 0)" "$(wt L_STALE)"
git -C "$scratch" worktree lock --reason "claude session L_LIVE (pid $live_pid start 0)"  "$(wt L_LIVE)"

# Orphan leftover dirs under .claude/worktrees that git does NOT track as worktrees
# (an earlier remove/prune lost its rmdir race, leaving the dir behind). --all's
# prune_orphan_dirs must rmdir an EMPTY one and KEEP a non-empty one (may hold work).
mkdir -p "$(wt ORPH_EMPTY)"
mkdir -p "$(wt ORPH_FULL)"
printf 'leftover\n' > "$(wt ORPH_FULL)/keep.txt"

# ---- act: make worktree-rm-merged (--all, non-force) -----------------------
echo "== run: cleanup_worktree.sh --all =="
run_cleanup --all

wt_gone    "$(wt A)"  && pass "A ancestor-merged removed"            || die "A ancestor-merged NOT removed"
! branch_exists featA && pass "A branch deleted"                     || die "A branch survived"
wt_gone    "$(wt B)"  && pass "B mergeflow-merged removed (FIX)"     || die "B mergeflow-merged NOT removed (regression)"
! branch_exists featB && pass "B branch deleted"                     || die "B branch survived"
wt_present "$(wt C)"  && pass "C unmerged kept"                      || die "C unmerged WRONGLY removed (DATA LOSS)"
branch_exists featC   && pass "C branch kept"                        || die "C branch wrongly deleted"
wt_gone    "$(wt D)"  && pass "D fresh-at-head removed by --all (clean+merged)" || die "D fresh-at-head NOT removed (policy: clean at main HEAD is removed)"
! branch_exists freshD && pass "D branch deleted"                    || die "D branch survived"
wt_present "$(wt E)"  && pass "E dirty(tracked)-merged kept"         || die "E dirty-merged WRONGLY removed (DATA LOSS)"
wt_present "$(wt F)"  && pass "F orphan kept"                        || die "F orphan WRONGLY removed (DATA LOSS)"
wt_present "$(wt G)"  && pass "G netzero-revert kept (regression)"   || die "G netzero-revert WRONGLY removed (DATA LOSS)"
{ branch_exists netG && commit_reachable "$G_ADD" netG; } \
                      && pass "G unique commit still reachable"      || die "G unique commit ORPHANED (DATA LOSS)"
wt_gone    "$(wt H)"  && pass "H squash-merged removed (patch-id)"   || die "H squash-merged NOT removed (patch-id regression)"
! branch_exists featH && pass "H branch deleted"                     || die "H branch survived"
[ "$(git -C "$scratch" show main:s 2>/dev/null)" = "S1" ] \
                      && pass "H squashed content survives in main"  || die "H squashed content MISSING from main (DATA LOSS)"
wt_present "$(wt I_notes)"  && pass "I_notes dirty-gitignored kept"  || die "I_notes WRONGLY removed (lost notes.md)"
wt_gone    "$(wt I_target)" && pass "I_target (only target/) removed" || die "I_target NOT removed (regenerable ignored should not block)"
wt_gone    "$(wt I_pycache)" && pass "I_pycache (only scripts/__pycache__/) removed" || die "I_pycache NOT removed (nested __pycache__ should not block)"
wt_gone    "$(wt I_local)"  && pass "I_local (only .claude/settings.local.json) removed" || die "I_local NOT removed (machine-local config should not block)"
wt_present "$(wt L)"  && pass "L locked-without-pid kept"            || die "L locked-without-pid WRONGLY removed"
wt_gone    "$(wt L_STALE)" && pass "L_STALE stale-locked (dead pid) auto-cleared -> removed" || die "L_STALE stale-locked NOT removed (auto-unlock regression)"
! branch_exists lockStale  && pass "L_STALE branch deleted"         || die "L_STALE branch survived"
wt_present "$(wt L_LIVE)"  && pass "L_LIVE live-locked (alive pid) kept" || die "L_LIVE live-locked WRONGLY removed"
wt_gone    "$(wt ORPH_EMPTY)" && pass "empty orphan dir pruned"      || die "empty orphan dir NOT pruned (空ディレクトリ residue)"
wt_present "$(wt ORPH_FULL)"  && pass "non-empty orphan dir kept"    || die "non-empty orphan dir WRONGLY pruned (may hold unsaved work)"

# ---- remove_one guards via targeted modes ----------------------------------
echo "== run: cleanup_worktree.sh --name C (unmerged, targeted) =="
run_cleanup --name C
wt_present "$(wt C)" && pass "C unmerged kept by --name (merge guard)" || die "C unmerged WRONGLY removed by --name (DATA LOSS)"
branch_exists featC  && pass "C branch survives targeted --name"       || die "C branch wrongly -D'd by --name"

echo "== run: cleanup_worktree.sh --path <outside .claude/worktrees> =="
run_cleanup --path "$scratch/outside_dir"
wt_present "$scratch/outside_dir" && pass "outside worktree kept (location guard)" || die "location guard BYPASSED (escaped .claude/worktrees)"
branch_exists outsideBr           && pass "outside branch kept"                    || die "outside branch wrongly deleted"

echo "== run: cleanup_worktree.sh --path <main worktree> =="
run_cleanup --path "$scratch"
[ -e "$scratch/base" ] && pass "main worktree refused" || die "DISASTER: cleanup deleted/entered the main worktree"

echo "== run: cleanup_worktree.sh --path <detached worktree> =="
run_cleanup --path "$(wt DET)"
wt_present "$(wt DET)" && pass "detached worktree kept (detached-HEAD guard + US parse)" || die "detached worktree WRONGLY removed (DATA LOSS; tab-collapse regression?)"

# ---- positive paths: --name and --force ------------------------------------
# D (fresh-at-head) is already gone via --all now, so the non-force --name removal
# positive is exercised on a freshly-made merged+clean worktree M instead.
echo "== run: cleanup_worktree.sh --name M (merged-clean, non-force removal) =="
git -C "$scratch" branch nameM "$GI"               # ancestor of main -> merged + clean
git -C "$scratch" worktree add -q "$(wt M)" nameM
run_cleanup --name M
wt_gone "$(wt M)" && pass "M merged removed by --name (non-force)" || die "M merged NOT removed by --name"
! branch_exists nameM && pass "M branch deleted by --name"        || die "M branch survived --name"

echo "== run: cleanup_worktree.sh --name C --force (force bypasses merge guard) =="
run_cleanup --name C --force
wt_gone "$(wt C)" && pass "C removed by --force"  || die "C NOT removed by --force"
! branch_exists featC && pass "C branch deleted by --force" || die "C branch survived --force"

echo
if [ "$fail" -eq 0 ]; then echo "ALL PASS"; else echo "FAILURES PRESENT"; fi
exit "$fail"
