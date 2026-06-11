# PreToolUse hook (Bash / PowerShell): about to `git commit` or launch daw_gui for
# jikki-kensho (real-device verification) -> warn if there is a PENDING gui_01
# request in docs/gui_01_conversation.md (= the trailing "## #NNN" section has no
# gui_01 resolution tag yet).
#
# Rule (feedback_progress_while_waiting_gui01): while waiting for gui_01 landing,
# the ONLY things to defer are commit and jikki-kensho; do them ONCE, AFTER landing.
# Keep daw_01 parked until then. This hook is the action-time interruption
# (check_antipattern.ps1 style): stdout + exit 0, NEVER blocks.
#
# ASCII-only (memory feedback_powershell_ascii_hooks: PS 5.1 + no-BOM mojibakes
# non-ASCII string literals). The conversation file itself is read as UTF-8; only
# ASCII tags are matched.
#
# stdin: Claude Code JSON { tool_name, tool_input: { command } }

$ErrorActionPreference = 'SilentlyContinue'
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }
try { $data = $input_str | ConvertFrom-Json } catch { exit 0 }

$cmd = [string]$data.tool_input.command
if (-not $cmd) { exit 0 }

# Only care about commit or launching daw_gui for verification.
$isCommit = $cmd -match 'git\s+commit'
$isRun = ($cmd -match 'cargo\s+run\b[^|]*daw_gui') -or ($cmd -match 'daw_gui\.exe')
if (-not ($isCommit -or $isRun)) { exit 0 }

$conv = Join-Path $PSScriptRoot '..\docs\gui_01_conversation.md'
if (-not (Test-Path $conv)) { exit 0 }
$text = Get-Content -Raw -Encoding UTF8 $conv
if (-not $text) { exit 0 }

# Split into per-request sections at each "## #NNN" header; inspect the LAST one
# (= the request most likely being waited on).
$sections = [regex]::Split($text, '(?m)^##\s+#\d+')
if ($sections.Count -lt 2) { exit 0 }
$last = $sections[$sections.Count - 1]

# Resolved if gui_01 wrote any resolution / reply tag (ASCII) in that section.
# gui_01's legend uses [Replied] when it has implemented and responded.
if ($last -match '\[(Replied|Resolved|Withdrawn|Ack|Landed|Done|Closed)\]') { exit 0 }

$action = if ($isCommit) { 'git commit' } else { 'daw_gui run (jikki-kensho)' }
$lines = @()
$lines += "[gui_01 pending check]"
$lines += ""
$lines += "About to run: $action"
$lines += "But the trailing request in docs/gui_01_conversation.md has NO gui_01"
$lines += "reply/resolution tag ([Replied] / [Resolved] / [Landed] / [Ack]) yet."
$lines += ""
$lines += "Rule (feedback_progress_while_waiting_gui01): while waiting for gui_01"
$lines += "landing, the ONLY things to defer are commit and jikki-kensho. Do them"
$lines += "ONCE, AFTER landing. Keep daw_01 parked until then."
$lines += ""
$lines += "If gui_01 has in fact landed (user said so, or a reply is present),"
$lines += "proceed. Otherwise STOP and wait for gui_01."
$lines += ""
$lines += "(warning only, not a block. Confirm the gui_01 reply before continuing.)"
Write-Output ($lines -join "`n")
exit 0
