#!/usr/bin/env python3

# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later

"""自作ファイルの先頭に SPDX ライセンスヘッダを入れる (r.md #60、REUSE Specification 3.3)。

なぜ 1 ファイルずつ書くのか
---------------------------
README や LICENSE に 1 度だけ「このプロジェクトは GPL」と書いても、**ファイルが別の
プロジェクトへコピーされた瞬間にライセンスの痕跡が消える**。GNU の gpl-howto が
per-file notice を求める理由がこれで、REUSE Specification はその notice を機械可読な
2 行 (SPDX-FileCopyrightText / SPDX-License-Identifier) に圧縮したもの。

  https://www.gnu.org/licenses/gpl-howto.html  ("Why license notices?")
  https://reuse.software/spec-3.3/

第三者の著作物には**付けない**
------------------------------
vendored ヘッダ (ARA / Signalsmith) や、そこから生成した bindgen 出力に GPL ヘッダを
足すと「他人のコードを GPL と誤表示」する事故になる。そういうファイルの帰属は
ルートの `REUSE.toml` が宣言していて、本スクリプトは REUSE.toml が触れている path を
**SSoT として読み取り**、対象から外す (除外リストを 2 箇所に書かない)。

使い方
------
    python scripts/add_spdx_headers.py            # 不足しているファイルにヘッダを入れる
    python scripts/add_spdx_headers.py --check    # 入れずに不足を列挙 (不足あれば exit 1)
    python scripts/add_spdx_headers.py --dry-run  # 何を書き換えるかだけ表示

対象は `git ls-files` (= 追跡ファイル)。.gitignore 済みのもの (third_party/ 等) は
REUSE Specification でも Covered File ではないので触らない。冪等 (既に SPDX-License-
Identifier を持つファイルは飛ばす)。改行コードはファイルごとに保存する。
"""

from __future__ import annotations

import argparse
import fnmatch
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Windows の既定コンソール encoding (cp932 等) だと、下の日本語メッセージが
# UnicodeEncodeError で落ちて「検査が失敗した」ように見える。表示を UTF-8 に固定し、
# 出せない文字は置換して**検査結果の exit code を絶対に殺さない**。
for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):  # pragma: no cover - 非 TextIO へのリダイレクト時
        pass

ROOT = Path(__file__).resolve().parent.parent

COPYRIGHT = "Copyright (C) 2026 Tahara Yoshinori"
LICENSE_ID = "GPL-3.0-or-later"

# --- コメント様式 -----------------------------------------------------------
# REUSE ツール (reuse-tool src/reuse/comment.py) の CommentStyle と同じ割り当てに
# しておくと、`reuse lint` を入れた環境でも同じ結果になる。
SLASH = "slash"      # // ...
HASH = "hash"        # # ...
HTML = "html"        # <!-- ... -->

STYLE_BY_SUFFIX = {
    ".rs": SLASH,
    ".js": SLASH,
    ".wgsl": SLASH,
    ".c": SLASH,
    ".h": SLASH,
    ".cpp": SLASH,
    ".hpp": SLASH,
    ".py": HASH,
    ".sh": HASH,
    ".toml": HASH,
    ".cmake": HASH,
    ".yml": HASH,
    ".yaml": HASH,
    ".gitignore": HASH,
    ".worktreeinclude": HASH,
    ".md": HTML,
    ".markdown": HTML,
    ".html": HTML,
    ".xml": HTML,
    ".svg": HTML,
}

# 既にヘッダを持つか判定する。**行頭**にタグがあることを要求するのが肝で、
# そうしないと本スクリプト自身のようにタグを文字列として含むソースを
# 「もう付いている」と誤判定して永久に飛ばしてしまう。
HAS_TAG_RE = re.compile(r"^\s*(?://+|#+|<!--)?\s*SPDX-License-Identifier:", re.MULTILINE)

STYLE_BY_NAME = {
    "Makefile": HASH,
    ".gitignore": HASH,
    ".worktreeinclude": HASH,
}

# ヘッダを書けない / 書くべきでない拡張子 (帰属は REUSE.toml が宣言する)。
NO_COMMENT_SUFFIXES = {".json", ".mp4", ".png", ".jpg", ".ico", ".wav", ".lock"}

# REUSE Specification が Covered File から外すもの (spec 3.3 "Covered Files")。
# ルートの LICENSE / COPYING、LICENSES/ 配下、*.license、REUSE.toml 自身。
IGNORED_NAME_RE = re.compile(r"^(LICEN[CS]E([-.].*)?|COPYING([-.].*)?|REUSE\.toml|.*\.license)$")


def tracked_files() -> list[Path]:
    out = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=ROOT, check=True, capture_output=True,
    ).stdout.decode("utf-8")
    return [Path(p) for p in out.split("\0") if p]


