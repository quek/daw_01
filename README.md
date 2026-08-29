# daw_01

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。クリップベースのタイムライン、CLAP / VST 3
プラグインホスト (ARA 2 対応)、ビルトインの映像合成 (動画 / 画像 / 字幕 / 立ち絵 + 口パク)
を持つ。

設計の正本は [DESIGN.md](DESIGN.md)、開発時の不変条件は [CLAUDE.md](CLAUDE.md)。

## 構成

Cargo workspace (Rust Edition 2024)。実行時は独立した 3 プロセスが協調する。

| ディレクトリ | 役割 |
|---|---|
| `common/` | 共有型・IPC プロトコル・shared memory・データモデル |
| `daw_gui/` | GUI プロセス。Song ドキュメントの SSoT |
| `daw_audio/` | オーディオエンジンプロセス (CPAL / WASAPI) |
| `daw_plugin_host/` | プラグインホストプロセス (CLAP / VST 3 / ARA) |
| `ui/` | 自作 immediate-mode GUI ライブラリ daw-ui (winit + wgpu) |

## ビルド

`Makefile` が SSoT。素の `cargo build --workspace` / `cargo test --workspace` は使わない
(テスト 0 個の crate まで毎回フルビルドするので、`make` 側の scoping が効かなくなる)。

```bash
make fetch-ffmpeg   # third_party/ffmpeg を取得 (各マシン 1 回。build/test/check の前提)
make build          # 実行 3 exe を debug ビルド
make run            # daw_gui をビルドして起動
make test           # テストを持つ package のみ実行 (一部は daw_gui を起動する。下記)
make test-nolaunch  # そのうち daw_gui を起動しない target だけ
make clippy         # clippy をエラー扱いで
make arch-lint      # アーキテクチャ不変条件の機械検査
make license-check  # ライセンス表示の機械検査 (下記)
make audit          # 依存の脆弱性 / 供給網攻撃の検査 (network 要)
```

`make test` / `make run` は **daw_gui が起動中だと前提条件チェックで止まる**
(`scripts/preflight_no_running_app.sh`)。テストの一部が daw_gui 本体を subprocess として
起動して audio device を開くので、DAW を開いたまま回すと再生が壊れるため。

現状 Windows (x86_64-pc-windows-msvc) が対象。動画デコード / エンコードは vendored な
FFmpeg 共有ライブラリに依存する。

VOICEVOX の合成機能を使うには **VOICEVOX を別途インストール**しておく
(daw_01 は `http://localhost:50021` に HTTP で話しかけるだけで、VOICEVOX 本体は同梱しない)。
**手で起動しておく必要は無い** — vocal トラックが 1 つでもできた時点で、daw_01 が
ヘッドレスエンジン (`vv-engine/run.exe`) を自分で立ち上げ、daw_01 の終了と一緒に落とす。
既に :50021 が応答していれば (VOICEVOX エディタを手で開いている場合も含め) それを使う。

