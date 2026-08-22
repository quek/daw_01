#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
# SPDX-License-Identifier: GPL-3.0-or-later
#
# ミラー release に上げる成果物を `dist/ffmpeg-mirror/` に用意する。
# **このスクリプトはアップロードしない。** ファイルを揃えるところまで。
# 上げ方は docs/ffmpeg_mirror.md。
#
# なぜミラーが要るか
# ------------------
# BtbN/FFmpeg-Builds の保持ポリシーは「月末ビルドは 2 年、日次ビルドは直近 14 日」
# (リポジトリ README の Release Retention Policy)。我々が pin している
# autobuild-2026-08-16-13-00 は日次ビルドなので、放っておけば消える。
# 実際 2026-08 には latest から n7.1 系 asset が丸ごと消えて、fresh machine が
# 何もビルドできない状態になった。
#
# 何を置くか — LGPL の「対応するソース」
# --------------------------------------
# ミラーに zip を置く = **我々が FFmpeg のバイナリを convey する**ということ。
# LGPL-3.0 は GPL-3.0 の条項を取り込んでいるので、GPL-3.0 §6(d) が効く:
# 「object code を置いた場所から、対応するソースにも equivalent access を提供せよ」。
# 対応するソース (GPL-3.0 §1) は「その object code を生成・インストール・実行し、
# 改変するのに必要な全ソース。**それらの作業を制御するスクリプトを含む**」。
#
# したがって同じ release に 3 つ置く:
#   1. バイナリ zip (BtbN の**無改変**ビルド)
#   2. FFmpeg のソース (asset 名が示す exact commit)
#   3. BtbN のビルドレシピ (= 「作業を制御するスクリプト」。scripts.d/*.sh が
#      静的リンクされる各依存の版を SCRIPT_COMMIT / SCRIPT_REV / SCRIPT_HGREV で
#      ピン止めしているので、これがあれば全依存の版が確定する)
#
# 完全性の担保は **commit SHA** で取る。GitHub の archive tarball は生成時の
# メタデータでバイト列が変わりうる (同一内容でも commit 指定とタグ指定でサイズが違う
# ことを実測) ので、tarball の sha256 を同一性の根拠にしない。ここでは git で
# fetch して `rev-parse` が pin と一致することを確かめてから tar を作る。
#
# 使い方:
#   scripts/prepare_ffmpeg_mirror.sh                 # dist/ffmpeg-mirror/ に用意
#   scripts/prepare_ffmpeg_mirror.sh --out DIR       # 出力先を変える
#   scripts/prepare_ffmpeg_mirror.sh --zip PATH      # DL 済みバイナリ zip を使う
#   scripts/prepare_ffmpeg_mirror.sh --dry-run       # 取得せず、対象依存を列挙するだけ
#   scripts/prepare_ffmpeg_mirror.sh --no-deps       # 外部ライブラリのソースを入れない
#                                                    # (**不完全**。配布に使ってはいけない)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---- pin (fetch_ffmpeg.sh と揃える。ここを動かすなら向こうも動かす) --------
FFMPEG_ASSET="ffmpeg-n7.1.5-16-g9a4bb2c579-win64-lgpl-shared-7.1.zip"
FFMPEG_RELEASE_TAG="autobuild-2026-08-16-13-00"
FFMPEG_SHA256="a950596cea0bf9766f169dae6f1e6eb623aa1ccfd2822cd20cd2874b120d4086"
BTBN_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_RELEASE_TAG/$FFMPEG_ASSET"

# asset 名 n7.1.5-16-g9a4bb2c579 は FFmpeg の `git describe` 出力。実 commit はこれ。
FFMPEG_SRC_REPO="https://github.com/FFmpeg/FFmpeg.git"
FFMPEG_SRC_COMMIT="9a4bb2c579a16b0469759743d6917d9e8e3cb8c6"
# release tag が指す BtbN のレシピ commit (release API の target_commitish は
# "master" という**ブランチ名文字列**なので使えない。git/refs/tags から取った値)。
BTBN_RECIPE_REPO="https://github.com/BtbN/FFmpeg-Builds.git"
BTBN_RECIPE_COMMIT="590a6612d7d961e9258429e501619e0b7d7cbedf"

