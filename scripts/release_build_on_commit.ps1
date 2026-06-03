# PostToolUse hook (.claude/settings.json, matcher "Bash" / if "Bash(git *)")。
# git commit 成功後に release build を走らせ、「commit したら release が通る」
# ことを毎回保証する。release は debug とは別 profile なので、debug が通って
# いても release で壊れるケースを commit のたびに検知できる
# (CLAUDE.md「ビルドと検証の区別」)。
#
# 【commit 判定はこのスクリプト側で行う】
# settings.json の if は "Bash(git *)" で git 系コマンド全般に絞る pre-filter。
# Claude Code の if/matcher はコマンド前方一致で `-C` 等のフラグを正規化しない
# ため (`Bash(git commit*)` は `git -C <path> commit` に不一致 = 旧フックが
# git -C / worktree 起動で発火しなかった原因)、実際の commit 判定は stdin の
# tool_input.command を読んでここで行う (= 起動形式に非依存の single gate)。
# git commit でなければ即 exit 0 で no-op (非 commit の git コマンドでは何もしない)。
#
# build が壊れていたら exit 2 で PostToolUse を block し、cargo の error 出力を
# Claude に渡して即修正させる。build は cwd (= commit が起きた repo / worktree)
# で行うため、worktree セッションでも正しいツリーをビルドする。

$raw = [Console]::In.ReadToEnd()
$cmd = ''
try { $cmd = ($raw | ConvertFrom-Json).tool_input.command } catch { }

# git commit を検出: 素の `git commit` / `git -C <path> commit` / `git -c k=v commit`
# / 先頭 VAR=val を許容。flag は "-x value" の形のみ吸収し、その後に subcommand
# commit が来る形だけにマッチさせ、`git log ... commit` や `git commit-tree` は除外。
$commitRx = '(?:^|[&|;]\s*)(?:\w+=\S*\s+)*git\s+(?:-\S+\s+\S+\s+)*commit(?:\s|$)'
if ($cmd -notmatch $commitRx) {
    exit 0
}

cargo build --workspace --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "release build FAILED after commit (cargo exit code $LASTEXITCODE). Fix the release build before continuing."
    exit 2
}
