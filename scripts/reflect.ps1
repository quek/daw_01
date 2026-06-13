# Stop hook: detect recurring friction patterns in the current session's metrics
# and UPSERT them as trackable work-items into the AHE backlog, so the next
# session's SessionStart hook can FORCE triage (promote / request-hook / dismiss).
#
# Replaces the old behavior of appending prose "candidate for ..." lines to
# reflection_latest.md -- a file no hook ever read, so suggestions accreted but
# were never actuated into a skill/hook/command (the loop dead-ended at memory).
#
# input log:  %USERPROFILE%\.claude\projects\F--dev-daw-01\metrics\YYYY-MM.jsonl
# backlog:    %USERPROFILE%\.claude\projects\F--dev-daw-01\ahe_backlog.md
#   per-project user dir => shared across main repo + ALL worktrees (no git churn,
#   no per-worktree silo, no merge conflicts). Persistent: rows are never
#   truncated, only status-flipped (open -> done | dismissed | needs-user).
# silent on failure (exit 0). All SCRIPT literals ASCII-only (PS 5.1 encoding);
# the backlog file is read/written via .NET UTF-8 (no BOM) so any non-ASCII notes
# a human adds round-trip safely.

$ErrorActionPreference = 'SilentlyContinue'

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }
try { $data = $input_str | ConvertFrom-Json } catch { exit 0 }
$session_id = $data.session_id
if (-not $session_id) { exit 0 }
$session_short = $session_id.Substring(0, [Math]::Min(8, $session_id.Length))

$logdir = Join-Path $env:USERPROFILE ".claude\projects\F--dev-daw-01\metrics"
$logfile = Join-Path $logdir "$(Get-Date -Format yyyy-MM).jsonl"
if (-not (Test-Path $logfile)) { exit 0 }

# Load current-session entries only
$entries = New-Object System.Collections.ArrayList
Get-Content $logfile -Encoding utf8 | ForEach-Object {
    if ($_) {
        try {
            $e = $_ | ConvertFrom-Json
            if ($e.session -eq $session_id) { [void]$entries.Add($e) }
        } catch {}
    }
}
if ($entries.Count -lt 3) { exit 0 }

# --- Pattern detection -> normalized records @{ kind; matcher; target; desc } ---
$detected = New-Object System.Collections.ArrayList
$bash  = @($entries | Where-Object { $_.tool -eq "Bash" -and $_.matcher })
$edits = @($entries | Where-Object { ($_.tool -eq "Edit" -or $_.tool -eq "Write") -and $_.matcher })
$reads = @($entries | Where-Object { $_.tool -eq "Read" -and $_.matcher })

# Pattern 1: same Bash command repeated 3+ consecutively
for ($i = 0; $i -lt $bash.Count - 2; $i++) {
    if ($bash[$i].matcher -eq $bash[$i+1].matcher -and $bash[$i+1].matcher -eq $bash[$i+2].matcher) {
        [void]$detected.Add(@{ kind = "bash-repeat"; matcher = $bash[$i].matcher; target = "command";
            desc = 'Bash repeated 3+ in a row: `' + $bash[$i].matcher + '`' })
        break
    }
}
# Pattern 2: Edit/Write same file 5+ times
if ($edits.Count -ge 5) {
    $top = ($edits | Group-Object matcher | Sort-Object Count -Descending)[0]
    if ($top.Count -ge 5) {
        [void]$detected.Add(@{ kind = "edit-hotspot"; matcher = $top.Name; target = "skill";
            desc = 'Edit hotspot: `' + $top.Name + '` (x' + $top.Count + ')' })
    }
}
# Pattern 3: 2+ consecutive Bash failures
$err_streak = 0
foreach ($b in $bash) {
    if ($b.status -eq "error") {
        $err_streak++
        if ($err_streak -ge 2) {
            [void]$detected.Add(@{ kind = "bash-failure"; matcher = $b.matcher; target = "skill";
                desc = 'Bash repeated failure: `' + $b.matcher + '`' })
            break
        }
    } else { $err_streak = 0 }
}
# Pattern 4: Read same file 5+ times
if ($reads.Count -ge 5) {
    $top = ($reads | Group-Object matcher | Sort-Object Count -Descending)[0]
    if ($top.Count -ge 5) {
        [void]$detected.Add(@{ kind = "read-hotspot"; matcher = $top.Name; target = "memory";
            desc = 'Read hotspot: `' + $top.Name + '` (x' + $top.Count + ')' })
    }
}

if ($detected.Count -eq 0) { exit 0 }

