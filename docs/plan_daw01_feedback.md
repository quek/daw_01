# Plan: daw_01 フィードバック対応 (11 項目、P0-P3)

## Context

daw_01 (実 DAW プロトタイプ、`F:/dev/daw_01`、gui_01 を path 依存) で M7 / M8 を使った結果、改善要望が 11 項目届いた。優先度: **P0** (即必要、実害あり) → **P1** (高利得、boilerplate の主原因) → **P2** (中利得、領域整備) → **P3** (低利得、あれば嬉しい)。

本ドキュメントは [docs/plan.md](plan.md) (M9 = Real DAW Validation) と並行する API 改善プラン。Phase 41-44 (piano_roll note edit / sample_edit_ops Undoable / debug overlay / ergonomic 評価) の前提となる API 整備として、本プランの P0-P1 を先行実施することが多い (例: P0-1 は piano_roll の `Shift+/` shortcut で必要、P1-3 は piano_roll の rect-select で必要、P1-4 は piano_roll の double-click で Piano Roll タブへ飛ぶ UX で必要)。

### 全体方針

- 各項目は **独立 commit**。`cargo test --workspace` と `cargo clippy --workspace -- -D warnings` を維持
- 「後方互換性は捨てて大胆に書き換えてよい」「`#[deprecated]` 残置不要、リネーム / 削除 OK」(CLAUDE.md の「理想とベストプラクティスを追求する」方針)
- 既存 example (mixer / piano_roll / arrangement / daw_prototype 等) は変更に合わせて 1 commit 内で更新
- 各項目の実装前に設計選択 (本ドキュメントで「**未決**」とマークされた箇所) を AskUserQuestion で確認
- 実装後に該当項目を本ファイルでマーク (✅ done) し、`docs/history.md` に完了記録を 1 ブロック追記

### 優先順序ロードマップ

| 順 | Pri | # | テーマ | 着手契機 |
|---|---|---|---|---|
| 1 | P0 | 1 | `Shortcut::parse` 記号キー受理 + `try_parse` | daw_01 起動 panic、最小影響 |
| 2 | P0 | 2 | `tab_view` 外部 selected state | daw_01 のクリップ → Piano Roll タブ UX |
| 3 | P1 | 3 | `HeavyCtx` に input/popup pull API delegate | piano_roll multi-select の 2 段構造解消 |
| 4 | P1 | 4 | `take_double_click_in_rect` API | clip / note の double-click 編集 UX |
| 5 | P1 | 5 | menu item 動的 enable + shortcut hint | Undo (label) / disabled menu / "Ctrl+Z" 表記 |
| 6 | P2 | 6 | time_ruler / bar_beat_grid の beat 単位 overload | piano_roll 系で `* spb` boilerplate 解消 |
| 7 | P2 | 7 | scroll_area の widget_state 内蔵版 | viewport 専用フィールドを抱えずに scroll 可 |
| 8 | P2 | 8 | focus 中 fader/knob の矢印 step | accessibility / power-user 操作 |
| 9 | P3 | 9 | `ArboardClipboard` の bytes 実装 | MIDI / audio buffer の clipboard |
| 10 | P3 | 10 | file dialog 非同期化 | 大量 file 列挙時の UI ブロック解消 |
| 11 | P3 | 11 | `eprintln!` を `tracing` crate に統一 | ログ環境統合 |

### 共通 DoD (各項目で同じ)

- `cargo build --workspace` ✅
- `cargo test --workspace` ✅ (新規 tests + 既存 tests pass)
- `cargo clippy --workspace --tests -- -D warnings` ✅
- 実機: 影響する example で `cargo run --bin <name>` 動作確認 (UI 系は目視)
- daw_01 build: `cd ../daw_01 && cargo build` で path 依存先の build が壊れていないこと
- 1 commit、message: `feat(M9): P<pri>-<#> — <短い概要>` + 詳細を bullet で
- 完了後、本ファイルを編集して該当項目を ✅ にマーク

---

## P0-1: `Shortcut::parse` が punctuation で panic する

✅ done (commit eeb2ea3)

### Problem

