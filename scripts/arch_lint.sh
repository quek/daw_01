#!/usr/bin/env bash
# アーキテクチャ不変条件の機械検査 (docs/plan_arch_refactor.md §11、CLAUDE.md「アーキテクチャ不変条件」)。
# 使い方: make arch-lint  /  bash scripts/arch_lint.sh
# **exit 0 = 「違反ゼロ、または scripts/arch_lint_baseline.txt に記録済みのものだけ」**。
# baseline に無い違反が 1 件でもあれば exit 1 (行単位 ratchet、下の「ratchet 機構」節)。
# ARCH_LINT_STRICT=1 で baseline 済みの負債も落とす (将来の一掃用)。
# 毎回 `baseline N 件 (解消 K) / 新規 M 件` を出すので、進捗トラッカーとしても使える。
set -u
cd "$(dirname "$0")/.." || exit 1

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
UNTAGGED_RE='^[[:space:]]*#[[]serde[(]untagged[)][]]'
PROTOCOL_RE='(MainToChild|ChildToMain)'

# positional キーの検出 (不変条件 1)。 **連想コンテナのキーがタプル** という形だけを
# 見る。 「(u32, u32) の並び」そのものを見るパターンは repo に 40 件当たり、その 8 割が
# gui_get_size / texture_size / decode_image 等の **寸法タプル** で、型から区別できない。
# 区別できないものを報告すると allow マーカーが散って検査が読まれなくなる
# (= 守ろうとしているものを壊す)。
# Vec / Option の生タプルは対象外 (寸法と区別不能)。 map/set のキーなら偽陽性ゼロ。
# 折り返し (`HashMap<` で改行 → 次行が `(u32, u32),`) も拾うので awk。
# バックスラッシュは使わない (make 経由で argv から落ちる、上の節)。
POSKEY_AWK='
FNR == 1 { prev = "" }
{
  if ($0 ~ /(HashMap|HashSet|BTreeMap|BTreeSet|IndexMap|IndexSet)[[:space:]]*<[[:space:]]*[(]u32,[[:space:]]*u32[,)]/)
      print FILENAME ":" FNR ":" $0
  else if (prev ~ /(HashMap|HashSet|BTreeMap|BTreeSet|IndexMap|IndexSet)[[:space:]]*<[[:space:]]*$/ && $0 ~ /^[[:space:]]*[(]u32,[[:space:]]*u32[,)]/)
      print FILENAME ":" FNR ":" $0
  prev = $0
}'

# 個別に正当化された箇所を落とす。**行内マーカー**にしているのは、除外の理由を
# その場に書かせるため (パターン側で広く除外すると、次に同じ形が入っても気付けない)。
# 出典: この慣習は **本ファイルの check 1 のコメント**が定めていたもの
# (「正当な待ち (worker の idle wake 等) は同一行に arch-lint: allow-infinite を付ける」)。
# 2026-08-22 に positional-key へも同じ形を広げた。docs/plan_arch_refactor.md 側には
# この慣習の記述は無い (新設ではなく既存慣習の一般化、という点は変わらない)。
strip_allowed() { grep -v "arch-lint: allow-$1"; }

canary_ok=1
# (1) 肯定側 — 検査が実際に違反を捕まえること。絞り込みが「常に何も検出しない」へ
#     退化したらここで落ちる。
printf 'WaitForSingleObject(h, INFINITE);\n' | grep -qE "$INFINITE_RE" || canary_ok=0
# POSITIONAL-KEY: 1 行 / HashSet / 折り返した 3 つ組 のいずれも拾うこと。
printf 'x: HashMap<(u32, u32), SlotInfo>,\n' | awk "$POSKEY_AWK" | grep -q . || canary_ok=0
printf 'x: HashSet<(u32,u32)>,\n'            | awk "$POSKEY_AWK" | grep -q . || canary_ok=0
printf 'p: std::collections::HashMap<\n    (u32, u32, u32),\n    f64,\n>,\n' \
    | awk "$POSKEY_AWK" | grep -q . || canary_ok=0
