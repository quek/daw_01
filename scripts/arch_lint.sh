#!/usr/bin/env bash
# アーキテクチャ不変条件の機械検査 (docs/plan_arch_refactor.md §11、CLAUDE.md「アーキテクチャ不変条件」)。
# 使い方: make arch-lint  /  bash scripts/arch_lint.sh
# **exit 0 = 「違反ゼロ、または scripts/arch_lint_baseline.txt に記録済みのものだけ」**。
# baseline に無い違反が 1 件でもあれば exit 1 (行単位 ratchet、下の「ratchet 機構」節)。
# ARCH_LINT_STRICT=1 で baseline 済みの負債も落とす (将来の一掃用)。
# 毎回 `baseline N 件 (解消 K) / 新規 M 件` を出すので、進捗トラッカーとしても使える。
set -u
cd "$(dirname "$0")/.." || exit 1

# 改行 / 復帰。**一時ファイルを使わずすべて shell 変数に持つ**ので、区切り文字を先に定義する
# (mktemp が作る `/tmp/...` は MSYS2 bash (make 経由) と Git の coreutils で **別のルートに
# 解決される**ため、書いた側と読む側がずれて中身が空に見えた。2026-08-22 実測)。
NL='
'
# baseline の行末 CR を落とすのに使う。**この trim が効かないと CRLF の baseline から
# 中身の無いキーが生える**ので、定義した場所で効くことを確かめておく
# (`$(...)` は末尾の改行しか落とさないので CR は残る、という前提の検査)。
CR=$(printf '\r')
_cr_probe="x$CR"
if [ "${#CR}" -ne 1 ] || [ "${_cr_probe%$CR}" != "x" ]; then
    printf 'arch-lint: [SELF-BROKEN] baseline 読み取りの CR trim が効きません。\n' >&2
    printf '  CRLF で保存された baseline から中身の無いキーが生え、毎回「解消」と誤案内します。\n' >&2
    exit 1
fi

# scripts/loc_budget.py を起動する python。Makefile:6 の PYTHON は **make 変数**なので、
# make 経由でも直接起動でも効くよう、環境変数 PYTHON を優先しつつ自前でも探す。
# **見つからなければ落とす** — 検査だけ黙って消えるのは、このファイルが一番警戒している
# false green (「緑だが検査が効いていない」) そのもの。
# なお strip_comments も loc_budget.py に依存するので、python が無い / 壊れていると
# **checks 1-12 が全部止まる**。これは意図的 (cargo-deny と同じ扱い)。
PY="${PYTHON:-}"
[ -n "$PY" ] || PY="$(command -v python 2>/dev/null || true)"
[ -n "$PY" ] || PY="$(command -v python3 2>/dev/null || true)"
if [ -z "$PY" ]; then
    printf 'arch-lint: [SELF-BROKEN] python が見つかりません。arch-lint を実行できません。\n' >&2
    printf '  make arch-lint PYTHON=/path/to/python3 か、PATH を通してください。\n' >&2
    exit 1
fi

# grep -Hn 出力 `path:line:content` から、**その行が実際にコメント (doc 含む) である**
# 行を落とす。行頭 `//` を見るだけの近似だった頃は 2 方向に間違えていた:
#   - raw string 中の行頭 `//` をコメントと誤判定して落とす = 違反の取りこぼし
#   - `/* … */` の中の違反パターンを落とせない = コメント内の言及を違反に数える
# 行分類の SSoT は scripts/loc_budget.py の lexer 1 か所。**パターンを shell に持たせない**。
# r.md #76。
#
# **fail-open にしない。** 呼び出し側は `hits=$(grep … | strip_comments || true)` の形で
# パイプの終了コードを潰すので、python が落ちたときに黙って空を返すと
# **check 1/2/3/5/12 が「違反ゼロ」になって exit 0** になる。代わりに番兵行を stdout へ流し、
# record() がそれを見つけたら SELF-BROKEN で落とす (record は main shell で動くので exit が効く)。
FILTER_BROKEN='LOC-FILTER-BROKEN'
strip_comments() {
    "$PY" scripts/loc_budget.py --filter-comments || printf '%s\n' "$FILTER_BROKEN"
}