既定のインストール先 (`%LOCALAPPDATA%\Programs\VOICEVOX\` / `C:\Program Files\VOICEVOX\`)
以外に入れているときだけ、環境変数 `DAW_VOICEVOX_PATH` か
`%LOCALAPPDATA%\daw_01\voicevox_engine_path.txt` に所在 (install root / `VOICEVOX.exe` /
`run.exe` のいずれか) を書く。

## Claude Code の hook は同梱していない

このリポジトリは [`.claude/settings.json`](.claude/settings.json) を git 追跡しているが、
**hook は 1 本も登録していない** (`{}`)。clone したツリーを Claude Code で開いても、
ツール呼び出しに割り込むローカルスクリプトは走らない。外部通信もしない。

コード品質の強制は `make` の検査 (`arch-lint` / `clippy` / `test` / `license-check` / `audit`) が
担う。どれも実行結果が出力に出るので、黙って壊れても気付ける。

## ライセンス

daw_01 は **GNU General Public License version 3 or later (GPL-3.0-or-later)** で頒布する。

    Copyright (C) 2026 Tahara Yoshinori

    This program is free software: you can redistribute it and/or modify it under
    the terms of the GNU General Public License as published by the Free Software
    Foundation, either version 3 of the License, or (at your option) any later
    version.

    This program is distributed in the hope that it will be useful, but WITHOUT ANY
    WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
    PARTICULAR PURPOSE.  See the GNU General Public License for more details.

    You should have received a copy of the GNU General Public License along with
    this program.  If not, see <https://www.gnu.org/licenses/>.

全文は [LICENSE](LICENSE) (= [LICENSES/GPL-3.0-or-later.txt](LICENSES/GPL-3.0-or-later.txt))。
アプリ内では **ヘルプ > バージョン情報** から同じ内容を読める。

### ライセンス表示の作り

[REUSE Specification 3.3](https://reuse.software/spec-3.3/) に従う。

- **著作権表示は [REUSE.toml](REUSE.toml) の一括宣言 1 箇所に集約する。ファイル先頭には
  書かない。** GPL-3.0 §4 が求めるのは「**各コピー**に適切な著作権表示を掲示する」ことで、
  この "each copy" はプログラム 1 本ごとであってファイルごとではない。ルートの
  [LICENSE](LICENSE) / この README / [NOTICE](NOTICE) と REUSE.toml の宣言で満たされる。
- **第三者コード** (vendored ヘッダ、bindgen 出力) は REUSE.toml の**個別宣言**で
  別のライセンスとして宣言する。一括宣言は先頭に置き、個別宣言を後ろに置く
  (REUSE spec は「同じファイルに当たったら**最後にマッチした宣言**を使う」)。
- 使っているライセンスの全文は [`LICENSES/`](LICENSES) に SPDX 識別子の名前で置く。
- 第三者コンポーネントの帰属は [NOTICE](NOTICE)、依存クレートの一覧は
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) (生成物)。

`make license-check` が次を機械検査する。外部ツールが無い環境でも Python 標準ライブラリ
だけで**必ず**走るので、「ツールが無いから skip」で表示が壊れることはない。

1. SPDX 式評価器の自己検査 (`scripts/dep_licenses.py --self-test`)
2. REUSE 適合 (`scripts/reuse_lint.py`)。一括宣言が全ファイルを覆っているか、一括宣言が
   **先頭**にあるか、`vendor/` 配下や「自分以外の著作権表示を持つファイル」が個別宣言で
   覆われているか — つまり **第三者のコードを GPL と誤表示していないか**を見る
3. 依存クレートが [deny.toml](deny.toml) の許可リストで満たせるか +
   THIRD-PARTY-NOTICES.md の鮮度 (`scripts/dep_licenses.py --check`)

`reuse` (`pipx install reuse`) と `cargo-deny` (`cargo install --locked cargo-deny`) が
入っていれば、それらも追加で走る。

新しいファイルを足しても**何もしなくてよい** (一括宣言が覆う)。第三者のコードを
持ち込んだときだけ REUSE.toml に個別宣言を足す。依存を変えたら
`python scripts/dep_licenses.py --write` を実行する。

## 第三者コンポーネント

帰属と条件の詳細は [NOTICE](NOTICE)。要約:

| コンポーネント | ライセンス | 形態 |
|---|---|---|
| [FFmpeg](https://ffmpeg.org/) | LGPL v3 | 動的リンク (DLL、無改変)。リポジトリには含まない |
| [ARA SDK](https://github.com/Celemony/ARA_SDK) (Celemony) | Apache-2.0 | `ara-sys/vendor/ARA_API/` に無改変で vendoring |
| [Signalsmith Stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch) / [Linear](https://github.com/Signalsmith-Audio/linear) | MIT | `signalsmith-sys/vendor/` に無改変で vendoring |
| VST 3 SDK (Steinberg) | MIT (SDK 3.8.0 以降) | `vst3` crate 経由。SDK は vendoring しない |
| [CLAP](https://github.com/free-audio/clap) | MIT | `clap-sys` crate 経由 |
| Rust クレート 389 件 | MIT / Apache-2.0 / BSD / ISC / Zlib / MPL-2.0 ほか | [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) |

いずれも GPL-3.0-or-later と互換。Apache-2.0 は GPLv3 とのみ互換なので、**このプロジェクトを
GPLv2 系へ後退させることはできない**。

### FFmpeg (LGPL v3)

> This software uses libraries from the FFmpeg project under the LGPLv3
> (<https://www.gnu.org/licenses/lgpl-3.0.html>).
> FFmpeg source: <https://ffmpeg.org/download.html>

`make fetch-ffmpeg` が取得するのは
[BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) の `n7.1 win64-lgpl-shared`
ビルド (`--enable-version3` 付き、`--enable-gpl` / `--enable-nonfree` なし)。実際の
configure 行は `third_party/ffmpeg/bin/ffmpeg.exe -version` と、アプリ内の
**ヘルプ > バージョン情報** で確認できる (後者は実行中のライブラリに問い合わせた実測値)。
DLL は改名せずそのまま使うので、同じ ABI (avcodec-61 / avformat-61 / avutil-59 /
swscale-8 / swresample-5) の自前ビルドに差し替えられる。

取得は **URL + sha256 固定**で、上流 (BtbN は日次ビルドを約 2 週間で削除する) が消えたら
このリポジトリの release に置いたミラーへ自動でフォールバックする。ミラーには**対応する
ソース一式**も併置している (GPL-3.0 §6(d))。設計と手順は
[docs/ffmpeg_mirror.md](docs/ffmpeg_mirror.md)、成果物の用意は `make ffmpeg-mirror`
(アップロードは自動化していない)。

**ビルド済みバイナリを配布するときは** FFmpeg の DLL に加えて
`third_party/ffmpeg/LICENSE.txt` (LGPL v3 全文) と GPL v3 全文を同梱し、対応する FFmpeg の
ソース入手先を同じ場所に示すこと (LGPL-3.0 §4(b)(d) / GPL-3.0 §6)。

### VOICEVOX

daw_01 は VOICEVOX ENGINE に HTTP で問い合わせ、必要なら別プロセスとして起動するだけで、
VOICEVOX のコード・音声モデル・辞書・キャラクター音声は一切含まない。生成した音声の利用は
[VOICEVOX の利用規約](https://voicevox.hiroshiba.jp/term/) と各キャラクターの規約に従うこと。

### 実行時に読み込むプラグイン

ユーザーがインストールした CLAP / VST 3 プラグインは別個のプログラムであり、daw_01 と一緒に
配布されるものではない。それぞれのライセンスに従う。
