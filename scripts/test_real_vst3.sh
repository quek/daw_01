#!/usr/bin/env bash
# 実物の VST3 を読む検査を回す (`make test-real-vst3`、および `make test` の最後)。
#
# 使い方: bash scripts/test_real_vst3.sh [--if-configured]
#
# なぜ要るか
# ----------
# 商用プラグイン (MeldaProduction MCenter / MCompressor) を読む検査は、clone した人の環境にも
# 無いのが普通なので `#[ignore]` で既定の `cargo test` から外してある。走るのは、プラグインの
# 場所を env (`DAW01_TEST_VST3_*`) で渡し、**かつ** `-- --ignored` を付けたときだけ。どちらが
# 欠けても「0 passed / 1 ignored」で緑に見え、何も確かめていないことに気付けない
# (2026-05〜10、音源の入れ口が no-op になって書き出しが全部無音のまま、env だけ渡して回す手順で
# 誰も気付かなかった)。だから回し方をここ 1 か所に持つ:
#   - 対象の検査と env の対応は **ソースから導く** (`DAW01_TEST_VST3_*` を参照する
#     daw_gui/tests/*.rs)。手で列挙しない — 検査が増減した瞬間に静かにずれる。
#   - 既定 (`make test-real-vst3`): env が 1 つでも欠けていれば **落ちる**。
#   - `--if-configured` (`make test` の最後): env のある検査だけ回し、回さなかった検査と
#     足りない env を **必ず表示する** (黙って通さない)。
#   - 検査の側も、走ったのに env が無ければ panic する (`daw_gui/tests/real_vst3_support`)。
#
# **make へは環境変数ではなく引数で渡す**: `make test-real-vst3 DAW01_TEST_VST3_MCENTER=<path> ...`。
# Git Bash から起動した MSYS2 の make には POSIX 環境変数が 1 つも継承されない (Makefile 冒頭の
# 「Windows のログオン環境の復元」)。make の引数で渡した変数は recipe へ export され、
# `/usr/bin/bash` (= $(BASH)) 経由ならバックスラッシュも保ったまま cargo と検査まで届く (実測)。
#
# daw_gui を `--script` で起動する検査なので、呼び出し側 (Makefile) が preflight と build を
# 前提条件に持つ。
set -eu

mode="${1:-required}"
case "$mode" in
    required | --if-configured) ;;
    *)
        echo "usage: $0 [--if-configured]" >&2
        exit 2
        ;;
esac

cd "$(dirname "$0")/.."

targets=()
missing=()
# `grep -o` は複数ファイルに対して `path:一致` を並べる。sort -u で (検査, env) 1 組 1 行。
pairs="$(grep -o 'DAW01_TEST_VST3_[A-Z0-9_]*' daw_gui/tests/*.rs | sort -u || true)"
if [ -z "$pairs" ]; then
    # 導出が空 = 検査の改名・移動で基準が外れた。0 件を「回すものが無い」と読まない。
    echo "ERROR: DAW01_TEST_VST3_* を参照する検査が daw_gui/tests/*.rs に見つかりません (この script の導出が古い)" >&2
    exit 1
fi
while IFS=: read -r file var; do
    name="$(basename "$file" .rs)"
    if [ -n "${!var:-}" ]; then
        targets+=(--test "$name")
    else
        missing+=("$name ($var)")
    fi
done <<<"$pairs"

if [ "${#missing[@]}" -gt 0 ]; then
    if [ "$mode" = required ]; then
        echo "ERROR: 実物の VST3 の検査に要る env が未設定です: ${missing[*]}" >&2
        echo "       .vst3 のパスを make の引数で渡す: make test-real-vst3 <env>=<.vst3 のパス> ..." >&2
        echo "       (シェルの環境変数は make に継承されない — scripts/test_real_vst3.sh 冒頭)" >&2
        exit 1
    fi
    for m in "${missing[@]}"; do
        echo "test-real-vst3: 回していない (env 未設定): $m — 回し方は make test-real-vst3 <env>=<.vst3 のパス>"
    done
fi
if [ "${#targets[@]}" -eq 0 ]; then
    exit 0
fi
exec cargo test -p daw_gui --features daw_gui/script "${targets[@]}" -- --ignored
