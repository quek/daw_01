---
name: research-similar-impl
description: |
  類似 DAW / CLAP ホスト / Rust オーディオプロジェクト（clap-host, clap-validator, clack,
  nih-plug, Meadowlark 等）、Vizia、VOICEVOX のソースコードと公式リファレンスを調査し、
  実装方針レポートを出力する。
  「実装して」「追加して」「修正して」「対応して」「機能を作って」「バグを直して」等、
  コー���変更を���う指示があったとき、または CLAP / cpal / Vizia / VOICEVOX API の使い方が
  不明なときに発動。調査のみ行い、コードの編集は行わない。
argument-hint: "[調査対象の機能名]"
allowed-tools: Bash(git clone *), Bash(git pull *), Read, Grep, Glob, WebSearch, WebFetch, Agent
---

# 類似プロダクト & API リファレンス調査

$ARGUMENTS に関する調査を行い、daw_01 での実装方針を立てるためのレポートを出力��る。

## 手順

### 1. 調査対象の特定

ユーザーの要求から実装対象の機能 / CLAP インターフェース / Vizia API / VOICEVOX API を特定する。
[references.md](references.md) の「機能と API の対応例」を参照。

### 2. リポジトリのクローン

[references.md](references.md) の調査対象プロジェクトから、機能に関連するものを `/tmp` にクローンする。

```bash
[ -d /tmp/clap-host ]      || git clone --depth 1 https://github.com/free-audio/clap-host.git /tmp/clap-host
[ -d /tmp/clap ]           || git clone --depth 1 https://github.com/free-audio/clap.git /tmp/clap
[ -d /tmp/clack ]          || git clone --depth 1 https://github.com/prokopyl/clack.git /tmp/clack
[ -d /tmp/nih-plug ]       || git clone --depth 1 https://github.com/robbert-vdh/nih-plug.git /tmp/nih-plug
[ -d /tmp/clap-validator ] || git clone --depth 1 https://github.com/free-audio/clap-validator.git /tmp/clap-validator
[ -d /tmp/meadowlark ]     || git clone --depth 1 https://github.com/MeadowlarkDAW/Meadowlark.git /tmp/meadowlark
[ -d /tmp/vizia ]          || git clone --depth 1 https://github.com/vizia/vizia.git /tmp/vizia
```

- クローン先は `/tmp` 配下（作業ディレクトリを汚さない）
- `--depth 1` で軽量クローン
- 既存ならスキップ

### 3. 自プロジェクト（前作）の参照

sing_like_coding (`F:\dev\sing_like_coding`) に類似実装がある場合、まずそちらを確認する。
前作で動いているパターンは最も信頼性が高い参照元。

### 4. 並列調査（Agent を並列起動）

以下の A) と B) を **Agent を並列起動して同時に** 実行する。

**A) 類似プロダクトのソースコード調査**

クローン済みリポジトリを Grep / Read で横断検索。調査ポイント:
- CLAP インターフェース（`clap_plugin_*`, `clap_host_*`, `clap_process`, `clap_event_*`）の呼び出し順序・契約
- RT オーディオスレッドの設計（ロックフリー・SPSC キュー・事前確保）
- プラグインのライフサイクル
- CLAP スキャン
- オートメーション / パラメータ変更
- MIDI → CLAP イベント変換
- プラグインウィンドウ埋め込み（`clap_plugin_gui`、Win32 の `HWND` 親子関係）
- サブプロセス化とその IPC

**B) 公式 API リファレンス・ガイド調査**

[references.md](references.md) の API ドキュメント URL を WebFetch / WebSearch で調査:
- CLAP 公式仕様・拡張（`ext/*.h`）
- `cpal` のスレッドモデルと制約
- `vizia` のカスタムビュー、キーボードイベント、IME、Lens/Binding
- `windows` crate の該当 API シ���ネチャ
- VOICEVOX Engine の HTTP API 仕様
- Rust 側のベストプラクティスとサンプル

### 5. clap-sys / windows crate の API 確認

`~/.cargo/registry/src/` 内のソースを Grep して実際の Rust シグネチャを確認する。

確認ポイント:
- `clap-sys` の型定義（関数ポインタのシグネチャ、`*const`/`*mut`、配列長フィールド）
- `windows` crate の COM メソッド引数型
- `vizia` の内部 API（View trait、Event 処理、Context メソッド）

#### ⚠️ バージョン整合性の確認（最優先）

`/tmp/<crate>` の clone は **GitHub main ブランチ**。crates.io で公開されている stable 版と
API が違うことがある。**main だけ見てレポートすると誤った設計判断になる**。

**Vizia がまさにこの罠**:
- crates.io `vizia = "0.3.0"` = **Lens ベース**（`#[derive(Lens)]`, `Binding`, `AppData::song.map(...)`)
- GitHub main = **Signal ベースに移行中**（`Signal::new`, `ReadSignal`, `WriteSignal`）
- 初回実装で Signal 前提のコードを書いたところ全部 compile error で書き直した実績あり

対策手順:
1. `F:\dev\daw_01\Cargo.lock` で実際に solver が選んだバージョンを確認
2. `~/.cargo/registry/src/index.crates.io-*/<crate>-<version>/` の実ファイルを必ず Read / Grep
3. `/tmp/<crate>` の情報と食い違ったら **crates.io 側（実際にビルドされる方）を信じる**
4. Agent に指示するときは「crates.io の `<crate> = \"X.Y.Z\"` を基準に調査」と明示

他の既知差:
- `windows` crate は 0.58 / 0.61 で HANDLE が `isize` → `*mut c_void` に変更
- `tokio` の `net::windows::named_pipe` は 1.x 前提
- `bincode` 2.x は `Encode`/`Decode` に刷新（1.x とは別 API）

### 6. レポート出力

[report-template.md](report-template.md) の形式で日本語でまとめる。

## 制約

- **調査のみ**。ファイルの編集・作成・ビルド・インストール等は一切行わない
- CLAP 仕様に関わる機能では `clap-host` と `clap` 本体を最優先で参照する
- Rust 設計パターンは `clack` / `nih-plug` を参照する
- 自プロジェクト前作 (`sing_like_coding`) の既存パターンも参照する
- windows crate / clap-sys / vizia の API 確認は省略しない
