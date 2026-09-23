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
   なる。並列作業中は特に。知らせるだけで、許可や返事は待たない。
2. **二重起動しない** (`feedback_no_duplicate_app_launch`): 対話起動の 2 つ目は single-instance gate
   (`daw_gui/src/single_instance.rs`) が既存の窓を前面化して即終了させるので、新しいビルドを確かめている
   つもりで**既存 (古いビルド) の窓を見る**ことになる (ログ `daw_gui already running; brought the existing
   window to front, exiting`)。`--script` / `--smoke-test` は gate の対象外で、audio device を奪い合いうる。
   gate が入る前は IPC を奪い合って入力不能 (「何もクリックできない」) になった。起動前に必ずプロセス確認 (下記 §2)。
3. **自分で起動する** (`feedback_launch_app_for_verification`): ユーザーに「起動して」と頼まず、
   `run_in_background` で自分が起動する。ただし振る舞いの目視 (メニュー hover 等 UI 操作が要る
   検証) はユーザーに依頼する。パイプ (`| tail` / `| tee` 等) を付けて起動しない (`feedback_launch_no_tail_pipe` —
   `| tail` 越しの起動が I/O abort で落ち、コードは正常なのにクラッシュに見えた)。ログは §3 のファイルで読む。
4. **動いているアプリを kill しない** (`feedback_no_kill_running_app`): `taskkill` 等で止めない。
   既存起動があればユーザーに「閉じてください」と依頼。

## 手順

### 1. ビルド (挙動を変えた場合)
- daw_audio / daw_plugin_host / `common` を変えたら **`make build`** で 3 exe を揃える
  (CLAUDE.md「ビルドと検証の区別」)。`cargo run -p daw_gui` は daw_gui しかビルドしないので、子 exe は
  古いまま動く (protocol = bincode derive 型が変わっていれば handshake / decode に失敗する)。
- 起動中のプロセスの exe は上書きされないことがある (Windows ERROR 5)。
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
make run   # run_in_background: true。パイプを付けない
```
- `run_in_background: true` で起動 (フォアグラウンド blocking 不可)。`make run` は §2 と同じ preflight →
  `make build` (3 exe) → `cargo run -p daw_gui` の順に回る。
- ログの読み先は `%LOCALAPPDATA%` 配下の `daw_01/logs/<daw_gui|daw_audio|daw_plugin_host>.YYYY-MM-DD`
  (プロセスごとの日次ファイル。日付は UTC、ANSI 無し、同じ日の前回起動分も入っている)。
  background task の出力 (debug ビルドは子プロセスも同じ stdout に出る。ANSI 付き) も読める。

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
grep -iE "<確認したいイベント>" <§3 のログ> | sed -E 's/\x1b\[[0-9;]*m//g' | tail -40
```
- 例: プラグイン GUI なら `open_slot_gui|received command other=OpenSlotGuiEmbedded|plugin gui opened|plugin editor closed|editor window destroyed|received SetSlotPlugin`。
  オーディオ経路なら `plugin shmem registered|plugin shmem dropped|song identity switched|received Play|received Stop`。
  command の全文 (`sending to plugin_host` / `sending to audio`) は debug レベルなので、`RUST_LOG=info,daw_gui=debug`
  を付けて起動したときだけ出る。

### 6. 操作が要る検証
- 操作は先に JS ドライバで駆動する: `cargo run -p daw_gui --features script -- --script <js> --gui`
  (`--gui` を付けると窓・wgpu・poller が生きた本物の GUI で同じ JS が走る。外すと headless)。
  曲を開く (`daw.appOpenProject`)・プラグイン挿入 (`daw.setSlotPlugin`)・再生 (`daw.appPlay`) などは既にある
  (一覧は `daw_gui/src/script.rs` の `DAW_API`)。足りない操作は script API に足す — 別 flag や UI 自動化を
  新設しない (`feedback_js_driver_is_universal` / `feedback_prefer_headless_verification`)。
- JS で駆動できない確認 (プラグインエディタ内のメニュー hover、手で行うドラッグの追従や手触り、見た目、音) だけを、起動した
  インスタンスで**ユーザーに手順を箇条書きで依頼**する (「1. ... を挿す 2. ... を hover」)。
- 結果報告を受けてログと突き合わせる。NG ならログの該当イベントから原因を追う。

### 7. 多面機能の検証 / 新機能のバグ報告の追い方
- **同じ capability が複数の UI 面に跨る機能は、全面を列挙してから「完成」と言う**。
  例: per-control modulation は param コントロールが複数ある —
  画像 PiP / グループ Transform / プラグイン param / テキスト / track vol-pan / song tempo。
  1 面 (画像) だけ配線して全機能完成と報告 → ユーザーの実使用面 (グループ Transform) が
  未配線で「動かない」になった。検証は**ユーザーが実際に使う面**で行い、配線済み面を全部試す
  ([[feedback_enumerate_complete_feature_set]] / [[feedback_new_feature_bug_suspect_own_wiring]])。
- **「動かない」報告は、ユーザー操作ミス・環境・飽和を仮定する前に、自分の新コード/未配線を
  第一容疑にする**。推測で原因を断定せず、**一時診断ログ** (`tracing::info!`、後で削除) を
  仕込んで実データを取ってから仮説を立てる (CLAUDE.md「デバッグ」)。
  - 切り分けは**既存 UI の観測量**を先に使う (例: source meter が動く=follower 正常 → bug は下流)。
  - パイプライン全体 (生成→IPC→poll→compose→描画) を上流から1点ずつ潰す。前提を確認せず
    「ユーザーが手順を抜かした」と決めつけない (実際は抜けていなかった)。

## 注意
- video preview 等の visual regression は `cargo run -p daw_gui -- --smoke-test <fixture.mp4>` の
  自動検証がある (exit 0 = healthy)。texture/shared-handle 周りはこちらを併用。これも窓を出すので、
  起動前の一声 (不変ルール 1) は同じく要る。
- 終了はユーザーが窓を閉じる (background task の exit 通知で分かる)。自分で kill しない。
