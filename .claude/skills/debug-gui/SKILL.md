---
name: debug-gui
description: |
  GUI のキーバインド・ボタン・イベントが期待通り動かないときの切り分け手順。
  「キーを押しても何も起きない」「ボタンが効かない」「カーソルが動かない」等、
  UI のフィードバックだけでは原因が特定できないときに発動。
  トレース挿入 → 再ビルド → 実行 → ログ確認 → 該当層を修正の流れを提供する。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(./target/debug/*)
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
│ Keyboard 単発 → Runner::dispatch_shortcut   │
│   → app.handle_event 直接                  │
│ それ以外 → InputAccumulator → ui.frame()   │
│   → view 側の pointer hit-test              │
└─────────────────────────────────────────────┘
           ↓
┌─ 3. emit 層 ───────────────────────────────┐
│ View 内 hctx.push_edit(Edit::mutate(...))  │
│ → frame 末尾で `&mut AppData` に apply     │
│ または background thread:                   │
│   event_proxy.send_event(AppEvent::X)       │
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
- Space → AppEvent::PlayToggle → `is_playing` が反転、`send_audio(Play)` 送信

### 2. 各層にトレースを仕込む

**handler 層（最上流で一番わかりやすい）**:

```rust
// daw_gui/src/app.rs の handle_event 冒頭
pub fn handle_event(&mut self, event: AppEvent) {
    tracing::info!(?event, "AppEvent received");
    if Self::is_undoable(&event) { ... }
    match event { ... }
}
```

**emit 層 (view から)**:

```rust
// view 内で push_edit する直前
hctx.push_edit(Edit::mutate(move |app: &mut AppData| {
    tracing::info!("about to apply Foo edit");
    app.handle_event(AppEvent::Foo);
}));
```

**配線層 (Runner)**:

```rust
// daw_gui/src/view/runner.rs の window_event の KeyboardInput アーム
WindowEvent::KeyboardInput { event, .. } => {
    tracing::info!(?event.physical_key, ?event.state, "raw keydown");
    // 既存の dispatch_shortcut / dispatch_platform_event
}
```

**winit 取り込み層 (一番下流)**:

```rust
// dispatch_platform_event 冒頭
fn dispatch_platform_event(&mut self, ev: PlatformEvent) {
    tracing::info!(?ev, "platform event");
    // ...
}
```

### 3. **再ビルドを明示**（必須）

```bash
make build       # = cargo build --workspace
```

`cargo clippy` / `cargo check` / `cargo test` だけでは **exe が更新されない**。
古いバイナリで検証すると「直したはずなのに動かない」で時間を溶かす。
（このプロジェクトで 2 回繰り返している。[feedback_build_after_clippy.md] 参照）

子プロセス側 (daw_audio / daw_plugin_host) も `make build` で workspace 全体を rebuild する。

### 4. 実行してキー操作 → ログを確認

```bash
make run
```

操作したら閉じて `grep` でログを絞る:

```bash
grep -E "AppEvent|raw keydown|platform event" <output-file>
```

### 5. どこで止まったかで切り分け

| 現象 | 原因の候補 | 対応 |
|---|---|---|
| raw keydown すら出ない | キーが winit まで届いていない / window focus が他にある (Plugin GUI 等別 HWND) | OS のフォーカスを daw_gui のメインウィンドウに移す。Plugin host window が focus を奪っていないか確認 |
| raw keydown は出るが dispatch_shortcut で消えている | 修飾キーの組み合わせ違い / `dispatch_shortcut` の match に漏れ / Ctrl のはずが Shift 単独などの誤判定 | `Modifiers { ctrl, shift, alt, logo }` の状態を log に出す。`only_ctrl` / `ctrl_shift` 判定ロジックを見直す |
| 配線層は通っているが AppEvent received が出ない | `app.handle_event` が呼ばれていない / Edit::mutate の closure が `Send + 'static` 制約で生成失敗 | view の hctx.push_edit が cached() の **外側** で呼ばれているか確認 (cached 内側は viewport_key 一致時にスキップ) |
| AppEvent received は出るが画面が変わらない | handler 内の state 変更が実際に行われていない / ui.frame の `&mut AppData` 側で apply が走っていない | AppData の該当フィールド変更を log に出す。UiHost::frame が呼ばれているか (= Runner::render_frame が走っているか) を確認 |
| 画面は変わるが古い状態が見える | 1 frame 遅延 (immediate-mode + Edit queue の宿命): edit は frame 描画の **後** に apply される | UiHost::frame が次フレームで自動 request_redraw を呼ぶので 2 frame 後には反映される。手動で window.request_redraw() を呼ぶと早まる |

### 6. 仕込んだトレースの後始末

確認が終わったらトレースを削除するか、debug feature で囲む:

```rust
#[cfg(feature = "debug-gui")]
tracing::info!(?event, "AppEvent received");
```

残しておくと毎フレーム log が出てうるさい (特に Tick / TrackPeaksTick)。

## gui_01 / daw-ui 固有のハマりどころ

- **`Ui::push_edit` は `pub(crate)`**: 通常 view から呼べない。代わりに `ui.heavy(id, |hctx| {
  hctx.push_edit(...) })` を使う。HeavyCtx は `push_edit` を pub で expose
- **`hctx.cached(viewport_key, |hctx| { ... })` 内の Edit は通常 OK だが、**描画コマンド**は
  cache hit 時にスキップされて再描画されない。動的 overlay (cursor 線、選択範囲) は cached
  の外側で `hctx.push_*` する
- **focused widget がキー入力を独占**: `text_input` が focus を持っているとき、Runner の
  global shortcut は dispatch_shortcut で skip される (`ui.focused_widget().is_none()` チェック)
- **scroll_delta は 1 frame 累積**: `pointer.scroll_delta` は次の `take_frame` までに入った
  ホイール回転量の合計 (pixels)。1 line ≈ 40px (LINE_HEIGHT_PX)
- **PointerFrame.modifiers**: 現在の修飾キーは `pointer.modifiers` で取れる。frame をまたいで
  保持される
- **ダブルクリック検出は自前**: gui_01 v1 は built-in 無し。`AppData::last_click` に最終クリック
  情報を持って 400ms+5px 以内なら double 判定 (arrangement_view / piano_roll_view 参照)
- **背景スレッドからの wake**: `event_proxy.send_event(AppEvent::X)` は失敗する (UI が閉じた)
  と `Err`。loop はそれで break する

## 参考コミット

- `8050184` GUI を Vizia から ../gui_01 (daw-ui) に置き換え (本ワークフローのリライト元)
- `4312dab` hjkl カーソル + ノート入力・編集（旧 Vizia 時代に本ワークフローで bug を切り分けた）
