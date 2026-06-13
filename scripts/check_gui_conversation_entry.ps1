# PreToolUse hook: warn when a new top-level entry written to
# docs/gui_01_conversation.md does not match the required template.
#
# Recurring friction (2026-06-13): a request entry was added as
#   "## [type] title ... date"
# instead of the required
#   "## #NNN [Open] YYYY-MM-DD [type] one-line title"  +  "### daw_01 ->" block.
# The user corrected this twice ("2nd time"). This hook catches it up front.
#
# Warning only (exit 0). ASCII-only source so PowerShell 5.1 (no-BOM UTF-8)
# never misreads a non-ASCII string literal (see memory
# feedback_powershell_ascii_hooks). The console is switched to UTF-8 so the
# Japanese body of the edited text is line-split correctly; the regex only
# matches the ASCII heading prefix.
#
# stdin: { tool_name, tool_input: { file_path, new_string?, content?, edits? } }

$ErrorActionPreference = 'SilentlyContinue'
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }
try { $data = $input_str | ConvertFrom-Json } catch { exit 0 }

$tool_name = $data.tool_name
if (-not ($tool_name -in @("Edit", "Write", "MultiEdit"))) { exit 0 }

$path = [string]$data.tool_input.file_path
if (-not ($path -match 'gui_01_conversation\.md$')) { exit 0 }

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

# Valid entry heading: "## #<num> [<status>] YYYY-MM-DD [" (type bracket follows).
$template = '^## #\d+ \[(Open|Replied|Resolved|Withdrawn)\] \d{4}-\d{2}-\d{2} \['
$bad = @()
foreach ($line in ($text -split "`n")) {
    # A "## " heading that carries a "[" bracket is meant to be an entry; if it
    # does not match the template it is malformed. ("### ..." replies are 3
    # hashes and never match "^## ", so gui_01 replies are not flagged.)
    if (($line -match '^## ') -and ($line -match '\[') -and (-not ($line -match $template))) {
        $bad += $line.TrimEnd()
    }
}
if ($bad.Count -eq 0) { exit 0 }

$lines = @()
$lines += "[gui_01_conversation.md entry format check]"
$lines += ""
$lines += "A new entry heading does not match the required template:"
foreach ($b in $bad) { $lines += ("    " + $b) }
$lines += ""
$lines += "Required template (see the operation rules at the top of the file):"
$lines += "    ## #NNN [Open] YYYY-MM-DD [type] one-line title"
$lines += "        ### daw_01 ->   (type / related files / body)"
$lines += "        ### gui_01 ->   (empty, for gui_01 to fill)"
$lines += "        ---"
$lines += ""
$lines += "Number is sequential; status starts at [Open]; type is one of"
$lines += "[request]/[bug]/[question]/[consult] (the Japanese tag is fine)."
$lines += "(warning only; does not block.)"

Write-Output ($lines -join "`n")
exit 0