# BtbN の build target。asset 名 (win64 / lgpl-shared / addin 7.1) と対応させる。
# レシピ自身の ffbuild_enabled にこの 3 つを渡して「このビルドに入る依存」を確定する。
BTBN_TARGET="win64"
BTBN_VARIANT="lgpl-shared"
BTBN_ADDIN="7.1"

OUT="$ROOT/dist/ffmpeg-mirror"
zip_override=""
with_deps=1
dry_run=0
while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="$2"; shift 2 ;;
        --zip) zip_override="$2"; shift 2 ;;
        --no-deps) with_deps=0; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help)
            # 「使い方:」からコメントでない最初の行までを出し、その 1 行を落とす。
            sed -n '/^# 使い方:/,/^[^#]/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'
            exit 0 ;;
        *) echo "prepare_ffmpeg_mirror: unknown option: $1" >&2; exit 2 ;;
    esac
done

die() { echo "prepare_ffmpeg_mirror: ERROR: $*" >&2; exit 1; }
note() { echo "prepare_ffmpeg_mirror: $*"; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | awk '{print $NF}'
    else die "sha256 を計算できるコマンドが無い"; fi
}

command -v git >/dev/null 2>&1 || die "git が要ります"
command -v curl >/dev/null 2>&1 || die "curl が要ります"
command -v tar >/dev/null 2>&1 || die "tar が要ります"

mkdir -p "$OUT"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/ffmirror.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

# ---- 1. バイナリ (BtbN の無改変ビルド) -------------------------------------
bin_out="$OUT/$FFMPEG_ASSET"
if [ -f "$bin_out" ] && [ "$(sha256_of "$bin_out")" = "$FFMPEG_SHA256" ]; then
    note "binary already staged: $FFMPEG_ASSET"
elif [ -n "$zip_override" ]; then
    [ -f "$zip_override" ] || die "--zip のファイルが無い: $zip_override"
    cp "$zip_override" "$bin_out"
else
    note "downloading $BTBN_URL"
    curl -fL --connect-timeout 20 --max-time 1800 -o "$bin_out" "$BTBN_URL" \
        || die "BtbN からバイナリを取得できませんでした ($BTBN_URL)。
       日次ビルドは約 14 日で prune されます。既にローカルにあるなら --zip で渡してください。"
fi
actual="$(sha256_of "$bin_out")"
[ "$actual" = "$FFMPEG_SHA256" ] || die "バイナリの sha256 が pin と一致しません。
       expected: $FFMPEG_SHA256
       actual:   $actual"
note "binary sha256 ok"

# ---- 2/3. ソース (commit SHA で完全性を担保して tar を作る) ----------------
archive_commit() { # $1=repo url  $2=commit  $3=prefix  $4=out.tar.gz
    local url="$1" commit="$2" prefix="$3" out="$4"
    # `local a=... b="$a"` は同一 local 文の中で $a をまだ見られない (set -u で落ちる)。
    # work は必ず別の local 文で作る。
    local work="$tmp/w-$(basename "$out" .tar.gz)"
    if [ -f "$out" ]; then
        note "source already staged: $(basename "$out")"
        return 0
    fi
    note "fetching $url @ $commit"
    git init -q "$work"
    git -C "$work" remote add origin "$url"
    # GitHub は SHA 指定の fetch を許可している (uploadpack.allowReachableSHA1InWant)。
    git -C "$work" fetch -q --depth 1 origin "$commit"
    local head
    head="$(git -C "$work" rev-parse FETCH_HEAD)"
    [ "$head" = "$commit" ] || die "fetch した commit が pin と違う: $head != $commit"
    # gzip -n = タイムスタンプを埋めない (同じ git / gzip なら同じバイト列になる)。
    git -C "$work" archive --format=tar --prefix="$prefix/" FETCH_HEAD | gzip -n -9 > "$out"
    note "  -> $(basename "$out") ($(sha256_of "$out"))"
}