# 追跡外の空ディレクトリ列挙 (check 13)。同じ理由で fail-open にしない —
# **「1 件も出なかった」と「走査できなかった」を区別する**ための番兵。
# 列挙が空を返すのは正常な状態でもあるので、失敗を空で表現してはいけない。
EMPTYDIR_BROKEN='EMPTY-DIR-SCAN-BROKEN'
list_empty_dirs() {
    "$PY" scripts/empty_dirs.py || printf '%s\n' "$EMPTYDIR_BROKEN"
}

rs_dirs="common/src daw_gui/src daw_audio/src daw_plugin_host/src ui/crates"

# --- 正規表現にバックスラッシュを使わない (2026-08-22 発覚の偽グリーン対策) ---
# MSYS2 の make は `/usr/bin/bash` を **MSYS2 側の bash** に解決する (実測:
# make 経由 5.2.37(2) / 素のシェル 5.2.37(1) = Git for Windows 版)。その bash から
# **別ランタイムの** Git の grep を起動すると argv のバックスラッシュが落ちる。実測:
#     素のシェル : argv[2]=[HashMap<\(u32,\s*u32\)]
#     make 経由  : argv[2]=[HashMap<(u32,s*u32)]
# 結果 `\( \s \b \[` を含むパターンが全部別物になり、当時 8 チェック中 6 つが無言で
# 無効化され (現在は 12 チェック)、違反 7 行を抱えたまま「OK (違反なし)」を出していた。
# **シェル経由でも壊れない表記**
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

# (3) strip_comments (lexer 版) の配線 canary。**repo の内容に依存させない** —
#     読めないパスの行は「落とさずに残す」契約なので、これが消えたら配線が壊れている
#     (= 違反を黙って捨てる方向の故障)。分類そのものの正しさは loc_budget.py --self-test。
#     **grep 用の canary_ok に合流させない**: ここが落ちる原因は python 側なので、
#     「grep にパターンが通らない」というメッセージを出すと診断が別方向へ逸れる。
if ! printf 'selftest/does_not_exist.rs:1:    pool: HashMap<(u32, u32), Bogus>,\n' \
        | strip_comments 2>/dev/null | grep -q .; then
    printf 'arch-lint: [SELF-BROKEN] strip_comments (scripts/loc_budget.py --filter-comments) が\n' >&2
    printf '  読めないパスの行を素通しできていません。python 側の配線が壊れています。\n' >&2
    printf '  PY=%s\n' "$PY" >&2
    exit 1
fi

# (4) strip_comments が **落ちたときに黙って空を返さない**ことの canary。
#     呼び出し側の `|| true` がパイプの終了コードを潰すので、ここが fail-open だと
#     python が壊れた瞬間に check 1/2/3/5/12 が「違反ゼロ」になって exit 0 する。
_pysave="$PY"
PY="/nonexistent/python-for-arch-lint-canary"
_fb="$(printf 'x.rs:1:y\n' | strip_comments 2>/dev/null)"
PY="$_pysave"
case "$NL$_fb$NL" in
    *"$NL$FILTER_BROKEN$NL"*) : ;;
    *)  printf 'arch-lint: [SELF-BROKEN] strip_comments が失敗を報告しません (fail-open)。\n' >&2
        printf '  python が壊れたとき check 1/2/3/5/12 が黙って「違反ゼロ」になります。\n' >&2
        exit 1 ;;
esac

# (5) 空ディレクトリ検査 (check 13) の自己検証。**この検査は「何も出ない」が正常**
#     なので、検出器が壊れて何も出さなくなっても症状がゼロになる — NVIDIA litter が
#     main + worktree の 9 箇所に何か月も溜まりながら誰も気付かなかったのと同じ形。
#     よって検出器自身に合成ツリーで「実際に検出する / 誤検出しない / 子ではなく親を
#     名指しする / 走査が空振りしたら落ちる」を証明させ、駄目なら即 exit 1。
if ! _ed="$("$PY" scripts/empty_dirs.py --self-test 2>&1)"; then
    printf 'arch-lint: [SELF-BROKEN] empty_dirs.py の self-test が落ちました。\n' >&2
    printf '%s\n' "$_ed" >&2
    exit 1