# 否定側 — 寸法タプルと (u64, u32) を拾わないこと (拾うと allow マーカーが散る)。
printf 'fn size(&self) -> Option<(u32, u32)> {\n' | awk "$POSKEY_AWK" | grep -q . && canary_ok=0
printf 'let v: Vec<(u32, u32)> = Vec::new();\n'   | awk "$POSKEY_AWK" | grep -q . && canary_ok=0
printf 'x: HashMap<(u64, u32), f64>,\n'           | awk "$POSKEY_AWK" | grep -q . && canary_ok=0
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
    | awk "$POSKEY_AWK" | strip_allowed positional-key | grep -q . && canary_ok=0
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

# ---------------------------------------------------------------- ratchet 機構
# **「OK」は「違反ゼロ、または baseline に記録済みのものだけ」を意味する。**
#
# 以前は違反があっても常に exit 0 だった (「開発中の進捗トラッカーを兼ねる」ため)。
# その結果、各セッションが終了コードだけを見て「make arch-lint OK」と報告し続けた
# — 出力を読まないと違反に気付けない設計は、今 session の主題だった
# 「緑なのに実際は動いていない」の系譜そのもの。
#
# かといって常に exit 1 にすると、他人が直すべき既存違反で全セッションが止まる。
# なので **行単位の ratchet**: 既知の違反は baseline に理由付きで記録し、
# **baseline に無い違反が 1 件でもあれば exit 1**。件数は減る方向にしか動けない。
#
# **件数 baseline (旧 untagged 検査の形) にはしない**: N > 0 の件数比較だと
# 「1 件直して 1 件増やす」が素通りする。行単位なら通らない。
#
# fingerprint は **行番号ではなくマッチ行の内容**のハッシュ。行番号は無関係な編集で
# 毎回ずれるので、行番号で持つと baseline が壊れ続ける。
#
# baseline と行内マーカーは **別物**:
#   行内 `arch-lint: allow-*` … 恒久的に正当 (park / 寸法キー)。直す予定は無い。
#   baseline の 1 行          … 既知の負債。直す予定があり、理由と落とし所を書く。
# 区別しないと、負債が「正当」として永久に隠れる。
BASELINE=scripts/arch_lint_baseline.txt
# **一時ファイルを使わない**。mktemp が作る `/tmp/...` は MSYS2 bash (make 経由) と
# Git の coreutils で **別のルートに解決される**ため、書いた側と読む側がずれて
# 中身が空に見えた (2026-08-22 実測: make 経由だけ fingerprint が全部空文字の
# ハッシュになり、baseline と一致しなくなった)。すべて bash 変数に持つ。
NL='
'
HITS=""
HEADS=""
BASEKEYS=""
SEEN=""
NEW=""

# record <CHECK> <mode> <headline> <hits>
#   mode = grep       … hits が `path:line:content` 形式 (既定)
#          firstfield … hits の第 1 field が path (FILE-BUDGET など)
#
# 区切りは **半角スペース 1 個**、分解は **パラメータ展開だけ**で行う。タブ区切り +
# `IFS=$(printf '\t')` + `cut -f` でも書けるが、make 経由だと外部コマンドへ渡る引数が
# 壊れて分解が崩れ、fingerprint が環境によって変わった (2026-08-22 実測)。
# CHECK と mode に空白は入らないので、これで曖昧さなく分解できる。
record() {
    [ -n "$4" ] || return 0
    HEADS="$HEADS$1 $3$NL"
    while IFS= read -r _ln; do
        [ -n "$_ln" ] || continue
        HITS="$HITS$1 $2 $_ln$NL"
    done <<EOF
$4
EOF
}

