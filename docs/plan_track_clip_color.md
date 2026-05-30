# plan: トラック / クリップの色指定

## 目的

ユーザーがトラックとクリップに色を付けられるようにする。アレンジビューでの
視認性・整理性を上げる (REAPER / Ableton Live / Bitwig と同等の機能)。

## 確定要件 (2026-05-30 ユーザー承認)

### 色の継承モデル (継承 + 上書き)

- `Track.color: Option<[f32; 3]>`
  - `None` = トラック id から導出した安定パレット色 (`PALETTE[(id - 1) % N]`)。
    新規トラックは自動でパレット色が付く (= 自動割り当て)。reorder しても
    id ベースなので色が動かない。**導出可能な値は保存しない** (SSoT)。
  - `Some(rgb)` = ユーザー上書き。
- `Clip.color: Option<[f32; 3]>`
  - `None` = トラック色を継承 (= `effective_track_color(track)`)。
  - `Some(rgb)` = 個別上書き。
- 「トラック色に戻す」(Ableton 風) = `clip.color = None` に戻す操作。

### 色表現

- `common` 側は GUI renderer に依存させないので、`[f32; 3]` (RGB、不透明) を使う。
  既存 `TextContent.fill_color: [f32; 4]` (model.rs:1907) と同じ raw 配列慣習。
- view 層で `daw_ui_renderer::Color::rgb(r, g, b)` に変換して widget に渡す。

### パレット

Ableton Live のデフォルトパレットを参考にした 16 色 (彩度高め・暗背景で映える)。
`daw_gui` 側 (view 層) に定義する (model は色値の意味を持たない、描画の都合)。
導出 (`None` トラック) と picker スウォッチで同じ配列を共有する。

### UI

- **トリガー**: 右クリックメニュー (既存 `ui.context_menu_for` を流用)。
  - トラックヘッダ右クリック: 既存 `["Rename", "Delete"]` に `"色..."` を追加。
  - クリップ右クリック: 既存メニューに `"色..."` と `"トラック色に戻す"` を追加。
  - トラック inspector にも色スウォッチボタンを置き、同じ picker を開ける。
- **ピッカー本体**: gui_01 の新 `color_picker` widget (パレットスウォッチ +
  カスタム RGB/HSV)。daw_01 は `color_picker_target: Option<ColorPickerTarget>`
  を AppData に持ち、`"色..."` 選択でこれを Some にする。Some の間 picker を
  overlay 描画、選択で `SetTrackColor` / `SetClipColor` を発行して target を閉じる。

### 永続化 / Undo

- `Track.color` / `Clip.color` は model field なので保存は自動。`CURRENT_VERSION`
  を 17 → 18 に bump。v17 以前は `#[serde(default)]` で両 field `None`
  (= トラックは導出パレット色、クリップは継承)。既存 project は「全トラックが
  パレット色になる」見た目変化が出る (自動割り当てを選んだ仕様どおり)。
- 色変更 event は既存の Edit/Undo スナップショット経路に乗る。

## gui_01 依存 (要望を先に提出)

`docs/gui_01_conversation.md` #058, #059 として提出。

1. **#058 `color_picker` widget** — パレットスウォッチ + カスタム RGB/HSV。
   overlay popup として開閉、選択で `Color` を返す。
2. **#059 `ArrangementTrack.color: Option<Color>`** — トラックヘッダ / 行背景の
   色付け。`ArrangementClip.color` は既存対応済 (arrangement.rs:2645) なので不要。

クリップ色描画は gui_01 待ちなしで動く (既存 `ArrangementClip.color` を埋めるだけ)。
トラックヘッダ色 + picker widget は gui_01 landing 後に wire する。

## 実装ステップ

### daw_01 (gui_01 非依存で先行)

1. **model**: `Track.color` / `Clip.color` 追加 + `CURRENT_VERSION` bump +
   `#[serde(default)]`。`bincode::{Encode, Decode}` は struct 派生で自動。
2. **palette / 継承ヘルパー** (view 層): `PALETTE`, `effective_track_color(track)`,
   `effective_clip_color(track, clip)`。
3. **クリップ色描画**: `arrangement_view.rs` の `color: None` を
   `effective_clip_color(...)` (Some 上書き時のみ) に変更。継承色はトラック色を
   そのまま使うか、widget の share_group_color と競合しないよう調整。
4. **AppEvent**: `SetTrackColor { track, color: Option<[f32;3]> }`,
   `SetClipColor { clip, color: Option<[f32;3]> }`。handler で model 更新。
5. **右クリックメニュー**: `"色..."` / `"トラック色に戻す"` 項目追加 →
   `color_picker_target` をセット。
6. **inspector スウォッチ**: トラック inspector に色ボタン。

### gui_01 landing 後

7. **`color_picker` widget** を overlay として描画 + 選択 → event 発火 wire。
8. **トラックヘッダ色**: `ArrangementTrack.color` を `effective_track_color` で埋める。

## テスト

- model round-trip: `Track.color` / `Clip.color` を set した Song を
  bincode encode → decode で一致 (高レイヤー: serialize/deserialize)。
- v17 互換: color field 無しの JSON が `None` で load できる。
- 継承ロジック: `effective_clip_color` が None で track 色、Some で上書き色を返す。

## 参考

- REAPER: track color + item「inherit track color / custom color」(右クリック)。
- Ableton Live: track/clip 各自に色、作成時パレット自動割り当て、後から変更・
  「トラック色に合わせる」可。
- Bitwig: track color、clip は継承既定。
