# Robust, junction-safe removal of a merged daw_01 git worktree.
#
# Why this exists: removing a worktree by hand keeps tripping over three Windows
# hazards, every single time:
#   1. third_party/ffmpeg is sometimes a JUNCTION into the main repo's vendored
#      copy. A naive `git worktree remove` / `rm -rf` FOLLOWS the junction and
#      deletes the real ffmpeg (gitignored, not in checkout -> unrecoverable;
#      incident 2026-06-14). We must detach reparse points BEFORE deleting.
#   2. rust-analyzer (and leftover daw_gui/daw_audio/daw_plugin_host exes) hold
#      the worktree dir open -> "used by another process" -> removal fails.
#   3. `git worktree remove` leaves the branch behind; it must be deleted too.
#
# Invocation is MANUAL/EXPLICIT (Makefile: `make rm-worktree NAME=...`, or call
# this with -Name/-Path/-All). It is deliberately NOT wired into a git hook:
# an earlier auto-on-merge hook removed a *sibling* agent's active worktree
# (2026-06-15) because .githooks is live even uncommitted, merges advance main,
# and worktrees are commonly merged from inside themselves. Auto-removal is unsafe
# with concurrent worktrees, so removal stays an explicit, targeted action.
#
# ASCII-only: PowerShell 5.1 misparses BOM-less UTF-8 with non-ASCII text
# (Write/Edit emit BOM-less UTF-8). Keep this file ASCII-only.
#
# Modes (pick one):
#   -Name <name>      remove .claude/worktrees/<name>.
#   -Path <path>      remove the worktree at <path>.
#   -All              remove EVERY worktree under .claude/worktrees whose branch
#                     has its own commits and is fully merged into main.
#   -MergedTip <sha>  remove the worktree whose HEAD == <sha> (e.g. a just-merged
#                     branch tip). No-op if no worktree matches. For scripted use.
#
# Safety (skipped only with -Force):
#   * target must live under <repo>\.claude\worktrees\ (never the main worktree).
#   * branch must be fully merged into main (no unmerged work lost).
#   * working tree must be clean (no uncommitted tracked changes).
#   * a `git worktree lock`ed worktree is always respected (never removed).
param(
    [string]$Repo,
    [string]$MergedTip,
    [string]$Name,
    [string]$Path,
    [switch]$All,
    [switch]$Force
)

$ErrorActionPreference = 'Continue'

