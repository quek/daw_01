# PostToolUse hook (.claude/settings.json, matcher "Bash" / if "Bash(git commit*)")。
# git commit 成功後に release build を走らせ、「commit したら release が通る」
# ことを毎回保証する。 release は debug とは別 profile なので、 debug が
# 通っていても release で壊れるケースを commit のたびに検知できる
# (CLAUDE.md「ビルドと検証の区別」)。
#
# build が壊れていたら exit 2 で PostToolUse を block し、 cargo の error 出力を
# Claude に渡して即修正させる。 build は cwd (= commit が起きた repo / worktree)
# で行うため、 worktree セッションでも正しいツリーをビルドする。

cargo build --workspace --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "release build FAILED after commit (cargo exit code $LASTEXITCODE). Fix the release build before continuing."
    exit 2
}