fi

# (6) その列挙が **落ちたときに黙って空を返さない**ことの canary ((4) と同型)。
_pysave="$PY"
PY="/nonexistent/python-for-arch-lint-canary"
_eb="$(list_empty_dirs 2>/dev/null)"
PY="$_pysave"
case "$NL$_eb$NL" in
    *"$NL$EMPTYDIR_BROKEN$NL"*) : ;;
    *)  printf 'arch-lint: [SELF-BROKEN] 空ディレクトリ列挙が失敗を報告しません (fail-open)。\n' >&2
        printf '  python が壊れたとき check 13 が黙って「違反ゼロ」になります。\n' >&2
        exit 1 ;;
esac

# サイズ budget の判定器 (loc_budget.py) も、上の正規表現 canary と同格で自己検証する。
# **「出力が空 = 違反ゼロ」を信じないための土台**なので、失敗したら即 exit 1。
if ! _st="$("$PY" scripts/loc_budget.py --self-test 2>&1)"; then
    printf 'arch-lint: [SELF-BROKEN] loc_budget.py の self-test が落ちました。\n' >&2
    printf '%s\n' "$_st" >&2
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
# **一時ファイルを使わない** (区切り文字 NL / CR は冒頭で定義済み。理由もそこに書いてある)。
HITS=""
HEADS=""
BASEKEYS=""
SEEN=""
NEW=""

# record <CHECK> <mode> <headline> <hits>
#   mode = grep       … hits が `path:line:content` 形式 (既定)
#          firstfield … hits の第 1 field が path
#          budget     … hits が `key value 人間向けの説明…`。value は `/` 区切りの整数
#                       ベクトル。baseline の第 3 field を **ハッシュではなく天井**として
#                       読み、成分ごとに比較して 1 つでも超えたら新規違反。
#
# 区切りは **半角スペース 1 個**、分解は **パラメータ展開だけ**で行う。タブ区切り +
# `IFS=$(printf '\t')` + `cut -f` でも書けるが、make 経由だと外部コマンドへ渡る引数が
# 壊れて分解が崩れ、fingerprint が環境によって変わった (2026-08-22 実測)。
# CHECK と mode に空白は入らないので、これで曖昧さなく分解できる。
record() {
    [ -n "$4" ] || return 0
    # strip_comments (python) が落ちた印。**握り潰すと「違反ゼロ」になる**ので即落とす。
    case "$NL$4$NL" in
        *"$NL$FILTER_BROKEN$NL"*)
            printf 'arch-lint: [SELF-BROKEN] strip_comments (scripts/loc_budget.py --filter-comments) が\n' >&2
            printf '  異常終了しました。check %s の結果は信用できません (「違反ゼロ」とは判定しません)。\n' "$1" >&2
            printf '  PY=%s\n' "$PY" >&2
            exit 1 ;;
        *"$NL$EMPTYDIR_BROKEN$NL"*)
            printf 'arch-lint: [SELF-BROKEN] 空ディレクトリ列挙 (scripts/empty_dirs.py) が\n' >&2
            printf '  異常終了しました。check %s の結果は信用できません (「違反ゼロ」とは判定しません)。\n' "$1" >&2
            printf '  PY=%s\n' "$PY" >&2
            exit 1 ;;
    esac
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

# baseline_ceiling <CHECK> <key> — budget 行の天井 (整数ベクトル) を **グローバル CEIL** に
# 入れる。無ければ空。**stdout に出して `$(...)` で受けない** — command substitution は
# 1 回ごとに subshell を fork し、Windows では 1 件あたり数十 ms かかる。budget 行は今日
# 164 件あるので、それだけで arch-lint が数秒遅くなる (実測 11.4s → 9.6s)。
# here-doc を while の入力に与える形は subshell を作らないので、代入は呼び出し元に残る。
CEIL=""
baseline_ceiling() {
    CEIL=""
    while IFS= read -r _bk; do
        case "$_bk" in "$1|$2|"*) CEIL="${_bk##*|}"; return 0 ;; esac
    done <<EOF
