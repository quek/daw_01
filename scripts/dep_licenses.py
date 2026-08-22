#!/usr/bin/env python3
"""依存クレートのライセンス検査 + THIRD-PARTY-NOTICES.md の生成 (r.md #60)。

なぜ自前で書くのか
------------------
`cargo deny` / `cargo about` が入っていない環境でも `make license-check` が**必ず**同じ
不変条件を守れるようにするため。外部ツールが無いときに検査を skip すると「緑に見えるが
実は GPLv3 非互換のクレートが混入している」= false green になる。stdlib だけで完結させ、
外部ツールがあれば `make license-check` がそれも**追加で**走らせる。

許可リストの SSoT は `deny.toml` の `[licenses] allow`。cargo-deny と本スクリプトが同じ
1 つのリストを読むので、2 系統の許可リストが食い違う事故が起きない。

依存グラフの取り方 (一次情報: cargo metadata の JSON スキーマ)
------------------------------------------------------------
* `packages[].dependencies` は **Cargo.toml の宣言そのまま** で、`--filter-platform` を
  付けても他プラットフォーム専用の依存が残る。グラフには使わない。
* 正しいのは `resolve.nodes[].deps[].dep_kinds[].kind` (`null` = normal / `"build"` /
  `"dev"`)。`workspace_members` を起点に normal + build のエッジだけを BFS する
  (dev-dependencies は配布物に入らないので除外)。

使い方
------
    python scripts/dep_licenses.py --check      # 許可リスト違反 + NOTICES の陳腐化を検査
    python scripts/dep_licenses.py --write      # THIRD-PARTY-NOTICES.md を再生成
    python scripts/dep_licenses.py --write --embed-texts -o dist/THIRD-PARTY-FULL.md
                                                # 各クレートのライセンス全文まで埋め込む
                                                # (バイナリ配布物に同梱する版)

`--embed-texts` はビルド済み exe を配る段になって必要になるもの (MIT / BSD / ISC /
Zlib の「著作権表示と許諾文を複製物に含める」条項)。ソース公開だけなら一覧で足りる。
"""

from __future__ import annotations

import argparse
import hashlib
import json
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
DEFAULT_OUT = ROOT / "THIRD-PARTY-NOTICES.md"
# 配布している 3 つの exe のターゲット。ここを変えたら NOTICES も作り直す。
DEFAULT_TARGET = "x86_64-pc-windows-msvc"

LICENSE_FILE_RE = re.compile(r"^(LICENSE|LICENCE|COPYING|COPYRIGHT|NOTICE)([-._].*)?$", re.IGNORECASE)


# --------------------------------------------------------------------------
# SPDX License Expression の評価
# --------------------------------------------------------------------------
def normalize_expression(expr: str) -> str:
    """古い `MIT/Apache-2.0` 記法を SPDX の `MIT OR Apache-2.0` に直す。"""
    return expr.replace("/", " OR ")


def satisfied_by(expr: str, allowed: set[str]) -> bool:
    """SPDX License Expression が許可リストで満たせるか。

    OR = どちらかを選べればよい / AND = 両方必要 (SPDX は AND のほうが強く結合する)。
    `X WITH Y` は「例外付きの 1 識別子」として扱い、丸ごと一致しなければ基底の X を見る。
    """
    tokens = re.findall(r"\(|\)|[^\s()]+", normalize_expression(expr))
    pos = 0

    def peek() -> str | None:
        return tokens[pos] if pos < len(tokens) else None

    def parse_or() -> bool:
        value = parse_and()
        while peek() and peek().upper() == "OR":
            nonlocal pos
            pos += 1
            value = parse_and() or value
        return value

    def parse_and() -> bool:
        value = parse_atom()
        while peek() and peek().upper() == "AND":
            nonlocal pos
            pos += 1
            value = parse_atom() and value
        return value

    def parse_atom() -> bool:
        nonlocal pos
        tok = peek()
        if tok is None:
            return False
        if tok == "(":
            pos += 1
            value = parse_or()
            if peek() == ")":
                pos += 1
            return value
        pos += 1
        ident = tok.rstrip("+")  # `MIT+` のような非正規表記も基底で判定する
        if peek() and peek().upper() == "WITH" and pos + 1 < len(tokens):
            full = f"{tok} WITH {tokens[pos + 1]}"
            pos += 2
            return full in allowed or ident in allowed
        return ident in allowed

    result = parse_or()
    return result and pos == len(tokens)


