#!/usr/bin/env bash
#
# vendored FFmpeg (BtbN win64 LGPL shared) を取得する。`make fetch-ffmpeg` の実体。
#
# なぜ「latest から発見」ではなく URL 固定なのか (2026-08-22 に方針を反転)
# ---------------------------------------------------------------------
# 旧実装は BtbN の `releases/tags/latest` の asset 一覧から `n7.1.*win64-lgpl-shared`
# を **発見** していた。理由は「BtbN が asset 名にサフィックスを付け替えるから」。
# ところが 2026-08 に起きたのは名前の変更ではなく **asset の消滅** で、latest には
# master / n8.1 / n9.0 しか残っていない。発見方式では原理的に対応できない
# (見つからなければ落ちるだけ)。fresh machine が何もビルドできない状態だった。
#
# よって: 日付付き autobuild release の **正確な URL + sha256 に固定**する。
# BtbN は古い autobuild をいずれ刈るので、404 になったら **ミラー** へフォールバックする。
# どちらの経路でも同じ sha256 で検証するので、経路が変わっても中身は同一と保証できる。
#
# ミラーの中身 (バイナリ + 対応するソース) は `scripts/prepare_ffmpeg_mirror.sh` が用意する。
# 置き場所と手順は docs/ffmpeg_mirror.md。
#
# 使い方:
#   scripts/fetch_ffmpeg.sh                     # 無ければ取得 (既にあれば何もしない)
#   scripts/fetch_ffmpeg.sh --force             # 既にあっても取り直す
#   scripts/fetch_ffmpeg.sh --dest /tmp/ff      # 別の場所へ (検証用。実体を壊さない)
#   scripts/fetch_ffmpeg.sh --zip path/to.zip   # DL 済み zip から入れる (オフライン / 検証用)
#
# 環境変数で上書きできるもの:
#   FFMPEG_DIR / FFMPEG_URL / FFMPEG_MIRROR_URL / FFMPEG_SHA256
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---- pin (ここが取得元の SSoT) --------------------------------------------
# BtbN/FFmpeg-Builds の autobuild release。lgpl variant なので configure に
# --enable-version3 が付き、GPL 専用ライブラリはすべて無効 = LGPL v3。
FFMPEG_ASSET="ffmpeg-n7.1.5-16-g9a4bb2c579-win64-lgpl-shared-7.1.zip"
FFMPEG_RELEASE_TAG="autobuild-2026-08-16-13-00"
FFMPEG_SHA256_DEFAULT="a950596cea0bf9766f169dae6f1e6eb623aa1ccfd2822cd20cd2874b120d4086"
FFMPEG_SIZE_BYTES="62696009"
# ミラー release の tag。scripts/prepare_ffmpeg_mirror.sh と docs/ffmpeg_mirror.md と揃える。
FFMPEG_MIRROR_TAG="vendor-ffmpeg-n7.1.5-16-g9a4bb2c579"

# ミラーの host は Cargo.toml の [workspace.package] repository から導出する
# (公開先 URL を変えたら 1 箇所直せば About / README / ここが全部追従する)。
repo_url="$(sed -n 's/^repository *= *"\([^"]*\)".*/\1/p' "$ROOT/Cargo.toml" | head -n1)"

FFMPEG_DIR="${FFMPEG_DIR:-$ROOT/third_party/ffmpeg}"
FFMPEG_URL="${FFMPEG_URL:-https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_RELEASE_TAG/$FFMPEG_ASSET}"
FFMPEG_MIRROR_URL="${FFMPEG_MIRROR_URL:-${repo_url:+$repo_url/releases/download/$FFMPEG_MIRROR_TAG/$FFMPEG_ASSET}}"
FFMPEG_SHA256="${FFMPEG_SHA256:-$FFMPEG_SHA256_DEFAULT}"

# BtbN の zip に入っている LGPL v3 全文。取り込みそこねた既存インストールの復旧に使う。
FFMPEG_LGPL3_URL="${FFMPEG_LGPL3_URL:-https://raw.githubusercontent.com/FFmpeg/FFmpeg/master/COPYING.LGPLv3}"

force=0
zip_override=""
while [ $# -gt 0 ]; do
    case "$1" in
        --force) force=1; shift ;;
        --dest) FFMPEG_DIR="$2"; shift 2 ;;
        --url) FFMPEG_URL="$2"; shift 2 ;;
        --mirror) FFMPEG_MIRROR_URL="$2"; shift 2 ;;
        --sha256) FFMPEG_SHA256="$2"; shift 2 ;;
        --zip) zip_override="$2"; shift 2 ;;
        -h|--help)
            # 行番号での抜き出しはヘッダを足すたびにずれる (実際ずれた)。
            # 「使い方」節から最初の空コメント行までを機械的に出す。
            # 「使い方:」からコメントでない最初の行までを出し、その 1 行を落とす。
            sed -n '/^# 使い方:/,/^[^#]/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'
            exit 0 ;;
        *) echo "fetch_ffmpeg: unknown option: $1" >&2; exit 2 ;;
    esac
