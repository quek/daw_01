# PreToolUse hook (UNIVERSAL): warn on ANY Edit/Write/MultiEdit to ANY
# file whose text contains an English "compromise smell" phrase.
#
# This is the language-complement of check_antipattern.ps1 (which scans
# Japanese compromise keywords). Together they are the UNIVERSAL guard:
# every file, every domain -- NOT limited to one feature.
#
# Why universal (2026-06-04): I first added a METER-SPECIFIC widget hook.
# The user: "limiting it to METER dB SCALE is no good -- pursue the ideal
# in EVERYTHING." A feature-specific guard is itself a compromise (the
# narrow / easy patch). The principle is universal, so the guard is too.
#
# Hard limit (be honest): a text hook only catches compromise words that
# land in an EDIT. It cannot see compromise reasoning that stays in chat.
# The only real guarantee is discipline: on EVERY decision, in EVERY
# domain, first ask "what is the ideal?" and never put compromise on the
# table. This hook is a backstop, not the fix.
#
# ASCII-ONLY (feedback_powershell_ascii_hooks): PS 5.1 mis-parses
# non-ASCII in no-BOM UTF-8 scripts. Keep this file ASCII. Warn only.
#
# stdin JSON: { tool_name, tool_input: { file_path, new_string?, content?, edits? } }

$ErrorActionPreference = 'SilentlyContinue'
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }
try { $data = $input_str | ConvertFrom-Json } catch { exit 0 }

$tool_name = $data.tool_name
if (-not ($tool_name -in @("Edit", "Write", "MultiEdit"))) { exit 0 }

$text = ""
if ($data.tool_input.new_string) {
    $text = [string]$data.tool_input.new_string
} elseif ($data.tool_input.content) {
    $text = [string]$data.tool_input.content
} elseif ($data.tool_input.edits) {
    $parts = @()
    foreach ($e in $data.tool_input.edits) {
        if ($e.new_string) { $parts += [string]$e.new_string }
    }
    $text = $parts -join "`n"
}
if (-not $text) { exit 0 }

# English compromise-smell phrases (case-insensitive). High-signal
# wording that means "I am choosing the cheap/easy/narrow path".
$patterns = @(
    @{ name = "low risk";        pattern = 'low[\s-]?risk' }
    @{ name = "pragmatic";       pattern = 'pragmatic' }
    @{ name = "for now";         pattern = 'for now' }
    @{ name = "interim";         pattern = 'interim' }
    @{ name = "good enough";     pattern = 'good enough' }
    @{ name = "quick fix/hack";  pattern = 'quick (fix|hack)|\bhacky?\b|workaround' }
    @{ name = "compromise";      pattern = 'compromise' }
    @{ name = "cheaper/effort";  pattern = 'cheap(er|est)?|least effort|less effort|minimal (effort|change)' }
    @{ name = "avoid round-trip";pattern = 'avoid (the |a )?(round[\s-]?trip|dependency|dep\b)' }
    @{ name = "simpler to just";  pattern = 'simpler to (just )?|easier to (just )?' }
)

$hits = @()
foreach ($p in $patterns) {
    if ($text -imatch $p.pattern) { $hits += $p.name }
}
if ($hits.Count -eq 0) { exit 0 }

$lines = @()
$lines += "[compromise-smell check : universal]"
$lines += ""
$lines += "This edit contains wording that signals a compromise / the easy-narrow path:"
$lines += ("  " + ($hits -join ", "))
$lines += ""
$lines += "CLAUDE.md top: pursue the ideal and best practices; boldly destroy and rebuild."
$lines += "NOT 'measure then settle' -- never put compromise on the table, in ANY domain."
$lines += ""
$lines += "Ask only: (1) what is the ideal?  (2) what must be destroyed to reach it?"
$lines += "Do NOT ask: which is cheaper / lower risk / less effort / avoids a dependency."
$lines += ""
$lines += "(warn only; not blocked. A hook cannot see compromises that stay in your"
$lines += " reasoning -- the real guard is doing the ideal every time. See"
$lines += " memory/feedback_pursue_ideal_only.md)"

Write-Output ($lines -join "`n")
exit 0
