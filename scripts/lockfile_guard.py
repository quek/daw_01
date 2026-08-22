#!/usr/bin/env python3
"""供給網攻撃に対する一次防御の機械検査 (stdlib のみ、ネットワーク不要、常に走る)。

なぜこれが要るか — 2026-08-20 の実例
------------------------------------
crates.io の **arrayref 0.3.10** が汚染された (RUSTSEC-2026-0260)。typosquat の
`proc-macro1` への依存が足され、その build script が **コンパイル中にリモートの
バイナリを取得して実行**する。同じ攻撃者が 23 分の間に `internment` 0.8.7 と
`append-only-vec` 0.1.9 も汚染している。Rust Security Response Team は作者の
端末 / 資格情報の侵害と見ている。

**このリポジトリは無事だった。理由は Cargo.lock を commit していて、迂闊に
`cargo update` を走らせなかったから**である (lock の arrayref は 0.3.9)。
つまり運ではなく lockfile が効いたのだが、それを守る検査は無かった。ここで足す。

このスクリプトが見るもの (どれも semver 解釈を要さず、**厳密**に判定できるもの)
-------------------------------------------------------------------------------
  1. Cargo.lock が git 追跡下にあること
     — 追跡していなければ「毎回 solver が最新を引く」= 上流が汚染された瞬間に入る。
  2. `cargo metadata --locked` が通ること
     — lock と manifest が乖離していない (= ビルドが黙って lock を書き換えない)。
  3. **既知の汚染リリース (name, version) が lock に無いこと**
     — 範囲判定をしない完全一致なので誤判定がない。個別の事件を確実に押さえる。

範囲 (semver) を要する一般的な脆弱性検査は **cargo-deny** が行う
(`make audit` が両方を必ず走らせる)。ここで自前の semver 実装を書かないのは、
**間違った range 判定は「緑に見えるのに素通し」= false green** そのものだから。

使い方:
  python scripts/lockfile_guard.py              # 検査 (違反あれば exit 1)
  python scripts/lockfile_guard.py --self-test  # 判定器そのものの自己検査
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

for _stream in (sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, ValueError):  # pragma: no cover
        pass

ROOT = Path(__file__).resolve().parent.parent

# 既知の汚染リリース。**完全一致の (name, version)** だけを列挙する。
# 新しい事件を見つけたらここに 1 行足す (出典を必ず書く)。範囲で書きたくなったら
# それは cargo-deny (advisory DB) の仕事なので、ここには書かない。
KNOWN_COMPROMISED: list[tuple[str, str, str]] = [
    # RUSTSEC-2026-0260 (2026-08-20)。作者アカウント侵害。typosquat `proc-macro1` を
    # 依存に足し、その build script がコンパイル中にリモートバイナリを取得して実行する。
    ("arrayref", "0.3.10", "RUSTSEC-2026-0260"),
    ("internment", "0.8.7", "RUSTSEC-2026-0260 (同一攻撃者・同日)"),
    ("append-only-vec", "0.1.9", "RUSTSEC-2026-0260 (同一攻撃者・同日)"),
]

# 攻撃で持ち込まれた typosquat。**名前が lock に出た時点で異常** なので版を問わない。
KNOWN_MALICIOUS_NAMES: list[tuple[str, str]] = [
    ("proc-macro1", "RUSTSEC-2026-0260 の攻撃で注入された typosquat (正しくは proc-macro2)"),
]

_PKG_RE = re.compile(r'^\[\[package\]\]\s*$\n(.*?)(?=^\[\[package\]\]|\Z)', re.M | re.S)
_NAME_RE = re.compile(r'^name = "([^"]+)"', re.M)
_VER_RE = re.compile(r'^version = "([^"]+)"', re.M)


def parse_lock(text: str) -> list[tuple[str, str]]:
    """Cargo.lock から (name, version) を取り出す。tomllib でもよいが、
    lock は [[package]] の羅列だけなので正規表現で十分かつ速い。"""
    out = []
    for block in _PKG_RE.findall(text):
        n = _NAME_RE.search(block)
        v = _VER_RE.search(block)
        if n and v:
            out.append((n.group(1), v.group(1)))
    return out


def check_packages(packages: list[tuple[str, str]]) -> list[str]:
    """汚染リリース / 悪性 crate 名が含まれていないか。"""
    problems = []
    present = set(packages)
    names = {n for n, _ in packages}
    for name, version, ref in KNOWN_COMPROMISED:
        if (name, version) in present:
            problems.append(f"{name} {version} は汚染されたリリースです ({ref})")
    for name, ref in KNOWN_MALICIOUS_NAMES:
        if name in names:
            problems.append(f"{name} が依存グラフに居ます — {ref}")
    return problems


def self_test() -> int:
    """判定器が本当に検知することを確かめる。ここが壊れると静かに素通しになる。"""
    sample = """
