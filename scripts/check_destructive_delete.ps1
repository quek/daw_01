# PreToolUse hook (Bash | PowerShell): BLOCK a recursive/force file deletion
# whose target is a variable/env reference or a filesystem root -- i.e. a path
# whose runtime value the hook cannot verify, which is exactly how a wrong-tree
# wipe happens (2026-06-13: Remove-Item -Recurse -Force on an unchecked $env:TEMP
# -derived path). See memory feedback_verify_env_var_before_use.
#
# Unlike the warn-only principle hooks (check_antipattern.ps1), this one BLOCKS:
# PreToolUse exit code 2 + stderr feeds the reason back to Claude and cancels the
# tool call, forcing "print the value / use a literal path" before any delete.
#
# Detection requires BOTH:
#   (1) a recursive/force delete  (rm -rf, Remove-Item -Recurse, rd /s, ...)
#   (2) a dangerous target in the same statement: a variable/env reference
#       ($var, $env:X, ${X}, %X%, $(...)) OR a root-ish token (/, \, ~, ., *,
#       a bare drive like C:\).
# A recursive delete of a concrete literal subpath (rm -rf target/debug) is
# allowed -- low false positives.
#
# stdin: Claude Code JSON { tool_name, tool_input: { command } }
# All SCRIPT literals ASCII-only (PS 5.1 no-BOM encoding); message is English.

$ErrorActionPreference = 'SilentlyContinue'
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }
try { $data = $input_str | ConvertFrom-Json } catch { exit 0 }

$cmd = [string]$data.tool_input.command
if (-not $cmd) { exit 0 }

# Split into statements so a variable printed on one line does not taint a delete
# on another (e.g. "echo $x; rm -rf target" must stay allowed).
$segments = [regex]::Split($cmd, '(?:\r?\n|;|&&|\|\|)')

# recursive / force delete indicators
$recurseRe = '(?i)(-Recurse\b|-rec\b|--recursive\b|\s-[a-z]*r[a-z]*f[a-z]*\b|\s-[a-z]*f[a-z]*r[a-z]*\b|\s-r\b|\s-R\b|/s\b)'
$delVerbRe = '(?i)(\bRemove-Item\b|\brmdir\b|\brd\b|\bdel\b|\bri\b|\brm\b)'
# dangerous target: a variable/env reference ...
$varRe = '(\$env:|\$\{?[A-Za-z_]|%[A-Za-z_][A-Za-z0-9_]*%|\$\()'
# ... or a root-ish token bounded by whitespace/quote/end
$rootRe = '(?i)(^|\s|=|"|'')(/|\\|~|\.|\*|[A-Za-z]:[\\/]?)(\s|$|"|'')'

$hit = $null
foreach ($s in $segments) {
    if ($s -notmatch $delVerbRe) { continue }
    if ($s -notmatch $recurseRe) { continue }
    if ($s -match $varRe -or $s -match $rootRe) { $hit = $s.Trim(); break }
}

if (-not $hit) { exit 0 }

# Single-quoted literals throughout: in PowerShell the escape char is the
# backtick, not the backslash, so a C-style \" inside a double-quoted string
# would terminate the string and break parsing. Single quotes keep $ and " literal.
$lines = @()
$lines += '[BLOCKED: recursive/force delete on an unverified variable or root path]'
$lines += ''
$lines += ('statement: ' + $hit)
$lines += ''
$lines += 'A recursive/force delete (rm -rf, Remove-Item -Recurse -Force, rd /s) is'
$lines += 'targeting a path that is a variable/env reference or a filesystem root.'
$lines += 'The hook cannot see the runtime value, so this is blocked to avoid wiping'
$lines += 'the wrong tree (incident 2026-06-13: unchecked env var -> near-root delete).'
$lines += ''
$lines += 'Do this instead:'
$lines += '  1. Print the resolved value first and confirm it is the dir you mean:'
$lines += '       Write-Output ("[{0}]" -f $target)'
$lines += '  2. Use a verified LITERAL absolute path as the delete target, OR'
$lines += '  3. For a single file: [System.IO.File]::Delete($verifiedAbsolutePath)'
$lines += '  4. Guard first: if (-not $p -or $p.Length -lt 5) { throw }'
$lines += ''
$lines += '(ref: ~/.claude/projects/F--dev-daw-01/memory/feedback_verify_env_var_before_use.md)'

[Console]::Error.WriteLine(($lines -join "`n"))
exit 2
