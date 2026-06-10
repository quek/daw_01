---
name: verify-app
description: |
  daw_01 の変更を実機で確認するため daw_gui を起動し、子プロセス handshake と挙動を
  ログで検証する定型手順。二重起動チェック → background 起動 → ログ grep を1アクションに束ねる。
  「実機で確認」「動かして確認」「daw_gui を起動して」「変更が効いているか見て」等のとき発動。
  GUI / オーディオ / プラグイン挙動など unit test で拾えない変更の確認に使う。
allowed-tools: Read, Grep, Glob, Bash, PowerShell
---

# daw_01 実機検証ワークフロー

`cargo test` で拾えない GUI / オーディオ / プラグイン GUI / IPC 挙動を、実際に daw_gui を
起動して確認する。**3 つの不変ルール**を必ず守る (各々 memory に根拠あり)。

## 不変ルール
1. **二重起動しない** (`feedback_no_duplicate_app_launch`): 既存の daw_gui が動いていると
   IPC 衝突で入力不能 (「何もクリックできない」) になる。起動前に必ずプロセス確認。
2. **自分で起動する** (`feedback_launch_app_for_verification`): ユーザーに「起動して」と頼まず、
   `run_in_background` で自分が起動する。ただし振る舞いの目視 (メニュー hover 等 UI 操作が要る
   検証) はユーザーに依頼する。
3. **tail パイプ越しに起動しない** (`feedback_launch_no_tail_pipe`): `| tail` 越しだと I/O abort で
   誤クラッシュ。`run_in_background` + `tee <log>` でログをファイルに落とす。
4. **動いているアプリを kill しない** (`feedback_no_kill_running_app`): `taskkill`/`Stop-Process`
   しない。既存起動があればユーザーに「閉じてください」と依頼。

## 手順

### 1. ビルド (挙動を変えた場合)
- protocol (bincode derive 型) を変えたら **`cargo build --workspace`** (子バイナリ
  daw_audio / daw_plugin_host も再生成しないと古い protocol のまま handshake/decode 失敗)。
- それ以外でも `cargo run` は dependency を自動ビルドするが、子プロセスバイナリが上書き
  されないことがある (Windows ERROR 5)。疑わしければ先に `cargo build --workspace`。

### 2. 二重起動チェック
```powershell
Get-Process daw_gui,daw_audio,daw_plugin_host -ErrorAction SilentlyContinue | Select-Object Name,Id
```
- 何か出たら **起動しない**。ユーザーに「閉じてください」と依頼してから再確認。

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
- 例: プラグイン GUI なら `OpenSlotGuiEmbedded|SlotGuiOpened|editor window destroyed|SetSlotPlugin`。
  オーディオ経路なら `OpenPluginShmem|LoadSong|plugin_refs`。

### 6. UI 操作が要る検証はユーザーへ
- メニュー hover / プラグイン挿入 / ドラッグ等、プログラムから駆動できない確認は、起動した
  インスタンスで**ユーザーに手順を箇条書きで依頼**する (「1. ... を挿す 2. ... を hover」)。
- 結果報告を受けてログと突き合わせる。NG ならログの該当イベントから原因を追う。

## 注意
- video preview 等の visual regression は `cargo run -p daw_gui -- --smoke-test <fixture.mp4>` の
  自動検証がある (exit 0 = healthy)。texture/shared-handle 周りはこちらを併用。
- 終了はユーザーが窓を閉じる (background task の exit 通知で分かる)。自分で kill しない。
