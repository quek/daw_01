---
name: debug-ui
description: |
  UI のクリック・ドラッグ・キーバインド・focus・IME が期待どおり動かないときの切り分け手順。
  「クリックが効かない」「ドラッグが反応しない」「Ctrl+key が拾われない」「focus が外れる」
  「IME 候補が変な位置に出る」等、可視フィードバックでは原因が特定できないときに発動。
  トレース挿入 → 再ビルド → 実行 → ログ確認 → 該当層を修正の流れを提供する。
allowed-tools: Read, Grep, Glob, Edit, Bash(cargo build *), Bash(cargo run *), Bash(cargo test *)
---

<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# UI デバッグワークフロー (gui_01)

GUI のイベント (クリック / ドラッグ / キー / focus / IME) が動作不明なとき、どの層で
止まっているかを切り分ける。本プロジェクトは **winit + 自作 `Ui<'a, M>` + `Edit<M>`**
スタックなので、Vizia / iced とは違う固有の落とし穴がある。

## 3 層モデル

入力イベントは概ね以下の 3 層を通る。どの層で消えているかでアプローチが変わる。

```
┌─ 1. 入力取り込み層 ─────────────────────────┐
│ OS → winit::WindowEvent → AppEvent          │
│ → InputAccumulator (cur_pos / pending_keys) │
└────────────────────────────────────────────┘
           ↓
┌─ 2. widget 層 ──────────────────────────────────┐
│ Ui::frame closure 内で widget が                │
│ self.pointer (PointerFrame) / take_keyboard_..  │
│ を読む。hit-test / drag_anchor / press_started  │
└────────────────────────────────────────────────┘
           ↓
┌─ 3. Edit 層 ─────────────────────────────────┐
│ widget が Edit::mutate(...) を push          │
│ → frame() の戻りで apply、Model 状態が変わる  │
└─────────────────────────────────────────────┘
           ↓
     次フレームで描画反映 (request_redraw)
```

## 手順

### 1. 期待動作を明確にする

「何を入力したら何が起きるはずか」を書き出す。例:
- thumb をクリック → drag 開始、`primary_pressed=true` の間 anchor から計算 → `Edit::mutate(value)` 発行 → 次フレームで thumb が動く
- ダブルクリック → `last_click` 内側に Some、threshold 内なら `default_value` で Edit 発行、drag は始まらない

### 2. 各層にトレースを仕込む

#### Edit 層 (最上流、Model に到達したかが一目で見える)

```rust
// crates/examples/<name>/src/main.rs の build_ui の戻り側で:
let edits = self.ui.frame(...);
for e in edits {
    eprintln!("[edit] applying"); // ← 一時的に。released で削除
    e.apply(&mut self.model);
}
```

または widget 内で発行直前:

```rust
if (displayed_value - value).abs() > f32::EPSILON {
    eprintln!("[fader] emit value={} (was {})", displayed_value, value);
    self.push_edit(on_change(displayed_value));
}
```

#### widget 層 (state や hit-test の判定確認)

```rust
// fader.rs の press 判定箇所など
if pointer.primary_just_pressed {
    eprintln!(
        "[fader] press pos={:?} thumb={:?} contains={}",
        pointer.pos,
        thumb_rect,
        pointer.pos.is_some_and(|(x,y)| thumb_rect.contains(x,y)),
    );
}
```

`pointer.modifiers.ctrl` / `state.drag_anchor.is_some()` など、その時点の判定材料を全部出す。

#### 入力取り込み層 (winit / InputAccumulator が呼ばれているか)

```rust
// crates/platform/src/winit_backend.rs の window_event 内
WindowEvent::MouseInput { state, button, .. } => {
    eprintln!("[winit] MouseInput {:?} {:?}", button, state);
    ...
}
WindowEvent::ModifiersChanged(mods) => {
    eprintln!("[winit] modifiers {:?}", mods.state());
    ...
}
```

または `InputAccumulator::ingest` で:

```rust
pub fn ingest(&mut self, ev: &AppEvent) {
    eprintln!("[input] {:?}", ev);
    ...
}
```

### 3. 再ビルドを明示

```bash
cargo run --bin mixer                   # 例: mixer で再現させる
# または
cargo build && ./target/debug/mixer.exe
```

`cargo clippy` / `cargo check` / `cargo test` だけでは **exe が更新されない**。
古いバイナリで検証すると「直したはずなのに動かない」になるので必ず `cargo run` か `cargo build` を明示。

### 4. 操作 → ログを確認

操作したらウィンドウを閉じてログを絞る:

```bash
# bash (出力ファイルから)
grep -E "\[edit\]|\[fader\]|\[winit\]|\[input\]" output.log
```

### 5. どこで止まったかで切り分け

