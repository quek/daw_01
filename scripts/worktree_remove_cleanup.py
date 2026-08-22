#!/usr/bin/env python3
"""WorktreeRemove hook: Windows のファイルロックで消し残った worktree dir を回収する。

Claude Code が worktree を削除する (session 終了 / ExitWorktree / subagent 完了) とき、
default の `git worktree remove` は git 登録を外すが、Windows では rust-analyzer 等が
worktree 内の `target/` を掴んでいると **dir 本体を消せず孤立 dir が残る**
(2026-06-21 FIXME #80 で発生。手で taskkill rust-analyzer → rm して回収した)。

`WorktreeRemove` は non-blocking イベント (default 削除に *加えて* 走る) なので、ここで
孤立 dir が残っていたら rust-analyzer (= 状態を持たない respawnable な LSP) を落として
dir を消す。`make rm-worktree FORCE=1` (cleanup_worktree.sh の kill_holders) と同じ戦略を、
ハーネスの削除経路にも効かせる = 仕組化。

設計上の安全装置:
- **対象は `<repo_root>/.claude/worktrees/` 配下のみ**。main repo / 任意パスには絶対触らない。
- **dir が実在するときだけ kill する** (正常削除では no-op = rust-analyzer を無闇に殺さない)。
  ロックで本当に消し残ったケースだけ作用する。
- 落とすのは rust-analyzer / proc-macro-srv のみ。`daw_*` アプリは絶対に touch しない。
- **kill は Windows のみ** (`os.name == "nt"`)。Linux は open handle でも rm できるので、
  default 削除で既に消えており no-op になる。
- JSON parse が要るので Python (stdlib のみ、jq 不要)。policy [[feedback_no_powershell_cross_platform]]。
"""
import datetime
import json
import os
import shutil
import subprocess
import sys
import time


def log(repo_root: str, msg: str) -> None:
    line = f"[{datetime.datetime.now():%Y-%m-%d %H:%M:%S}] worktree_remove_cleanup: {msg}\n"
    try:
        target_dir = os.path.join(repo_root, "target")
        os.makedirs(target_dir, exist_ok=True)
        with open(os.path.join(target_dir, "worktree-cleanup.log"), "a", encoding="utf-8") as f:
            f.write(line)
    except OSError:
        pass
    # hook の stdout/stderr は debug log にのみ出る (transcript には出ない)。
    sys.stderr.write(line)


def _norm(p: str) -> str:
    return os.path.normcase(os.path.abspath(p)).rstrip("\\/")


def main() -> int:
    try:
        data = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        return 0
    if not isinstance(data, dict):
        return 0

    worktree_path = data.get("worktree_path") or ""
    repo_root = data.get("repo_root") or ""
    if not worktree_path or not repo_root:
        return 0

    # 安全ガード: <repo>/.claude/worktrees/ 配下のみ。main worktree / 任意パスは触らない。
    wtroot = _norm(os.path.join(repo_root, ".claude", "worktrees"))
    wt = _norm(worktree_path)
    if not (wt + os.sep).startswith(wtroot + os.sep):
        return 0

    # 正常に消えていれば何もしない (= rust-analyzer を無闇に kill しない)。
    if not os.path.exists(worktree_path):
        return 0

    log(repo_root, f"orphan worktree dir remains (locked): {worktree_path}")

    # Windows のみ: dir handle を掴む holder (respawnable な rust-analyzer) を落とす。
    if os.name == "nt":
        for image in ("rust-analyzer.exe", "rust-analyzer-proc-macro-srv.exe"):
            subprocess.run(
                ["taskkill", "/F", "/IM", image],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        time.sleep(1)

    # retry rm (handle 解放にラグがあるので数回)。
    for _ in range(3):
        if not os.path.exists(worktree_path):
            break
        shutil.rmtree(worktree_path, ignore_errors=True)
        if os.path.exists(worktree_path):
            time.sleep(1)

    # git 側の登録残骸を掃除 (default 削除前に走った場合の保険)。
    subprocess.run(
        ["git", "-C", repo_root, "worktree", "prune", "--expire=now"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )

    if os.path.exists(worktree_path):
        log(repo_root, f"STILL PRESENT after kill+retry (held by a non-rust-analyzer process?): {worktree_path}")
    else:
        log(repo_root, f"removed orphan dir: {worktree_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
