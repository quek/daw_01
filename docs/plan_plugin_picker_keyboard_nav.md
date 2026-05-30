# Plugin Picker 絞り込み後のカーソル選択 (type-ahead picker)

プラグインピッカー (`daw_gui/src/view/plugin_picker.rs`) で検索ボックスに入力して絞り込んだ
後、↑↓ キーで候補リストのカーソルを動かし、Enter でカーソル位置の候補を確定する
(VS Code コマンドパレット / Ableton ブラウザ等の type-ahead picker 標準挙動)。

## 最終形態 (完成イメージ)

1. modal を開くと検索ボックスが自動 focus (既存)。
2. すぐタイプして name / vendor で絞り込み (既存、subsequence マッチ)。
3. 絞り込み結果リストの **先頭にカーソル (青ハイライト) がデフォルトで乗る**。
4. **↓ で次候補、↑ で前候補** へカーソル移動。検索ボックスの focus は保ったまま
   (タイプし続けられる)。
5. カーソルは `[0, visible.len()-1]` で **clamp** (端で停止、wrap しない)。
6. **Enter はカーソル位置の候補を確定** (現状の「先頭固定」を廃止)。
7. タイプして絞り込み結果が変わったらカーソルを先頭 (0) にリセット。
8. マウス hover ハイライト / クリック即確定は既存どおり。

## 現状と問題

- 検索ボックス `pp_search` は `text_input_at_focused` で常時 focus を取る
  (`plugin_picker.rs:96`)。
- Enter 確定は `plugin_picker_visible.first()` 固定 (`plugin_picker.rs:119-126`)。
  → 2 番目以降を選べない。
- カーソル移動の経路が無い。

## なぜ daw_01 側だけで実現できないか (一次情報)

検索ボックスに focus があるまま ↑↓ を拾う必要があるが:

1. text_input は focus 中、`take_keyboard_events_if_focused` で **全 KeyEvent を
   `std::mem::take` で奪って空にする** (`gui_01 crates/ui/src/ui.rs:1693-1699`)。
2. text_input のキー処理ループに ArrowUp/Down のアームが無く `_` に落ち、`ev.text` が
   None なので **無視・破棄** される (`gui_01 crates/ui/src/widgets/text_input.rs:254-323`、
   特に :309 の `_` アーム)。→ ↑↓ は view に届かない。
3. 「修飾なし矢印を global shortcut bind」する逃げ道は gui_01 が明示的に禁止
   (`gui_01 crates/ui/src/shortcut.rs:211-216`)。bind すると **plugin picker が閉じている
   時も** 全アプリの ↑↓ を奪い、text_input の内部矢印処理 (Left/Right cursor 移動) と
   競合する。#056 (Phase 85) の typing 中 suppress は printable 文字キー限定で、矢印は
   対象外 (typing 中でも global 消費される) なので副作用は変わらない。
4. daw_01 view 層は現状 ↑↓ を一切使っていない (`daw_gui/src/view` で ArrowUp/Down 参照は
   runner.rs のキー変換テーブルのみ)。将来 ↑↓ を別 view 機能に使う余地を残すためにも
   global 占有は避けたい。

→ 理想は **gui_01 の text_input が focus 中の ↑↓ を呼び出し側に委譲** すること。
text_input は単一行で ↑↓ を内部利用していないので、委譲しても既存挙動を壊さない。

## gui_01 への要望 (#057)

`TextInputResponse` に focus 中の ↑↓ 押下を返す field を追加 (API 設計は gui_01 に委ねる)。
最小イメージ:

```rust
pub struct TextInputResponse {
    pub focused: bool,
    pub committed: bool,
    pub committed_text: Option<String>,
    /// focus 中にこのフレームで押された ↑ / ↓ (text_input は単一行で未使用)。
    /// type-ahead picker / combobox が候補リストの cursor 移動に使う。
    /// Left/Right は cursor 移動に使うため返さない。
    pub nav_up: bool,
    pub nav_down: bool,
}
```

## daw_01 側の実装 (gui_01 #057 解決後)

1. `AppData` にカーソル state `plugin_picker_cursor: usize` を追加。
2. カーソルを 0 リセットするタイミング = `refresh_picker_visible` を呼ぶ箇所
   (`OpenPluginPickerFor` / `SetPluginPickerQuery` / rescan 完了)。一番シンプルなのは
   `refresh_picker_visible` 末尾で `plugin_picker_cursor = 0`。
3. `AppEvent::MovePluginPickerCursor(delta: i32)` を追加し、
   `cursor = (cursor as i32 + delta).clamp(0, len as i32 - 1)` で更新 (len==0 は no-op)。
4. `plugin_picker.rs`:
   - `text_input_at_focused` の戻り値 `resp.nav_down` / `resp.nav_up` を読み、
     `MovePluginPickerCursor(+1 / -1)` を dispatch。
   - Enter (`resp.committed`) は `visible.first()` ではなく `visible.get(cursor)` を確定。
   - `list_view(..., Some(cursor), ...)` でカーソル行をハイライト。
   - hover / click は既存維持 (クリックで即確定)。

## 注意

- gui_01 #057 の API 追加が前提。解決前は daw_01 側は着手しない
  (interim 実装に走らない — feedback_gui_01_request_before_interim)。