# contains <haystack(改行区切り)> <needle> — grep を介さない完全一致検索
contains() {
    case "$NL$1" in
        *"$NL$2$NL"*) return 0 ;;
        *) return 1 ;;
    esac
}

# split_row <row> -> ROW_CHECK / ROW_MODE / ROW_LINE
split_row() {
    ROW_CHECK="${1%% *}"
    _rest="${1#* }"
    ROW_MODE="${_rest%% *}"
    ROW_LINE="${_rest#* }"
}

# fingerprint <mode> <hitline> -> "path|fp12"
fingerprint() {
    _fp_path=""
    _fp_content=""
    if [ "$1" = "firstfield" ]; then
        _fp_path="${2%% *}"
    else
        _fp_path="${2%%:*}"
        _fp_content="${2#*:}"
        _fp_content="${_fp_content#*:}"
    fi
    _fp_norm="$(printf '%s' "$_fp_content" | tr -s '[:space:]' ' ' | sed 's/^ //' | sed 's/ $//')"
    printf '%s|%s' "$_fp_path" "$(printf '%s' "$_fp_norm" | sha1sum | cut -c1-12)"
}

# classify <hits(改行区切り)> <quiet:0|1> -> N_NEW / N_BASE / N_RESOLVED / NEW を設定
classify() {
    N_NEW=0
    N_BASE=0
    N_RESOLVED=0
    SEEN=""
    NEW=""
    while IFS= read -r _row; do
        [ -n "$_row" ] || continue
        split_row "$_row"
        _key="$ROW_CHECK|$(fingerprint "$ROW_MODE" "$ROW_LINE")"
        if contains "$BASEKEYS" "$_key"; then
            SEEN="$SEEN$_key$NL"
            N_BASE=$((N_BASE + 1))
        else
            NEW="$NEW$_row$NL"
            N_NEW=$((N_NEW + 1))
        fi
    done <<EOF
$1
EOF

    if [ "$N_NEW" -gt 0 ] && [ "$2" != "1" ]; then
        _checks=""
        while IFS= read -r _row; do
            [ -n "$_row" ] || continue
            split_row "$_row"
            contains "$_checks" "$ROW_CHECK" || _checks="$_checks$ROW_CHECK$NL"
        done <<EOF
$NEW
EOF
        while IFS= read -r _c; do
            [ -n "$_c" ] || continue
            _head=""
            while IFS= read -r _h; do
                case "$_h" in "$_c "*) [ -n "$_head" ] || _head="${_h#* }" ;; esac
            done <<EOF
$HEADS
EOF
            printf 'arch-lint: [%s] %s\n' "$_c" "$_head"
            while IFS= read -r _row; do
                [ -n "$_row" ] || continue
                split_row "$_row"
                [ "$ROW_CHECK" = "$_c" ] || continue
                printf '  %s\n' "$ROW_LINE"
            done <<EOF
$NEW
EOF
        done <<EOF
$_checks
EOF
    fi

    while IFS= read -r _bk; do
        [ -n "$_bk" ] || continue
        if ! contains "$SEEN" "$_bk"; then
            N_RESOLVED=$((N_RESOLVED + 1))
            [ "$2" = "1" ] || printf 'arch-lint: [解消] baseline から削除してよい: %s\n' "$_bk"
        fi
    done <<EOF
$BASEKEYS
EOF
}

# baseline を読み込んで `CHECK|path|fp` の索引を作る (分解はパラメータ展開のみ)
if [ -f "$BASELINE" ]; then
    while IFS= read -r _bl; do
        case "$_bl" in ''|'#'*) continue ;; esac
        _bc="${_bl%%|*}"
        _r1="${_bl#*|}"
        _bp="${_r1%%|*}"
        _r2="${_r1#*|}"
        _bf="${_r2%%|*}"
        _bc="${_bc// /}"
        _bp="${_bp// /}"
        _bf="${_bf// /}"
        [ -n "$_bc" ] || continue
        BASEKEYS="$BASEKEYS$_bc|$_bp|$_bf$NL"
    done < "$BASELINE"