if (-not $Repo) { $Repo = (& git rev-parse --show-toplevel 2>$null) }
if (-not $Repo) { $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path }
$Repo = ([IO.Path]::GetFullPath($Repo)).TrimEnd('\')

$targetDir = Join-Path $Repo 'target'
if (-not (Test-Path -LiteralPath $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}
$logFile = Join-Path $targetDir 'worktree-cleanup.log'

function Write-Log([string]$msg) {
    $ts = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    $line = "[$ts] $msg"
    Add-Content -LiteralPath $logFile -Value $line -Encoding ascii
    Write-Output $line
}

# Worktrees live here; nothing outside this prefix may ever be removed.
$wtRoot = ([IO.Path]::GetFullPath((Join-Path $Repo '.claude\worktrees'))).TrimEnd('\')

# Parse `git worktree list --porcelain` into objects { Path, Head, Branch, Locked }.
function Get-Worktrees {
    $out = & git -C $Repo worktree list --porcelain 2>$null
    $list = @()
    $cur = $null
    foreach ($line in $out) {
        if ($line -like 'worktree *') {
            if ($cur) { $list += $cur }
            $p = $line.Substring(9)
            $cur = [pscustomobject]@{
                Path   = ([IO.Path]::GetFullPath($p)).TrimEnd('\')
                Head   = ''
                Branch = ''
                Locked = $false
            }
        } elseif ($line -like 'HEAD *') {
            if ($cur) { $cur.Head = $line.Substring(5).Trim() }
        } elseif ($line -like 'branch *') {
            if ($cur) { $cur.Branch = ($line.Substring(7).Trim() -replace '^refs/heads/', '') }
        } elseif ($line -eq 'locked' -or $line -like 'locked *') {
            if ($cur) { $cur.Locked = $true }
        }
    }
    if ($cur) { $list += $cur }
    return $list
}

# Find directory reparse points (junctions/symlinks) WITHOUT recursing into them
# (a manual walk; -Recurse would follow junctions and could loop / touch targets).
function Get-ReparseDirs([string]$root) {
    $found = @()
    $stack = New-Object System.Collections.Stack
    $stack.Push($root)
    while ($stack.Count -gt 0) {
        $dir = $stack.Pop()
        $children = $null
        try { $children = Get-ChildItem -LiteralPath $dir -Force -Directory -ErrorAction Stop } catch { continue }
        foreach ($c in $children) {
            if ($c.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                $found += $c.FullName     # junction: record, do NOT descend
            } else {
                $stack.Push($c.FullName)
            }
        }
    }
    return $found
}

function Remove-OneWorktree($wt) {
    $wtPath = $wt.Path
    $branch = $wt.Branch

    # --- guards --------------------------------------------------------------
    if ($wtPath -eq $Repo) { Write-Log "REFUSE: target is the main worktree ($wtPath)"; return }
    $under = $wtPath.StartsWith($wtRoot + '\', [StringComparison]::OrdinalIgnoreCase)
    if (-not $under) { Write-Log "REFUSE: not under .claude\worktrees: $wtPath"; return }
    if ($wt.Locked -and -not $Force) { Write-Log "SKIP (locked): $wtPath"; return }

    if (-not $Force) {
        if (-not $branch) { Write-Log "SKIP (detached HEAD, use -Force): $wtPath"; return }
        # fully merged into main? (exit 0 = ancestor = merged)
        & git -C $Repo merge-base --is-ancestor $branch main 2>$null
        if ($LASTEXITCODE -ne 0) { Write-Log "SKIP (branch '$branch' not merged into main): $wtPath"; return }
        # clean working tree? (tracked changes only)
        $dirty = & git -C $wtPath status --porcelain 2>$null
        if ($dirty) { Write-Log "SKIP (uncommitted changes): $wtPath"; return }
    }

    Write-Log "removing worktree: $wtPath (branch '$branch')"

    # Removal strategy = rename-to-trash. Renaming the worktree dir is BOTH the
    # lock test AND the first step of removal, in one atomic operation:
    #   * rename FAILS  -> a process holds the dir (a shell cwd / editor /
    #     rust-analyzer dir handle). DEFAULT: abort, leaving the worktree FULLY
    #     intact (still registered + on disk). We must never deregister a worktree
    #     we cannot delete -- doing that orphaned a sibling worktree on 2026-06-15 --
    #     and we never kill processes by default. -Force: terminate the holders
    #     (daw exes under the worktree + rust-analyzer, a respawnable LSP) and retry.
    #   * rename SUCCEEDS -> nothing holds it; the original path is now free, so we
    #     deregister cleanly and then delete the renamed copy.
    $trash = "$wtPath.__removing"
    if (Test-Path -LiteralPath $trash) {
        foreach ($rp in (Get-ReparseDirs $trash)) { & cmd /c rmdir "$rp" 2>$null }
        try { Remove-Item -LiteralPath $trash -Recurse -Force -ErrorAction SilentlyContinue } catch {}
    }

    $moved = $false
    try { [System.IO.Directory]::Move($wtPath, $trash); $moved = $true } catch {}

    if (-not $moved -and $Force) {
        Write-Log "  locked; FORCE: terminating holders (daw exes under worktree + rust-analyzer)"
        try {
            Get-Process -Name daw_gui, daw_audio, daw_plugin_host -ErrorAction SilentlyContinue |
                Where-Object { $_.Path -and $_.Path.StartsWith($wtPath, [StringComparison]::OrdinalIgnoreCase) } |
                Stop-Process -Force -ErrorAction SilentlyContinue
        } catch {}
        # rust-analyzer can't be targeted per-worktree (cwd-based, not in cmdline);
        # only -Force kills it (all instances; a stateless daemon the editor respawns).
        try {
            Get-Process -Name rust-analyzer, rust-analyzer-proc-macro-srv -ErrorAction SilentlyContinue |
                Stop-Process -Force -ErrorAction SilentlyContinue
        } catch {}
        Start-Sleep -Milliseconds 1200
        try { [System.IO.Directory]::Move($wtPath, $trash); $moved = $true } catch {}
    }

    if (-not $moved) {
        Write-Log "  LOCKED: held by another process; LEFT INTACT. Close the editor / Claude session for this worktree, then re-run (or use FORCE=1). ($wtPath)"
        return
    }

    # The original path is gone now -> deregister the missing worktree immediately
    # (--expire=now: prune the stale entry regardless of age).
    & git -C $Repo worktree prune --expire=now 2>$null

    # Delete the renamed copy, detaching any junctions FIRST (protect vendored ffmpeg).
    foreach ($rp in (Get-ReparseDirs $trash)) {
        Write-Log "  detaching reparse point: $rp"
        & cmd /c rmdir "$rp" 2>$null
    }
    try { Remove-Item -LiteralPath $trash -Recurse -Force -ErrorAction Stop }
    catch { Write-Log "  NOTE: registration removed, but some files were locked; leftover at $trash" }

    # Delete the (now unreferenced) branch.
    if ($branch) {
        if ($Force) { & git -C $Repo branch -D $branch 2>$null }
        else { & git -C $Repo branch -d $branch 2>$null }
        if ($LASTEXITCODE -eq 0) { Write-Log "  deleted branch '$branch'" }
        else { Write-Log "  NOTE: branch '$branch' not deleted (not fully merged?); kept" }
    }
    Write-Log "REMOVED: $wtPath"
}

# --- select targets ---------------------------------------------------------
# NOTE: PowerShell variable names are case-insensitive, so a local named $all would
# alias the [switch]$All parameter. Use $wts.
$wts = Get-Worktrees
$targets = @()

if ($MergedTip) {
    $tip = $MergedTip.Trim()
    $targets = $wts | Where-Object {
        $_.Path.StartsWith($wtRoot + '\', [StringComparison]::OrdinalIgnoreCase) -and
        $_.Head -and ($_.Head -eq $tip -or $_.Head.StartsWith($tip))
    }
    if (-not $targets) { Write-Log "no worktree matches merged tip $tip (nothing to clean)"; return }
}
elseif ($Path) {
    $full = ([IO.Path]::GetFullPath($Path)).TrimEnd('\')
    $targets = $wts | Where-Object { $_.Path -eq $full }
    if (-not $targets) { Write-Log "no registered worktree at $full"; return }
}
elseif ($Name) {
    $full = ([IO.Path]::GetFullPath((Join-Path $wtRoot $Name))).TrimEnd('\')
    $targets = $wts | Where-Object { $_.Path -eq $full }
    if (-not $targets) { Write-Log "no registered worktree named '$Name' (at $full)"; return }
}
elseif ($All) {
    foreach ($w in $wts) {
        if (-not $w.Path.StartsWith($wtRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { continue }
        if (-not $w.Branch) { continue }
        # has own commits? (branch tip is ahead of its merge-base with main)
        $mb = (& git -C $Repo merge-base $w.Branch main 2>$null)
        $tip = (& git -C $Repo rev-parse $w.Branch 2>$null)
        if (-not $mb -or -not $tip -or ($mb.Trim() -eq $tip.Trim())) { continue }  # no own work -> active/fresh, skip
        & git -C $Repo merge-base --is-ancestor $w.Branch main 2>$null
        if ($LASTEXITCODE -ne 0) { continue }                                       # not merged -> skip
        $targets += $w
    }
    if (-not $targets) { Write-Log "no fully-merged worktrees to remove"; return }
}
else {
    Write-Log "usage: cleanup_worktree.ps1 (-MergedTip <sha> | -Name <n> | -Path <p> | -All) [-Force]"
    return
}

foreach ($t in $targets) { Remove-OneWorktree $t }
