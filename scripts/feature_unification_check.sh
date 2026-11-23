#!/usr/bin/env bash
# daw_gui だけをビルドしたとき (`cargo build/test/run -p daw_gui`) と、3 exe をまとめて
# ビルドしたとき (`make build`) で、daw_gui が依存する crate の feature が一致することを確かめる。
#
# なぜ要るか
# ----------
# cargo はビルドに選んだ package の組み合わせごとに依存の feature を合算する。組み合わせで
# feature が違うと daw_gui を別物としてビルドし直す。ライブラリは feature ごとに別ファイルで
# 残るが、MSVC の exe は名前に hash が付かず target/debug/daw_gui.exe 1 つを共有するので、
# 組み合わせを行き来するたびに作り直しになる (2026-11-23 まで cpal (daw_audio) が windows crate に
# 足す feature で起きていた)。Linux の exe は hash 付きで別に残るのでこの問題は起きないが、
# どの host で回しても Windows の木を検査するよう --target を固定する。
#
# 直し方: ずれた crate の feature を Cargo.toml の [workspace.dependencies] でも宣言する
# (windows の注記が実例)。
#
# 見ないもの
# ----------
# - daw_audio / daw_plugin_host を単独でビルドしたときのずれ (今は 15〜21 行違う)。それらは
#   `make build` でしか作らないので踏まない。単独で build / run すると作り直しになる。
# - `--features script` の有無 (daw_gui 自身の feature なので避けられない)。
# - 同じ crate が host 用 (build-dep / proc-macro) と target 用で別 feature になる区別。出力の行に
#   種別が無いので、daw_audio だけが host 用に違う feature で使う crate があると偽の赤になりうる。
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-msvc

feature_tree() {
    cargo tree --locked --target "$TARGET" "$@" -f '{p} [{f}]' --prefix none | sed -e 's/ [(][*][)]$//' | sort -u
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

feature_tree -p daw_gui -e normal,build >"$tmp/alone"
feature_tree -p daw_gui -e normal,build,dev >"$tmp/alone_dev"
feature_tree -p daw_gui -p daw_audio -p daw_plugin_host -e normal,build >"$tmp/together"

# daw_gui.exe にリンクされる crate (= daw_gui 単独の normal/build の木) だけを比べる。
awk '{print $1" "$2}' "$tmp/alone" | sort -u >"$tmp/keys"
# 空の木を「全部一致」と読まない (awk の NR==FNR は 1 つ目のファイルが空だと 2 つ目を key として読む)。
if [ ! -s "$tmp/keys" ] || ! grep -q '^daw_gui ' "$tmp/keys"; then
    echo "ERROR: cargo tree -p daw_gui の出力が空か、daw_gui の行がありません。検査できません。" >&2
    exit 1
fi
only_linked() { awk 'NR==FNR{k[$0]=1;next} ($1" "$2) in k' "$tmp/keys" "$1" | sort -u; }
only_linked "$tmp/together" >"$tmp/together_linked"

status=0
for side in alone alone_dev; do
    only_linked "$tmp/$side" >"$tmp/${side}_linked"
    if ! diff -q "$tmp/${side}_linked" "$tmp/together_linked" >/dev/null; then
        status=1
        case $side in
            alone) cmd="cargo build -p daw_gui" ;;
            alone_dev) cmd="cargo test -p daw_gui" ;;
        esac
        echo "ERROR: \`$cmd\` と \`make build\` で feature が違う crate があります (daw_gui.exe が毎回作り直しになる):" >&2
        comm -3 "$tmp/${side}_linked" "$tmp/together_linked" | sed -e "s/^\t/  make build: /" -e "/^  make build: /!s/^/  $cmd: /" >&2
    fi
done
if [ "$status" -ne 0 ]; then
    echo "       Cargo.toml の [workspace.dependencies] で同じ feature を宣言して揃えること (windows の注記を参照)。" >&2
    exit 1
fi
echo "feature-unification: OK (daw_gui 単独 / make build で依存の feature が一致)"