[[package]]
name = "arrayref"
version = "0.3.10"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "proc-macro1"
version = "1.0.0"

[[package]]
name = "serde"
version = "1.0.0"
"""
    pkgs = parse_lock(sample)
    if ("arrayref", "0.3.10") not in pkgs or ("serde", "1.0.0") not in pkgs:
        print("lockfile-guard: self-test FAILED — Cargo.lock の parse が壊れている", file=sys.stderr)
        return 1
    hits = check_packages(pkgs)
    if len(hits) != 2:
        print(f"lockfile-guard: self-test FAILED — 汚染 2 件を検知できていない: {hits}", file=sys.stderr)
        return 1
    clean = check_packages([("arrayref", "0.3.9"), ("serde", "1.0.0")])
    if clean:
        print(f"lockfile-guard: self-test FAILED — 健全な lock を誤検知: {clean}", file=sys.stderr)
        return 1
    print(f"lockfile-guard: self-test ok (汚染 {len(KNOWN_COMPROMISED)} 件 + "
          f"悪性名 {len(KNOWN_MALICIOUS_NAMES)} 件の判定を検証)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()
    if args.self_test:
        return self_test()

    problems: list[str] = []

    # 1. Cargo.lock が追跡されているか
    tracked = subprocess.run(["git", "ls-files", "--error-unmatch", "Cargo.lock"],
                             cwd=ROOT, capture_output=True)
    if tracked.returncode != 0:
        problems.append(
            "Cargo.lock が git 追跡下にありません。lock を commit しないと、ビルドのたびに "
            "solver が上流の最新を引き、汚染されたリリースが公開された瞬間に取り込まれます。"
        )

    lock = ROOT / "Cargo.lock"
    if not lock.is_file():
        problems.append("Cargo.lock がありません")
        packages: list[tuple[str, str]] = []
    else:
        packages = parse_lock(lock.read_text(encoding="utf-8"))
        if not packages:
            problems.append("Cargo.lock を parse できませんでした ([[package]] が 0 件)")

    # 2. lock と manifest が乖離していないか (--locked は乖離があれば失敗する)
    meta = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked",
                           "--no-deps", "--offline"],
                          cwd=ROOT, capture_output=True)
    if meta.returncode != 0:
        err = meta.stderr.decode("utf-8", "replace").strip().splitlines()
        detail = err[-1] if err else "(詳細なし)"
        problems.append(
            f"`cargo metadata --locked` が失敗しました: {detail}\n"
            "       Cargo.lock が Cargo.toml と食い違っています。ビルド時に lock が黙って"
            "書き換わる状態なので、意図した版を固定できていません。"
        )

    # 3. 既知の汚染リリース
    problems.extend(check_packages(packages))

    if problems:
        for p in problems:
            print(f"  {p}")
        print(f"\nlockfile-guard: NOT ok — {len(problems)} 件", file=sys.stderr)
        return 1

    print(f"lockfile-guard: ok — Cargo.lock は追跡下・manifest と同期・"
          f"既知の汚染リリースなし ({len(packages)} packages)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