# --------------------------------------------------------------------------
# cargo metadata
# --------------------------------------------------------------------------
# cargo-deny 0.16 で **削除** された [licenses] のキー。書いてあると cargo-deny は
# deprecated 診断 (既定 lint level = error) で落ちる。cargo-deny が入っていない環境では
# その失敗に気付けないので、ここで代わりに検出する。
# https://github.com/EmbarkStudios/cargo-deny/blob/main/CHANGELOG.md (0.16.0 "Removed")
REMOVED_DENY_KEYS = {"unlicensed", "deny", "copyleft", "allow-osi-fsf-free", "default"}
# 現行 [licenses] の有効キー (cargo-deny src/licenses/cfg.rs)。
VALID_DENY_KEYS = {
    "private", "confidence-threshold", "allow", "unused-allowed-license",
    "clarify", "exceptions", "unused-license-exception", "include-dev", "include-build",
    "version",
}


def allowed_licenses() -> set[str]:
    data = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
    licenses = data.get("licenses", {})
    removed = sorted(set(licenses) & REMOVED_DENY_KEYS)
    if removed:
        raise SystemExit(
            f"deny.toml: [licenses] に cargo-deny 0.16 で削除されたキーがあります: {removed}。"
            "許可されていないライセンスは既定で全部 deny されるので、これらは不要です。"
        )
    unknown = sorted(set(licenses) - VALID_DENY_KEYS)
    if unknown:
        raise SystemExit(f"deny.toml: [licenses] に未知のキーがあります: {unknown}")
    allow = licenses.get("allow")
    if not allow:
        raise SystemExit("deny.toml: [licenses] allow が空。許可リストの SSoT が壊れている。")
    return set(allow)


def cargo_metadata(target: str) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", target, "--locked"],
        cwd=ROOT, check=True, capture_output=True,
    ).stdout
    return json.loads(out)


def reachable_packages(md: dict) -> tuple[list[dict], dict[str, set[str]]]:
    """workspace から normal + build のエッジで到達する **外部** クレート。"""
    packages = {p["id"]: p for p in md["packages"]}
    nodes = {n["id"]: n for n in md["resolve"]["nodes"]}
    workspace = set(md["workspace_members"])

    kinds: dict[str, set[str]] = {}
    seen: set[str] = set()
    stack = list(workspace)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            # dep_kinds は Cargo 1.40 以降。無ければ normal 扱い。
            dep_kinds = {dk.get("kind") for dk in dep.get("dep_kinds", [])} or {None}
            wanted = {k for k in dep_kinds if k in (None, "build")}
            if not wanted:
                continue
            kinds.setdefault(dep["pkg"], set()).update("normal" if k is None else k for k in wanted)
            stack.append(dep["pkg"])

    external = [packages[i] for i in seen - workspace]
    external.sort(key=lambda p: (p["name"].lower(), p["version"]))
    return external, kinds


