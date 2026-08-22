#!/usr/bin/env bash

# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later

# アーキテクチャ不変条件の機械検査 (docs/plan_arch_refactor.md §11、CLAUDE.md「アーキテクチャ不変条件」)。
# 使い方: make arch-lint  /  bash scripts/arch_lint.sh
# 既定は違反を列挙して exit 0 (開発中の進捗トラッカーを兼ねる)。
# ARCH_LINT_STRICT=1 で違反ありなら exit 1 (CI / commit ゲート用)。
set -u
cd "$(dirname "$0")/.." || exit 1
fail=0
warn() { printf 'arch-lint: [%s] %s\n' "$1" "$2"; fail=1; }

# grep -Hn 出力 `path:line:content` から、content が行頭コメント (`//` `///` `//!`) の
# 行だけを除外する。arch 検査は「コメント内の言及」(撤去の履歴・移行の経緯説明) を
# 違反に数えず実コードだけを見る。相対パス走査なので `path` にコロンは無い前提。
strip_comments() { grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'; }

rs_dirs="common/src daw_gui/src daw_audio/src daw_plugin_host/src ui/crates"

# 1. RT 境界の無限待ち (plugin_ref.rs poisoning contract / 不変条件 4)。
#    正当な待ち (worker の idle wake 等、RT deadline を握らないもの) は同一行に
#    「arch-lint: allow-infinite」コメントを付けて明示する。
hits=$(grep -rnE 'WaitForSingleObject\([^,]*,\s*INFINITE' \
    daw_audio/src common/src/plugin_ref.rs daw_plugin_host/src 2>/dev/null \
    | grep -v 'arch-lint: allow-infinite' | strip_comments || true)
if [ -n "$hits" ]; then
    warn RT-INFINITE "RT 境界に無限待ち。有界 dispatch + quarantine (DISPATCH_TIMEOUT_MS) が不変条件:"
    printf '%s\n' "$hits"
fi

# 2. positional pair キー (不変条件 1)。device/slot の bookkeeping は安定
#    device_id (u64) 一本。 (track,index) tuple キーの map を作らない。
hits=$(grep -rnE 'HashMap<\(u32,\s*u32\)' $rs_dirs 2>/dev/null | strip_comments || true)
if [ -n "$hits" ]; then
    warn POSITIONAL-KEY "positional (u32,u32) キーの map。安定 id (device_id: u64 等) でキーする:"
    printf '%s\n' "$hits"
fi

# 3. 旧単一 protocol enum の復活 (不変条件 3)。
hits=$(grep -rnE '\b(MainToChild|ChildToMain)\b' --include='*.rs' \
    common daw_gui daw_audio daw_plugin_host ui 2>/dev/null | strip_comments || true)
if [ -n "$hits" ]; then
    warn LEGACY-PROTOCOL "MainToChild/ChildToMain の参照が残存。宛先型 (AudioCommand/AudioEvent/PluginCommand/PluginEvent) を使う:"
    printf '%s\n' "$hits"
fi

# 4. serde(untagged) の増殖 (plan §10)。判別が field 集合の pairwise 非交差に
#    依存し、variant 追加で silent misparse リスクが 2 乗成長する。
#    baseline: 0 (S5 §10 で ClipContent を tagged 化済み)。パターンを `^\s*#[...]` に
#    anchor し、doc-comment 中の `#[serde(untagged)]` 言及 (移行の経緯説明) を誤カウント
#    しないよう実属性だけを数える。
untagged_baseline=0
untagged_re='^[[:space:]]*#\[serde\(untagged\)\]'
n=$(grep -rnE "$untagged_re" --include='*.rs' common/src 2>/dev/null | wc -l)
if [ "$n" -gt "$untagged_baseline" ]; then
    warn UNTAGGED "serde(untagged) が baseline($untagged_baseline) を超過 ($n)。新規 untagged enum を作らない:"
    grep -rnE "$untagged_re" --include='*.rs' common/src
fi

# 5. protocol への bulk blob 直載せ (不変条件 2)。
hits=$(grep -HnE 'Vec<f32>|Arc<\[u8\]>' common/src/protocol.rs 2>/dev/null | strip_comments || true)
if [ -n "$hits" ]; then
    warn BLOB-IN-PROTOCOL "protocol.rs に bulk 型。blob は専用 message / WAV materialize で運ぶ:"
    printf '%s\n' "$hits"
fi

# 6. god file budget (不変条件 9): 生成物を除く .rs は 3,000 行以内。
hits=$(find common/src daw_gui/src daw_audio/src daw_plugin_host/src ui/crates -name '*.rs' \
    -not -path '*/target/*' \
    -not -name 'binding_ffmpeg*' -not -name 'bindings.rs' 2>/dev/null \
    | xargs wc -l 2>/dev/null | awk '$1 > 3000 && $2 != "total" { print $1, $2 }')
if [ -n "$hits" ]; then
    warn FILE-BUDGET "3,000 行超の .rs (分割してから足す):"
    printf '%s\n' "$hits"
fi

# 7. common の依存縮退 (plan §9): common = model + protocol + wire/shmem + 純関数。
#    HTTP / GUI / スキャナ系の重量依存を持たない。
hits=$(grep -nE '^(reqwest|rfd|image|winit|wgpu)\b' common/Cargo.toml 2>/dev/null || true)
if [ -n "$hits" ]; then
    warn COMMON-DEPS "common に域外依存 (GUI/HTTP へ移設する):"
    printf '%s\n' "$hits"
fi

# 8. daw-ui core のドメイン知識 (不変条件 8、S4 以降 0 が期待値)。
#    行頭コメント (`//` `///` `//!`) 内の言及 (撤去の履歴・rationale 説明) は実装では
#    ないので除外し、実コードのドメイン参照だけを検出する。
hits=$(grep -rnE 'ArrangementEditRequest|split_into_morae|Edit::Undoable' \
    --include='*.rs' ui/crates/ui/src 2>/dev/null | strip_comments || true)
if [ -n "$hits" ]; then
    warn UI-DOMAIN "daw-ui core に DAW ドメイン/mirror 機構が残存 (daw_gui 側へ):"
    printf '%s\n' "$hits" | head -20
fi

if [ "$fail" -eq 0 ]; then
    echo "arch-lint: OK (違反なし)"
elif [ "${ARCH_LINT_STRICT:-0}" = "1" ]; then
    exit 1
fi
exit 0
