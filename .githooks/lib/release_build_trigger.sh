#!/bin/sh
# post-commit / post-merge 共通: main ブランチで release build を detached 起動する。
#
# なぜ helper に切り出すか: git は `git commit` で post-commit を、`git merge` /
# `git pull` で post-merge を発火する (git 2.50 で実測。merge は post-commit を一切
# 呼ばない)。両方からこの 1 箇所を呼ぶことで、main にコードが載る全経路 (直接 commit /
# fast-forward / no-ff merge / conflict 解決 commit / pull) が必ず release build される
# (= CLAUDE.md「commit したら必ず release build」の SSoT)。
#
# 実ビルドは scripts/release_build_bg.ps1 に detached (Start-Process -Hidden) で投げ、
# commit / merge は即座に戻る (非ブロッキング)。結果は target/release-build.log、失敗時は
# target/.release-build-failed marker + ダイアログ。

# rebase / am の最中は replay される commit ごとに post-commit が走り build storm に
# なるので skip する。merge は単発なので skip しない (= FIXME #66 follow-up の主眼)。
for d in rebase-merge rebase-apply; do
  p="$(git rev-parse --git-path "$d" 2>/dev/null)"
  if [ -n "$p" ] && [ -d "$p" ]; then exit 0; fi
done

# release build は main でのみ走らせる (FIXME #65)。worktree の feature ブランチは skip し、
# release 検証は main への統合時に一度だけ行う。
branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
if [ "$branch" != "main" ]; then exit 0; fi

# Start-Process は即時に戻る -> hidden な独立プロセスで release_build_bg.ps1 が走る。
repo="$(git rev-parse --show-toplevel)"
powershell -NoProfile -ExecutionPolicy Bypass -Command \
  "Start-Process -WindowStyle Hidden -FilePath powershell -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','$repo/scripts/release_build_bg.ps1','-Repo','$repo'" \
  >/dev/null 2>&1

exit 0