fi

# ratchet 自体が効くことを毎回確かめる。**これが無いなら ratchet を入れる意味が無い**
# — 「新規違反で落ちる」を検査しないと、今日 3 回見た偽グリーンが 1 段上に移るだけ。
classify 'SELFTEST grep selftest/synthetic.rs:1:    pool: HashMap<(u32, u32), Bogus>,' 1
if [ "$N_NEW" -ne 1 ]; then
    printf 'arch-lint: [SELF-BROKEN] ratchet が新規違反を検出できていません (N_NEW=%s)。\n' "$N_NEW" >&2
    printf '  baseline に無い違反を「新規」に分類できない状態では、違反ゼロの報告は信用できません。\n' >&2
    exit 1
fi

# 1. RT 境界の無限待ち (plugin_ref.rs poisoning contract / 不変条件 4)。
#    正当な待ち (worker の idle wake 等、RT deadline を握らないもの) は同一行に
#    「arch-lint: allow-infinite」コメントを付けて明示する。
hits=$(grep -rnE "$INFINITE_RE" \
    daw_audio/src common/src/plugin_ref.rs daw_plugin_host/src 2>/dev/null \
    | strip_allowed infinite | strip_comments || true)
record RT-INFINITE grep "RT 境界に無限待ち。有界 dispatch + quarantine (DISPATCH_TIMEOUT_MS) が不変条件:" "$hits"

# 2. positional pair キー (不変条件 1)。device/slot の bookkeeping は安定
#    device_id (u64) 一本。 (track,index) tuple キーの map/set を作らない。
hits=$(find $rs_dirs -name '*.rs' -not -path '*/target/*' -print0 2>/dev/null \
    | xargs -0 awk "$POSKEY_AWK" 2>/dev/null \
    | strip_allowed positional-key | strip_comments || true)
record POSITIONAL-KEY grep "positional (u32,u32) キーの map/set。安定 id (device_id: u64 等) でキーする:" "$hits"

# 3. 旧単一 protocol enum の復活 (不変条件 3)。
hits=$(grep -rnwE "$PROTOCOL_RE" --include='*.rs' \
    common daw_gui daw_audio daw_plugin_host ui 2>/dev/null | strip_comments || true)
record LEGACY-PROTOCOL grep "MainToChild/ChildToMain の参照が残存。宛先型 (AudioCommand/AudioEvent/PluginCommand/PluginEvent) を使う:" "$hits"

# 4. serde(untagged) の増殖 (plan §10)。判別が field 集合の pairwise 非交差に
#    依存し、variant 追加で silent misparse リスクが 2 乗成長する。
#    baseline: 0 (S5 §10 で ClipContent を tagged 化済み)。パターンを `^\s*#[...]` に
#    anchor し、doc-comment 中の `#[serde(untagged)]` 言及 (移行の経緯説明) を誤カウント
#    しないよう実属性だけを数える。
#    かつては件数 baseline (`n > 0`) だったが、行単位 ratchet に一本化した
#    (件数比較は「1 件直して 1 件増やす」が素通りする)。
hits=$(grep -rnE "$UNTAGGED_RE" --include='*.rs' common/src 2>/dev/null || true)
record UNTAGGED grep "新規の serde(untagged)。判別が field 集合の pairwise 非交差に依存し、variant 追加で silent misparse リスクが 2 乗成長する:" "$hits"

# 5. protocol への bulk blob 直載せ (不変条件 2)。
hits=$(grep -HnE 'Vec<f32>|Arc<[[]u8[]]>' common/src/protocol.rs 2>/dev/null | strip_comments || true)
record BLOB-IN-PROTOCOL grep "protocol.rs に bulk 型。blob は専用 message / WAV materialize で運ぶ:" "$hits"