def license_texts(pkg: dict) -> list[tuple[str, str]]:
    """クレートのソースディレクトリにある LICENSE 系ファイルの (ファイル名, 本文)。"""
    src = Path(pkg["manifest_path"]).parent
    found: list[tuple[str, str]] = []
    if not src.is_dir():
        return found
    for f in sorted(src.iterdir()):
        if f.is_file() and LICENSE_FILE_RE.match(f.name):
            try:
                found.append((f.name, f.read_text(encoding="utf-8").replace("\r\n", "\n")))
            except (OSError, UnicodeDecodeError) as exc:
                # 黙って落とすと「著作権表示を 1 件取りこぼした NOTICES」が静かに出来上がる。
                # 読めなかった事実は必ず表に出す (配布前に人が判断できるように)。
                print(f"warning: {pkg['name']} {pkg['version']}: {f.name} を読めません ({exc})",
                      file=sys.stderr)
    return found


# --------------------------------------------------------------------------
# 生成
# --------------------------------------------------------------------------
def render(external: list[dict], kinds: dict[str, set[str]], target: str, embed_texts: bool) -> str:
    by_license: dict[str, list[dict]] = {}
    for p in external:
        by_license.setdefault(p.get("license") or "(宣言なし)", []).append(p)

    out: list[str] = []
    # 著作権 / ライセンス表示はファイルごとに書かない。REUSE.toml の一括宣言が
    # このファイルも含めて被覆する (r.md #60、per-file ヘッダは 2026-08-22 に撤去)。
    out.append("# Third-party crate notices")
    out.append("")
    out.append("**このファイルは生成物です。手で編集しないでください。**")
    out.append("再生成: `python scripts/dep_licenses.py --write` (`make license-check` が鮮度を検査します)")
    out.append("")
    out.append(
        f"daw_gui / daw_audio / daw_plugin_host に取り込まれる Rust クレート **{len(external)} 件** の"
        "ライセンス一覧です。"
    )
    out.append(
        f"依存グラフは `cargo metadata --filter-platform {target} --locked` の `resolve` から、"
        "normal と build のエッジだけを辿って求めています (dev-dependencies は配布物に入らないので除外)。"
    )
    out.append("")
    out.append(
        "crate ではない第三者コンポーネント (FFmpeg / ARA / Signalsmith / VST 3 / CLAP / VOICEVOX) の"
        "帰属は [`NOTICE`](NOTICE) にあります。daw_01 自身のライセンスは [`LICENSE`](LICENSE) "
        "(GPL-3.0-or-later)。"
    )
    out.append("")

    out.append("## ライセンス別の内訳")
    out.append("")
    out.append("| 件数 | ライセンス (SPDX) |")
    out.append("|---:|---|")
    for lic, pkgs in sorted(by_license.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        out.append(f"| {len(pkgs)} | `{lic}` |")
    out.append("")

    out.append("## クレート一覧")
    out.append("")
    out.append("| クレート | バージョン | ライセンス (SPDX) | 用途 | 配布元 |")
    out.append("|---|---|---|---|---|")
    for p in external:
        kind = "build" if kinds.get(p["id"]) == {"build"} else "link"
        repo = p.get("repository") or ""
        repo_cell = f"<{repo}>" if repo else "—"
        out.append(
            f"| {p['name']} | {p['version']} | `{p.get('license') or '(宣言なし)'}` | {kind} | {repo_cell} |"
        )
    out.append("")

    if embed_texts:
        out.append("## ライセンス全文")
        out.append("")
        out.append(
            "各クレートのソースに同梱されているライセンス / 著作権表示ファイルの全文です。"
            "同一内容のものは 1 度だけ載せ、使っているクレートを併記しています。"
        )
        out.append("")
        buckets: dict[str, tuple[str, list[str]]] = {}
        for p in external:
            for name, text in license_texts(p):
                digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
                entry = buckets.setdefault(digest, (text, []))
                entry[1].append(f"{p['name']} {p['version']} ({name})")
        for digest, (text, users) in sorted(buckets.items(), key=lambda kv: kv[1][1][0].lower()):
            out.append(f"### {', '.join(sorted(set(users)))}")
            out.append("")
            out.append("```")
            out.append(text.rstrip("\n"))
            out.append("```")
            out.append("")

    return "\n".join(out).rstrip("\n") + "\n"


def self_test() -> int:
    """`satisfied_by` の自己検査。`make license-check` から必ず走る。

    ここが壊れると「GPL-2.0-only 単独のクレートが混入しても検査が通る」という
    静かな false green になる (AND/OR の結合順を間違えるだけで起きる) ので、
    許可リスト検査そのものより先に確かめる。
    """
    allowed = {"MIT", "Apache-2.0", "Unicode-3.0", "MPL-2.0"}
    cases = [
        ("MIT", True),
        ("MIT OR Apache-2.0", True),
        ("MIT/Apache-2.0", True),                        # 旧記法の / は OR
        ("Apache-2.0 OR GPL-2.0-only", True),            # self_cell: 左を選ぶ
        ("GPL-2.0-only", False),                         # GPLv3 非互換は必ず落ちる
        ("GPL-2.0-only OR CDDL-1.0", False),
        ("(MIT OR Apache-2.0) AND Unicode-3.0", True),   # encoding_rs 型
        ("MIT AND Apache-2.0", True),
        ("Apache-2.0 AND ISC", False),                   # AND は両方必要
        ("Apache-2.0 WITH LLVM-exception", True),
        ("Zlib", False),
    ]
    failures = [
        f"{expr!r}: got {satisfied_by(expr, allowed)}, want {want}"
        for expr, want in cases
        if satisfied_by(expr, allowed) != want
    ]
    if failures:
        for f in failures:
            print(f"  {f}")
        print(f"dep-licenses: self-test FAILED ({len(failures)})", file=sys.stderr)
        return 1
    print(f"dep-licenses: self-test ok ({len(cases)} SPDX expressions)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--self-test", action="store_true", help="SPDX 式の評価器を自己検査する")
    ap.add_argument("--check", action="store_true", help="許可リスト違反と NOTICES の陳腐化を検査 (違反で exit 1)")
    ap.add_argument("--write", action="store_true", help="THIRD-PARTY-NOTICES.md を再生成")
    ap.add_argument("--embed-texts", action="store_true", help="各クレートのライセンス全文を埋め込む")
    ap.add_argument("-o", "--output", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--target", default=DEFAULT_TARGET)
    args = ap.parse_args()
    if args.self_test:
        return self_test()
    if not (args.check or args.write):
        ap.error("--self-test / --check / --write のいずれかを指定してください")

    allowed = allowed_licenses()
    md = cargo_metadata(args.target)
    external, kinds = reachable_packages(md)

    violations = [
        f"{p['name']} {p['version']}: {p.get('license') or '(宣言なし)'}"
        for p in external
        if not (p.get("license") and satisfied_by(p["license"], allowed))
    ]

    text = render(external, kinds, args.target, args.embed_texts)

    if args.write:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text, encoding="utf-8", newline="\n")
        shown = args.output
        # -o でリポジトリ外 (配布物の staging 等) を指せるので relative_to は失敗しうる。
        if args.output.is_relative_to(ROOT):
            shown = args.output.relative_to(ROOT)
        print(f"wrote {shown} ({len(external)} crates)")

    if args.check:
        if violations:
            print(f"# GPL-3.0-or-later と非互換の可能性があるクレート ({len(violations)})")
            for v in violations:
                print(f"  {v}")
            print(
                "\ndep-licenses: NOT ok — deny.toml の allow で満たせないライセンスがあります。",
                file=sys.stderr,
            )
            print("許可してよいかは https://www.gnu.org/licenses/license-list.html で確認すること。", file=sys.stderr)
            return 1
        current = args.output.read_text(encoding="utf-8") if args.output.exists() else ""
        if current != text:
            print(
                f"dep-licenses: {args.output.name} が古い。`python scripts/dep_licenses.py --write` を実行してください。",
                file=sys.stderr,
            )
            return 1
        print(f"dep-licenses: ok — {len(external)} crates, all satisfiable by deny.toml allow-list.")

    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