done

die() { echo "fetch_ffmpeg: ERROR: $*" >&2; exit 1; }
note() { echo "fetch_ffmpeg: $*"; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        die "sha256 を計算できるコマンドが無い (sha256sum / shasum / openssl のいずれかが要る)"
    fi
}

# 既存インストールに LICENSE.txt が無ければ復旧する。LGPL-3.0 §4(b) は
# 「GNU GPL と本ライセンス文書の写し」の同梱を求めるので、バイナリを配る前に必須。
# 旧 Makefile の `cp -r bin lib include` が zip ルートの LICENSE.txt を捨てていた。
restore_license() {
    [ -f "$FFMPEG_DIR/LICENSE.txt" ] && return 0
    note "LICENSE.txt が無い (r.md #60 より前に取得したもの) — LGPLv3 全文を復旧します"
    if curl -fsSL --max-time 60 -o "$FFMPEG_DIR/LICENSE.txt" "$FFMPEG_LGPL3_URL"; then
        return 0
    fi
    rm -f "$FFMPEG_DIR/LICENSE.txt"
    echo "fetch_ffmpeg: WARNING: LGPLv3 の全文を取得できませんでした。" >&2
    echo "fetch_ffmpeg:          バイナリを配布するときは必ず同梱してください (LGPL-3.0 §4(b))。" >&2
    echo "fetch_ffmpeg:          オンラインで 'make fetch-ffmpeg' を再実行すれば入ります。" >&2
    return 0
}

# ---- 既にあるなら何もしない (idempotent) -----------------------------------
if [ -f "$FFMPEG_DIR/lib/avcodec.lib" ] && [ "$force" -eq 0 ]; then
    note "FFmpeg present: $FFMPEG_DIR"
    restore_license
    exit 0
fi

# CLAUDE.md の junction ハザード: third_party を辿る rm で本体を消した事故がある。
# reparse point / symlink の上では入れ替えをしない (手で外してから再実行させる)。
if [ -L "$FFMPEG_DIR" ]; then
    die "$FFMPEG_DIR は symlink / junction です。実体を巻き込んで壊すので触りません。
       先に 'cmd //c rmdir \"$FFMPEG_DIR\"' で reparse point を外してから再実行してください。"
fi

command -v unzip >/dev/null 2>&1 || die "unzip が要ります"
command -v curl  >/dev/null 2>&1 || die "curl が要ります"

# 一時領域は repo 内 (target/tmp) に取り、mixed path (C:/...) で持つ。MSYS の /tmp は
# 環境ごとに実体が違い (Git Bash = %TEMP%、MSYS2 = <msys root>/tmp)、herdr の worktree では
# curl が「Failed to open the file」で書けなかった (2026-09-05)。mixed path なら MSYS 版 /
# Windows 版どちらの curl / unzip でも同じ場所を指す。
tmp_root="$(cygpath -m "$ROOT" 2>/dev/null || printf '%s' "$ROOT")/target/tmp"
mkdir -p "$tmp_root"
tmp="$(mktemp -d "$tmp_root/fetch_ffmpeg.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

zip="$tmp/$FFMPEG_ASSET"