# 6. god file budget (不変条件 9): 生成物を除く .rs は 3,000 行以内。
hits=$(find common/src daw_gui/src daw_audio/src daw_plugin_host/src ui/crates -name '*.rs' \
    -not -path '*/target/*' \
    -not -name 'binding_ffmpeg*' -not -name 'bindings.rs' 2>/dev/null \
    | xargs wc -l 2>/dev/null | awk '$1 > 3000 && $2 != "total" { print $2, $1 " 行" }')
# path を第 1 field に置く (行数は増減するので fingerprint に含めない)。
record FILE-BUDGET firstfield "3,000 行超の .rs (分割してから足す):" "$hits"

# 7. common の依存縮退 (plan §9): common = model + protocol + wire/shmem + 純関数。
#    HTTP / GUI / スキャナ系の重量依存を持たない。
hits=$(grep -HnE '^(reqwest|rfd|image|winit|wgpu)[^A-Za-z0-9_-]' common/Cargo.toml 2>/dev/null || true)
record COMMON-DEPS grep "common に域外依存 (GUI/HTTP へ移設する):" "$hits"

# 8. daw-ui core のドメイン知識 (不変条件 8、S4 以降 0 が期待値)。
#    行頭コメント (`//` `///` `//!`) 内の言及 (撤去の履歴・rationale 説明) は実装では
#    ないので除外し、実コードのドメイン参照だけを検出する。
hits=$(grep -rnE 'ArrangementEditRequest|split_into_morae|Edit::Undoable' \
    --include='*.rs' ui/crates/ui/src 2>/dev/null | strip_comments || true)
record UI-DOMAIN grep "daw-ui core に DAW ドメイン/mirror 機構が残存 (daw_gui 側へ):" "$hits"

# ---------------------------------------------------------------- 判定
classify "$HITS" 0

printf 'arch-lint: baseline %d 件 (解消 %d) / 新規 %d 件\n' "$N_BASE" "$N_RESOLVED" "$N_NEW"
if [ "$N_RESOLVED" -gt 0 ]; then
    printf '  ↑ 解消した行は %s から削除してください (良い変更なのでここでは落としません)。\n' "$BASELINE"
fi

if [ "$N_NEW" -gt 0 ]; then
    # ARCH_LINT_EMIT_BASELINE=1 は貼り付け用の行を出すだけで、**書き込みはしない**。
    # baseline に載せるかどうかは理由を書く人の判断で、自動生成にすると
    # 「とりあえず全部 baseline」になって ratchet が死ぬ。exit 1 のままにしてある。
    if [ "${ARCH_LINT_EMIT_BASELINE:-0}" != "0" ]; then
        printf '\narch-lint: baseline 候補 (理由を書いてから %s に貼る):\n' "$BASELINE"
        while IFS= read -r _row; do
            [ -n "$_row" ] || continue
            split_row "$_row"
            _k="$(fingerprint "$ROW_MODE" "$ROW_LINE")"
            printf '%s | %s | %s | 理由 / いつ消えるか\n' "$ROW_CHECK" "${_k%%|*}" "${_k##*|}"
        done <<EOF
$NEW
EOF
        printf '\n'
    fi
    printf 'arch-lint: NG — baseline に無い違反が %d 件あります。\n' "$N_NEW" >&2
    printf '  直すか、直せない理由と落とし所を %s に書いてください。\n' "$BASELINE" >&2
    exit 1
fi
if [ "$N_BASE" -gt 0 ] && [ "${ARCH_LINT_STRICT:-0}" = "1" ]; then
    printf 'arch-lint: NG (STRICT) — baseline 済みの違反が %d 件残っています。\n' "$N_BASE" >&2
    exit 1
fi
if [ "$N_BASE" -eq 0 ]; then
    echo "arch-lint: OK (違反なし)"
else
    echo "arch-lint: OK (新規違反なし。baseline の負債は上記のとおり)"
fi
exit 0
