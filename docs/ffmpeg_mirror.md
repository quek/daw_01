# vendored FFmpeg の固定とミラー

`make fetch-ffmpeg` が取ってくる FFmpeg を **URL 固定 + sha256 検証**にし、上流が消えても
壊れないよう **自前のミラー**を持つための設計と手順。

- 取得: `scripts/fetch_ffmpeg.sh` (pin の SSoT)
- ミラーの成果物作成: `scripts/prepare_ffmpeg_mirror.sh` (= `make ffmpeg-mirror`)
- ライセンス面の帰属: リポジトリの [`NOTICE`](../NOTICE)

## 1. なぜ固定したか (方針の反転)

以前の `Makefile` は「BtbN の `releases/tags/latest` の asset 一覧から
`n7.1.*win64-lgpl-shared` を **発見**して取る」方式だった。CLAUDE.md にも
「BtbN の asset 名変更に耐えるよう URL 固定でなく」と書いてあった。

**2026-08 に起きたのは asset 名の変更ではなく asset の消滅だった。**
`latest` に残っているのは master / n8.1 / n9.0 だけで、n7.1 系は 1 件も無い
(実測: `n7.1.*win64-lgpl-shared` のヒット 0)。発見方式は「見つからなければ落ちる」ので
原理的に対応できず、**third_party/ffmpeg を持っていないマシンは何もビルドできない**
状態になっていた (既存マシンは `avcodec.lib` があって skip されるので誰も気付かない)。

BtbN の保持ポリシーは README の Release Retention Policy にあるとおり
**「月末ビルドは 2 年、日次ビルドは直近 14 本」**。我々が pin しているのは日次ビルドなので、
固定しただけではいずれ 404 になる。だから固定とミラーはセットで要る。

## 2. 何に固定しているか

| 項目 | 値 |
|---|---|
| asset | `ffmpeg-n7.1.5-16-g9a4bb2c579-win64-lgpl-shared-7.1.zip` |
| 一次 URL | `https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-16-13-00/…` |
| size | 62,696,009 bytes |
| sha256 | `a950596cea0bf9766f169dae6f1e6eb623aa1ccfd2822cd20cd2874b120d4086` |
| FFmpeg ソース | `FFmpeg/FFmpeg` commit `9a4bb2c579a16b0469759743d6917d9e8e3cb8c6` (= `git describe` の n7.1.5-16-g9a4bb2c579) |
| ビルドレシピ | `BtbN/FFmpeg-Builds` commit `590a6612d7d961e9258429e501619e0b7d7cbedf` |

ライブラリ版数は従来 (`n7.1.5-20260620`) と**完全に同一**で、public header も
`libavutil/ffversion.h` の版数文字列 1 行を除いて一致、import library (`*.lib`) は 7 本とも
byte 一致。つまり `daw_gui/ffmpeg/binding_ffmpeg_7.1.rs` はそのまま有効で、動画の挙動は変わらない。

> release API の `target_commitish` は `"master"` という**ブランチ名の文字列**で commit SHA
> ではない。レシピ commit は `git/refs/tags/<tag>` から取ること。

## 3. 取得の流れ (`scripts/fetch_ffmpeg.sh`)

1. `third_party/ffmpeg/lib/avcodec.lib` があれば何もしない (冪等)。
   `LICENSE.txt` だけ欠けていれば復旧する。
2. 一次 URL (BtbN) → ミラー URL の順に取得を試す。
3. **どちらの経路でも sha256 で検証**する。一致しなければ次の候補へ回し、全部だめなら失敗。
4. 展開して `bin` / `lib` / `include` / `LICENSE.txt` を揃え、**検証してから**既存と入れ替える。

ミラー URL は `Cargo.toml` の `[workspace.package] repository` から組み立てる。公開先を
変えたらそこ 1 箇所を直せば追従する。環境変数 `FFMPEG_URL` / `FFMPEG_MIRROR_URL` /
`FFMPEG_SHA256` / `FFMPEG_DIR` で上書きできる。

> **curl の罠**: `curl --retry N` は最終的に 404 で終わっても **exit code 0** を返す
> (curl 8.14.1 で実測)。exit code だけを見てフォールバックを判断すると、一次 URL が消えた
> 本番でミラーに回らず空ファイルを掴む。だから成否は「ファイルが実在して中身があるか」と
> 「sha256」で判定している。

## 4. ミラーに置くもの — LGPL の「対応するソース」

ミラーに zip を置く = **我々が FFmpeg のバイナリを convey する**。LGPL-3.0 は GPL-3.0 の
条項を取り込んでいるので GPL-3.0 §6 が効き、GitHub release という形態では §6(d) 一択:

