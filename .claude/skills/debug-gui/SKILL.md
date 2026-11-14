---
name: debug-gui
description: |
  daw_gui アプリで GUI のキーバインド・ボタン・イベントが期待通り動かないときの切り分け手順
  (winit 取り込み → 配線 → emit → `AppData::handle_event` の 4 層)。
  「キーを押しても何も起きない」「ボタンが効かない」「カーソルが動かない」等、
  UI のフィードバックだけでは原因が特定できないときに発動。
  daw-ui ライブラリ (ui/) の widget 内部や examples で click / drag / focus / IME を追うときは debug-ui。
  トレース挿入 → 再ビルド → 実行 → ログ確認 → 該当層を修正の流れを提供する。
allowed-tools: Read, Grep, Glob, Edit, Bash(make build), Bash(cargo build *), Bash(cargo test -p *), Bash(./target/debug/*)
---

# GUI デバッグワークフロー (gui_01 / daw-ui ベース)

GUI のイベント（キー入力・ボタンクリック・ショートカット）が動作不明なときに、
どの層で止まっているかを切り分ける。

## 4 層モデル

GUI イベントは概ね以下の 4 層を通る。どこで消えているかでアプローチが変わる。

```
┌─ 1. winit 取り込み層 ──────────────────────┐
│ OS → winit → Runner::window_event           │
│ → daw_ui_platform::AppEvent 変換            │
│ → InputAccumulator::ingest                  │
└─────────────────────────────────────────────┘
           ↓
┌─ 2. 配線層 ────────────────────────────────┐
│ キー → UiHost::frame が ShortcutMap で照合  │
│   (typing 中に譲るキー / repeat は除外)     │
│   → view/root.rs::dispatch_shortcuts が     │
│     ui.take_shortcut(name) で受ける         │
│ pointer → ui.frame() → view 側の hit-test   │
└─────────────────────────────────────────────┘
           ↓
┌─ 3. emit 層 ───────────────────────────────┐
│ View 内 ui.push_edit / hctx.push_edit      │
│   (Edit::mutate(...))                       │
│ → frame 末尾で `&mut AppData` に apply     │
│ または background thread:                   │
│   event_proxy.send / proxy.send_event       │
│   → Runner::user_event → app.handle_event   │
└─────────────────────────────────────────────┘
           ↓
┌─ 4. handler 層 ────────────────────────────┐
│ AppData::handle_event(event) の match 分岐 │
│ → 実際の state 変更                        │
└─────────────────────────────────────────────┘
           ↓
     UiHost が自動 request_redraw → 次フレームで反映
```

## 手順

### 1. 期待動作を明確にする

「何を押したら何が起きるはずか」を書き出す。例:
- Space → shortcut `daw.play_toggle` → `AppEvent::PlayToggle` → `send_audio(AudioCommand::Play { .. })` 送信 → engine の Tick を受けて `is_playing` が立つ (handler 自身は `is_playing` を書かない)

### 2. 各層にトレースを仕込む

`tracing::info!` を下の位置に入れる (判定材料になる値を全部出す)。handler 層から入れると、届いたかどうかが
最初に分かる。

| 層 | 入れる場所 |
|---|---|
| 4. handler | `daw_gui/src/app.rs` の `AppData::handle_event` 冒頭 (`?event`) |
| 3. emit | view で `push_edit` する直前と、`Edit::mutate` の closure の中 |
| 2. 配線 | `daw_gui/src/view/root.rs` の `dispatch_shortcuts` で `ui.take_shortcut(name)` が true になった所 |
| 1. winit 取り込み | `daw_gui/src/view/runner.rs` の `window_event` の `KeyboardInput` アーム (`physical_key` / `state`) と、`dispatch_platform_event` 冒頭 |

キー → shortcut 名 → `AppEvent` の配線 (2〜4 層) は、アプリを起動しなくても headless のテストで確かめられる。
`daw_gui/src/view/root/undo_event_tests.rs` の `press()` が型 (`UiHost::no_redraw()` に本番の
`daw_shortcut_map()` を載せ、キー 1 個の `FrameInput` で `dispatch_shortcuts` を 1 フレーム回し、出た Edit を
`AppData` に適用する。`dispatch_shortcuts` は private なので `view/root/` の子 module に置く)。
起動して確かめるのは 1 層 (OS / winit / 窓のフォーカス) を疑うときと、最後の確認だけにする。

### 3. 再ビルド

`make build` で 3 exe を作り直す (clippy / check / test では exe が更新されない。CLAUDE.md「ビルドと検証の区別」)。

### 4. 実行してキー操作 → ログを確認

起動は `/verify-app` の手順で行う (起動前に一声かける・二重起動チェック・background 起動の規則は
verify-app が正本)。キーやボタンの操作が要るなら、ユーザーに手順を書いて頼む。

操作が済んだら、verify-app §3 のログ (`tracing` の日次ファイル
`%LOCALAPPDATA%\daw_01\logs\daw_gui.<YYYY-MM-DD>`。ANSI なし、既定の filter は `info`) を絞る:

```bash
grep -E "AppEvent|raw keydown|platform event|shortcut" "<上のログファイル>"
```

### 5. どこで止まったかで切り分け

| 現象 | 原因の候補 | 対応 |
|---|---|---|
| raw keydown すら出ない | キーが winit まで届いていない / window focus が他にある (Plugin GUI 等別 HWND) | OS のフォーカスを daw_gui のメインウィンドウに移す。Plugin host window が focus を奪っていないか確認。プラグインエディタが前面の間は、`SHORTCUTS` で `forward_from_external_window: true` のキーのうちプラグインが消化しなかったものだけが転送されてくる (`daw_plugin_host/src/editor_keys.rs`)。転送分は raw keydown を通らず、`PluginEvent::EditorKey` → `ui.inject_shortcut(name)` で shortcut 名として届く (`daw_gui/src/view/runner.rs`) |
| raw keydown は出るが shortcut が発火しない | `daw_gui/src/view/shortcuts.rs` の `SHORTCUTS` 表の `keys` (修飾キー) 違い / text_input が typing 中で、Ctrl / Alt / Win の付かない文字キーや `typing_only` の shortcut を text_input に譲った / OS の auto-repeat (`repeatable` でない shortcut は repeat では発火しない) / modal popup がキーボードを取っている / `set_key_grab` が生キーを先に取っている | `Modifiers { ctrl, shift, alt, logo }` を log に出す。`view/root.rs::dispatch_shortcuts` で `ui.take_shortcut(name)` が true になるかを見る (照合は `ui/crates/ui/src/ui.rs` の shortcut 層) |
| 配線層は通っているが AppEvent received が出ない | `app.handle_event` が呼ばれていない / Edit::mutate の closure が `Send + 'static` 制約で生成失敗 | view の hctx.push_edit が cached() の **外側** で呼ばれているか確認 (cached 内側は viewport_key 一致時にスキップ) |
| AppEvent received は出るが画面が変わらない | handler 内の state 変更が実際に行われていない / ui.frame の `&mut AppData` 側で apply が走っていない | AppData の該当フィールド変更を log に出す。UiHost::frame が呼ばれているか (= Runner::render_frame が走っているか) を確認 |
| 画面は変わるが古い状態が見える | 1 frame 遅延 (immediate-mode + Edit queue の宿命): edit は frame 描画の **後** に apply される | UiHost::frame が Edit を apply して自動で request_redraw するので、次のフレームで反映される (手で redraw を呼ぶ必要は無い)。次のフレームでも古いままなら、apply で state が変わっていない (上の行) か、redraw が抑止されている (`UiHost::no_redraw()` / `set_redraw_suppressed(true)`) |

### 6. 仕込んだトレースの後始末

確認が終わったらトレースを削除する。残すなら `tracing::debug!` に落とす (既定の filter は `info` なので、
`RUST_LOG=info,daw_gui=debug` を付けて起動したときだけ出る):

```rust
tracing::debug!(?event, "AppEvent received");
```

`daw_gui` に `debug-gui` のような cargo feature は無い。宣言していない feature で `#[cfg(feature = ...)]` を
書くと `unexpected_cfgs` の警告になり、`make clippy` (`-D warnings`) で落ちる。

残しておくと毎フレーム log が出てうるさい (特に Tick / TrackPeaksTick)。

## gui_01 / daw-ui 固有のハマりどころ

- **Edit の積み方**: view からは `ui.push_edit(Edit::mutate(...))`、heavy ブロック内からは
  `hctx.push_edit(...)` (どちらも pub)。
- **`hctx.cached(viewport_key, |hctx| { ... })` の中身は、viewport_key が前フレームと同じなら実行
  されない** (前フレームの描画コマンドを再生するだけ)。中で呼んだ `push_edit` もヒットテストも飛ぶので、
  Edit・ヒットテスト・動的 overlay (cursor 線、選択範囲) は cached の外側で行う
- **typing 中は一部の shortcut が text_input に譲られる**: text_input が typing focus を持つ間、
  Ctrl / Alt / Win の付かない文字キー (英数字 / Space。Shift だけ付きも含む) と、`typing_only` を宣言した
  shortcut (矢印 / Home / End / Delete / Ctrl+C・V・X・A 等) は発火せず text_input に届く。それ以外
  (Ctrl+S 等の修飾付きや F1 / F2 / F12 等) は typing 中も発火する (ui/CLAUDE.md「shortcut の属性は登録側が宣言する」)
- **scroll_delta は 1 frame 累積**: `pointer.scroll_delta` は次の `take_frame` までに入った
  ホイール回転量の合計 (pixels)。1 line ≈ 40px (LINE_HEIGHT_PX)
- **PointerFrame.modifiers**: 現在の修飾キーは `pointer.modifiers` で取れる。frame をまたいで
  保持される
- ダブルクリックの取り方と、背景スレッドからの wake は ui/CLAUDE.md「daw_01 側からの使い方」。handler が起こす
  thread の `event_proxy.send` は、送信失敗 (event loop が閉じた) を黙って捨てる