archive_commit "$FFMPEG_SRC_REPO"  "$FFMPEG_SRC_COMMIT" \
    "ffmpeg-$FFMPEG_SRC_COMMIT" "$OUT/ffmpeg-source-$FFMPEG_SRC_COMMIT.tar.gz"
archive_commit "$BTBN_RECIPE_REPO" "$BTBN_RECIPE_COMMIT" \
    "FFmpeg-Builds-$BTBN_RECIPE_COMMIT" "$OUT/ffmpeg-builds-recipe-$BTBN_RECIPE_COMMIT.tar.gz"

# ---- 3b. DLL に静的リンクされた外部ライブラリのソース ----------------------
# BtbN は --pkg-config-flags=--static でこれらを **DLL の中に取り込む**ので、
# GPL-3.0 §1 の除外句「the work の一部ではない、未改変で一般に入手可能な汎用ツール」
# には当たらない = Corresponding Source に含まれる。
# 「どれが入るか」はレシピ自身の ffbuild_enabled が唯一の正解なので、推測せず
# scripts.d/* を実際に source して判定する (win64 / lgpl-shared / addin 7.1)。
# 判定は over-inclusive 側に倒す: ビルドツール (cmake / nasm 等) も含めて全部入れる。
# 足りないより多い方が安全で、何が足りないかを人が判断する必要が無くなる。
DEPS_OUT="$OUT/ffmpeg-external-sources-$BTBN_RECIPE_COMMIT.tar.gz"

enumerate_deps() { # レシピを展開したディレクトリで、有効な stage の取得先を列挙する
    local recipe="$1"
    ( cd "$recipe" && for stage in scripts.d/*.sh scripts.d/*/*.sh; do
        [ -f "$stage" ] || continue
        (
            SELF="$stage"
            # shellcheck disable=SC1091
            source util/vars.sh "$BTBN_TARGET" "$BTBN_VARIANT" "$BTBN_ADDIN" >/dev/null 2>&1
            # shellcheck disable=SC1090
            source "$stage" >/dev/null 2>&1
            ffbuild_enabled >/dev/null 2>&1 || exit 0
            name="$(basename "$stage" .sh)"
            # SCRIPT_REPO を優先する。BtbN の clone は SCRIPT_MIRROR を使う script が
            # あるが (libiconv)、その mirror は `git://` で塞がれている環境が多い。
            # どちらも同じ履歴なので、https 側を先に試して両方を候補に残す。
            repo="${SCRIPT_REPO:-${SCRIPT_MIRROR:-}}"
            alt="${SCRIPT_MIRROR:-}"
            [ "$alt" = "$repo" ] && alt=""
            [ -n "$repo" ] || exit 0
            repo="$repo${alt:+ $alt}"
            if [ -n "${SCRIPT_REV:-}" ]; then
                printf '%s\tsvn\t%s\t%s\n' "$name" "$repo" "$SCRIPT_REV"
            elif [ -n "${SCRIPT_COMMIT:-}" ]; then
                printf '%s\tgit\t%s\t%s\n' "$name" "$repo" "$SCRIPT_COMMIT"
            fi
            # libiconv だけ 2 本目 (gnulib) を持つ。
            if [ -n "${SCRIPT_MIRROR2:-}" ] && [ -n "${SCRIPT_COMMIT2:-}" ]; then
                printf '%s\tgit\t%s\t%s\n' "$name-2" "$SCRIPT_MIRROR2" "$SCRIPT_COMMIT2"
            fi
        )
    done ) | sort -u
}

