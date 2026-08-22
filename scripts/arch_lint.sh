#!/usr/bin/env bash
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

# --- 正規表現にバックスラッシュを使わない (2026-08-22 発覚の偽グリーン対策) ---
# MSYS2 の make は `/usr/bin/bash` を **MSYS2 側の bash** に解決する (実測:
# make 経由 5.2.37(2) / 素のシェル 5.2.37(1) = Git for Windows 版)。その bash から
# **別ランタイムの** Git の grep を起動すると argv のバックスラッシュが落ちる。実測:
#     素のシェル : argv[2]=[HashMap<\(u32,\s*u32\)]
#     make 経由  : argv[2]=[HashMap<(u32,s*u32)]
# 結果 `\( \s \b \[` を含むパターンが全部別物になり、8 チェック中 6 つが無言で無効化され、
# 違反 7 行を抱えたまま「OK (違反なし)」を出していた。**シェル経由でも壊れない表記**
# (POSIX ブラケット式 `[(]` `[)]` `[[]` `[]]` `[[:space:]]`、単語境界は grep -w) だけを使い、
# 下の canary で毎回「検査器が実際に効いているか」を確かめる。

# 検査パターンは **1 箇所で定義**して canary と本体で同じものを使う。別々に書くと
# 「canary は通るが本体は壊れている」が成立してしまい、canary が証明にならない。
INFINITE_RE='WaitForSingleObject[(][^,]*,[[:space:]]*INFINITE'
POSKEY_RE='HashMap<[(]u32,[[:space:]]*u32[)]'
UNTAGGED_RE='^[[:space:]]*#[[]serde[(]untagged[)][]]'
PROTOCOL_RE='(MainToChild|ChildToMain)'

# 個別に正当化された箇所を落とす。**行内マーカー**にしているのは、除外の理由を
# その場に書かせるため (パターン側で広く除外すると、次に同じ形が入っても気付けない)。
strip_allowed() { grep -v "arch-lint: allow-$1"; }

canary_ok=1
# (1) 肯定側 — 検査が実際に違反を捕まえること。絞り込みが「常に何も検出しない」へ
#     退化したらここで落ちる。
printf 'HashMap<(u32, u32), SlotInfo>\n' | grep -qE "$POSKEY_RE" || canary_ok=0
printf 'WaitForSingleObject(h, INFINITE);\n' | grep -qE "$INFINITE_RE" || canary_ok=0
# UNTAGGED_RE は行頭 anchor 付き (doc comment の言及を数えないため) なので、
# canary の入力も実際の属性行と同じ形にする。**パターンを共有した瞬間に、
# 旧 canary が anchor 無しの別パターンを試していたことが露見した** — 検査対象と
# 別物を試す canary は証明にならない、という実例。
printf '    #[serde(untagged)]\n' | grep -qE "$UNTAGGED_RE" || canary_ok=0
printf 'use MainToChild;\n' | grep -qwE "$PROTOCOL_RE" || canary_ok=0
# doc comment 中の言及は数えないこと (撤去の経緯説明が common/src に 40 行残っている)
printf '/// 旧 #[serde(untagged)] は撤去済み\n' | grep -qE "$UNTAGGED_RE" && canary_ok=0
# (2) 否定側 — 除外マーカーが実際に効くこと。効かなければ正当な箇所を毎回報告し続ける。
printf 'WaitForSingleObject(h, INFINITE); // arch-lint: allow-infinite\n' \
    | grep -E "$INFINITE_RE" | strip_allowed infinite | grep -q . && canary_ok=0
printf 'p: HashMap<(u32, u32), SizePool>, // arch-lint: allow-positional-key\n' \
    | grep -E "$POSKEY_RE" | strip_allowed positional-key | grep -q . && canary_ok=0
if [ "$canary_ok" -ne 1 ]; then
    printf 'arch-lint: [SELF-BROKEN] 検査器の正規表現が効いていません。\n' >&2
    printf '  この環境の grep に既知のパターンが通りませんでした。違反ゼロの報告は信用できません。\n' >&2
    printf '  grep=%s / %s\n' "$(command -v grep)" "$(grep --version | head -1)" >&2
    exit 1
fi
# 上の canary はバックスラッシュを使わないので、argv 破壊そのものは検出できない。
# 「この環境ではバックスラッシュが落ちる」を名指しで検出して、パターンを足す人に見せる。
if ! printf 'a(b\n' | grep -qE 'a\(b' 2>/dev/null; then
    printf 'arch-lint: [NOTE] この環境では正規表現のバックスラッシュが argv で落ちます\n'
    printf '  (MSYS2 make -> MSYS2 bash -> Git の grep のクロスランタイム起動)。\n'
    printf '  パターンにバックスラッシュを使わないこと。現在の全パターンは対応済みです。\n'
fi

# 1. RT 境界の無限待ち (plugin_ref.rs poisoning contract / 不変条件 4)。
#    正当な待ち (worker の idle wake 等、RT deadline を握らないもの) は同一行に
#    「arch-lint: allow-infinite」コメントを付けて明示する。
hits=$(grep -rnE "$INFINITE_RE" \
    daw_audio/src common/src/plugin_ref.rs daw_plugin_host/src 2>/dev/null \
    | strip_allowed infinite | strip_comments || true)
if [ -n "$hits" ]; then
    warn RT-INFINITE "RT 境界に無限待ち。有界 dispatch + quarantine (DISPATCH_TIMEOUT_MS) が不変条件:"
    printf '%s\n' "$hits"
fi

# 2. positional pair キー (不変条件 1)。device/slot の bookkeeping は安定
#    device_id (u64) 一本。 (track,index) tuple キーの map を作らない。
hits=$(grep -rnE "$POSKEY_RE" $rs_dirs 2>/dev/null \
    | strip_allowed positional-key | strip_comments || true)
if [ -n "$hits" ]; then
    warn POSITIONAL-KEY "positional (u32,u32) キーの map。安定 id (device_id: u64 等) でキーする:"
    printf '%s\n' "$hits"
fi

# 3. 旧単一 protocol enum の復活 (不変条件 3)。
hits=$(grep -rnwE "$PROTOCOL_RE" --include='*.rs' \
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
n=$(grep -rnE "$UNTAGGED_RE" --include='*.rs' common/src 2>/dev/null | wc -l)
if [ "$n" -gt "$untagged_baseline" ]; then
    warn UNTAGGED "serde(untagged) が baseline($untagged_baseline) を超過 ($n)。新規 untagged enum を作らない:"
    grep -rnE "$UNTAGGED_RE" --include='*.rs' common/src
fi

# 5. protocol への bulk blob 直載せ (不変条件 2)。
hits=$(grep -HnE 'Vec<f32>|Arc<[[]u8[]]>' common/src/protocol.rs 2>/dev/null | strip_comments || true)
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
hits=$(grep -nE '^(reqwest|rfd|image|winit|wgpu)[^A-Za-z0-9_-]' common/Cargo.toml 2>/dev/null || true)
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