# curl の落とし穴 (curl 8.14.1 で実測、2026-08-22):
# **`--retry` を付けると、最終的に 404 で終わっても exit code が 0 になる**
# (`--retry` 無しなら正しく 22)。exit code だけを見てフォールバックを判断すると、
# 一次 URL が消えた本番でミラーへ回らず「空ファイルを掴んで先へ進む」。
# よって成否は **ファイルが実在して中身があるか** と **sha256** で判定する。
fetch_one() { # $1 = url -> 0 なら $zip に検証済みの中身がある
    local url="$1" actual
    note "downloading $url"
    rm -f "$zip"
    if ! curl -fL --retry 2 --connect-timeout 20 --max-time 1800 -o "$zip" "$url" || [ ! -s "$zip" ]; then
        rm -f "$zip"
        echo "fetch_ffmpeg: 取得に失敗しました (次の候補へ): $url" >&2
        return 1
    fi
    actual="$(sha256_of "$zip")"
    if [ "$actual" != "$FFMPEG_SHA256" ]; then
        rm -f "$zip"
        echo "fetch_ffmpeg: sha256 が一致しません (次の候補へ): $url" >&2
        echo "fetch_ffmpeg:   expected: $FFMPEG_SHA256" >&2
        echo "fetch_ffmpeg:   actual:   $actual" >&2
        return 1
    fi
    note "sha256 ok: $actual"
    return 0
}

if [ -n "$zip_override" ]; then
    [ -f "$zip_override" ] || die "--zip のファイルが無い: $zip_override"
    note "using local zip: $zip_override"
    cp "$zip_override" "$zip"
    actual="$(sha256_of "$zip")"
    if [ "$actual" != "$FFMPEG_SHA256" ]; then
        rm -f "$zip"
        die "sha256 が一致しません。取得物を破棄しました。
       expected: $FFMPEG_SHA256
       actual:   $actual
       期待するのは $FFMPEG_ASSET ($FFMPEG_SIZE_BYTES bytes) です。"
    fi
    note "sha256 ok: $actual"
else
    got=""
    for url in "$FFMPEG_URL" "$FFMPEG_MIRROR_URL"; do
        [ -n "$url" ] || continue
        if fetch_one "$url"; then
            got="$url"
            break
        fi
    done
    [ -n "$got" ] || die "一次 URL もミラーも、検証を通る形で取得できませんでした。
       一次:   $FFMPEG_URL
       ミラー: ${FFMPEG_MIRROR_URL:-(未設定)}
       BtbN が古い autobuild を刈った可能性があります。ミラーがまだ無いなら
       docs/ffmpeg_mirror.md の手順で用意してください。
       期待する中身: $FFMPEG_ASSET ($FFMPEG_SIZE_BYTES bytes, sha256 $FFMPEG_SHA256)"
fi

# ---- 展開して、検証してから入れ替える --------------------------------------
mkdir -p "$tmp/x"
unzip -q "$zip" -d "$tmp/x"
inner="$(find "$tmp/x" -maxdepth 1 -type d -name 'ffmpeg-*' | head -n1)"
[ -n "$inner" ] || die "展開結果に ffmpeg-* ディレクトリがありません"
[ -f "$inner/LICENSE.txt" ] || die "zip のルートに LICENSE.txt がありません (LGPLv3 全文が要る)"

staging="$tmp/staging"
mkdir -p "$staging"
cp -r "$inner/bin" "$inner/lib" "$inner/include" "$staging/"
cp "$inner/LICENSE.txt" "$staging/LICENSE.txt"
[ -f "$staging/lib/avcodec.lib" ] || die "展開結果に lib/avcodec.lib がありません"

# 既存を消すのは **新しい方が揃ってから**。途中で落ちても既存を壊さない
# (旧実装は cp の前に rm -rf していたので、失敗すると復旧不能だった)。
parent="$(dirname "$FFMPEG_DIR")"
mkdir -p "$parent"
old=""
if [ -e "$FFMPEG_DIR" ]; then
    old="$FFMPEG_DIR.old.$$"
    mv "$FFMPEG_DIR" "$old"
fi
if ! mv "$staging" "$FFMPEG_DIR"; then
    [ -n "$old" ] && mv "$old" "$FFMPEG_DIR"
    die "$FFMPEG_DIR への設置に失敗しました (元の状態に戻しました)"
fi
[ -n "$old" ] && rm -rf "$old"

note "FFmpeg fetched into $FFMPEG_DIR ($FFMPEG_ASSET)"