[crates/ui/src/shortcut.rs:115](../crates/ui/src/shortcut.rs#L115) `panic!("Shortcut::parse: unknown key token")`。`parse_key_token` は alphanumeric / 特殊キー / F1-F24 のみ受理。`/` `;` `,` `.` `[` `]` `\` `'` 等で起動時 panic。

daw_01 で `m.bind("toggle_help", "Shift+/")` (Help 慣用 binding) で panic。

### Design

**A. `PhysicalKey::Char(char)` を ASCII 印字可能キー全般に domain 拡張** (採用)。

理由: 現在の暗黙ルール「圧縮可能 family は generic+payload」(26 letters → `Char(char)`、10 digits → `Digit(u8)`、24 funcs → `F(u8)`) と整合。11 dedicated variant を加えると 26 letters と非対称になる。

不採用: B (11 個の dedicated variant) / C (`Punct(char)` 別 variant)。

### Proposed API

```rust
// crates/ui/src/shortcut.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutParseError {
    pub spec: String,
    pub reason: String,
}
impl std::fmt::Display for ShortcutParseError { /* "Shortcut::parse: <reason> in <spec>" */ }
impl std::error::Error for ShortcutParseError {}

impl Shortcut {
    pub fn parse(spec: &str) -> Self {                        // 既存、起動時 const literal 用 (panic on bad)
        Self::try_parse(spec).unwrap_or_else(|e| panic!("{e}"))
    }
    pub fn try_parse(spec: &str) -> Result<Self, ShortcutParseError>;  // 新規、runtime spec 用
}
```

### 受理する記号キー (11 個)

`/  ;  ,  .  -  =  [  ]  \  '  \``  → それぞれ `PhysicalKey::Char(c)`

### Files

- [crates/platform/src/event.rs](../crates/platform/src/event.rs) — `PhysicalKey::Char` の doc コメント拡張 (「ASCII 印字可能キー」)
- [crates/platform/src/winit_backend.rs](../crates/platform/src/winit_backend.rs) — `map_phys_key` に `KeyCode::{Slash, Semicolon, Comma, Period, Minus, Equal, BracketLeft, BracketRight, Backslash, Quote, Backquote} => Char(c)` の 11 行追加
- [crates/ui/src/shortcut.rs](../crates/ui/src/shortcut.rs) — `ShortcutParseError` struct、`try_parse` 追加、`parse` を `try_parse` 委譲化、`parse_key_token` を Result 化 + 記号受理、doc コメント更新、tests 追加
- [crates/ui/src/lib.rs](../crates/ui/src/lib.rs) — `ShortcutParseError` re-export

### Tests (shortcut.rs `#[cfg(test)] mod tests`)

- `parse_punctuation_keys` — `/ ; , . [ ' \``  各 char に正しく解釈
- `try_parse_returns_error_for_unknown_token` / `_for_empty_token` / `_for_no_key`
- `try_parse_succeeds_for_valid` — `Ctrl+Shift+/`
- `format_for_punctuation` — `display_for("toggle_help")` → `"Shift+/"`

### Out of scope

- `with_default_bindings()` への `Shift+/` (Help) 追加 — daw_01 自前 bind
- `+` の escape 仕様 — 要望なし、現状制約継続
- 非 ASCII キー (JP `¥` 等) — `Other(u32)` fallback 継続
- 既存 examples の更新 — API 互換のため不要

### Open question (実装前確認済 ✅)

なし。設計 A は確定済み。

### Commit

```
feat(M9): P0-1 — Shortcut::parse で記号キー受理 + try_parse 追加
```

---

## P0-2: `Ui::tab_view` で外部から `selected` を変更できない

✅ done (commit e66edaa)

実装結論:
- 未決 1 → A 案 (両方残す) を採用。`tab_view` (内部 state) + `tab_view_with_state` (外部 `&mut usize`) の 2 method を併存。同 id なら widget_state を共有し、外部値が internal を上書き sync するため途中切替も動く (回帰テスト `tab_view_internal_and_with_state_share_widget_state` で固定)。
- 未決 2 → clamp + 書き戻し採用。dynamic な tab 増減 (close tab 等) で外部値が古くなっても次フレームで自動修正される。

### Problem

[crates/ui/src/widgets/tab_view.rs:19](../crates/ui/src/widgets/tab_view.rs#L19) `TabState` は `pub(crate)`、`Ui::widget_state` も `pub(crate)`。tab_view 内部 state を library 外から触る手段がない。

daw_01 のアレンジビューでクリップを double click → Piano Roll タブに飛ぶ UX が再現できず、tab_view 採用を棄却した。

### Design

**A. 外部 state 版 `tab_view_with_state` を追加 + 既存 `tab_view` (内部 state) は keep** (推奨)。

理由: 外部 state 版だけにすると単純な使い方 (just show tabs, don't care which is selected) で利用者が `let mut selected = 0;` を書く必要が出て KISS に反する。両方 keep が最もユーザにとって楽。同じ widget_state を共有 (id 一致時) すれば「途中から外部制御」も自然。

不採用: B (setter `Ui::set_tab_selected(id, idx)`) — ユーザが「現在の状態」を query して書き戻す形になり、inversion of control が損なわれる。

### Proposed API

```rust
impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 内部 widget_state で selected を保持する (現状の `tab_view` と同じ)。
    pub fn tab_view<F>(&mut self, id: impl Hash, rect: Rect, f: F)
    where F: FnOnce(&mut TabBuilder<'_, 'a, M>);

    /// 外部 `&mut usize` で selected を borrow する。同じ id で `tab_view` と
    /// `tab_view_with_state` を切り替えても widget_state は共有 (内部 selected が
    /// 外部 selected と sync する)。
    pub fn tab_view_with_state<F>(&mut self, id: impl Hash, rect: Rect, selected: &mut usize, f: F)
    where F: FnOnce(&mut TabBuilder<'_, 'a, M>);
}
```

外部 selected が tab 数を超えていた場合の clamp ポリシー: **builder 内で last-valid に clamp** (内部 selected と同じ flow)。

### Files

- [crates/ui/src/widgets/tab_view.rs](../crates/ui/src/widgets/tab_view.rs) — `tab_view_with_state` 実装、`TabState` の selected と外部 `&mut usize` を sync
- [crates/examples/daw_prototype/src/main.rs](../crates/examples/daw_prototype/src/main.rs) — メニュー or footer に「Open Piano Roll」ボタンを追加し、外部から tab を切替するデモ

### Tests

- `tab_view_with_state_respects_external` — 外部 `selected = 2` で tab[2] が active
- `tab_view_with_state_writes_back_on_click` — tab click で外部 `selected` が更新される
- `tab_view_with_state_clamps_out_of_bounds` — 外部 `selected = 99` で last tab に clamp + 書き戻し

### Out of scope

- tab 順序の dynamic 変更 (add / remove / reorder API) — 要望なし
- closable tab — 要望なし

### Open question (実装前確認)

- **未決 1**: `tab_view` (内部) と `tab_view_with_state` (外部) を両方残すか、`tab_view` を削除して `tab_view_with_state` だけにするか?
  - 推奨: 両方残す (single-tab demos が冗長にならない)
- **未決 2**: 外部 selected が範囲外のとき clamp / panic / error log のどれ?
  - 推奨: clamp + 書き戻し (no-panic、debug 時に気付ける)

### Commit

```
feat(M9): P0-2 — tab_view_with_state で外部 selected を borrow 可能に
```

---

## P1-3: `HeavyCtx` に input/popup pull API を delegate

### Problem

`HeavyCtx` には `pointer` / `push_edit` / 描画系はあるが、`take_drag_rect_in_rect` / `take_file_drop_in_rect` / `take_clipboard_paste` / `context_menu_for` / `take_shortcut` は `Ui` のみ。

daw_01 の piano_roll 矩形選択で「heavy 外で drag を pull → drag_consumed フラグで heavy 内 release を抑制」の 2 段構造になり、release 処理が分散。

### Design

**A. `HeavyCtx` に input/popup pull API を delegate** (推奨、ユーザ提案通り)。

理由: heavy() は「巨大ビュー全体を 1 widget として扱う」抽象。その内部で input を pull できないのは抽象の漏れ。delegate なら implementation cost も低い (各 method は 1 行 forward だけ)。

不採用: B (heavy を pure 描画キャッシュに絞る doc 規約) — 全 example に適用するコストが高く、heavy 外で input pull する規約が逆に不自然。

### Proposed API

`HeavyCtx` に以下を delegate (すべて 1 行 forward):

```rust
impl<'b, 'a, M: ?Sized + 'static> HeavyCtx<'b, 'a, M> {
    // 既存
    pub fn pointer(&self) -> PointerFrame;
    pub fn screen(&self) -> PhysicalSize;
    pub fn push_rect / push_text / push_lines / push_edit / waveform / label_at / button_at;

    // 新規 (input pull)
    pub fn take_scroll_in_rect(&mut self, rect: Rect) -> (f32, f32);
    pub fn take_drag_rect_in_rect(&mut self, wid: WidgetId, bounds: Rect) -> Option<DragRect>;
    pub fn take_file_drop_in_rect(&mut self, rect: Rect) -> Option<Vec<PathBuf>>;
    pub fn is_file_hovering_in_rect(&self, rect: Rect) -> bool;
    pub fn take_double_click_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)>;  // P1-4 後
    pub fn take_shortcut(&mut self, name: &'static str) -> bool;
    pub fn take_clipboard_paste(&mut self) -> Option<String>;
    pub fn set_clipboard_text(&mut self, text: impl Into<String>);

    // 新規 (popup)
    pub fn context_menu_for<F>(&mut self, rect: Rect, items: &[&str], on_select: F)
        where F: FnOnce(usize) -> Edit<M>;

    // 新規 (request)
    pub fn request_redraw(&self);
    pub fn request_undo(&mut self);
    pub fn request_redo(&mut self);
}
```

### Files

- [crates/ui/src/widgets/heavy.rs](../crates/ui/src/widgets/heavy.rs) — delegate methods 追加
- [crates/examples/piano_roll/src/main.rs](../crates/examples/piano_roll/src/main.rs) — heavy 内で take_drag_rect_in_rect / context_menu_for を使うように書き換え (現状の 2 段構造を解消、release 処理を heavy 内に集約)

### Tests

- `heavy_take_drag_rect_in_rect` — heavy 内で drag rect が取れる、Ui の状態と共有
- `heavy_context_menu_for` — heavy 内で右クリック → popup 表示

### Out of scope

- popup の z-order 変更 (heavy 内 popup の rendering layer) — 既存 popup_layer の deferred buffer がそのまま動く前提
- heavy 内で `Ui::set_cursor` を呼ぶ — P0-2 の cursor 公開 (別途) で対応、本項目では追加 delegate のみ

### Open question

- **未決 1**: delegate する API の範囲はユーザ提案の 5 個 (drag_rect / file_drop / clipboard_paste / context_menu_for / take_shortcut) のみか、上記の包括版 (scroll / double_click / clipboard set / request_redraw / request_undo / request_redo / file_hover / dialog 系も) か?
  - 推奨: 包括版。input pull 系で `Ui` にあり `HeavyCtx` にないものは漏れなく delegate (heavy 抽象の漏れを塞ぐ)
- **未決 2**: dialog 系 (`request_open_file_dialog` 等) も delegate すべきか?
  - 推奨: yes。heavy 内で Open ボタンを押すケースを想定

### Commit

```
feat(M9): P1-3 — HeavyCtx に input/popup pull API を包括 delegate
```

---

## P1-4: ダブルクリック判定の public API

✅ done (commit 予定)

実装結論:
- 未決 1 → **global state** 採用 (UiHost-level の `last_click: Option<(Instant, f32, f32)>` + `double_click_threshold: (Duration, f32)`)。real DAW で「同時に 2 つの widget で double-click 中」は発生しない前提。
- 未決 2 → **release ベース** 採用 (drag と区別しやすい、daw_01 piano_roll の AddNote と同パターン)。
- fader/knob の internal logic は **keep** (CLAUDE.md「3 回繰り返したら抽象化」原則で 2 件は閾値未満、widget 固有の press ベース reset 動作を独立に保つ)。

### Problem

ダブルクリック検出は fader/knob 内部の private 実装のみ。daw_01 では `AppData::last_click: Option<(Instant, x, y)>` を view 越しに自前管理。

### Design

UiHost-level に **single global last-click state** を持たせる。`take_double_click_in_rect(rect)` がフレーム内で primary press を検出 + 直前 click が threshold 内 + 同じ rect → 消費して `Some((x, y))` 返却。

これは fader/knob の既存内部 logic と同じ pattern。

### Proposed API

```rust
impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// `rect` 内のダブルクリック (既定 400ms / 5px) を 1 度だけ消費。
    pub fn take_double_click_in_rect(&mut self, rect: Rect) -> Option<(f32, f32)>;
}

impl<M: ?Sized + 'static> UiHost<M> {
    /// ダブルクリック判定の閾値を変更 (default: 400ms, 5px)。
    pub fn set_double_click_threshold(&mut self, ms: u64, px: f32);
}
```

内部: `UiHost` に `last_click: Option<(Instant, f32, f32)>` + `double_click_threshold: (Duration, f32)` フィールド追加。`InputAccumulator` 経由で primary press 時に `last_click` を更新。`take_*` はフレーム内で 1 度だけ Some を返す (consume フラグで multi-call を防止)。

### Files

- [crates/ui/src/ui.rs](../crates/ui/src/ui.rs) — UiHost フィールド + Ui method 追加
- [crates/ui/src/widgets/fader.rs](../crates/ui/src/widgets/fader.rs) / [knob.rs](../crates/ui/src/widgets/knob.rs) — 内部の自前 last-click logic を `take_double_click_in_rect` に置き換え (DRY)
- [crates/examples/daw_prototype/src/main.rs](../crates/examples/daw_prototype/src/main.rs) — Arrangement クリップ double-click → last_action に記録、デモ

### Tests

- `take_double_click_in_rect_within_threshold` — 2 回 click が 400ms / 5px 内 → Some
- `take_double_click_in_rect_outside_time` — 2 回目が 500ms 後 → None
- `take_double_click_in_rect_outside_position` — 2 回目が 10px ずれる → None
- `take_double_click_in_rect_consumes` — 同フレーム内 2 回目の take は None
- `set_double_click_threshold_works` — 閾値変更が反映

### Out of scope

- triple-click — text_input 系で別途必要だが本項目では扱わない
- right-click double-click — pointer.secondary 用 API は要望なし

### Open question

- **未決 1**: グローバル state (UiHost-level) vs widget-local state (id 引数で per-widget)?
  - 推奨: グローバル。fader/knob の既存 internal logic と同じパターン、real DAW で「同時に 2 つの widget で double-click 中」状況は実用上発生しない
- **未決 2**: pointer.primary_just_pressed と primary_just_released のどちらをトリガにするか?
  - 推奨: released (現状 fader/knob と同じ、drag との誤判定を避ける)

### Commit

```
feat(M9): P1-4 — Ui::take_double_click_in_rect API + fader/knob の重複ロジック削除
```

---

## P1-5: menu item の動的有効化と shortcut hint

### Problem

`m.item(label, on_click)` のみ。`can_undo()` が false でも item は active 表示。"Undo (Ctrl+Z)" のような hint も描けない、disabled menu item も作れない。

### Design

`MenuItemSpec` 構造体 + `item_with` メソッドを導入。短縮 alias は keep。

### Proposed API

```rust
pub struct MenuItemSpec<'a, M: ?Sized + 'static> {
    pub label: &'a str,
    pub enabled: bool,
    /// 右端に灰色で表示。`ShortcutMap::display_for(name)` の結果をそのまま渡せる。
    pub shortcut_hint: Option<&'a str>,
    pub on_click: Box<dyn FnOnce() -> Edit<M> + 'a>,
}

impl MenuBuilder<'_, '_, M> {
    pub fn item(&mut self, label: &str, on_click: impl FnOnce() -> Edit<M> + 'static);   // 既存、shortcut: keep
    pub fn item_with(&mut self, spec: MenuItemSpec<'_, M>) -> &mut Self;                    // 新規
    pub fn item_disabled_if(&mut self, label: &str, cond: bool, on_click: ...) -> &mut Self;  // 短縮形
}
```

レイアウト変更:
- popup 幅は `max(label_width) + max(hint_width) + 2*pad` で計算 (現状 label のみ)
- 描画: label を左 align、hint を右 align、hint 色 = `Color::rgb(0.5, 0.5, 0.55)` (灰色)
- disabled: 全テキストを灰色化、hover highlight を出さない、click を ignore

### Files

- [crates/ui/src/widgets/menu.rs](../crates/ui/src/widgets/menu.rs) — `MenuItemSpec` 構造体、`item_with` 実装、popup 幅計算 + 描画修正
- [crates/examples/daw_prototype/src/main.rs](../crates/examples/daw_prototype/src/main.rs) — Edit menu の Undo / Redo を `item_with` で書き換え (`enabled: ui.can_undo()`、`shortcut_hint: shortcut_map.display_for("undo").as_deref()`)

### Tests

- `item_with_disabled_does_not_fire` — `enabled: false` で click しても Edit が発行されない
- `item_with_shortcut_hint_drawn` — popup 幅が hint 込みで計算される (内部 measure 経由で確認)
- 視覚確認は `cargo run --bin daw_prototype` で「Undo (Ctrl+Z)」が右端に灰色表示されること

### Out of scope

- icon 付き menu item — M9 では feature creep、後送り
- separator (区切り線) の追加 — 別項目に分離 (本項目は disabled + hint のみ)
- nested checkbox / radio — 別項目

### Open question

- **未決 1**: `item_disabled_if` 短縮形は必要か、`item_with(MenuItemSpec { enabled: cond, ... })` で十分か?
  - 推奨: 短縮形は要らない (`item_with` で十分、API 表面を増やさない)
- **未決 2**: shortcut_hint の lifetime — `Option<&'a str>` で `'a = builder closure` に縛られるが、`String` 所有版 (`Option<String>`) のほうが ergonomic か?
  - 推奨: `&'a str`。`display_for` は `Option<String>` を返すので `.as_deref()` で 1 段噛ませばよく、所有を持たせると alloc が増える

### Commit

```
feat(M9): P1-5 — menu item に enabled / shortcut_hint 拡張 (item_with)
```

---

## P2-6: `time_ruler` / `bar_beat_grid` の beat 単位 overload

### Problem

[crates/ui/src/widgets/time_grid.rs](../crates/ui/src/widgets/time_grid.rs) は viewport が **sample 単位前提** (`mapping.samples_per_beat()` で内部換算)。

daw_01 は scroll_beat (beat) で持っており、毎呼び出し `* spb` で sample 換算する boilerplate。

### Design

**`ViewportState1D` 自身に unit を持たせる** (推奨、ユーザ提案の enum を viewport に統合)。

```rust
pub enum ViewportUnit { Samples, Beats }

pub struct ViewportState1D {
    pub view_start: f64,
    pub view_len: f64,
    pub unit: ViewportUnit,  // 新規
}

impl ViewportState1D {
    pub const fn samples(start: f64, len: f64) -> Self { ... }
    pub const fn beats(start: f64, len: f64) -> Self { ... }
    pub const fn new(start: f64, len: f64) -> Self { Self::samples(start, len) }  // 互換用
}
```

`bar_beat_grid` / `time_ruler` は `viewport.unit` で分岐し、`Beats` なら mapping 経由の sample 換算を skip。

不採用: 案 A (widget に unit hint 引数追加) — 利用者がドメイン情報を毎回渡す boilerplate。案 B (ViewportState1D を generic 化 `<U: ViewportUnit>`) — 既存全 example のコンパイル不能、API surface が広がる。

### Proposed API

既存 `bar_beat_grid` / `time_ruler` の signature は変更なし、内部分岐のみ。`ViewportState1D::new` を Samples 既定で残す (既存 example のコンパイル維持)、新 factory `samples` / `beats` を導入。

### Files

- [crates/ui/src/viewport.rs](../crates/ui/src/viewport.rs) — `ViewportUnit` enum、`unit` フィールド、`samples` / `beats` factory
- [crates/ui/src/widgets/time_grid.rs](../crates/ui/src/widgets/time_grid.rs) — `viewport.unit` 分岐、Beats なら spb 乗算を skip
- [crates/examples/*](../crates/examples/) — 既存の `ViewportState1D::new` 呼び出しは互換維持で動くが、可読性のため明示的に `samples(...)` / `beats(...)` に書き換え推奨 (1 commit 内で全更新)

### Tests

- `bar_beat_grid_with_samples_viewport` — sample 単位で beat 線が正しい位置
- `bar_beat_grid_with_beats_viewport` — beat 単位で beat 線が正しい位置 (mapping.bpm 変化を無視)
- `time_ruler_unit_switch` — 同じ rect で Samples / Beats を切替えると表示が一致 (single source of truth)

### Out of scope

- Seconds / Frames / SMPTE 単位 — mapping 経由で対応可能、本項目では扱わない (ViewportUnit には Samples / Beats のみ)
- f32 viewport (low-precision DAW) — 現状 f64 のみ、要望なし

### Open question

- **未決 1**: `ViewportState1D::new` を「`samples` の alias」のまま残すか、削除して `samples` / `beats` を必須にするか?
  - 推奨: alias 維持 (互換、後方互換性は捨てるが既存コードへの影響を最小化)
- **未決 2**: `pan_pixels` / `zoom_at` / `clamp_to` は unit 非依存で同じ実装で OK か?
  - 推奨: yes (どちらも `f64` 演算で完結、unit は widget 側でしか参照しない)

### Commit

```
feat(M9): P2-6 — ViewportState1D に unit (Samples/Beats) を持たせ time_ruler/bar_beat_grid を両受理
```

---

## P2-7: `Ui::scroll_area` の widget_state 内蔵版

### Problem

ユーザの記述「現状: `&mut ViewportState1D` を外部から借す形のみ」は、現在の `scroll_area` が pixel-based offset を内部で持つ (`ScrollState`) 形と矛盾する。daw_01 では「ViewportState1D ベースの time-domain scroll」を内部 state で扱いたい意図と推察。

要は「単純な time-domain scroll bar が欲しいだけで、AppData に viewport 専用フィールドを抱えたくない」というケース。

### Design

実装前にユーザに「どのケースの scroll を内蔵化したいか」を確認する必要あり (現状 `scroll_area` で足りているのか、別 API が必要か)。

**仮の方針** (確認後に修正可能):

`Ui::scroll_area_internal(id, rect, content_total_unit, f)` を追加。`content_total_unit` は時間軸スクロール (e.g. 総 sample 数) で、内部に `ScrollState` (px-domain) を持たせ、内側 closure に `(view_start_unit, view_len_unit)` を渡す。

```rust
impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    pub fn scroll_area_internal<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        content_total_unit: f64,
        f: F,
    )
    where F: FnOnce(&mut Ui<'a, M>, /* view_start_unit, view_len_unit */ (f64, f64));
}
```

### Files

- [crates/ui/src/widgets/scroll_area.rs](../crates/ui/src/widgets/scroll_area.rs) — `scroll_area_internal` 実装
- [crates/examples/arrangement/src/main.rs](../crates/examples/arrangement/src/main.rs) — 現状 manual viewport 管理を `scroll_area_internal` で置換するデモ (DRY 効果実証)

### Tests

- `scroll_area_internal_pans_with_drag`
- `scroll_area_internal_zooms_with_wheel` (zoom 機能を含めるかは設計選択)

### Out of scope

- 既存 `scroll_area` (px-based) の API 変更 — 互換維持

### Open question

- **未決 1**: ユーザの要望は本当に「ViewportState1D 内蔵版」か、それとも別の不満か?
  - 確認必須 (本項目着手前に AskUserQuestion)
- **未決 2**: zoom 機能を `scroll_area_internal` に含めるか? (wheel で zoom、Ctrl+wheel で fast scroll など)
  - 推奨: zoom 含める。time-domain scroll の DAW UX では zoom 必須

### Commit

```
feat(M9): P2-7 — scroll_area_internal で time-domain scroll を widget_state 内蔵化
```

---

## P2-8: focus 中の fader/knob を矢印で増減

### Problem

`ui.focusable(wid, rect)` で focus は付くが、Up/Down で値を変える処理は view 側 boilerplate。

### Design

`FaderStyle` / `KnobStyle` に `arrow_step: f32` (既定 0.01) を追加。fader_at / knob_at 内部で `is_focused(wid)` のとき `take_shortcut("focus_up") / "focus_down"` を消費し、value を ±arrow_step して **`Edit::with_inverse` を発行** (キーボード nudge は discrete action なので 1 press = 1 history step)。

`arrow_step: 0.0` で従来動作 (キーボード nudge 無効)。

shortcut name は既存 `focus_up` / `focus_down` を再利用 (fader が focus 中なら fader が consume、それ以外は focus traversal が consume)。

### Proposed API

```rust
pub struct FaderStyle {
    // 既存 ...
    pub arrow_step: f32,         // default 0.01
    pub arrow_step_fine: f32,    // Shift+Up/Down 時、default 0.001
}
// KnobStyle も同様
```

### Files

- [crates/ui/src/widgets/fader.rs](../crates/ui/src/widgets/fader.rs) — Style 拡張、focused 時の arrow shortcut consume + Edit::with_inverse 発行
- [crates/ui/src/widgets/knob.rs](../crates/ui/src/widgets/knob.rs) — 同上
- [crates/examples/mixer/src/main.rs](../crates/examples/mixer/src/main.rs) — fader を `focusable` でフォーカス可能にしてキーボード nudge デモ

### Tests

- `fader_arrow_up_increments_value` — focused fader で Up 押下 → value +0.01、Undoable Edit 発行
- `fader_arrow_down_decrements`
- `fader_arrow_step_zero_disables` — arrow_step=0 で no-op
- `fader_shift_arrow_uses_fine_step`
- `fader_arrow_does_not_fire_when_unfocused` — focus 外なら focus traversal が consume

### Out of scope

- PgUp/PgDn での粗い step — Shift+arrow で fine step を採用、PgUp/PgDn は別項目
- マウスホイールでの value 変更 — 別項目

### Open question

- **未決 1**: 1 press = 1 history step (Undoable) か、押し続け中は 1 history (drag 模倣) か?
  - 推奨: 1 press = 1 history (discrete action として最も予測可能、Logic / Bitwig 同等)
- **未決 2**: shortcut name `focus_up` / `focus_down` を再利用するか、新たに `value_up` / `value_down` を追加するか?
  - 推奨: 再利用 (focus stack で priority 制御、widget tree で fader が claim)

### Commit

```
feat(M9): P2-8 — fader/knob の focus + arrow step ナッジ (Undoable per press)
```

---

## P3-9: `ArboardClipboard` の bytes 実装

### Problem

[crates/ui/src/clipboard.rs](../crates/ui/src/clipboard.rs) の `ArboardClipboard` は text のみ。`set_bytes` / `get_bytes` は default no-op。

### Design

調査必要 (実装前):
- arboard 3.x が MIME 付き bytes を扱えるか (Windows / macOS / Linux で挙動が異なる可能性)
- 不可なら base64 encoded text fallback (`daw-ui:bytes:<mime>:<b64>` prefix) で擬似実装

### Proposed (調査次第)

Plan A: arboard が MIME bytes 直接対応 → 直接実装、 platform 差異は内部で吸収
Plan B: text 経由の base64 fallback (利用側は `set_bytes/get_bytes` のまま、library が prefix 付き text に encode/decode)

### Files (調査後確定)

- [crates/ui/src/clipboard.rs](../crates/ui/src/clipboard.rs)

### Tests

- `arboard_bytes_roundtrip` — MIDI bytes / audio bytes を set → get で同じ bytes が戻る
- `arboard_text_does_not_decode_as_bytes` — 通常 text を `get_bytes` で読んでも prefix 不一致なら None

### Out of scope

- 大容量 bytes (>10MB) のパフォーマンス最適化 — 後送り
- Linux X11/Wayland 細かい互換性 — best-effort

### Open question

- **未決 1**: arboard の native bytes API を調査して Plan A / B / hybrid を選択
- **未決 2**: prefix 形式は `daw-ui:bytes:` で固定するか、generic な `application/x-daw-ui` MIME 風にするか?

### Commit

```
feat(M9): P3-9 — ArboardClipboard の bytes 実装 (MIDI / audio buffer の clipboard 経路)
```

---

## P3-10: file dialog の非同期化

### Problem

[crates/ui/src/dialog.rs](../crates/ui/src/dialog.rs) で `request_open_file_dialog` 内で rfd 同期実行 → UI スレッドブロック。

### Design

内部スレッド + `std::sync::mpsc::channel` で結果 push。`take_dialog_result(name)` は次フレーム以降の poll で取れる形に。`request_open_file_dialog` 自体は即 return。

非同期 runtime (tokio 等) は導入しない (既存 pollster 依存だけで完結)。

### Proposed API

シグネチャ変更なし、挙動のみ変更:
- `request_open_file_dialog` は thread spawn して即 return
- `take_dialog_result` は channel.try_recv() で結果取得、未到達なら None

### Files

- [crates/ui/src/dialog.rs](../crates/ui/src/dialog.rs) — 内部 thread + channel 化
- [crates/ui/src/ui.rs](../crates/ui/src/ui.rs) — UiHost に `pending_dialogs: HashMap<&'static str, Receiver<DialogResult>>`

### Tests

- `dialog_request_returns_immediately` — request 直後の同フレームで take_dialog_result は None
- `dialog_completes_in_subsequent_frame` — 別 thread で結果 push、次 frame で take

### Out of scope

- `rfd::AsyncFileDialog` への移行 — async runtime 必須で過剰

### Open question

- **未決 1**: thread 直接 spawn か、`std::thread::Builder::name(...)` で命名するか? (tracing 観点)
  - 推奨: 命名する (`"daw-ui-dialog-{name}"`)
- **未決 2**: dialog 中に同 name の追加 request が来たら? (cancel / queue / drop)
  - 推奨: drop + warn log (DAW 標準の modal UX)

### Commit

```
feat(M9): P3-10 — file dialog を thread + channel で非同期化、UI ブロック解消
```

---

## P3-11: `eprintln!` を `tracing` crate に統一

### Problem

[crates/ui/src/clipboard.rs:65](../crates/ui/src/clipboard.rs#L65) 等で `eprintln!` 直書き。daw_01 は tracing で統合ログを取りたい。

### Design

`tracing` を **optional feature dep** (feature `tracing`) で追加。feature 有効時は `tracing::warn!` / `tracing::debug!`、無効時は `eprintln!` フォールバック。

### Proposed

```rust
// crates/ui/src/log.rs (新規)
#[cfg(feature = "tracing")]
pub(crate) fn warn(args: std::fmt::Arguments<'_>) { tracing::warn!("{args}"); }

#[cfg(not(feature = "tracing"))]
pub(crate) fn warn(args: std::fmt::Arguments<'_>) { eprintln!("daw-ui: {args}"); }

// マクロ:
macro_rules! warn { ($($arg:tt)*) => { $crate::log::warn(format_args!($($arg)*)); }; }
```

利用側は `crate::warn!("clipboard set failed: {e}")` のように呼ぶ (eprintln 直書きを置き換え)。

### Files

- [Cargo.toml](../Cargo.toml) — workspace dep に `tracing` (optional) 追加
- [crates/ui/Cargo.toml](../crates/ui/Cargo.toml) — `[features] tracing = ["dep:tracing"]`
- [crates/ui/src/log.rs](../crates/ui/src/log.rs) (新規) — log helper
- [crates/ui/src/clipboard.rs](../crates/ui/src/clipboard.rs) — eprintln → log helper
- 他 eprintln 直書き箇所 (grep で発見) を一括置換

### Tests

- feature off / on 両方で build pass を CI で確認 (本 commit では手動)
- ログ出力テストは現実的でない、置換漏れ grep で確認

### Out of scope

- structured logging (event 構造化) — 後送り
- log level filter — tracing 側設定で対応 (library は warn / debug / trace のみ使う)

### Open question

なし。

### Commit

```
feat(M9): P3-11 — log helper 経由で tracing crate 統合 (optional feature)
```

---

## 進捗管理

各項目完了時に本ファイルの該当節先頭に `✅ done (commit <hash>)` を追記する。`docs/history.md` には完了 commit の概要を「M9 P0-P3 daw_01 feedback」節として 1 ブロック追記 (item 11 個まとめて記録)。

session 内の todo (`TodoWrite`) はメタレベルで継続管理。