fetch_git_one_url() { # $1=url $2=rev(SHA かタグ) $3=destdir
    local url="$1" rev="$2" dest="$3"
    rm -rf "$dest"
    # ① SHA 直 fetch (GitHub / GitLab は許可)。② partial clone。③ 素の clone。
    # ホストによって使える手が違うので順に落とす。
    if git init -q "$dest" 2>/dev/null \
        && git -C "$dest" remote add origin "$url" 2>/dev/null \
        && git -C "$dest" fetch -q --depth 1 origin "$rev" 2>/dev/null \
        && git -C "$dest" checkout -q FETCH_HEAD 2>/dev/null; then
        :
    else
        rm -rf "$dest"
        if ! git clone -q --filter=blob:none "$url" "$dest" 2>/dev/null; then
            rm -rf "$dest"
            git clone -q "$url" "$dest" 2>/dev/null || return 1
        fi
        git -C "$dest" checkout -q "$rev" 2>/dev/null || return 1
    fi
    # pin と一致するか。SCRIPT_COMMIT は **タグ名のこともある** (openssl-3.6.3 /
    # v4.2.0 / v1.4.359)。しかも `git fetch --depth 1 origin <tag>` はローカルに
    # tag ref を作らないので `rev-parse <tag>^{commit}` は解決できない (実測)。
    # そこで:
    #   - 40 桁 SHA の pin  … HEAD と厳密一致を要求する
    #   - タグ名の pin      … サーバに **その ref を指定して**取った結果が FETCH_HEAD
    #                        なので、ローカルで解決できるならそれと一致を要求し、
    #                        解決できないなら server の回答をそのまま採る
    local head want
    head="$(git -C "$dest" rev-parse HEAD 2>/dev/null)" || return 1
    if printf '%s' "$rev" | grep -qE '^[0-9a-f]{40}$'; then
        [ "$head" = "$rev" ] || return 1
    else
        # `git rev-parse <解決できない ref>` は失敗しても **引数の文字列をそのまま
        # stdout に出す**。2>/dev/null だけでは掴めないので `--verify -q` を使う
        # (解決できなければ何も出さずに非ゼロ)。ここを間違えると tag pin の依存
        # (openssl / mbedtls / vulkan-headers) が「取れたのに不一致」で全部落ちる。
        want="$(git -C "$dest" rev-parse --verify -q "${rev}^{commit}" 2>/dev/null || true)"
        if printf '%s' "$want" | grep -qE '^[0-9a-f]{40}$' && [ "$want" != "$head" ]; then
            return 1
        fi
    fi
    printf '%s' "$head" > "$dest/../.resolved-$(basename "$dest")"
    rm -rf "$dest/.git"
    return 0
}

# svn の依存は 1 本だけ (libmp3lame)。svn クライアントが無い環境でも Corresponding
# Source を欠かさないよう、SourceForge の snapshot API を fallback に持つ
# (POST でスナップショット生成を要求 → tarball_status を polling → 生成された
#  code-snapshots の zip を取る)。revision が URL に入るので pin は保たれる。
fetch_svn() { # $1=url $2=rev $3=destdir
    local url="$1" rev="$2" dest="$3"
    rm -rf "$dest"
    if command -v svn >/dev/null 2>&1; then
        svn export -q -r "$rev" "$url" "$dest" && return 0
        rm -rf "$dest"
    fi
    local project path page status zipurl i
    case "$url" in
        https://svn.code.sf.net/p/*/svn/*) ;;
        *) echo "prepare_ffmpeg_mirror:     svn クライアントが無く、SourceForge でもない: $url" >&2
           return 1 ;;
    esac
    project="$(printf '%s' "$url" | sed -n 's#https://svn.code.sf.net/p/\([^/]*\)/svn/.*#\1#p')"
    path="$(printf '%s' "$url" | sed -n 's#https://svn.code.sf.net/p/[^/]*/svn\(/.*\)#\1#p')"
    page="https://sourceforge.net/p/$project/svn/$rev/tarball?path=$path"
    echo "prepare_ffmpeg_mirror:     svn 無し → SourceForge snapshot API を使います" >&2
    curl -fsSL -X POST -o /dev/null "$page" || return 1
    for i in $(seq 1 60); do
        status="$(curl -fsSL "https://sourceforge.net/p/$project/svn/$rev/tarball_status?path=$path" || true)"
        case "$status" in *complete*) break ;; esac
        sleep 5
    done
    case "$status" in *complete*) ;; *) echo "prepare_ffmpeg_mirror:     snapshot 生成が完了しません" >&2; return 1 ;; esac
    zipurl="$(curl -fsSL "$page" | grep -oE 'https?://[^"'"'"']*code-snapshots[^"'"'"']*\.zip' | head -n1)"
    [ -n "$zipurl" ] || return 1
    curl -fL -o "$tmp/svnsnap.zip" "$zipurl" || return 1
    rm -rf "$tmp/svnsnap"
    unzip -q "$tmp/svnsnap.zip" -d "$tmp/svnsnap" || return 1
    local inner
    inner="$(find "$tmp/svnsnap" -mindepth 1 -maxdepth 1 -type d | head -n1)"
    [ -n "$inner" ] || return 1
    mv "$inner" "$dest"
    rm -rf "$tmp/svnsnap" "$tmp/svnsnap.zip"
    return 0
}