def reuse_annotated_globs() -> list[str]:
    """REUSE.toml が帰属を宣言している path glob。ここが除外リストの SSoT。"""
    data = tomllib.loads((ROOT / "REUSE.toml").read_text(encoding="utf-8"))
    globs: list[str] = []
    for ann in data.get("annotations", []):
        p = ann["path"]
        globs.extend([p] if isinstance(p, str) else p)
    return globs


def matches_reuse_glob(rel: str, globs: list[str]) -> bool:
    for g in globs:
        # REUSE の glob: `**` はパス区切りを含めて一致、`*` は区切り以外。
        # fnmatch は `*` も区切りを跨ぐので、`**` を含まない glob だけ厳密化する。
        if "**" in g:
            if fnmatch.fnmatchcase(rel, g.replace("**", "*")):
                return True
        elif fnmatch.fnmatchcase(rel, g) and rel.count("/") == g.count("/"):
            return True
    return False


def style_for(rel: Path) -> str | None:
    if rel.name in STYLE_BY_NAME:
        return STYLE_BY_NAME[rel.name]
    return STYLE_BY_SUFFIX.get(rel.suffix)


def header_lines(style: str) -> list[str]:
    body = [f"SPDX-FileCopyrightText: {COPYRIGHT}", f"SPDX-License-Identifier: {LICENSE_ID}"]
    if style == SLASH:
        return [f"// {b}" for b in body]
    if style == HASH:
        return [f"# {b}" for b in body]
    return ["<!--"] + [f"{b}" for b in body] + ["-->"]


def insertion_index(lines: list[str], style: str, suffix: str) -> int:
    """ヘッダを差し込む行番号。先頭に置けないもの (shebang / XML 宣言 /
    DOCTYPE / YAML front matter) の**直後**へ回す。"""
    if not lines:
        return 0
    first = lines[0].lstrip("﻿")
    if first.startswith("#!"):
        return 1
    if first.lower().startswith("<?xml"):
        return 1
    if first.lower().startswith("<!doctype"):
        return 1
    # YAML front matter (.claude/skills/*/SKILL.md)。先頭より前にコメントを置くと
    # front matter として解釈されなくなり skill が壊れるので閉じ `---` の後に入れる。
    if suffix in (".md", ".markdown") and first.strip() == "---":
        for i in range(1, min(len(lines), 60)):
            if lines[i].strip() == "---":
                return i + 1
    return 0


def eol_of(text: str) -> str:
    return "\r\n" if text.count("\r\n") * 2 >= text.count("\n") and "\r\n" in text else "\n"


def process(path: Path, globs: list[str], write: bool) -> str | None:
    """ヘッダを足したら理由文字列を返す。対象外 / 既に有りなら None。"""
    rel = path.as_posix()
    if IGNORED_NAME_RE.match(path.name) or rel.startswith("LICENSES/"):
        return None
    if path.suffix in NO_COMMENT_SUFFIXES:
        return None
    if matches_reuse_glob(rel, globs):
        return None
    style = style_for(path)
    if style is None:
        return f"UNKNOWN-STYLE {rel}"

    abs_path = ROOT / path
    raw = abs_path.read_bytes()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return f"NOT-UTF8 {rel}"
    if HAS_TAG_RE.search(text[:4096]):
        return None

    eol = eol_of(text)
    lines = text.split("\n")
    # split("\n") で残る \r を落として行単位に正規化し、書き戻しで eol を復元する。
    lines = [ln[:-1] if ln.endswith("\r") else ln for ln in lines]
    idx = insertion_index(lines, style, path.suffix)
    block = header_lines(style)
    # 差し込み位置の直後が空行でなければ 1 行空けて既存の内容と分ける。
    if idx < len(lines) and lines[idx].strip():
        block = block + [""]
    if idx > 0 and lines[idx - 1].strip():
        block = [""] + block
    new_lines = lines[:idx] + block + lines[idx:]
    if write:
        abs_path.write_bytes(eol.join(new_lines).encode("utf-8"))
    return f"+ {rel}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="書き換えず、不足を列挙して exit 1")
    ap.add_argument("--dry-run", action="store_true", help="書き換えず、対象を列挙 (exit 0)")
    args = ap.parse_args()

    globs = reuse_annotated_globs()
    write = not (args.check or args.dry_run)
    problems: list[str] = []
    for path in tracked_files():
        if not (ROOT / path).is_file():
            continue
        r = process(path, globs, write)
        if r:
            problems.append(r)

    if not problems:
        print("SPDX headers: all tracked files are covered.")
        return 0
    for p in problems:
        print(p)
    if args.check:
        print(f"\n{len(problems)} file(s) without an SPDX header.", file=sys.stderr)
        print("Run: python scripts/add_spdx_headers.py", file=sys.stderr)
        return 1
    print(f"\n{len(problems)} file(s) {'would be' if args.dry_run else ''} updated.")
    # UNKNOWN-STYLE / NOT-UTF8 は書き込めていないので失敗として扱う。
    return 1 if any(p.startswith(("UNKNOWN-STYLE", "NOT-UTF8")) for p in problems) else 0


if __name__ == "__main__":
    sys.exit(main())