| 現象 | ありがちな原因 | 対応 |
|---|---|---|
| `[winit] MouseInput` すら出ない | OS が winit にイベントを届けていない / focus が他ウィンドウ | OS 側を疑う、別アプリで確認 |
| `[winit] MouseInput` は出るが `[input] PointerInput` が出ない | `InputAccumulator::ingest` の match 漏れ / `MouseButton::Left` 以外で来ている | match の他ボタン (Middle/Right/Other) を追加、`primary` 限定の判定を見直す |
| `[input]` は出るが widget の press 判定 trace が出ない | widget が hit-test に失敗 (`pointer.pos = None` / rect.contains が false) | **Alt-Tab 復帰直後の罠**: `cur_pos = None` のまま MouseInput が来るケース。winit_backend の `query_cursor_pos_in_window` が動いているか確認 (CLAUDE.md「既知の罠」参照) |
| widget は press 判定通るが Edit が出ない | `(displayed_value - value).abs() > f32::EPSILON` を満たしていない / on_change closure が呼ばれていない | drag 中の値計算が止まっていないか、`drag_anchor` が None になっていないか |
| Edit は出るが画面が更新されない | `apply` 後に `request_redraw` が呼ばれていない / `had_edits` 検出パスが動いていない | mixer の `App::on_render` の `if had_edits || ... { request_redraw }` を確認 (CLAUDE.md「immediate-mode + Edit queue の必然」参照) |
| Ctrl+drag が効かない | `pointer.modifiers.ctrl` が false / `WindowEvent::ModifiersChanged` が拾えていない | winit_backend の ModifiersChanged 分岐を確認、winit 0.30 の `mods.state().control_key()` を使っているか |
| ダブルクリックが反応しない | `last_click` が None のまま / 距離 / 時間しきい値外 | `Instant::now()` の duration、`hypot` の閾値、thumb_rect.contains が両方の press で true か |
| focus が外れて keyboard が拾われない | クリック先が誰も `set_focus` を呼ばない widget なので blur した | text_input 等の focus を持つ widget が pending_focus 経由で同フレーム反映できているか確認 |
| IME 候補ウィンドウが変な位置 | `set_ime_cursor_area` が古い座標で呼ばれている / focus 変化時に再要求していない | `UiHost::ime_request()` の Some/None 切替差分が App 側で正しく拾われているか |

### 6. 仕込んだトレースの後始末

確認が終わったら trace を削除する。一時的にだけ残したいなら:

```rust
#[cfg(debug_assertions)]
eprintln!("[fader] press pos={:?}", pointer.pos);
```

`crates/examples/` の中なら release ビルドでも残しても影響少ないが、`crates/ui/` / `crates/renderer/` / `crates/platform/` には残さない (ライブラリ呼び出し側に流れる)。

## gui_01 固有のハマりどころ

CLAUDE.md「既知の罠」と一部重複するが、デバッグ視点で再掲:

- **Alt-Tab 復帰直後のクリックで hit-test が空振り**: `cur_pos = None` のまま MouseInput が来る Windows の挙動。`winit_backend.rs` の `query_cursor_pos_in_window` workaround が動いているか先に確認。これを疑う前に他の原因を追うと時間を溶かす。
- **convenience method の widget は cursor + next_y で full-width 配置**: `Ui::button` 等は `cursor.w - pad*2` で横幅 100% を取る。右側に別領域 (例: mixer のチャンネルストリップ) を置くと視覚的に重なる。`button_at` で rect 限定するか、ストリップの y を下にずらす。
- **press_started_inside パターン**: button / checkbox は press 時に `inside` を state に保持し、release 時の click 判定で使う。armed 状態を見ずに pure pointer event だけ見るとクリックが拾えない。
- **focus の即時反映**: `set_focus` 呼び出しは `pending_focus` 経由で同フレーム内に `is_focused()` に反映される。前フレームの focused widget を見るには `UiHost::focused_widget()` 経由。
- **Ctrl mid-drag toggle で値 jump**: drag 中に Ctrl 状態が変わったら anchor を `(現在 py, 現在 value, 現在 ctrl)` に張り直す。さもなくば cumulative-from-anchor delta が一気にスケール変わって jump する (Phase 4d)。

## 参考実装 / コミット

- `e649e5a` Phase 1 — fader + 入力周りバグ修正 (Alt-Tab 復帰、armed-state、edit 後の追加 redraw)
- `b926dca` Phase 4d — fader/knob ダブルクリックリセット + Ctrl+drag (modifier 配線、mid-drag re-anchor)
- `crates/ui/src/widgets/fader.rs::tests` — UiHost::frame 経由での挙動テストの様式
- `crates/platform/src/winit_backend.rs` — winit Event の取り込み、modifier 処理、focus-click workaround