fetch_git_commit() { # $1="url [alt-url]" $2=rev $3=destdir
    local urls="$1" rev="$2" dest="$3" u
    for u in $urls; do
        if fetch_git_one_url "$u" "$rev" "$dest"; then
            return 0
        fi
        echo "prepare_ffmpeg_mirror:     取得できず (次の URL を試します): $u" >&2
    done
    return 1
}

if [ "$with_deps" -eq 1 ]; then
    if [ -f "$DEPS_OUT" ]; then
        note "external sources already staged: $(basename "$DEPS_OUT")"
    else
        recipe_dir="$tmp/recipe"
        mkdir -p "$recipe_dir"
        tar xzf "$OUT/ffmpeg-builds-recipe-$BTBN_RECIPE_COMMIT.tar.gz" -C "$recipe_dir" --strip-components=1
        deps_list="$tmp/deps.tsv"
        enumerate_deps "$recipe_dir" > "$deps_list"
        note "external sources to fetch: $(wc -l < "$deps_list") (target=$BTBN_TARGET variant=$BTBN_VARIANT addin=$BTBN_ADDIN)"

        if [ "$dry_run" -eq 1 ]; then
            echo
            cat "$deps_list"
            echo
            note "--dry-run: 取得はしていません"
            exit 0
        fi

        # 作業ディレクトリは tmp ではなく OUT 配下に置く。76 リポジトリ ≒ 2GB 超を
        # 取るので、1 件失敗しただけで全部やり直しになるのは無駄が大きい。
        # 完了したものは .done マーカーで飛ばす (再実行が差分になる)。
        work="$OUT/.work/extsrc"
        mkdir -p "$work"
        failed=""
        while IFS=$'\t' read -r name kind url rev; do
            [ -n "$name" ] || continue
            if [ -f "$work/.done-$name" ]; then
                note "  [$kind] $name  (取得済み)"
                continue
            fi
            note "  [$kind] $name  $url @ $rev"
            case "$kind" in
                git)
                    if fetch_git_commit "$url" "$rev" "$work/$name"; then
                        : > "$work/.done-$name"
                    else
                        failed="$failed $name"
                    fi ;;
                svn)
                    if fetch_svn "$url" "$rev" "$work/$name"; then
                        : > "$work/.done-$name"
                    else
                        failed="$failed $name"
                    fi ;;
            esac
        done < "$deps_list"

        if [ -n "$failed" ]; then
            # 取れなかったものを黙って落とすと「不完全な Corresponding Source」を
            # 配ることになる。ここは必ず失敗させる。
            die "次の依存ソースを取得できませんでした:$failed
       ネットワークを確認して再実行してください (取得済みのものは飛ばします)。
       不完全なバンドルは配布に使えません (GPL-3.0 §6 は complete sources を要求する)。"
        fi

        # pin が tag のものは実際に解決された commit を manifest に残す
        # (「どの commit を配ったか」が後から辿れるように)。
        {
            printf 'stage\tkind\turl\tpinned_rev\tresolved_commit\n'
            while IFS=$'\t' read -r name kind url rev; do
                [ -n "$name" ] || continue
                printf '%s\t%s\t%s\t%s\t%s\n' "$name" "$kind" "$url" "$rev" \
                    "$(cat "$work/.resolved-$name" 2>/dev/null || echo '-')"
            done < "$deps_list"
        } > "$work/MANIFEST.tsv"

        ( cd "$work/.." && tar --exclude='.done-*' --exclude='.resolved-*' -cf - extsrc ) \
            | gzip -n -9 > "$DEPS_OUT"
        note "  -> $(basename "$DEPS_OUT") ($(sha256_of "$DEPS_OUT"))"
    fi