$BASEKEYS
EOF
    return 1
}

# budget_le <value> <ceiling> — `/` 区切りの整数ベクトルを成分ごとに比較する。
# 全成分が天井以下なら 0。**成分数が違う / 数字でない成分がある場合は 1** (= 新規違反)。
# 書き間違えた baseline 行を「天井無し」として黙って通さないため。
budget_le() {
    _v="$1"; _c="$2"
    while [ -n "$_v" ] || [ -n "$_c" ]; do
        [ -n "$_v" ] || return 1
        [ -n "$_c" ] || return 1
        _v1="${_v%%/*}"; _c1="${_c%%/*}"
        case "$_v1" in ''|*[!0-9]*) return 1 ;; esac
        case "$_c1" in ''|*[!0-9]*) return 1 ;; esac
        [ "$_v1" -le "$_c1" ] || return 1
        if [ "$_v" = "$_v1" ]; then _v=""; else _v="${_v#*/}"; fi
        if [ "$_c" = "$_c1" ]; then _c=""; else _c="${_c#*/}"; fi
    done
    return 0
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
        if [ "$ROW_MODE" = "budget" ]; then
            _bkey="${ROW_LINE%% *}"
            _brest="${ROW_LINE#* }"
            _bval="${_brest%% *}"
            baseline_ceiling "$ROW_CHECK" "$_bkey" || true
            _ceil="$CEIL"
            _blkey="$ROW_CHECK|$_bkey|$_ceil"
            if [ -n "$_ceil" ] && budget_le "$_bval" "$_ceil"; then
                SEEN="$SEEN$_blkey$NL"
                N_BASE=$((N_BASE + 1))
            else
                # **天井を超えたときも SEEN に積む** (baseline 行が存在する場合)。
                # 積まないと下の「解消」ループが同じ baseline 行を「削除してよい」と
                # 案内する = **太ったファイルに対して天井を消せと言う**ことになり、
                # 案内どおりに消すと次回から無検査になる。「解消 (もう違反していない)」と
                # 「超過 (天井を突破した)」は別の事象なので、SEEN への積み方で分離する。
                [ -z "$_ceil" ] || SEEN="$SEEN$_blkey$NL"
                NEW="$NEW$_row$NL"
                N_NEW=$((N_NEW + 1))
            fi
            continue
        fi
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
        # **行末 CR を落とす。** baseline を CRLF で保存すると、空行が `\r` になって
        # 「空行」判定を素通りし、`||` という中身の無い baseline キーが混ざる。それは
        # SEEN に現れないので **毎回「解消 — baseline から削除してよい」と案内される** =
        # 案内どおりに消すと、消したのが実在しない行なので気付けないまま次の人が
        # 本物の行も消す経路になる (2026-08-28 に実際に踏んだ)。
        _bl="${_bl%$CR}"
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

# baseline の読み取りが壊れていないことを確かめる。**成分が欠けたキーが混ざると、それは
# SEEN に現れないので毎回「解消 — baseline から削除してよい」と案内される** =
# 案内に従う人が実在しない行を探し、本物の行まで消す経路になる (2026-08-28 に実際に踏んだ)。
_seen_budget=""
while IFS= read -r _bk; do
    [ -n "$_bk" ] || continue
    case "$_bk" in
        '|'*|*'||'*|*'|'|*"$CR"*)
            printf 'arch-lint: [SELF-BROKEN] baseline に成分の欠けたキーが混ざっています: [%s]\n' "$_bk" >&2
            printf '  %s の行末が CRLF か、`CHECK | key | 天井/ハッシュ | 理由` の書式が壊れています。\n' "$BASELINE" >&2
            exit 1 ;;
    esac
    # budget 行 (第 3 field が `/` 区切りの整数ベクトル) の **キー重複**を弾く。
    # baseline_ceiling は最初にマッチした行を返すので、同じキーが 2 行あると
    # 「古い低い天井で NG のまま、後から足した行は毎回『解消』と案内される」という
    # 抜け出せない状態になる。CHECK 名の一覧を持たずに **値の形**で budget 行を見分ける。
    _bv="${_bk##*|}"
    case "$_bv" in
        ''|*[!0-9/]*) : ;;
        *)  _bkk="${_bk%|*}"
            case "$NL$_seen_budget" in
                *"$NL$_bkk$NL"*)
                    printf 'arch-lint: [SELF-BROKEN] baseline に同じ budget キーが 2 行あります: %s\n' "$_bkk" >&2
                    printf '  天井は 1 キー 1 行です。**行を足すのではなく既存行の第 3 field を書き換えて**ください。\n' >&2
                    exit 1 ;;
            esac
            _seen_budget="$_seen_budget$_bkk$NL" ;;
    esac
