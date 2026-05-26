# Text Overlay / Title Generator 計画 — 動画 / 画像の上にテキストを重ねる

ステータス: **P1-P8 + P5.B (full inspector) 着地** (2026-05-26)。 gui_01 #049
(Phase 78) の commit + runtime smoke test (preview / mp4 export での text
描画目視確認) が残作業。

| Phase | Commit | 内容 |
|---|---|---|
| P1 | `f19f849` | data model (ClipContent::Text / TextEvent / TextBuiltinParam 23) + CURRENT_VERSION 16 |
| P2 | `33536b2` | render_video.rs を OffscreenRenderer 経由に移行 |
| P3 | `0b5a2e6` | text_compose.rs + preview / render_video で text 描画 |
| P4 | `f98ade0` | arrangement view で text clip 表示 (本文 32 文字 preview label) |
| P5 | `b8044c5` | inspector MVP (Mute / Text / Font / FontSize / Opacity + automate) |
| P5.B | `183f085` | inspector full (TextNumField dispatch で 25 numeric field + Align / Fade Curve dropdown) |
| P6 | `2be7333` | preview drag handle が text 対応 (rect + rotation) |
| P7 + P8 | `1dec923` | Add Text Clip menu + TextBuiltin 23 lane add/remove |

関連:
- [plan_image_overlay.md](plan_image_overlay.md) — PiP 画像 overlay の data model / composite pipeline
- [plan_image_automation.md](plan_image_automation.md) — image 各 field の track-level automation
- [plan_video.md](plan_video.md) — video clip pipeline

## 0. 動機と現状

- ユーザーは MV 制作で「タイトル」 「字幕」 「credits」 等のテキストを動画上に重ねたい。
- 動画編集ソフト (After Effects / Premiere / DaVinci) は「Text Layer」 を独立 clip として持ち、 keyframe で位置・サイズ・色を変化させる機能を備える。 daw_01 にも同等の機能が必要。
- 現状の `ClipContent` enum は `Midi / Audio / Automation / Video / Image` の 5 variant。 テキストを表現する型は無い。
- 現状 `TrackKind { Audio, Video }` で audio / video を別 kind に分離していて、 同 track 上に混在できない (= REAPER 流儀と乖離)。 本計画では Text 導入に合わせて **TrackKind 廃止 + clip 混在化** も同時に行う (= ユーザー要望)。

## 1. 採用方針 (grilling 結果)

### 1.1 1 clip = 1 テキスト (単一行)

- 採用理由: image / video / audio と同じ clip idiom。 split / glue / drag move / trim がそのまま流用できる。 改行は禁止 (= ユーザーは複数行が欲しければ clip を縦に並べる)。
- 拒絶: 1 clip = 複数 segment (= karaoke 流)。 「テキストは 1 時点で 1 行しか出ない」 ため 1 clip に複数詰める意味が薄い。

### 1.2 font 指定 = system font name (文字列)

- 採用: `font_family: String` (例 `"Yu Gothic"`)。 glyphon に system font として渡し、 解決できなければ fallback chain で代替。
- 拒絶 A: system default 固定 (= フォントの個性が出せない)。
- 拒絶 B: project bundle TTF (= 理想だが import path / GPU glyph cache / fallback chain で実装規模が爆発、 post-MVP に回す)。

### 1.3 描画 backend = **preview / export 両方 glyphon**

- 採用理由: preview (gui_01 `push_text`) と mp4 export (`render_video::render_mp4`) で byte-exact 同一の描画。 将来の shader 拡張 (shadow blur, gradient, glow) を GPU pass で受けられる。
- 拒絶 A: preview=glyphon、 export=ab_glyph (= preview/export で 1-2 px 位置ズレ可能性、 日本語フォント metrics 差異)。
- 拒絶 C: 両方 ab_glyph (= glyphon の日本語/絵文字 fallback を手動再実装する debt)。
- **実装影響**: `render_video.rs` を `OffscreenRenderer` ベースに rewrite (= video frame / image / text を GPU pass で composite → wgpu readback で pixel buffer)。 規模は大きいが「妥協を選択肢に上げない」 (`CLAUDE.md` 冒頭) に沿う。

### 1.4 rotation 対応 (image と同 idiom)

- 採用理由: MV で「タイトルが斜めに出る」 演出を可能にする、 image rotation と同じ track-level lane (`TextBuiltin::Rotation`) で automation。
- gui_01 #049 で `GlyphArea.rotation_radians` 追加要望。

