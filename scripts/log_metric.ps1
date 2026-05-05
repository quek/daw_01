# PostToolUse hook: 毎 tool call で 1 行 jsonl を追記する
# stdin: Claude Code から JSON (session_id / tool_name / tool_input / tool_response)
# 出力先: %USERPROFILE%\.claude\projects\F--dev-daw-01\metrics\YYYY-MM.jsonl
# 副作用は追記のみ。失敗時は silent exit 0 (commit / tool 実行をブロックしない)。

$ErrorActionPreference = 'SilentlyContinue'

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }

try {
    $data = $input_str | ConvertFrom-Json
} catch {
    exit 0
}

$session_id = $data.session_id
$tool_name  = $data.tool_name
$tool_input = $data.tool_input
$tool_response = $data.tool_response

# matcher: tool 別に「何を対象にしたか」を 1 文字列で要約
$matcher = ""
if ($tool_name -eq "Bash" -and $tool_input.command) {
    $first_line = ($tool_input.command -split "`n" | Select-Object -First 1).Trim()
    $tokens = $first_line -split "\s+"
    $take = [Math]::Min(2, $tokens.Count)
    $matcher = ($tokens[0..($take-1)] -join " ")
} elseif ($tool_input.file_path) {
    $matcher = $tool_input.file_path
} elseif ($tool_input.pattern) {
    $p = [string]$tool_input.pattern
    $matcher = $p.Substring(0, [Math]::Min(40, $p.Length))
}

# status: tool_response.is_error が真なら "error"
$status = "ok"
if ($tool_response -and $tool_response.is_error) {
    $status = "error"
}

$ts = Get-Date -Format "yyyy-MM-ddTHH:mm:ss"
$entry = [PSCustomObject]@{
    ts      = $ts
    session = $session_id
    tool    = $tool_name
    matcher = $matcher
    status  = $status
} | ConvertTo-Json -Compress

$logdir = Join-Path $env:USERPROFILE ".claude\projects\F--dev-daw-01\metrics"
if (-not (Test-Path $logdir)) {
    New-Item -ItemType Directory -Path $logdir -Force | Out-Null
}
$logfile = Join-Path $logdir "$(Get-Date -Format yyyy-MM).jsonl"
Add-Content -Path $logfile -Value $entry -Encoding utf8

exit 0