done <<EOF
$BASEKEYS
EOF

# ratchet 自体が効くことを毎回確かめる。**これが無いなら ratchet を入れる意味が無い**
# — 「新規違反で落ちる」を検査しないと、今日 3 回見た偽グリーンが 1 段上に移るだけ。
classify 'SELFTEST grep selftest/synthetic.rs:1:    pool: HashMap<(u32, u32), Bogus>,' 1
if [ "$N_NEW" -ne 1 ]; then
    printf 'arch-lint: [SELF-BROKEN] ratchet が新規違反を検出できていません (N_NEW=%s)。\n' "$N_NEW" >&2
    printf '  baseline に無い違反を「新規」に分類できない状態では、違反ゼロの報告は信用できません。\n' >&2
    exit 1
fi

# budget モードにも同じ強度の自己検証を置く。**これが無いと新モードだけ無検証になる**。
# classify は毎回 SEEN / NEW / カウンタを作り直すので、最後の本番 classify に影響しない。
_budget_broken=0
_bk_save="$BASEKEYS"

BASEKEYS="SELFTEST-BUDGET|selftest/a.rs|100$NL"
classify 'SELFTEST-BUDGET budget selftest/a.rs 100 ncloc' 1
{ [ "$N_BASE" -eq 1 ] && [ "$N_NEW" -eq 0 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-BUDGET budget selftest/a.rs 101 ncloc' 1
# 超過は「新規違反」に出て、かつ「解消」には出ない (baseline 行を消せと案内しない)
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-BUDGET budget selftest/a.rs 99 ncloc' 1
{ [ "$N_NEW" -eq 0 ] && [ "$N_BASE" -eq 1 ]; } || _budget_broken=1   # 縮んだら緑
classify 'SELFTEST-BUDGET budget selftest/z.rs 1 ncloc' 1
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 1 ]; } || _budget_broken=1   # 未記録は必ず新規