else
    note "WARNING: --no-deps — 静的リンクされた外部ライブラリのソースを入れていません"
    cat > "$OUT/INCOMPLETE.txt" <<'EOF'
This staging directory was produced with --no-deps and is NOT a complete
Corresponding Source bundle.  Do not publish it.  Re-run
scripts/prepare_ffmpeg_mirror.sh without --no-deps.
EOF
fi

# ---- 3c. ライセンス本文 ----------------------------------------------------
# BtbN の zip に入っている LICENSE.txt は LGPLv3 **本文のみ**。LGPLv3 は GPLv3 への
# 追加許諾なので、GPLv3 本文が無いと条件が読めない。ミラー側で必ず添える。
cp "$ROOT/LICENSES/GPL-3.0-or-later.txt" "$OUT/gpl-3.0.txt"
if [ ! -f "$OUT/lgpl-3.0.txt" ]; then
    unzip -p "$bin_out" '*/LICENSE.txt' > "$OUT/lgpl-3.0.txt" 2>/dev/null \
        || curl -fsSL --max-time 60 -o "$OUT/lgpl-3.0.txt" \
             "https://raw.githubusercontent.com/FFmpeg/FFmpeg/master/COPYING.LGPLv3"
fi
[ -s "$OUT/lgpl-3.0.txt" ] || die "LGPLv3 本文を用意できませんでした"

# ---- 3d. configure 行 (FFmpeg legal.html が明示的に要求する) ---------------
# 「どうコンパイルしたか、例えば configure 行を、ソースのルートに置いたテキストで示せ」。
# 表示ではなく **実物から** 取る (ハードコードすると実際のビルドと乖離する)。
if [ ! -s "$OUT/BUILD_CONFIGURATION.txt" ]; then
    ffdir="$tmp/ffbin"
    mkdir -p "$ffdir"
    unzip -q -o "$bin_out" -d "$ffdir"
    ffexe="$(find "$ffdir" -name 'ffmpeg.exe' | head -n1)"
    conf=""
    if [ -n "$ffexe" ] && "$ffexe" -version >/dev/null 2>&1; then
        conf="$("$ffexe" -version 2>/dev/null | sed -n 's/^configuration: //p')"
    fi
    if [ -z "$conf" ]; then
        # 実行できない環境 (Linux 等) では DLL に埋まっている文字列から拾う。
        avutil="$(find "$ffdir" -name 'avutil-*.dll' | head -n1)"
        [ -n "$avutil" ] && conf="$(LC_ALL=C tr -c '[:print:]' '\n' < "$avutil" \
            | grep -m1 -- '--prefix=/ffbuild/prefix' || true)"
    fi
    [ -n "$conf" ] || die "configure 行を取り出せませんでした"
    {
        echo "FFmpeg configure line for $FFMPEG_ASSET"
        echo "(read back from the shipped binary itself, not hand-copied)"
        echo
        echo "$conf"
    } > "$OUT/BUILD_CONFIGURATION.txt"
    note "BUILD_CONFIGURATION.txt written"
fi

# ---- 4. 由来と再ビルド手順 (英語。国際的な受領者が読む法的文書) ------------
cat > "$OUT/README.txt" <<EOF
FFmpeg binaries mirrored for daw_01, with their Corresponding Source
====================================================================

