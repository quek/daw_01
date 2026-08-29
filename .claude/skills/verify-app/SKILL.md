---
name: verify-app
description: |
  daw_01 の変更を実機で確認するため daw_gui を起動し、子プロセス handshake と挙動を
  ログで検証する定型手順。二重起動チェック → background 起動 → ログ grep を1アクションに束ねる。
  「実機で確認」「動かして確認」「daw_gui を起動して」「変更が効いているか見て」等のとき発動。
  GUI / オーディオ / プラグイン挙動など unit test で拾えない変更の確認に使う。
allowed-tools: Read, Grep, Glob, Bash
---

# daw_01 実機検証ワークフロー

`cargo test` で拾えない GUI / オーディオ / プラグイン GUI / IPC 挙動を、実際に daw_gui を
起動して確認する。**4 つの不変ルール**を必ず守る (各々 memory に根拠あり)。

## 不変ルール
1. **起動する前に一声かける** (`feedback_ask_before_launching_app`): 窓が前面に出て作業の邪魔に
   なる。並列作業中は特に。
2. **二重起動しない** (`feedback_no_duplicate_app_launch`): 既存の daw_gui が動いていると
   IPC 衝突で入力不能 (「何もクリックできない」) になる。起動前に必ずプロセス確認 (下記 §2)。
3. **自分で起動する** (`feedback_launch_app_for_verification`): ユーザーに「起動して」と頼まず、
   `run_in_background` で自分が起動する。ただし振る舞いの目視 (メニュー hover 等 UI 操作が要る
   検証) はユーザーに依頼する。`| tail` 越しに起動しない (`feedback_launch_no_tail_pipe` —
   I/O abort で誤クラッシュする)。`tee <log>` でログをファイルに落とす。
4. **動いているアプリを kill しない** (`feedback_no_kill_running_app`): `taskkill` 等で止めない。
   既存起動があればユーザーに「閉じてください」と依頼。

## 手順

### 1. ビルド (挙動を変えた場合)
- protocol (bincode derive 型) を変えたら **`make build`** (実行 3 exe を生成。子バイナリ
  daw_audio / daw_plugin_host も再生成しないと古い protocol のまま handshake/decode 失敗)。
- それ以外でも `cargo run` は dependency を自動ビルドするが、子プロセスバイナリが上書き
  されないことがある (Windows ERROR 5)。疑わしければ先に `make build`。
- 素の `cargo build --workspace` は使わない (CLAUDE.md「Makefile が SSoT」。examples 等
  テスト 0 個の crate まで毎回フルビルドして無駄に遅い)。1 crate に閉じるなら
  `cargo build -p <crate>` まで絞る。

### 2. 二重起動チェック

**専用のコマンドを書かず、`make run` / `make test` と同じ判定器を使う** (SSoT。PowerShell は
使わない — `feedback_no_powershell_cross_platform`)。

```bash
bash scripts/preflight_no_running_app.sh verify-app
```

- **exit 1 (= 起動中)** … 起動しない。ユーザーに「閉じてください」と依頼してから再実行。
- **exit 0** … 起動してよい。ただしスクリプトが `[警告]` を出していたら、それは
  「**起動していない**」ではなく「**判定できなかった**」(プロセス一覧が取れない環境)。
  その場合は緑と読まず、ユーザーに確認する。
- 判定は `tasklist` → `pgrep` → `ps` の順に使える手段を選ぶので Windows 以外でも動く。
  `DAW01_PREFLIGHT_APP=<必ず居るプロセス名>` を付けて **検査自身が実際に止まること**を
  確かめられる。

### 3. background 起動
```bash
cargo run -p daw_gui 2>&1 | tee target/verify_run.log   # run_in_background: true
```
- `run_in_background: true` で起動 (フォアグラウンド blocking 不可)。ログは `target/verify_run.log`。

### 4. handshake / 起動確認
- 数秒待ってログを確認。正常起動の目印:
```
daw_audio handshake complete
daw_plugin_host handshake complete
plugin-main thread running
```
- これらが出れば IPC 層は健全 (protocol 変更の退行なし)。出ない/エラーなら handshake 失敗を疑う。

### 5. 挙動のログ確認
- 確認したい挙動の IPC / イベントを grep (ANSI を除去すると読みやすい):
```bash
grep -iE "<確認したいイベント>" target/verify_run.log | sed -E 's/\x1b\[[0-9;]*m//g' | tail -40
```
- 例: プラグイン GUI なら `OpenSlotGuiEmbedded|SlotGuiGeometry|editor window destroyed|SetSlotPlugin`。
  オーディオ経路なら `OpenPluginShmem|LoadSong|plugin_refs`。

### 6. UI 操作が要る検証はユーザーへ
- メニュー hover / プラグイン挿入 / ドラッグ等、プログラムから駆動できない確認は、起動した
  インスタンスで**ユーザーに手順を箇条書きで依頼**する (「1. ... を挿す 2. ... を hover」)。
- 結果報告を受けてログと突き合わせる。NG ならログの該当イベントから原因を追う。

### 7. 多面機能の検証 / 新機能のバグ報告の追い方 (2026-06-13 modulation で6ラウンド浪費)
- **同じ capability が複数の UI 面に跨る機能は、全面を列挙してから「完成」と言う**。
  例: per-control modulation は param コントロールが複数ある —
  画像 PiP / グループ Transform / プラグイン param / テキスト / track vol-pan / song tempo。
  1 面 (画像) だけ配線して全機能完成と報告 → ユーザーの実使用面 (グループ Transform) が
  未配線で「動かない」になった。検証は**ユーザーが実際に使う面**で行い、配線済み面を全部試す
  ([[feedback_enumerate_complete_feature_set]] / [[feedback_new_feature_bug_suspect_own_wiring]])。
- **「動かない」報告は、ユーザー操作ミス・環境・飽和を仮定する前に、自分の新コード/未配線を
  第一容疑にする**。推測で原因を断定せず、**一時診断ログ** (`tracing::info!`、後で削除) を
  仕込んで実データを取ってから仮説を立てる (CLAUDE.md「Debugging Methodology / 実データから始める」)。
  - 切り分けは**既存 UI の観測量**を先に使う (例: source meter が動く=follower 正常 → bug は下流)。
  - パイプライン全体 (生成→IPC→poll→compose→描画) を上流から1点ずつ潰す。前提を確認せず
    「ユーザーが手順を抜かした」と決めつけない (実際は抜けていなかった)。

## 注意
- video preview 等の visual regression は `cargo run -p daw_gui -- --smoke-test <fixture.mp4>` の
  自動検証がある (exit 0 = healthy)。texture/shared-handle 周りはこちらを併用。
- 終了はユーザーが窓を閉じる (background task の exit 通知で分かる)。自分で kill しない。
