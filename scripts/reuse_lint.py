#!/usr/bin/env python3
"""REUSE Specification 3.3 の適合検査 (stdlib のみ、cross-platform、外部ツール不要)。

なぜ自前で書くのか
------------------
公式ツール `reuse lint` (pipx install reuse) が入っていない環境でも、`make license-check`
が**必ず**同じ不変条件を守れるようにするため。入っていない環境で検査を skip すると
「緑に見えるが実は表示が壊れている」= false green になり、これは大原則違反。
公式ツールが入っていれば `make license-check` はそれも追加で走らせる (両方 green が正)。

検査する項目 (reuse-tool の `ReportStatus.is_compliant` と同じ 9 カテゴリのうち、
SPDX ライセンス一覧の取得を要しないものすべて):

  1. files_without_copyright  … 著作権表示が無い Covered File
  2. files_without_licenses   … ライセンス表示が無い Covered File
  3. missing_licenses         … 参照されているが LICENSES/ に無いライセンス
  4. unused_licenses          … LICENSES/ にあるが誰からも参照されないライセンス
                                (spec: "MUST NOT include License Files for licenses
                                 under which none of the files in the Project are licensed")
  5. licenses_without_extension … LICENSES/<id> に拡張子が無い
  6. extra_files_in_licenses  … LICENSES/ に全文以外のものが入っている
  7. deprecated_licenses      … SPDX が deprecate した書き方 (GPL-3.0+ 等)
  8. read_errors              … UTF-8 として読めない
  9. untracked_covered_files  … git 追跡外だが .gitignore もされていないファイル
                                (公式 reuse は os.walk するのでこれも Covered File になる)

  https://reuse.software/spec-3.3/
  https://github.com/fsfe/reuse-tool  (src/reuse/report.py の is_compliant)

per-file ヘッダ廃止に伴う追加検査 (2026-08-22)
----------------------------------------------
著作権表示は `REUSE.toml` の一括宣言 (`path = "**"`) 1 箇所に集約し、ファイル先頭の
SPDX ヘッダは全部撤去した。これで「新規ファイルにヘッダを入れ忘れる」経路は消えたが、
代わりに **一括宣言が第三者のファイルまで巻き込んで GPL と誤表示する**という別の穴が開く。
ヘッダ投入スクリプトを消して検査が緩くならないよう、その穴を塞ぐ検査を足してある:

  10. blanket_declaration     … `path = "**"` の一括宣言が存在し、かつ **先頭**にあるか
                                (spec は「最後にマッチした table を使う」ので、一括宣言が
                                 後ろにあると第三者の個別宣言を上書きしてしまう)
  11. undeclared_third_party  … 自分以外の著作権表示を持つファイルなのに、個別宣言
                                (一括宣言以外の [[annotations]]) で覆われていない
  12. undeclared_vendor_path  … `vendor/` / `third_party/` 配下なのに個別宣言が無い

使い方:  python scripts/reuse_lint.py   (違反あれば exit 1)
"""

from __future__ import annotations

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
LICENSES_DIR = ROOT / "LICENSES"

# Covered File から外れるもの (spec 3.3 "Covered Files")。
IGNORED_NAME_RE = re.compile(r"^(LICEN[CS]E([-.].*)?|COPYING([-.].*)?|REUSE\.toml|.*\.license)$")
IGNORED_DIRS = {"LICENSES", ".git", ".hg", ".sl", ".reuse"}

# ヘッダを探す範囲。reuse-tool も先頭のみを見る。
HEADER_BYTES = 4096

# SPDX が deprecate した識別子の書き方 (よく間違える形だけ列挙)。
DEPRECATED_IDS = {
    "GPL-3.0", "GPL-3.0+", "GPL-2.0", "GPL-2.0+",
    "LGPL-3.0", "LGPL-3.0+", "LGPL-2.1", "LGPL-2.1+",
    "AGPL-3.0", "AGPL-1.0", "BSD-2-Clause-FreeBSD", "Nunit",
    "wxWindows", "eCos-2.0", "GFDL-1.3", "StandardML-NJ",
}

SPDX_OPERATORS = {"AND", "OR", "WITH", "and", "or", "with"}
COPYRIGHT_RE = re.compile(r"SPDX-FileCopyrightText:|Copyright\s|©|\(c\)", re.IGNORECASE)

# --- per-file ヘッダ廃止後の不変条件 (REUSE.toml が唯一の帰属宣言) ----------
BLANKET_GLOB = "**"
COPYRIGHT_HOLDER = "Tahara Yoshinori"
COPYRIGHT_HOLDER_LINE = f"Copyright (C) 2026 {COPYRIGHT_HOLDER}"
PROJECT_LICENSE = "GPL-3.0-or-later"
VENDOR_DIRS = {"vendor", "third_party", "3rdparty", "external"}

