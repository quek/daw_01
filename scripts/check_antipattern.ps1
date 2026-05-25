# PreToolUse hook: Edit / Write / MultiEdit の対象 string に CLAUDE.md
# principle 違反の anti-pattern キーワードが含まれていたら警告を emit する。
#
# CLAUDE.md 冒頭:
#   理想とベストプラクティスを追求する。 そのためは大胆に破壊して作り直す。
#
# 「妥協を選択肢に上げない」 が principle。 「実測してから妥協 OK」 ではない。
# 警告のみ。 block はしない (= exit 0)。 stdout に書いた warning は
# Claude Code の hook 仕組みで system reminder として次の Assistant
# turn に注入される。
#
# stdin: Claude Code から JSON
#   { session_id, tool_name, tool_input: { file_path, new_string?, content?, edits? } }

$ErrorActionPreference = 'SilentlyContinue'

# PowerShell 5.1 のデフォルト stdin / stdout encoding は OEM code page
# (= 日本語 Windows なら Shift-JIS)。 Claude Code は JSON を UTF-8 で
# 送り、 hook stdout も UTF-8 として読む。 両方を UTF-8 へ切り替える。
# これをしないと日本語キーワード (実装コスト / 妥協 / 許容範囲 等) が
# mojibake して regex に当たらない / 警告メッセージが化ける。
[Console]::InputEncoding = [System.Text.Encoding]::UTF8
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$input_str = [Console]::In.ReadToEnd()
if (-not $input_str) { exit 0 }

try {
    $data = $input_str | ConvertFrom-Json
} catch {
    exit 0
}

$tool_name = $data.tool_name
if (-not ($tool_name -in @("Edit", "Write", "MultiEdit"))) {
    exit 0
}

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

# Anti-pattern キーワード = 「妥協を選択肢に上げる」 思考の smell
$patterns = @(
    @{ name = "実装コスト";     pattern = "実装コスト";         reason = "コストを選択肢比較に持ち込んでいる。 理想だけ問う" }
    @{ name = "影響範囲が広い"; pattern = "影響範囲(が広い|が大きい)"; reason = "破壊を恐れている。 大胆に破壊して作り直す" }
    @{ name = "boilerplate +";  pattern = "boilerplate\s*\+\s*[0-9]+"; reason = "caller に負担転嫁する設計。 API 側で吸収する" }
    @{ name = "許容範囲";       pattern = "許容範囲";           reason = "妥協の婉曲表現。 妥協は選択肢に上げない" }
    @{ name = "現実的に";       pattern = "現実的に(は|考えると)?"; reason = "principle 放棄の婉曲表現。 理想だけ追求する" }
    @{ name = "妥協";           pattern = "妥協(?!なし)";       reason = "そもそも選択肢に上げない" }
    @{ name = "連鎖する";       pattern = "連鎖(する|してしまう)?"; reason = "破壊を恐れる思考。 連鎖を恐れず破壊する" }
)

$hits = @()
foreach ($p in $patterns) {
    if ($text -match $p.pattern) {
        $hits += $p
    }
}

if ($hits.Count -eq 0) { exit 0 }

$lines = @()
$lines += "[CLAUDE.md anti-pattern check]"
$lines += ""
$lines += "編集対象に principle 違反の smell キーワードが含まれています:"
$lines += ""
foreach ($h in $hits) {
    $lines += ("  - " + $h.name + " : " + $h.reason)
}
$lines += ""
$lines += "CLAUDE.md 冒頭:"
$lines += "  > 理想とベストプラクティスを追求する。 そのためは大胆に破壊して作り直す。"
$lines += ""
$lines += "問うべきこと:"
$lines += "  1. どれが理想か?"
$lines += "  2. 理想を実現するには何を破壊する必要があるか?"
$lines += ""
$lines += "問うてはいけないこと: 実装コスト / 影響範囲 / caller boilerplate / 現実的"
$lines += ""
$lines += "(block はしない。 思考の中断点として作用。 詳細: ~/.claude/projects/F--dev-daw-01/memory/feedback_pursue_ideal_only.md)"

$msg = $lines -join "`n"
Write-Output $msg
exit 0