daw_01 (https://github.com/quek/daw_01) links against the FFmpeg shared
libraries at build time and ships them next to its executables.  This release
exists so that the exact build daw_01 pins stays fetchable after the upstream
provider rotates it out, and so that the Corresponding Source travels with the
binaries as GPL-3.0 section 6(d) requires (LGPL-3.0 incorporates the GPL's
terms).

Nothing here is modified.  These are byte-for-byte copies of what upstream
published, plus source archives created from the exact upstream commits.

Files
-----

1. $FFMPEG_ASSET
   The FFmpeg shared-library build, produced by BtbN/FFmpeg-Builds.
   Unmodified copy of
     $BTBN_URL
   sha256: $FFMPEG_SHA256

2. ffmpeg-source-$FFMPEG_SRC_COMMIT.tar.gz
   The FFmpeg source these libraries were built from:
     https://github.com/FFmpeg/FFmpeg commit $FFMPEG_SRC_COMMIT
   (\`git describe\` = n7.1.5-16-g9a4bb2c579, i.e. 16 commits after the n7.1.5
   release tag, on the release/7.1 branch)

3. ffmpeg-builds-recipe-$BTBN_RECIPE_COMMIT.tar.gz
   The build recipe -- the "scripts used to control those activities" in the
   GPL's definition of Corresponding Source:
     https://github.com/BtbN/FFmpeg-Builds commit $BTBN_RECIPE_COMMIT
   Its scripts.d/*.sh pin the exact revision of every library that is statically
   linked into the FFmpeg DLLs (SCRIPT_COMMIT / SCRIPT_REV / SCRIPT_HGREV), so
   this archive is what fixes those versions.  BtbN/FFmpeg-Builds is MIT
   licensed (Copyright 2020-2021 BtbN <btbn@btbn.de>); its LICENSE file is
   inside the archive.

4. ffmpeg-external-sources-$BTBN_RECIPE_COMMIT.tar.gz
   The sources of every library that the recipe builds and links into the FFmpeg
   DLLs.  BtbN passes --pkg-config-flags=--static, so these are compiled *into*
   the shared libraries: they are part of the work, not tools used to build it,
   and the GPL's "general-purpose tools" exclusion does not reach them.
   The set is not a hand-written list -- it is produced by evaluating the
   recipe's own ffbuild_enabled predicate for target=win64, variant=lgpl-shared,
   addin=7.1, and it is deliberately over-inclusive (build tools are in here
   too).  MANIFEST.tsv inside the archive records, for each component, the
   upstream URL, the revision the recipe pins, and the commit that resolved to.

5. BUILD_CONFIGURATION.txt
   The configure line, read back from the shipped binary rather than copied by
   hand.  ffmpeg.org/legal.html asks for exactly this.

6. gpl-3.0.txt, lgpl-3.0.txt
   The licence texts.  The GPL v3 text is here because the upstream zip ships
   only the LGPL v3 text, and LGPL v3 is a set of additional permissions layered
   on top of the GPL v3 -- without the GPL text the terms cannot be read.

7. SHA256SUMS
   Checksums for every file in this release.

Licensing
---------

The FFmpeg libraries in this build are licensed under the GNU Lesser General
Public License version 3 or later.  The build is configured with
--enable-version3 and WITHOUT --enable-gpl and WITHOUT --enable-nonfree, and the
GPL-only external libraries (x264, x265, xvid, davs2, xavs2, vidstab, frei0r,
rubberband, avisynth, libdvd*) are disabled in the recipe, so no GPL-only or
non-free code is present.  The full LGPL v3 text ships inside the zip as
LICENSE.txt.

The exact configure line of the build is printed by
  ffmpeg.exe -version
and is also visible from daw_01 itself under Help > About.

Rebuilding
----------

  1. Unpack (3).  It is BtbN/FFmpeg-Builds; follow its README.
  2. It clones FFmpeg itself; to reproduce this exact build, check out
     $FFMPEG_SRC_COMMIT (or unpack (2) in its place).
  3. Build the "win64" target with the "lgpl-shared" variant and the "7.1" addin.

Note that BtbN's build image is \`FROM ubuntu:26.04\` with \`apt-get dist-upgrade\`,
so the toolchain is not version-pinned: the set and versions of the bundled
libraries are fixed by (3), but a bit-identical rebuild is not guaranteed.

Relinking
---------

daw_01 links these libraries dynamically and does not rename the DLLs, so a user
may replace them with their own build of the same ABI (avcodec-61, avformat-61,
avutil-59, avdevice-61, avfilter-10, swscale-8, swresample-5).  daw_01 itself is
GPL-3.0-or-later and its complete source is public, so LGPL-3.0 section 4(d) is
satisfied both ways -- by the shared-library mechanism (4d1) and by the full
source being available (4d0).
EOF

# ---- 4b. release notes -----------------------------------------------------
# GPL-3.0 §6(d) は「**object code の隣に**、対応するソースの在処を示す明確な案内を
# 維持せよ」と要求する。README の奥ではなく、ダウンロードリンクと同じ画面に出す必要が
# あるので、release notes そのものを生成する (docs/ffmpeg_mirror.md の手順で使う)。
cat > "$OUT/RELEASE_NOTES.md" <<EOF
Vendored FFmpeg for daw_01 — binaries **and their Corresponding Source**.

\`$FFMPEG_ASSET\` is an **unmodified** copy of the FFmpeg shared-library build
published by [BtbN/FFmpeg-Builds]($BTBN_URL).
sha256 \`$FFMPEG_SHA256\`.

These libraries are licensed under the **GNU Lesser General Public License,
version 3 or later**.

**Corresponding Source for the binary above is in this same release:**

| file | what it is |
|---|---|
| \`ffmpeg-source-$FFMPEG_SRC_COMMIT.tar.gz\` | FFmpeg source, commit \`$FFMPEG_SRC_COMMIT\` |
| \`ffmpeg-builds-recipe-$BTBN_RECIPE_COMMIT.tar.gz\` | the build recipe (BtbN/FFmpeg-Builds, commit \`$BTBN_RECIPE_COMMIT\`) |
| \`ffmpeg-external-sources-$BTBN_RECIPE_COMMIT.tar.gz\` | sources of every library statically linked into the DLLs |
| \`BUILD_CONFIGURATION.txt\` | the configure line, read back from the shipped binary |
| \`gpl-3.0.txt\`, \`lgpl-3.0.txt\` | licence texts |
| \`README.txt\` | provenance and rebuild instructions |
| \`SHA256SUMS\` | checksums for all of the above |

This release exists because upstream keeps daily builds for only about two weeks.
daw_01 pins this exact build; \`scripts/fetch_ffmpeg.sh\` falls back here when the
upstream URL stops resolving, and verifies the same sha256 either way.

daw_01 links these libraries dynamically and does not rename the DLLs, so they can
be replaced with any build of the same ABI (avcodec-61, avformat-61, avutil-59,
avdevice-61, avfilter-10, swscale-8, swresample-5).
EOF

# ---- 5. チェックサム一覧 ---------------------------------------------------
# アップロードする **全ファイル** を対象にする。個別に列挙すると、後から成果物を
# 足したときに取りこぼす (実際 external-sources / ライセンス本文 / 説明文が漏れていた)。
# `.work/` (取得キャッシュ、数 GB) は dotfile なので glob に入らない。
( cd "$OUT" && : > SHA256SUMS
  for f in *; do
      [ -f "$f" ] || continue
      [ "$f" = "SHA256SUMS" ] && continue
      if command -v sha256sum >/dev/null 2>&1; then sha256sum "$f" >> SHA256SUMS
      else shasum -a 256 "$f" >> SHA256SUMS; fi
  done )

note "done. staged in: $OUT"
echo
ls -l "$OUT"
echo
cat "$OUT/SHA256SUMS"
echo
echo "アップロードは **まだしていません**。手順は docs/ffmpeg_mirror.md を見てください。"