> d) Convey the object code by offering access from a designated place (gratis or for a
> charge), and offer equivalent access to the Corresponding Source **in the same way through
> the same place** at no further charge. … Regardless of what server hosts the Corresponding
> Source, **you remain obligated to ensure that it is available for as long as needed** to
> satisfy these requirements.

Corresponding Source の定義 (GPL-3.0 §1) は「その object code を生成・インストール・実行し、
改変するのに必要な全ソース。**それらの作業を制御するスクリプトを含む**。ただし System
Libraries と、**the work の一部ではない**未改変で一般に入手可能な汎用ツールは除く」。

BtbN は `--pkg-config-flags=--static` で外部ライブラリを **DLL の中に静的に取り込む**ので、
libmp3lame / libopus / gmp / libaribb24 / libvmaf / … は「ビルドに使った道具」ではなく
**成果物の一部**。よって除外句にかからず、Corresponding Source に含まれる。

**BtbN 自身は release にソースを 1 件も置いていない** (asset は zip / tar.xz と
checksums.sha256 のみ)。したがって「BtbN の release にリンクしておけば §6(d) の
clear directions になる」は成立しない。自分で揃える必要がある。

`make ffmpeg-mirror` が `dist/ffmpeg-mirror/` に用意するもの:

| ファイル | 中身 |
|---|---|
| `ffmpeg-…-win64-lgpl-shared-7.1.zip` | BtbN のバイナリ (**無改変**) |
| `ffmpeg-source-<sha>.tar.gz` | FFmpeg 本体のソース (pin した commit) |
| `ffmpeg-builds-recipe-<sha>.tar.gz` | BtbN のビルドレシピ (= 「作業を制御するスクリプト」)。MIT |
| `ffmpeg-external-sources-<sha>.tar.gz` | DLL に静的リンクされる外部ライブラリのソース一式 |
| `BUILD_CONFIGURATION.txt` | configure 行 (**実物のバイナリから読み出したもの**) |
| `gpl-3.0.txt` / `lgpl-3.0.txt` | ライセンス本文 |
| `README.txt` | 由来・再ビルド手順・再リンク可能性の説明 |
| `SHA256SUMS` | 上記のチェックサム |

外部ライブラリの集合は**レシピ自身の `ffbuild_enabled` を実際に評価して**決めている
(`target=win64` / `variant=lgpl-shared` / `addin=7.1`)。推測でリストを書かない。
判定は over-inclusive 側に倒してあり、ビルドツールも含めて全部入れる — 足りないより多い方が安全。

`gpl-3.0.txt` を別途入れているのは、**BtbN の zip 同梱 `LICENSE.txt` が LGPLv3 本文のみ**で、
LGPLv3 が追加許諾として乗る土台の GPLv3 本文が入っていないため。

### 前提ツール

`git` / `curl` / `unzip` / `tar` / `gzip`。svn 依存 (libmp3lame) は `svn` があればそれを使い、
無ければ SourceForge の snapshot API で取得する (revision が URL に入るので pin は保たれる)。

## 5. アップロード手順

> **このリポジトリの自動化は release を作らない。** 外部に出る操作は必ず人が実行する。

```bash
# 1. 成果物を用意する (ネットワークを使う。数分〜十数分かかる)
make ffmpeg-mirror

# 2. 中身とチェックサムを確認する
ls -l dist/ffmpeg-mirror
cat dist/ffmpeg-mirror/SHA256SUMS
sha256sum -c dist/ffmpeg-mirror/SHA256SUMS   # 全部 OK になること

# 3. release を作る (タグは fetch_ffmpeg.sh の FFMPEG_MIRROR_TAG と一致させること)
gh release create vendor-ffmpeg-n7.1.5-16-g9a4bb2c579 \
    --repo quek/daw_01 \
    --title "Vendored FFmpeg n7.1.5-16-g9a4bb2c579 (win64, LGPL v3 shared) + Corresponding Source" \
    --notes-file dist/ffmpeg-mirror/RELEASE_NOTES.md

# 4. asset を上げる
gh release upload vendor-ffmpeg-n7.1.5-16-g9a4bb2c579 \
    --repo quek/daw_01 \
    dist/ffmpeg-mirror/ffmpeg-n7.1.5-16-g9a4bb2c579-win64-lgpl-shared-7.1.zip \
    dist/ffmpeg-mirror/ffmpeg-source-9a4bb2c579a16b0469759743d6917d9e8e3cb8c6.tar.gz \
    dist/ffmpeg-mirror/ffmpeg-builds-recipe-590a6612d7d961e9258429e501619e0b7d7cbedf.tar.gz \
    dist/ffmpeg-mirror/ffmpeg-external-sources-590a6612d7d961e9258429e501619e0b7d7cbedf.tar.gz \
    dist/ffmpeg-mirror/BUILD_CONFIGURATION.txt \
    dist/ffmpeg-mirror/gpl-3.0.txt \
    dist/ffmpeg-mirror/lgpl-3.0.txt \
    dist/ffmpeg-mirror/README.txt \
    dist/ffmpeg-mirror/RELEASE_NOTES.md \
    dist/ffmpeg-mirror/SHA256SUMS
```

