#!/usr/bin/env bash
# 実行系 make target の前提条件チェック: daw_gui が起動していたら止める。
#
# 使い方: bash scripts/preflight_no_running_app.sh [呼び出し元の target 名]
#         DAW01_SKIP_PREFLIGHT=1 を付けると検査を飛ばす (下記「迂回」参照)。
#
# なぜ要るか
# ----------
# `make test` は `--features daw_gui/script` を付けて daw_gui の全 test target を回す。
# その中の一部 (基準: `grep -l CARGO_BIN_EXE_daw_gui daw_gui/tests/*.rs`) は
# **daw_gui 本体を subprocess として起動**し、それが daw_audio / daw_plugin_host まで
# spawn して audio device を開く。`--script` は窓を出さず single-instance gate も
# 素通りするので、実機を触っている最中に回すと **誰も気付かないまま** audio device を
# 奪い合う。`make run` の二重起動も同じ (IPC を奪い合って後発の窓が入力を受け付けなくなる)。
#
# これは Claude だけの問題ではない。**ユーザーが DAW を開いたまま手で `make test` を
# 打っても同じことが起きる**。だから知識を Makefile 側に置く。
#
# 迂回
# ----
# `DAW01_SKIP_PREFLIGHT=1 make test` で飛ばせる。CI / ヘッドレス環境や、プロセス一覧の
# 取り方がこのスクリプトの想定と違う環境で誤検知したときに詰まないための逃げ道。
# 飛ばしたことは必ず表示する (黙って通さない)。
#
# 判定できないとき
# ----------------
# tasklist も pgrep も ps も無ければ **警告して通す**。「検査できなかった」ことを
# 見えるようにするのが目的で、緑に見せかけないこと自体が要件
# (memory: reference_make_argv_backslash_loss / 偽グリーンを作らない)。
set -u

caller="${1:-make}"
# DAW01_PREFLIGHT_APP は検査対象のプロセス名を差し替える口。この検査自身を
# 「必ず居るプロセス名」で試して**実際に止まること**を確かめるために要る
# (居ないはずの名前で試すだけでは、常に素通りしていても緑に見える)。
APP="${DAW01_PREFLIGHT_APP:-daw_gui}"

if [ "${DAW01_SKIP_PREFLIGHT:-0}" != "0" ]; then
    printf 'preflight: DAW01_SKIP_PREFLIGHT により起動チェックを飛ばしました (%s)\n' "$caller"
    exit 0
fi

running=""
how=""
# プロセス一覧を 1 度だけ取り、**一覧そのものが妥当か**を先に確かめる。
# 「空の出力」を「起動していない」と読むのが偽陰性の作り方なので、
# 行数が明らかに足りなければ「判定できなかった」に倒す (黙って通さない)。
listing=""
if command -v tasklist >/dev/null 2>&1; then
    how="tasklist"
    # **引数を渡さない**。`//FI "IMAGENAME eq ..."` は MSYS のパス変換に晒されるうえ、
    # make 経由だとクロスランタイム起動で引数が壊れる (reference_make_argv_backslash_loss)。
    # 素の tasklist は 1 行目からイメージ名で始まるので、行頭一致で足りる。
    listing="$(tasklist 2>/dev/null)"
    pattern="^$APP([.]exe)?[[:space:]]"
elif command -v pgrep >/dev/null 2>&1; then
    how="pgrep"
    listing="$(pgrep -a . 2>/dev/null || pgrep -l . 2>/dev/null)"
    pattern="(^|[[:space:]/])$APP([[:space:]]|$)"
elif command -v ps >/dev/null 2>&1; then
    how="ps"
    # comm= はコマンド名だけを出すので、grep 自身のコマンドラインに引っかからない。
    listing="$(ps -e -o comm= 2>/dev/null)"
    pattern="(^|/)$APP([.]exe)?$"
else
    printf 'preflight: [警告] プロセス一覧を取る手段がありません (tasklist / pgrep / ps のいずれも無し)。\n' >&2
    printf '  %s が起動しているかを確認できないまま %s を続行します。\n' "$APP" "$caller" >&2
    printf '  実機を触っている最中なら、いったん閉じてから実行してください。\n' >&2
    exit 0
fi

# プローブの妥当性検査。どんなマシンでもプロセスは数十個あるので、数行しか返って
# こないなら一覧の取得自体が失敗している (権限 / 別セッション / 出力エンコーディング)。
# その状態を「起動していない」と読むと、静かに素通りする検査ができあがる。
if [ "$(printf '%s\n' "$listing" | wc -l)" -lt 5 ]; then
    printf 'preflight: [警告] プロセス一覧が取得できていません (%s の出力が %s 行)。\n' \
        "$how" "$(printf '%s\n' "$listing" | wc -l)" >&2
    printf '  %s が起動しているかを判定できないまま %s を続行します。\n' "$APP" "$caller" >&2
    printf '  実機を触っている最中なら、いったん閉じてから実行してください。\n' >&2
    exit 0
fi

if printf '%s\n' "$listing" | grep -qiE "$pattern"; then
    running="$APP"
fi

if [ -n "$running" ]; then
    printf 'preflight: [中止] %s が起動しています (%s で検出)。\n' "$APP" "$how" >&2
    printf '\n' >&2
    printf '  %s は daw_gui を起動するテスト / 実行を含みます。2 つ目のインスタンスが立ち上がり、\n' "$caller" >&2
    printf '  audio device を奪い合って、開いているプロジェクトの再生が壊れます。\n' >&2
    printf '  (script モードのテストは窓を出さず single-instance gate も素通りするので、\n' >&2
    printf '   起動したことに気付けません)\n' >&2
    printf '\n' >&2
    printf '  どうすればよいか:\n' >&2
    printf '    1. daw_gui を閉じてから実行し直す (自分で kill しないこと)\n' >&2
    printf '    2. 起動しないテストだけでよければ  make test-nolaunch\n' >&2
    printf '    3. 承知のうえで回すなら           DAW01_SKIP_PREFLIGHT=1 make %s\n' "$caller" >&2
    exit 1
fi

exit 0