# ファイル先頭に「自分以外の著作権表示」があるかを見るための行パターン。
# 行頭 (コメント接頭辞は許す) の Copyright 行だけを拾う。散文中の「Copyright」や
# ライセンス全文の引用を誤検出しないよう、**先頭 40 行**に限る。
COPYRIGHT_LINE_RE = re.compile(
    r"^[ \t]*(?://+|#+|/\*+|\*|<!--|;+|--)?[ \t]*"
    r"(?:SPDX-FileCopyrightText:[ \t]*)?"
    r"(?:Copyright|\(c\)|©)[ \t]*(?:\(c\)|©)?[ \t]*(?P<rest>.+)$",
    re.IGNORECASE,
)
THIRD_PARTY_SCAN_LINES = 40
# **行頭**のタグだけを拾う。タグを文字列リテラルとして含むソース (本スクリプト自身)
# の regex / f-string を、ライセンス宣言と誤読しないため。
LICENSE_TAG_RE = re.compile(
    r"^\s*(?://+|#+|<!--)?\s*SPDX-License-Identifier:\s*(.+?)\s*(?:-->)?$", re.MULTILINE
)


def run_git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, check=True, capture_output=True
    ).stdout.decode("utf-8")


def tracked_files() -> list[Path]:
    return [Path(p) for p in run_git("ls-files", "-z").split("\0") if p]


def untracked_unignored() -> list[str]:
    return [p for p in run_git("ls-files", "-z", "--others", "--exclude-standard").split("\0") if p]


def is_covered(rel: Path) -> bool:
    if IGNORED_NAME_RE.match(rel.name):
        return False
    return not any(part in IGNORED_DIRS for part in rel.parts[:-1])


def ids_in_expression(expr: str) -> list[str]:
    """SPDX License Expression から識別子だけを取り出す。"""
    tokens = re.split(r"[\s()]+", expr.strip())
    return [t for t in tokens if t and t not in SPDX_OPERATORS]


def reuse_toml_annotations() -> list[dict]:
    data = tomllib.loads((ROOT / "REUSE.toml").read_text(encoding="utf-8"))
    if data.get("version") != 1:
        raise SystemExit("REUSE.toml: version は 1 でなければならない (spec 3.3)")
    return data.get("annotations", [])


def annotation_matches(rel: str, ann: dict) -> bool:
    paths = ann["path"]
    for g in [paths] if isinstance(paths, str) else paths:
        if "**" in g:
            if fnmatch.fnmatchcase(rel, g.replace("**", "*")):
                return True
        elif fnmatch.fnmatchcase(rel, g) and rel.count("/") == g.count("/"):
            return True
    return False