# --- Backlog upsert ---
$backlog = Join-Path $env:USERPROFILE ".claude\projects\F--dev-daw-01\ahe_backlog.md"
$today   = Get-Date -Format "yyyy-MM-dd"
$utf8    = New-Object System.Text.UTF8Encoding $false   # no BOM (bash reads this)
$START   = "<!-- AHE-TABLE-START -->"
$END     = "<!-- AHE-TABLE-END -->"
$HEADER  = "| id | status | sessions | target | first-seen | last-seen | last-session | pattern | notes |"
$SEP     = "|----|--------|----------|--------|------------|-----------|--------------|---------|-------|"

function New-Template {
    $t = @'
# AHE backlog

Detected recurring friction (from session metrics) as trackable work-items.
Managed by the Stop hook (scripts/reflect.ps1, UPSERT) and surfaced by the
SessionStart hook (OPEN rows become a Required Action). Triage each OPEN row with
the /promote-reflection skill: promote it into the suggested artifact, or dismiss
it with a reason. status flows: open -> done | dismissed | needs-user.

- target=hook rows cannot be auto-applied (settings.json edits are
  classifier-blocked). /promote-reflection writes a ready-to-paste spec under
  "hook requests" and sets status=needs-user until you apply it; then flip to done.
- done / dismissed rows are TERMINAL: reflect.ps1 never re-surfaces them, so a
  pattern stops nagging once you have decided what to do about it.

## hook requests (awaiting your approval)

(none)

## patterns

__START__
__HEADER__
__SEP__
__END__
'@
    $t = $t.Replace("__START__", $START).Replace("__HEADER__", $HEADER).Replace("__SEP__", $SEP).Replace("__END__", $END)
    return $t
}

function Compute-Id($kind, $matcher) {
    $sha = [System.Security.Cryptography.SHA1]::Create()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes("$kind|$matcher")
    $h = ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString("x2") }) -join ""
    return "$kind-" + $h.Substring(0, 8)
}

# Read (or initialize) the backlog as raw text, BOM-safe
if (Test-Path $backlog) {
    $raw = [System.IO.File]::ReadAllText($backlog)
    if ($raw.IndexOf($START) -lt 0) { $raw = New-Template }
} else {
    $dir = Split-Path $backlog
    if (-not (Test-Path $dir)) { [void](New-Item -ItemType Directory -Force -Path $dir) }
    $raw = New-Template
}

$startIdx = $raw.IndexOf($START)
if ($startIdx -lt 0) { $raw = New-Template; $startIdx = $raw.IndexOf($START) }
$endIdx = $raw.IndexOf($END)
if ($endIdx -lt 0 -or $endIdx -lt $startIdx) {
    # END marker lost/corrupted: drop everything from START onward; rebuilt below.
    $before = $raw.Substring(0, $startIdx)
    $after  = "`n"
    $region = ""
} else {
    $before = $raw.Substring(0, $startIdx)
    $after  = $raw.Substring($endIdx + $END.Length)
    $region = $raw.Substring($startIdx, $endIdx - $startIdx)
}

# Parse existing data rows (9 cells each) preserving order
$rows = New-Object System.Collections.ArrayList
foreach ($line in ($region -split "`n")) {
    $t = $line.Trim()
    if ($t.StartsWith("|") -and $t -ne $HEADER -and $t -notmatch '^\|-') {
        $cells = $t.Trim('|').Split('|') | ForEach-Object { $_.Trim() }
        if ($cells.Count -ge 9 -and $cells[0] -ne "id") {
            [void]$rows.Add(@($cells[0], $cells[1], $cells[2], $cells[3], $cells[4], $cells[5], $cells[6], $cells[7], $cells[8]))
        }
    }
}

foreach ($d in $detected) {
    $id   = Compute-Id $d.kind $d.matcher
    $desc = ($d.desc -replace '\|', '/')   # never break the table cell
    $existing = $null
    foreach ($r in $rows) { if ($r[0] -eq $id) { $existing = $r; break } }
    if ($existing) {
        if ($existing[1] -eq "done" -or $existing[1] -eq "dismissed") { continue }  # terminal
        if ($existing[6] -ne $session_short) {
            $existing[2] = ([int]$existing[2] + 1).ToString()   # bump distinct-session count
            $existing[6] = $session_short
        }
        $existing[5] = $today   # last-seen
        # status / target / first-seen / pattern / notes preserved (human/skill owned)
    } else {
        [void]$rows.Add(@($id, "open", "1", $d.target, $today, $today, $session_short, $desc, ""))
    }
}

# Rebuild the table region
$lines = New-Object System.Collections.ArrayList
[void]$lines.Add($START)
[void]$lines.Add($HEADER)
[void]$lines.Add($SEP)
foreach ($r in $rows) { [void]$lines.Add("| " + ($r -join " | ") + " |") }
# Re-append the END marker: it lives between $region and $after, so it is in
# neither -- the rebuilt region must carry it or it gets dropped on every write.
$newRegion = ($lines -join "`n") + "`n" + $END

$final = $before + $newRegion + $after
[System.IO.File]::WriteAllText($backlog, $final, $utf8)
exit 0
