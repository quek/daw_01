# Detached background release build, launched by .githooks/post-commit after EVERY commit.
#
# ASCII-only: PowerShell 5.1 misparses BOM-less UTF-8 files containing non-ASCII text
# (Write/Edit emit BOM-less UTF-8), which previously corrupted string literals. Keep
# this file ASCII-only.
#
# Appends all cargo output to target/release-build.log. On failure: writes the marker
# target/.release-build-failed and shows an error dialog. Exit code is not consumed by
# git (this process is detached from the commit), so failure is surfaced via marker + log.
param([string]$Repo)

$ErrorActionPreference = 'Continue'

if (-not $Repo) { $Repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path }
Set-Location -LiteralPath $Repo

$targetDir = Join-Path $Repo 'target'
if (-not (Test-Path -LiteralPath $targetDir)) {
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
}
$log    = Join-Path $targetDir 'release-build.log'
$marker = Join-Path $targetDir '.release-build-failed'
try { Remove-Item -LiteralPath $marker -ErrorAction Stop } catch {}

$sha   = (& git rev-parse --short HEAD).Trim()
$start = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
Set-Content -LiteralPath $log -Value "[post-commit] release build start $sha at $start" -Encoding ascii

# Use cmd's OS-level redirection (>> 2>&1) rather than PowerShell's *>> so PS 5.1 does
# NOT wrap cargo's stderr (its normal progress output) as NativeCommandError records,
# and the log stays single-encoding. $LASTEXITCODE carries cargo's real exit code.
cmd /c "cargo build --workspace --release >> `"$log`" 2>&1"
$code = $LASTEXITCODE

$end = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
if ($code -ne 0) {
    Add-Content -LiteralPath $log -Value "[post-commit] release build FAILED (exit $code) at $end" -Encoding ascii
    New-Item -ItemType File -Path $marker -Force | Out-Null
    try {
        Add-Type -AssemblyName System.Windows.Forms
        $msg = "release build FAILED after commit $sha (exit $code).`r`nSee target\release-build.log"
        [System.Windows.Forms.MessageBox]::Show($msg, 'daw_01: release build FAILED', 'OK', 'Error') | Out-Null
    } catch {}
} else {
    Add-Content -LiteralPath $log -Value "[post-commit] release build OK at $end" -Encoding ascii
}