BASEKEYS="SELFTEST-NEST|selftest/b.rs::f|7/20$NL"
classify 'SELFTEST-NEST budget selftest/b.rs::f 7/20 indent' 1
{ [ "$N_BASE" -eq 1 ] && [ "$N_NEW" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 7/21 indent' 1
# 深さ据え置きで「6 段以上の行」だけ増えても新規違反になること (FN-NESTING の肥大検出)
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 8/20 indent' 1
{ [ "$N_NEW" -eq 1 ] && [ "$N_RESOLVED" -eq 0 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 6/5 indent' 1
{ [ "$N_NEW" -eq 0 ] && [ "$N_BASE" -eq 1 ]; } || _budget_broken=1
classify 'SELFTEST-NEST budget selftest/b.rs::f 7/x indent' 1
{ [ "$N_NEW" -eq 1 ]; } || _budget_broken=1   # 数字でない成分を天井無し扱いにしない

BASEKEYS="$_bk_save"
if [ "$_budget_broken" = "1" ]; then
    printf 'arch-lint: [SELF-BROKEN] budget モードの ratchet が壊れています。\n' >&2
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

# 6-10. サイズ budget (不変条件 9)。**測り方の SSoT は scripts/loc_budget.py**
#      (Rust の字句解析。実コード行だけを数え、テスト / doc / コメント / 空行は数えない)。
#      物理行 (wc -l) で測っていた頃は「テストを厚くすると分割を迫られる /
#      doc を書くと分割を迫られる」逆インセンティブになっていて、実際に tests を別ファイルへ
#      移すだけの commit が 2 件生えた (eefdea1 / 720e2c1)。r.md #76。
#      **パターンを shell に持たせない** — 冒頭に書いた argv バックスラッシュ消失の
#      地雷原に戻らないため。ここは python の stdout を読むだけ。
_budget_rc=0
budget_out="$("$PY" scripts/loc_budget.py --check 2>&1)" || _budget_rc=$?
if [ "$_budget_rc" -ne 0 ]; then
    printf 'arch-lint: [SELF-BROKEN] loc_budget.py が exit %s で落ちました。\n' "$_budget_rc" >&2
    printf '  走査が空 / 保存則違反 / git 失敗のいずれか。**緑にはしません**。\n' >&2
    printf '%s\n' "$budget_out" >&2
    exit 1
fi
case "$NL$budget_out$NL" in
    *"${NL}LOC-BUDGET-OK "*) : ;;
    *)  printf 'arch-lint: [SELF-BROKEN] loc_budget.py が完走マーカーを出しませんでした。\n' >&2
        printf '  出力が空でも「違反ゼロ」とは判定しません。\n' >&2
        printf '%s\n' "$budget_out" >&2
        exit 1 ;;
esac

# pick <CHECK> — budget_out から該当行を取り出し、先頭の CHECK 名を落とす。
pick() {
    while IFS= read -r _l; do
        case "$_l" in "$1 "*) printf '%s\n' "${_l#* }" ;; esac
    done <<EOF
$budget_out
EOF
}

# pick の配線 canary。**5 本すべてを試す。** ここが黙って空を返すようになると
# 「違反ゼロ」に見えたうえで baseline 全行が「解消 — 削除してよい」と案内され、
# 案内どおり消すと検査が永久に消える (この項目が塞ごうとしている false green そのもの)。
_bo_save="$budget_out"
budget_out="FILE-BUDGET selftest/a.rs 9999 ncloc>1000
FN-BUDGET selftest/a.rs::f 999 ncloc>300
FN-NESTING selftest/a.rs::f 9/9 indent>6
UNRESOLVED-MOD selftest/a.rs:1:mod nope;
KEY-COLLISION selftest/a.rs:2:key collision: k
LOC-BUDGET-OK 1 files"
_pick_ok=1
[ "$(pick FILE-BUDGET)" = "selftest/a.rs 9999 ncloc>1000" ] || _pick_ok=0
[ "$(pick FN-BUDGET)" = "selftest/a.rs::f 999 ncloc>300" ] || _pick_ok=0
[ "$(pick FN-NESTING)" = "selftest/a.rs::f 9/9 indent>6" ] || _pick_ok=0
[ "$(pick UNRESOLVED-MOD)" = "selftest/a.rs:1:mod nope;" ] || _pick_ok=0
[ "$(pick KEY-COLLISION)" = "selftest/a.rs:2:key collision: k" ] || _pick_ok=0
[ -z "$(pick NO-SUCH-CHECK)" ] || _pick_ok=0
budget_out="$_bo_save"
if [ "$_pick_ok" != "1" ]; then
    printf 'arch-lint: [SELF-BROKEN] loc_budget.py の出力を取り出す配線 (pick) が壊れています。\n' >&2
    printf '  サイズ budget の違反が黙って 0 件になります。\n' >&2
    exit 1
fi

record FILE-BUDGET budget "実コード 1,000 行超の .rs (テスト/doc/コメントは数えない。分割してから足す):" "$(pick FILE-BUDGET)"
record FN-BUDGET budget "実コード 300 行超の関数 (単位を切って分割する):" "$(pick FN-BUDGET)"
record FN-NESTING budget "インデント 6 段を超える関数 (早期 return / ヘルパ抽出でほどく。計測値は 最大段数/6段以上の行数):" "$(pick FN-NESTING)"
record UNRESOLVED-MOD grep "#[cfg(test)] mod の解決に失敗 (#[path] 属性か。テストが production として課金される = 測定のバグ):" "$(pick UNRESOLVED-MOD)"
record KEY-COLLISION grep "関数キーが衝突 (2 関数が 1 つの天井を共有し、片方の違反が消える = 測定のバグ):" "$(pick KEY-COLLISION)"

# 11. common の依存縮退 (plan §9): common = model + protocol + wire/shmem + 純関数。
#    HTTP / GUI / スキャナ系の重量依存を持たない。
hits=$(grep -HnE '^(reqwest|rfd|image|winit|wgpu)[^A-Za-z0-9_-]' common/Cargo.toml 2>/dev/null || true)
record COMMON-DEPS grep "common に域外依存 (GUI/HTTP へ移設する):" "$hits"

# 12. daw-ui core のドメイン知識 (不変条件 8、S4 以降 0 が期待値)。
#    行頭コメント (`//` `///` `//!`) 内の言及 (撤去の履歴・rationale 説明) は実装では
#    ないので除外し、実コードのドメイン参照だけを検出する。
hits=$(grep -rnE 'ArrangementEditRequest|split_into_morae|Edit::Undoable' \
    --include='*.rs' ui/crates/ui/src 2>/dev/null | strip_comments || true)
record UI-DOMAIN grep "daw-ui core に DAW ドメイン/mirror 機構が残存 (daw_gui 側へ):" "$hits"

# 13. 追跡外の空ディレクトリ (r.md #81)。**git では原理的に検出できない** —
#     git はディレクトリを追跡しないので、ファイルを 1 つも含まないディレクトリは
#     `??` にも `!!` にも出ず、check-ignore も「どのルールにも当たらない」を返すだけ。
#     実際 `NVIDIA Corporation/umdlogs` (GPU ドライバが %ProgramData% を解決できず cwd に
#     作る) と `daw_guitestsfixtures` (shell が backslash を落とした跡) が main + worktree の
#     9 箇所に何か月も溜まっていて、誰も気付けなかった。
#     **.gitignore に足すのは禁止** — もともと出力に出ていないので症状は消えず、
#     原因が残ったまま「無害な既知のゴミ」に格上げされるだけ。
#     mode は firstfield (hits が path だけ)。列挙と自己検証は empty_dirs.py が持つ
#     (パターンを shell に書かない = 上の backslash 節の方針)。
hits=$(list_empty_dirs || true)
record EMPTY-DIR firstfield "追跡外の空ディレクトリ (git では見えない。原因を直して削除する。.gitignore に足さない):" "$hits"

# ---------------------------------------------------------------- 判定
# 「検査器が実際に何を見たか」を毎回可視化する (出力が空 = 違反ゼロ、を信じないための土台)。
while IFS= read -r _l; do
    case "$_l" in "LOC-BUDGET-OK "*) printf 'arch-lint: [size] %s\n' "${_l#* }" ;; esac
done <<EOF
$budget_out
EOF

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
            if [ "$ROW_MODE" = "budget" ]; then
                _bkey="${ROW_LINE%% *}"
                _brest="${ROW_LINE#* }"
                baseline_ceiling "$ROW_CHECK" "$_bkey" || true
                _old="$CEIL"
                if [ -n "$_old" ]; then
                    # **既にある行の天井を突破したケース。行を足させない** — 同じキーが
                    # 2 行あると baseline_ceiling が古い方を返し続けて永久に NG のまま、
                    # かつ足した行が毎回「解消」と案内される (抜け出せない状態になる)。
                    printf '# 既存行の天井を書き換える (行を足さない): %s | %s | %s -> %s\n' \
                        "$ROW_CHECK" "$_bkey" "$_old" "${_brest%% *}"
                else
                    printf '%s | %s | %s | 理由 / いつ消えるか\n' "$ROW_CHECK" "$_bkey" "${_brest%% *}"
                fi
            else
                _k="$(fingerprint "$ROW_MODE" "$ROW_LINE")"
                printf '%s | %s | %s | 理由 / いつ消えるか\n' "$ROW_CHECK" "${_k%%|*}" "${_k##*|}"
            fi
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