### 1.5 alignment = Left / Center / Right

- 採用: `align: TextAlign { Left, Center, Right }` enum。 (x, y, w, h) box 内で horizontal align、 vertical は単一行なので center 固定。
- 拒絶: Left 固定 (= タイトルで center align が必須、 不便)。

### 1.6 font_size 単位 = project resolution 基準 px

- 採用: `font_size_px: f32` (例 `48.0` = 1920x1080 で 48 px)。 preview window は letterbox 内 scale、 mp4 export は project resolution でそのまま 48 px。 デザインソフト (Photoshop / Premiere) 慣行に合致。
- 拒絶 B: normalized 0..=1 (= 「0.044」 をユーザーが頭で換算する手間あり、 unit が image と統一されるメリットより直感性が重要)。

### 1.7 色とエフェクト = solid fill + outline + shadow (3 つ MVP に含む)

- 採用理由: MV で必須の見栄え (= 「黒アウトライン付きの白文字 + ドロップシャドウ」 が title generator の標準)。
- gui_01 #049 で `GlyphArea` に outline / shadow field 追加要望 (= rotation と同 PR で 1 件に集約)。

### 1.8 text rect = image と同 idiom (x/y/w/h normalized)

- 採用理由: preview drag handle / inspector / ImageBuiltin lane infrastructure を全部流用、 mental model 統一 (After Effects 流)。 forward compat (= 将来 multi-line / word-wrap 対応時に w/h が意味を持つ)。
- 拒絶: text 独自 idiom (x/y のみ、 w/h 無し)。

### 1.9 track kind 廃止 + clip 混在化 (= REAPER 流)

- 採用理由: 「audio / video / image / text が同 track に混在可能」 が動画編集ソフト標準。 既存の `TrackKind { Audio, Video }` 分離は engine path simplification のためだったが、 ユーザー UX を優先。
- **実装影響**: `Track.kind: TrackKind` field 削除、 全 track が audio path + visual composite path 両方を持つ。 旧 Video track は instrument: None / fx_chain: vec![] / volume 1.0 で audio mix 上は silent、 visual composite には参加する。

### 1.10 v15 → v16 migration = Unify

- 採用: 旧 `TrackKind::Audio` / `TrackKind::Video` を全 unify track にマイグレート。 旧 Video track は audio path defaults (instrument: None / fx_chain: vec![] / volume: 1.0 / pan: 0.0 / muted: 既存値) で補完。
- 既存 .daw file は問題なく roundtrip。 ユーザーは「これまで video だった track に audio clip を入れられるようになる」 ことを期待。

### 1.11 本文編集 UX = Enter で commit

- 採用理由: 既存 audio / image inspector の数値欄と同 idiom。 typing 中は buffer に保持、 Enter (or focus 喪失) で `SetClipTextText` event 発火 + undo step 1 個。

### 1.12 automation 対象 = 23 lane (全 field)

- 採用理由: 「`枠の色 / 影の色も automation できる`」 ユーザー指示。 動画編集ソフト標準。 lane は user が「A」 toggle 押下時にだけ作成されるので、 inspector の見た目が肥大化しない (= 普段は lane 無し)。

| カテゴリ | lane | 個数 |
|---|---|---|
| 位置 / サイズ | X, Y, W, H | 4 |
| 形 | Opacity, Rotation, FontSize | 3 |
| 塗り色 | FillR, FillG, FillB, FillA | 4 |
| 枠 | OutlineR, OutlineG, OutlineB, OutlineA, OutlineWidth | 5 |
| 影 | ShadowR, ShadowG, ShadowB, ShadowA, ShadowOffsetX, ShadowOffsetY, ShadowBlur | 7 |

= 計 **23 lane**。 全 `AutomationTarget::TextBuiltin(TextBuiltinParam)` で表現。

## 2. データモデル変更 (v15 → v16)

### 2.1 `TrackKind` 廃止 + Track unification

```rust
// 削除:
// pub enum TrackKind { Audio, Video }
// pub struct Track { ..., kind: TrackKind, ... }

// 残るのは unified Track:
pub struct Track {
    pub id: u32,
    pub name: String,
    // audio path (旧 Audio track field)
    pub instrument: Option<PluginInstance>,
    pub midi_fx_chain: Vec<PluginInstance>,
    pub fx_chain: Vec<PluginInstance>,
    pub volume: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub armed: bool,
    pub source: InstrumentSource,
    // clips: audio / midi / automation / video / image / text 混在可能
    pub clips: Vec<Clip>,
    pub next_clip_id: u32,
    pub parent_group_id: Option<u32>,
    pub reported_latency_samples: u32,
    pub automation_lanes: Vec<AutomationLane>,
    pub next_lane_id: u32,
}
```