> `RELEASE_NOTES.md` は `--notes-file` で release 本文にもなるが、**asset としても上げる**。
> `SHA256SUMS` がこれを含んでいるので、上げないと利用者の `sha256sum -c SHA256SUMS` が
> 1 件だけ落ちる (2026-08-22 に実際の公開手順で発覚)。`.work/` は中間生成物なので上げない。

```bash

# 5. フォールバックが実際に効くことを確かめる (一次 URL をわざと壊す)
/usr/bin/bash scripts/fetch_ffmpeg.sh --dest /tmp/ffcheck \
    --url "https://github.com/BtbN/FFmpeg-Builds/releases/download/does-not-exist/nope.zip"
```

### release notes に必ず書くこと

GPL-3.0 §6(d) は「**object code の隣に**、対応するソースの在処を示す明確な案内を維持せよ」と
言っている。README の奥や docs ではなく、**ダウンロードリンクと同じ視野**に置くこと。
`make ffmpeg-mirror` が `RELEASE_NOTES.md` を生成するので、それをそのまま使う。

### 運用上の約束

- **バイナリ asset を降ろすときは、対応するソース asset も同時に降ろす** (逆はしない)。
  §6(d) の「必要な限り提供し続ける」義務は、ホストが誰であっても我々に残る。
- pin を更新するときは `scripts/fetch_ffmpeg.sh` と `scripts/prepare_ffmpeg_mirror.sh` の
  両方の pin ブロックを同時に直し、新しい tag で release を作り直す
  (`FFMPEG_MIRROR_TAG` も上げる)。
- DLL は**絶対にリネームしない**。利用者が同じ ABI の自前ビルドに差し替えられること
  (LGPL-3.0 §4(d)(1)) が壊れる。

## 6. worktree と third_party — junction を辿る削除で本体が消える

`third_party/ffmpeg` は gitignore なので **git から復元できない**。ここに reparse point
(junction / symlink) を張ったまま worktree を削除すると、削除処理が junction を辿って
**main checkout 側の実体を消す**。

**2026-06-14 に実際に起きた。** worktree の削除が内部の `third_party` junction を辿り、
main checkout の `third_party/ffmpeg` ごと消えた。git に無いので `git checkout` では戻らず、
`make fetch-ffmpeg` で取り直して復旧した。

現在の構成ではこのハザードは無い:

- Claude Code の worktree (`--worktree` / EnterWorktree / subagent) は、リポジトリ直下の
  `.worktreeinclude` (`/third_party/`) により main checkout から **実コピー**される。
  reparse point ではないので、消しても main 側には影響しない。よって Claude が作る worktree で
  `make fetch-ffmpeg` は不要 (ただし **main checkout 自身が未取得なら先に取っておく** —
  `.worktreeinclude` は「main にある物を持ち込む」だけ)。
- 手動 `git worktree add` で作った worktree はこの経路を通らない。従来どおり
  `make fetch-ffmpeg` で取得すること。
- それでも手で junction を張ったら、**worktree を消す前に内部の reparse point を
  `cmd //c rmdir <junction>` で外す**。削除ツールは reparse point 属性でスキャンしてから消す
  (memory: `feedback_junction_safe_removal`)。

参考: <https://code.claude.com/docs/en/worktrees>

## 7. 既知の限界

- BtbN のビルドイメージは `FROM ubuntu:26.04` + `apt-get dist-upgrade` で、ツールチェーンの
  版が固定されていない。**同梱ライブラリの版はレシピ commit で確定する**が、
  **ビット単位で同一のバイナリを再現できる保証は無い**。GPL は完全一致の再現ビルドまでは
  要求していないので義務は満たすが、この 2 つを混同しないこと。
- SourceForge の svn snapshot 経由で取る libmp3lame だけは、他と違って git の
  オブジェクトハッシュによる検証ができない (revision 指定の URL を信頼する)。