def main() -> int:
    problems: dict[str, list[str]] = {
        "files_without_copyright": [],
        "files_without_licenses": [],
        "missing_licenses": [],
        "unused_licenses": [],
        "licenses_without_extension": [],
        "extra_files_in_licenses": [],
        "deprecated_licenses": [],
        "read_errors": [],
        "untracked_covered_files": [],
        "blanket_declaration": [],
        "undeclared_third_party": [],
        "undeclared_vendor_path": [],
    }

    annotations = reuse_toml_annotations()

    # --- 10. 一括宣言が存在し、かつ先頭にあること ---------------------------
    # spec 3.3: "If a Covered File is covered by multiple [[annotations]] tables in the
    # same REUSE.toml file, then exclusively the last matching table in the file is used."
    # → 一括宣言が後ろにあると第三者コードの個別宣言を上書きして GPL と誤表示する。
    blanket_idx = next(
        (i for i, a in enumerate(annotations) if BLANKET_GLOB in ([a["path"]] if isinstance(a["path"], str) else a["path"])),
        None,
    )
    if blanket_idx is None:
        problems["blanket_declaration"].append(
            f'path = "{BLANKET_GLOB}" の一括宣言が REUSE.toml に無い'
            " (per-file ヘッダを撤去したので、これが唯一の帰属宣言)"
        )
    elif blanket_idx != 0:
        problems["blanket_declaration"].append(
            f"一括宣言が {blanket_idx} 番目にある。**先頭**でなければ後続の第三者宣言を"
            "上書きしてしまう (最後にマッチした table が勝つ)"
        )
    else:
        b = annotations[0]
        if b.get("SPDX-FileCopyrightText") != COPYRIGHT_HOLDER_LINE:
            problems["blanket_declaration"].append(
                f"一括宣言の SPDX-FileCopyrightText が想定と違う: {b.get('SPDX-FileCopyrightText')!r}"
            )
        if b.get("SPDX-License-Identifier") != PROJECT_LICENSE:
            problems["blanket_declaration"].append(
                f"一括宣言の SPDX-License-Identifier が想定と違う: {b.get('SPDX-License-Identifier')!r}"
            )

    def specific_annotation(posix: str) -> dict | None:
        """一括宣言**以外**でこのファイルを覆う宣言。第三者コードの判定に使う。"""
        for i, a in enumerate(reversed(annotations)):
            if blanket_idx is not None and len(annotations) - 1 - i == blanket_idx:
                continue
            if annotation_matches(posix, a):
                return a
        return None

    referenced: set[str] = set()

    for rel in tracked_files():
        if not (ROOT / rel).is_file() or not is_covered(rel):
            continue
        posix = rel.as_posix()
        ann = next((a for a in reversed(annotations) if annotation_matches(posix, a)), None)
        specific = specific_annotation(posix)

        # --- 12. vendor / third_party 配下は必ず個別宣言で覆う ---------------
        # 一括宣言に任せると「他人のコードを GPL と誤表示」になる。
        if specific is None and any(p in VENDOR_DIRS for p in rel.parts[:-1]):
            problems["undeclared_vendor_path"].append(posix)

        # 全体を UTF-8 で読めるかでテキスト / バイナリを判定する (先頭 N バイトだけを
        # 切ると多バイト文字の途中で切れて偽の decode エラーになる)。ヘッダ探索自体は
        # 先頭 HEADER_BYTES 文字のみ = reuse-tool と同じ範囲。
        head = ""
        try:
            head = (ROOT / rel).read_text(encoding="utf-8")[:HEADER_BYTES]
        except UnicodeDecodeError:
            # バイナリは REUSE.toml が帰属を宣言していれば適合。
            if ann is None:
                problems["read_errors"].append(posix)
                continue

        has_copyright = bool(COPYRIGHT_RE.search(head)) or (ann and "SPDX-FileCopyrightText" in ann)
        tag = LICENSE_TAG_RE.search(head)
        expr = tag.group(1).strip() if tag else None
        if ann and "SPDX-License-Identifier" in ann and (expr is None or ann.get("precedence") == "override"):
            a = ann["SPDX-License-Identifier"]
            expr = a if isinstance(a, str) else " AND ".join(a)

        if not has_copyright:
            problems["files_without_copyright"].append(posix)
        if not expr:
            problems["files_without_licenses"].append(posix)
        else:
            referenced.update(ids_in_expression(expr))

        # --- 11. 自分以外の著作権表示を持つのに個別宣言が無い -----------------
        # per-file ヘッダを撤去した以上、「新しく持ち込んだ第三者ファイルが
        # 一括宣言に飲まれて GPL と誤表示される」のが唯一の現実的な事故経路。
        # ファイル自身が名乗っている著作権者を見て、自分でなければ個別宣言を要求する。
        if specific is None:
            for line in head.splitlines()[:THIRD_PARTY_SCAN_LINES]:
                m = COPYRIGHT_LINE_RE.match(line)
                if not m:
                    continue
                rest = m.group("rest")
                if COPYRIGHT_HOLDER in rest:
                    continue
                # 年だけの行や「Copyright notice」のような散文は無視する。
                if not re.search(r"[A-Za-z]{3}", re.sub(r"\b(19|20)\d{2}\b", "", rest)):
                    continue
                problems["undeclared_third_party"].append(f"{posix}: {line.strip()[:100]}")
                break

    # LICENSES/ ディレクトリの検査。
    if not LICENSES_DIR.is_dir():
        problems["missing_licenses"].append("LICENSES/ ディレクトリが無い")
    else:
        present: set[str] = set()
        for f in sorted(LICENSES_DIR.iterdir()):
            if not f.is_file():
                problems["extra_files_in_licenses"].append(f.name)
                continue
            if not f.suffix:
                problems["licenses_without_extension"].append(f.name)
                continue
            present.add(f.stem)
        for ident in sorted(referenced):
            if ident in DEPRECATED_IDS:
                problems["deprecated_licenses"].append(ident)
            if ident not in present:
                problems["missing_licenses"].append(ident)
        for ident in sorted(present - referenced):
            problems["unused_licenses"].append(ident)

    problems["untracked_covered_files"] = [
        p for p in untracked_unignored()
        if is_covered(Path(p)) and not p.startswith("LICENSES/")
    ]

    total = sum(len(v) for v in problems.values())
    if total == 0:
        print("reuse-lint: compliant (REUSE Specification 3.3)")
        return 0

    for key, values in problems.items():
        if not values:
            continue
        print(f"\n# {key} ({len(values)})")
        for v in values[:40]:
            print(f"  {v}")
        if len(values) > 40:
            print(f"  ... and {len(values) - 40} more")
    print(f"\nreuse-lint: NOT compliant — {total} issue(s).", file=sys.stderr)
    print("帰属の宣言は REUSE.toml 1 箇所です (per-file ヘッダは置きません)。", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
