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
  - トラックヘッダ右クリック: `"色..."`(track 色 picker)と
    `"クリップ色をトラックに揃える"`(Ableton 流に、その track の全 clip の
    上書きを外して継承に戻す = 一括 reset)を追加。
  - クリップ右クリック: `"色..."`(個別 clip 色の上書き)を追加。
    継承へ戻す操作は **track 側メニューの一括 reset** に集約(Ableton と同様、
    clip 個別の「トラック色に戻す」は置かない)。
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

## 追加要件 (2026-06-03 ユーザー要望)

トラック色ストライプの「見える場所」をグループ階層と mixer に揃える。色モデル
(`effective_track_color`) は不変、 描画位置だけの拡張。

### A. arrangement: 色ストライプを group indent に追従させる (gui_01 #069)

- 現状 (gui_01 #059): トラックヘッダの色ストライプは行の**絶対左端** (`row.x`)
  に幅 `track_color_strip_w` (4px) で描かれる。名前 / M/S/R ボタンは
  `header_x = row.x + depth * indent_px` でインデントされるが、色ストライプは
  インデントされず左端固定 (`crates/ui/src/widgets/arrangement.rs:7800-7816`)。
- **要望**: 子トラック (depth > 0) では色ストライプも `row.x + depth * indent_px`
  に揃え、名前と同じだけ右にインデントさせる。インデント分の左余白は背景
  (header_bg / group_bg / selected_bg) のまま = 「色ストライプが行コンテンツの
  左マージンとして名前と一緒にネストする」見た目。depth = 0 は現状と pixel 一致。
- gui_01 依存。daw_01 側は既に `color` / `depth` を渡しており API 追加は不要
  (widget が自前の `depth * indent_px` で strip x をずらすだけ)。

### B. mixer: strip にも track 色ストライプ (daw_01 完結・実装済み)

- mixer チャンネル strip 左端に縦 4px の色ストライプを描く (`COLOR_STRIP_W`)。
  arrangement header と同 idiom・同 helper (`effective_track_color`)。
- `TrackMixEntry.color: [f32; 3]` を `track_mix()` で計算し、`draw_strip` が
  `Some(color)` のとき bg (group=青 / return=緑 tint) の上に重ねて描く。
  master strip は track ではないので `None` (neutral 背景のまま)。
- panel が角丸 (radius 4) なので strip の左 2 隅 (tl, bl) のみ丸める
  (`radius: [4.0, 0.0, 0.0, 4.0]`)。

## 追加要件 (2026-06-08 grill-me 確定: FIXME #8 / #9)

### 真因

- **#8 (色を選んでも・トラックに揃えても変わらない)**: 2 つの問題の合わせ技。
  - (a) clip 色の塗りに source が **2 本**ある。`effective_clip_color` (個別/継承) と、
    共有 clip (refcount >= 2) の share-group hue。widget は共有 clip で **hue を優先し
    `color` を無視**する設計 (`gui_01 crates/ui/src/widgets/arrangement.rs:2982-2999`、
    #019/#022)。色を付けようとした clip が linked clip (⇌ 付き) だったため hue に上書き
    されていた。→ **gui_01 #086 で解消** (fill は `clip.color` 一本)。
  - (b) `SetClipColor` が target 1 clip しか塗らず、共有先に伝播しなかった。ユーザー期待は
    「共有クリップの色を変えれば共有先全部が変わる」。→ **handler で伝播**して解消。
- **#9 (ピッカのドラッグが下の clip に透過)**: `color_picker` widget は
  `open_popup(.., modal=true)` だが **capture_input=false** (`gui_01 crates/ui/src/ui.rs:929`
  経路)。背景の arrangement が同じ press を**先に**処理して clip drag を開始するため、
  SV / Hue ドラッグが clip 移動に化ける。後から `consume_pointer_click` しても手遅れ。
  → **gui_01 #087 で解消** (#065 真モーダル化)。

### 確定動作 (ユーザー承認)

1. **クリップで色を選ぶ → その content を共有する linked clip 全部が同じ色になる**
   (SET は共有先へ**伝播**。cross-track 共有でも全 member が同色)。
2. **「クリップ色をトラックに揃える」 → そのトラック内の clip *だけ* がトラック色に戻る。
   他トラックにある共有 clip の色は変えない**(RESET は **track-scoped**)。
3. **linked clip の「自動見分け色」 (share-group hue fill) は撤去**。clip は
   「個別色 or トラック色」 だけで塗られる。リンクの印は名前左の **⇌ glyph のみ**。
4. **color_picker を開いている間は背景 (arrangement) 入力を完全遮断**。ピッカ上の
   ドラッグは下の clip を動かさない。ピッカ外 click / Esc で閉じる。

### データモデルは per-clip `Clip.color` を維持 (content 移設は却下)

色は **`Clip.color: Option<[f32;3]>` (per-clip, v18, version 22 のまま)**。移行も version
bump も無し。

**なぜ共有名 (`clip_content_names`, content 単位) の idiom を採らないか**: 名前は「pure な
共有属性」(rename one = rename all、常に) だが、色は違う。確定動作 1 (SET = group-wide) と
2 (RESET = track-scoped) を**両立**させる必要があり、色を content 単位の 1 値にすると
**cross-track 共有 content を track-scoped に reset できない**(他トラックの clip も道連れに
なる = 確定動作 2 違反)。per-clip 所有なら SET は伝播 / RESET は track-local で両立する。
→ いったん `Song.clip_content_colors` へ移設 (version 23) したが、確定動作 2 と衝突するため
**revert** し per-clip に戻した (2026-06-08)。

- `effective_clip_color(track, clip)`: `clip.color.unwrap_or_else(|| effective_track_color(track))`
  (v18 のまま不変)。
- `Clip.color` は既存 v18 field。protocol も version も既存のまま (bincode 変更なし)。

### イベント handler 変更 (daw_01)

- `SetClipColor { target, color }` (app.rs:4917): target 1 clip だけ塗っていたのを、
  **target の `content_id` を共有する全 track の全 clip へ伝播**する
  (`for clip in all_tracks.flat_map(clips).filter(|c| c.content_id == cid) { clip.color = color }`)。
  `content_id == 0` (未採番 sentinel) のときは伝播せず target のみ (defensive)。
- `ResetTrackClipColors { track }` (app.rs:4772): **既存のまま変更不要**。当該 track の
  clip だけ `color = None` にする (= track-scoped、他 track の共有 clip は不変 = 確定動作 2)。
- **clip コピー / split は `color` を引き継ぐ** (per-clip なので「同じ clip を複製/分割したら
  色も同じ」 が自然)。新 clip を `color: None` で組んでいた 5 箇所を source 色写しに修正:
  `duplicate_clip_shared` (D) / `duplicate_clip_unique` (Alt+D) / `clone_clips_linked`
  (Ctrl+drag) / `clone_clips_independent` (Ctrl+Shift+drag) / `split_clip_at_beat` の
  back half (E)。口パク自動生成 clip (`auto_lipsync`) は派生物なので `None` のまま。
  glue (merge) は「どの色が勝つか」 が別問題かつ未報告なので対象外。
- `snapshot_for_color_edit` / picker session の undo 経路は不変。

### gui_01 依存 (#086 / #087 で landing 済、daw_01 無修正)

- **#086** (Phase 114) — `draw_clip` / `draw_video_clip` / `draw_automation_lane` で
  `share_group_color = Some(hue)` を **fill / border に使わず ⇌ glyph 専用**にし、静的
  fill+border は常に `clip.color` を唯一 source に。#068 hover 連動ハイライトは
  **identity-neutral (明るい中立色リング)** へ変更 (ユーザー色と喧嘩しないため)。
  daw_01 は `content_id_to_hue` を引き続き渡す (⇌ 判定用)。
- **#087** (Phase 114) — `color_picker` widget を **#065 真モーダル (capture_input=true)**
  で開く。開いている間 背景 pointer/keyboard を遮断、outside-click / Esc で dismiss。
  daw_01 無修正で #9 が直る。

### テスト

- `effective_clip_color` unit test (track_color.rs) は v18 のまま (個別 Some 上書き / None 継承)。
- SET 伝播 / RESET track-scoped は handler 挙動 (実機 + 目視で検証)。
- gui_01 側は #086/#087 の unit test + offscreen PNG + `color_picker_verify` で検証済。

## 参考

- REAPER: track color + item「inherit track color / custom color」(右クリック)。
- Ableton Live: track/clip 各自に色、作成時パレット自動割り当て、後から変更・
  「トラック色に合わせる」可。mixer チャンネルにもトラック色が出る。
- Bitwig: track color、clip は継承既定。