旧 v15 file の migration:
- `TrackKind::Audio` → そのまま (= 全 field 既存値で migrate)
- `TrackKind::Video` → audio path defaults を付与 (instrument: None / fx_chain: vec![] / volume: 1.0 / pan: 0.0 / muted: 既存値)

### 2.2 `ClipContent::Text(TextContent)` variant 追加

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct TextContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<TextEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TextEvent {
    /// 表示する文字列。 単一行 (= '\n' 禁止)、 UTF-8。
    pub text: String,
    /// system font 名 (例 `"Yu Gothic"`)。 glyphon が解決失敗時は fallback。
    pub font_family: String,
    /// project resolution 基準 px (= 1920x1080 で 48.0 なら 48 px)。
    pub font_size_px: f32,
    /// 塗り色 RGBA (0.0..=1.0)。
    pub fill_color: [f32; 4],
    /// アウトライン色 RGBA。 `outline_width_px == 0.0` ならアウトライン無効。
    pub outline_color: [f32; 4],
    /// アウトライン太さ (project resolution 基準 px、 0.0 で無効)。
    pub outline_width_px: f32,
    /// ドロップシャドウ色 RGBA。 `shadow_offset == (0, 0)` && `shadow_blur == 0.0`
    /// なら shadow 無効と見なす (= color が non-zero でも描画 skip)。
    pub shadow_color: [f32; 4],
    /// シャドウオフセット (project resolution 基準 px)。
    pub shadow_offset_px: (f32, f32),
    /// シャドウぼかし半径 (project resolution 基準 px、 0.0 で hard shadow)。
    pub shadow_blur_px: f32,
    /// horizontal alignment。 vertical は単一行で center 固定。
    pub align: TextAlign,
    /// clip 内 event の時間軸 (image / audio event と同 idiom)。
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    /// box (image PiP と同 idiom、 normalized 0..=1)。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 全体透明度 (0..=1)。 fade envelope と multiply。
    pub opacity: f32,
    /// box 中心を旋回中心とする 2D 回転 (radians)。
    pub rotation_radians: f32,
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}
```

### 2.3 `AutomationTarget::TextBuiltin(TextBuiltinParam)` variant 追加

```rust
pub enum AutomationTarget {
    TrackBuiltin(TrackBuiltinParam),
    PluginParam { slot: PluginSlot, param_id: u32 },
    SongTempo,
    SongTimeSigNumerator,
    ImageBuiltin(ImageBuiltinParam),
    TextBuiltin(TextBuiltinParam),  // 新規
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum TextBuiltinParam {
    X, Y, W, H,
    Opacity, Rotation, FontSize,
    FillR, FillG, FillB, FillA,
    OutlineR, OutlineG, OutlineB, OutlineA, OutlineWidth,
    ShadowR, ShadowG, ShadowB, ShadowA, ShadowOffsetX, ShadowOffsetY, ShadowBlur,
}
```

### 2.4 Migration (`CURRENT_VERSION: u32 = 16`)

- v15 file は forward-migrate:
  - 全 track から `kind` field を読み、 削除して unify
  - `kind == Video` だった track は audio defaults を付与
- bincode encoding: `AutomationTarget` enum の末尾に `TextBuiltin` を追加 = backward compat
- `ClipContent` enum の末尾に `Text` を追加 = backward compat
- `#[serde(deny_unknown_fields)]` の `ClipContent` 判別: `TextEvent.text: String` が他 variant の disjoint required field なので serde untagged で discriminate 可能

## 3. プロセス境界 & threading

text の rendering はすべて **daw_gui プロセス内で完結**。 daw_audio / daw_plugin_host は text を扱わない (= IPC 追加なし)。

```
┌──────────────────────────────────────────────────────────────────┐
│ daw_gui                                                          │
│                                                                  │
│  main thread (winit + wgpu)                                      │
│   - drive_preview_playback で active_text_sources_at を取得       │
│   - preview window: scene.push_text(GlyphArea {                  │
│       text, font_family, font_size, fill_color, outline,         │
│       shadow, rotation_radians, ...                              │
│     })                                                           │
│                                                                  │
│  render thread (export 中だけ)                                   │
│   - render_video::render_mp4 が OffscreenRenderer に同じ          │
│     GlyphArea を push、 readback で BGRA8 取得、 H.264 encode      │
└──────────────────────────────────────────────────────────────────┘
```

## 4. Phase 分け (= 実装順序)

### P1. データモデル + migration + TrackKind 廃止

- `common/src/model.rs`: `TrackKind` 削除 / `Track` から `kind` field 削除 / `ClipContent::Text` 追加 / `TextContent` / `TextEvent` / `TextAlign` 追加 / `AutomationTarget::TextBuiltin` + `TextBuiltinParam` 追加 / `CURRENT_VERSION = 16`
- v15 → v16 forward migration (custom Deserialize で旧 `kind` field を tolerate して捨てる + Video のときに audio defaults を付与)
- bincode `Encode`/`Decode` derive 全体確認
- 単体 test:
  - v15 file をシリアル → v16 で deserialize → 旧 Video track が audio defaults を持つ
  - `Song::ensure_ids` が unify track にも動く

### P2. render_video.rs を OffscreenRenderer 移行

- 現状 `blit_layer` CPU pixel blit pipeline を廃止し、 OffscreenRenderer (= wgpu offscreen target) に。
- 各 frame:
  1. video frame texture を OffscreenRenderer に upload
  2. image PiP texture を `push_textured_quad` で重ね
  3. text を `push_text` で重ね (= P3 で対応)
  4. `renderer.render()` で composite
  5. readback で BGRA8 → H.264 encoder へ
- 既存 image / video の見た目が regression しないこと (= `render_mp4_video_only_smoke` test pass)

### P3. composite + render (text 描画)

- `daw_gui/src/text_compose.rs` 新設: `active_text_sources_at(song, playhead_beat) -> Vec<ActiveTextFrame>`
- `ActiveTextFrame`: text 描画に必要な全 field (= text, font_family, font_size_px, colors, x/y/w/h, opacity, rotation, align 等) を持つ
- preview window (`view/preview_window.rs`): video / image layer の上に text を `scene.push_text(GlyphArea { ... })` で描画
- render_video (`render_video.rs`): 同じく OffscreenRenderer scene に push_text
- gui_01 #049 が landing するまでは:
  - rotation は不適用 (= axis-aligned のみ)
  - outline / shadow は不適用 (= solid fill のみ)
  - landing 後に 1 行 wire で全機能有効

### P4. arrangement view で text clip 表示

- 既存 `arrangement_view::draw` 内で `ClipContent::Text` の clip 描画を追加
- thumbnail = 「T」 アイコン + clip 名 (= text の先頭 20 文字程度を clip 上に preview)
- 既存 image clip 表示と同 idiom

### P5. inspector で text 編集 + automate toggle

- `view/track_inspector.rs` に "Text Event" section 追加
- fields:
  - Text (text_input、 Enter で commit)
  - Font (text_input、 system font 名)
  - Size (number、 px)
  - Align (dropdown: Left / Center / Right)
  - X / Y / W / H (number、 normalized)
  - Opacity / Rotation (deg) (number)
  - Fill color (R/G/B/A、 color picker)
  - Outline color (R/G/B/A) + Width
  - Shadow color (R/G/B/A) + Offset X/Y + Blur
  - Fade In / Out (beats + curve dropdown)
  - Mute toggle
- 各数値欄の隣に 「A」 toggle (= image と同 idiom、 23 lane の追加 / 削除)

### P6. preview drag handle

- 既存 image PiP の preview drag (`view/runner.rs::hit_test_handles` / `handle_preview_drag`) を text にも適用
- selected_clip が `ClipContent::Text` なら text 用 overlay (= 縁取り + 4 corner + center + rotate handle、 image と全く同 idiom)
- drag で SetClipTextX/Y/W/H/Rotation/FontSize event 発火
- 「lane があれば点を打つ」 (= image automation drag) も同 idiom

### P7. File menu「Add Text Clip」 + default 値

- File menu に "Add Text Clip" 項目 (画像 import の隣)
- 押下で `AppEvent::AddTextClip` 発火 → 新 unify track (Video kind が消えているので普通の track) + 新 text clip + 1 event with defaults を追加
- defaults:
  - text: "Title"
  - font_family: "" (= system default)
  - font_size_px: 64.0
  - fill_color: [1.0, 1.0, 1.0, 1.0] (= 白)
  - outline_color: [0.0, 0.0, 0.0, 1.0] / outline_width_px: 0.0 (= 無効)
  - shadow_color: [0.0, 0.0, 0.0, 0.5] / shadow_offset_px: (0, 0) / shadow_blur_px: 0.0 (= 無効)
  - align: Center
  - x/y/w/h: (0.0, 0.4, 1.0, 0.2) (= preview 中央付近の横帯)
  - opacity: 1.0
  - rotation_radians: 0.0
  - event_length_beats: 8.0 (= 約 4 秒 @ 120 bpm)
- 「Add Image Clip」 と並ぶ位置

### P8. automation: TextBuiltin 23 lane

- `image_compose::resolve_image_fields` と同 idiom で `text_compose::resolve_text_fields`
- track-level lane に `TextBuiltin(field)` が enabled なら lane の値で event の field を override
- inspector の「A」 toggle / `AddTextAutomationLane { field }` / `RemoveTextAutomationLane { field }` AppEvent
- 既存 `record_automation_points_for_tick` を流用 (= 再生中 + drag 中の keyframe recording)

### P9. gui_01 #049: GlyphArea に outline / shadow / rotation 追加

- 別途 conversation file で要望提出 (= 1 PR にまとめてもらう)
- daw_01 側は P3 完了時点で「field を 0 値で push、 効果なし」 状態、 #049 landing で wire 1 行追加

## 5. gui_01 への要望リスト

### 要望 #049: `GlyphArea` に outline / shadow / rotation_radians 追加

- `pub outline_color: Color` (= 0.0 alpha なら描画なし)
- `pub outline_width_px: f32` (= 0.0 で描画なし)
- `pub shadow_color: Color`
- `pub shadow_offset_px: (f32, f32)`
- `pub shadow_blur_px: f32` (= 0.0 でハードシャドウ、 >0 でガウスぼかし)
- `pub rotation_radians: f32` (= rect 中心 = `(left + width/2, top + height/2)` 旋回、 clockwise positive)
- shader 内で text の各 glyph に rotation 行列適用、 outline は 1 pass 拡大描画、 shadow は別 pass で blur
- `GlyphArea::new()` の default は outline 0 / shadow 0 / rotation 0 (= 既存 caller 互換)
- 関連仕様: `docs/plan_text_overlay.md` §4 P9

## 6. Out-of-scope (post-MVP)

- multi-line text + word-wrap (= 1 clip = 1 line MVP、 改行禁止)
- project bundle TTF/OTF (= system font 経由のみ MVP)
- vertical text (= 縦書き、 日本語 MV では稀)
- gradient fill (= solid 色のみ)
- 個別 glyph animation (= 文字ごとに位置 / 色 keyframe)
- text along path (= path に沿って文字配置)
- emoji の絵文字 layer (= glyphon 標準 fallback 任せ、 細かい色制御はしない)
- font weight / italic / underline (= system font 全部 normal、 weight 指定したいなら font_family に "Yu Gothic Bold" 等を書く)

## 7. 未確定事項

- **OffscreenRenderer の 4K 性能**: 4K text render が export 中に GPU readback bottleneck になるか? まず 1080p で MVP、 4K は実機計測。
- **glyphon 日本語 fallback**: glyphon が `font_family: "Yu Gothic"` を解決できない時の fallback chain (= 日本語 default → Latin default → ?)。 glyphon docs を確認、 必要なら gui_01 で fallback chain を expose してもらう。
- **shadow_blur のガウス実装**: gui_01 #049 で shader 内 multi-tap gaussian。 RT 性能 (= 60 fps preview を維持) は実機計測。
- **TrackKind 廃止後の mixer strip ソート順**: 既存「Audio track が先、 Video track が後」 のような暗黙ソート / display 規則は無さげ。 ユーザーは track 配置順 (= song.tracks[] order) で並べる。
- **既存 instrument: None + fx_chain: vec![] の旧 Video track が mixer に出る件**: ユーザーがこれまで「Video track は mixer に出ない」 期待をしていた場合、 違和感あり。 plan §1.10 で明示通知済。

## 8. 関連 plan / 参照

- `docs/plan_image_overlay.md` — image PiP の data model / preview composite / render path (= text の rendering pipeline を踏襲)
- `docs/plan_image_automation.md` — image 各 field の track-level lane (= TextBuiltin lane も同 idiom)
- `docs/plan_video.md` — video clip pipeline、 OffscreenRenderer 移行で同 path に乗る
- `docs/plan_automation.md` — automation lane / clip / point の data model
- `docs/gui_01_conversation.md` — #049 (GlyphArea 拡張) 提出予定
