<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Image Overlay (PiP) 計画 — 静止画像を動画 preview に重ねる

ステータス: **設計完了** (2026-05-26)、着手前。 grilling session 未実施
(= 短い「画像を動画に重ねられるようにして」 要望 + AskUserQuestion で PiP 確定)。

関連: [plan_video.md](plan_video.md) (= video clip pipeline)、
[plan_video_perf.md](plan_video_perf.md) (= zero-copy + ring buffer)、
[plan_audio_clip.md](plan_audio_clip.md) (= clip ⊃ event 階層モデル)。

## 0. 動機と現状

- ユーザーは MV 制作で動画の上に **ジャケット画像 / 歌詞画像 / ロゴ /
  ウォーターマーク** を重ねたい。 用途上、 全画面アスペクトフィットでは
  なく **PiP (= 任意の位置・サイズで部分的に重ねる)** が必須
  (AskUserQuestion 2026-05-26 で「PiP (位置・サイズ指定)」 確定)。
- 現状の `ClipContent` enum は `Midi / Audio / Automation / Video` の
  4 variant ([common/src/model.rs](../common/src/model.rs))。 画像を
  表現する型は無い。 import / composite / render すべて新規。
- 既存の動画 composite ([daw_gui/src/view/preview_window.rs:60-74](../daw_gui/src/view/preview_window.rs))
  は `CompositeLayer { texture, width, height, alpha }` を全画面
  letterbox で描画する pattern。 PiP は **`rect: Option<NormalizedRect>`**
  を `CompositeLayer` に追加すれば clean に拡張可能 (調査済み)。

## 1. 採用方針 (一次情報根拠)

### 1.1 PiP の位置・サイズ単位 = **normalized (0.0-1.0)**

- 採用理由: `Song.video_resolution` を変更 (1080p → 4K 等) しても
  画像の相対位置が崩れない (= 解像度独立)。 REAPER の image overlay も
  video processor preset の中で `gfx_w / gfx_h` 比で位置指定するのが
  慣例 (= 同じ思想)。
- 拒絶: pixel 絶対値 (= 解像度依存で MV を別解像度に書き出す時に崩れる)、
  anchor + offset (= UI 操作が直感的でない、 normalized で 4 corner
  drag が最も明快)。

### 1.2 image format = **PNG / JPEG / WebP**

- 採用理由: image crate (Rust ecosystem 標準) が built-in 対応。 公式
  docs [https://docs.rs/image](https://docs.rs/image) に PNG / JPEG /
  WebP / BMP / TIFF / TGA / GIF (static) 全対応。 MV 用途では PNG (透過
  対応のジャケット / ロゴ) + JPEG (写真素材) + WebP (近代 web ソース)
  が 95%。
- Out-of-scope: animated GIF / APNG (= 静止画像のみ MVP)、 16bit color
  depth (= 8bit BGRA8 のみ)、 ICC color profile (= sRGB 仮定)。

### 1.3 配置先 track = **既存の TrackKind::Video track**

- REAPER 流: video item / image item は track 制約せず混在可能、 source
  file で type が決まる。 daw_01 は `Track.kind: TrackKind { Audio,
  Video }` の discriminator なので、 image clip は **video track の
  clips リストに ClipContent::Image として混在**。
- 拒絶: 新規 `TrackKind::Image` 追加 (= 「video と image を別 track に
  分けないと作業できない」 という UX 制約は user が望まない、 同 track
  上で video clip と image clip の crossfade / オーバーレイを自然に
  扱える方が clean)。

### 1.4 透過 (alpha) の扱い = **PNG の alpha チャネル + per-event opacity**

- PNG decode 結果は image crate が `RgbaImage` を返す (= alpha 含む)。
  BGRA8 に reorder して GPU texture へ upload、 fragment shader が
  src-over blend (= 既存 video composite と同じ wgpu pass を再利用)。
- `ImageEvent.opacity: f32` (0.0-1.0) を multiply (= per-clip 全体の
  透明度を user が調整可能)。 fade_in/out は既存 audio/video の
  FadeCurve を流用。

### 1.5 GPU upload は **import 時に 1 回だけ** (= 静止画は frame ごと
decode 不要)

- 動画は frame ごとに WMF decode → ring buffer に push する pipeline
  だが、 画像は静止なので **import 時に BGRA8 を 1 度 GPU texture に
  upload して TextureHandle を `preview.image_textures` に永続キャッシュ**。
  preview composite は毎フレーム同 TextureHandle を `push_textured_quad`
  に渡すだけ (= worker thread 不要、 decode コスト ゼロ)。
- Render mp4 export 時も同 BGRA8 を 1 度 `image::open` で decode →
  メモリに keep → 各 frame で `blit_layer` に渡すだけ (= 既存
  `render_video.rs::blit_layer` は rect-aware なので変更不要)。

## 2. データモデル変更 (v12 → v13 migration)

### 2.1 `ImageSource` / `ImageSourceId` / `ImageSourcePath`

```rust
pub type ImageSourceId = u32;

#[derive(.., bincode::Encode, bincode::Decode)]
pub enum ImageSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
}

#[derive(.., bincode::Encode, bincode::Decode)]
pub struct ImageSource {
    pub path: ImageSourcePath,
    /// import 元ファイルの元名 (拡張子込み、 sanitize / hash 前)。 source を
    /// 直接列挙する UI (inspector / 口パク mapping) の表示用 SSoT。 on-disk
    /// path は content addressing で sanitize / hash 済 (= 日本語名が潰れる)
    /// ため別途保持。 v22 追加、 v21 以前は `#[serde(default)]` で空。
    #[serde(default)]
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// PNG / JPEG / WebP / 他、 image crate が判別したフォーマット名。
    /// メタデータ表示用、 internal 処理では使わない。
    pub format: String,
}
```

`VideoSource` と同 idiom。 `audio_source_id` のような bind は無い
(= 画像は audio を持たない)。

### 2.2 `ClipContent::Image(ImageContent)` variant 追加

```rust
#[derive(.., bincode::Encode, bincode::Decode)]
pub enum ClipContent {
    Midi(MidiContent),
    Audio(AudioContent),
    Automation(AutomationContent),
    Video(VideoContent),
    Image(ImageContent),  // 新規
}

#[derive(.., bincode::Encode, bincode::Decode)]
pub struct ImageContent {
    pub events: Vec<ImageEvent>,
}

#[derive(.., bincode::Encode, bincode::Decode)]
pub struct ImageEvent {
    pub source_id: ImageSourceId,
    /// 既存 audio/video event と同じ「clip 内の時間軸」 で表現。
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    /// PiP rect (normalized 0-1)。 `(x, y)` は左上 corner、 `(w, h)` は
    /// 幅・高さ。 例: `(0.5, 0.5, 0.3, 0.3)` = preview 中央左上 50%
    /// から 30% × 30% で描画。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 全体 opacity (0.0-1.0)。 fade_in/out と multiply。
    pub opacity: f32,
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}
```

`#[serde(untagged)]` 互換: 既存 4 variant とは disjoint な field set
(`events` 名は重複するが内側型が違う、 serde はそれで discriminate
できる)。

### 2.3 `Song.image_sources` プール

```rust
pub struct Song {
    // 既存 fields
    #[serde(default)]
    pub image_sources: HashMap<ImageSourceId, ImageSource>,
    #[serde(default)]
    pub next_image_source_id: ImageSourceId,
}
```

v12 file は全 default で v13 へ forward-migrate (= 空 map + 0)。
`Song::ensure_image_sources()` は既存 audio/video と同 idiom で
`next_image_source_id` を最大 id + 1 にリフト。
`Song::gc_image_sources()` で未参照 source を drop。

### 2.4 Migration (`CURRENT_VERSION: u32 = 13`)

- v12 file は全 default で v13 へ forward-migrate
- `Song::ensure_image_sources` / `gc_image_sources` / `alloc_image_source_id`
- `Song::clip_content_refcount` を Image variant にも対応

## 3. プロセス境界 & threading

画像 import / composite / render はすべて **daw_gui プロセス内で完結**。
daw_audio / daw_plugin_host は画像を扱わない (= IPC 追加なし)。

```
┌──────────────────────────────────────────────────────────────────┐
│ daw_gui                                                          │
│                                                                  │
│  import worker thread (per import)                               │
│   - image::open(path) → BGRA8 bytes                              │
│   - <project>/images/<basename>_<hash8>.<ext> に copy            │
│   - ImageSource 登録 + AppEvent::ImageImported                   │
│                                                                  │
│  main thread (winit + wgpu)                                      │
│   - AppEvent::ImageImported 受信時に Renderer::upload_texture_bgra │
│     で TextureHandle を取得 → preview.image_textures に保存       │
│   - drive_preview_playback で active_image_sources_at を取得     │
│   - composite_layers に rect: Some(...) で push (= PiP)          │
│                                                                  │
│  render thread (export 中だけ)                                   │
│   - CPU で image::open → BGRA8 → blit_layer(rect, opacity)       │
└──────────────────────────────────────────────────────────────────┘
```

- worker thread 不要 (= 静止画は decode コストが 1 回切り、 frame
  ごとの先読みは無意味)
- GPU texture pool は import 時 1 度作成、 process 終了まで永続
  (= 4K PNG でも 30 MB 程度、 100 個 import しても 3GB、 MVP は許容)

## 4. Phase 分け (= 実装順序)

### P1. データモデル + migration

- `common/src/model.rs`: `ImageSource`, `ImageSourceId`, `ImageSourcePath`,
  `ClipContent::Image`, `ImageContent`, `ImageEvent`, `Song.image_sources`,
  `next_image_source_id` 追加
- `CURRENT_VERSION = 13`、 docs comment 更新
- `Song::ensure_image_sources` / `gc_image_sources` /
  `alloc_image_source_id` / `Song::image_source_refcount`
- bincode `Encode`/`Decode` derive 全体確認
- 単体 test:
  - v12 file をシリアル → v13 で deserialize roundtrip
  - `ensure_image_sources` が orphan を drop
  - `Song::clip_content_refcount` が Image variant でも動く

### P2. Image import path

- `daw_gui/src/import_image.rs` 新設:
  - `image::open(path)` で PNG/JPEG/WebP decode (= image crate が
    `DynamicImage` を返す)
  - `into_rgba8()` → `RgbaImage` (= alpha-aware buffer)
  - BGRA8 reorder (= 既存 video preview path と同じ format で GPU に
    送る)
  - `<project>/images/<basename>_<hash8>.<ext>` に copy
  - `ImageSource` 登録
- File menu / drag&drop で `.png / .jpg / .jpeg / .webp / .bmp` を受け取る
- `AppEvent::ImportImage` (path) → import worker thread → 完了時
  `AppEvent::ImageImported` (source_id, bgra_bytes, width, height) で
  main thread に通知
- workspace `Cargo.toml` に `image = "0.25"` を追加

### P3. Preview composite + render

- `daw_gui/src/view/preview_window.rs`:
  - `CompositeLayer` に `rect: Option<NormalizedRect>` 追加
    (`NormalizedRect { x: f32, y: f32, w: f32, h: f32 }`)
  - `render_placeholder` の loop で `rect.is_some()` 時は normalized →
    screen px 換算で `push_textured_quad` に渡す
  - `image_textures: HashMap<ImageSourceId, (TextureHandle, u32, u32)>`
    を `PreviewWindowState` に追加 (= 静止 GPU texture cache)
  - `upload_image_bgra(source_id, bgra, w, h)` で texture upload (=
    import 完了時に 1 度呼ぶ)
- `daw_gui/src/view/runner.rs`:
  - `AppEvent::ImageImported` 受信時に `preview.upload_image_bgra(...)`
  - `drive_preview_playback` の active sources 収集を `VideoPlaybackEngine::
    active_sources_at` から「image も含む統合版」 に拡張
    → 新 helper `active_image_sources_at(song, playhead_beat) -> Vec<ActiveImageFrame>`
    を `image_compose.rs` (新設) に置く
  - composite_layers に image layer を push (= rect: Some, alpha:
    opacity × fade)
- `daw_gui/src/render_video.rs`:
  - `render_frame_composite` で video layers を blit した後、 image
    layers を上に blit
  - 既存 `blit_layer` は rect-aware (調査済み)、 そのまま image rect
    (normalized → pixel 換算) を渡せる
- 既存 `fade_in / fade_out / FadeCurve` は image にも適用 (= video と
  同 helper `event_alpha` を image_compose.rs にも再利用 or 共通化)

### P4. Arrangement UI + Inspector

- `daw_gui/src/view/arrangement_view.rs`: image clip の row 描画
  (= video clip と同じ row、 background 色を区別、 thumbnail = 縮小
  BGRA8 を中央に描画)
- `daw_gui/src/view/inspector.rs` (= 該当 widget、 既存ファイル名に
  依存): 選択中 image event の x/y/w/h (normalized) 数値入力 +
  opacity + fade_in/out 数値入力
- Split (E) / Glue (J) / drag move / 左右 trim を image clip にも適用
  (= 既存 audio/video clip の機構を再利用)

### P5. Preview drag handle + 検証

- `daw_gui/src/view/preview_window.rs`:
  - 選択中 image event の rect を preview window 上に描画 (= 縁取り +
    4 corner + center handle)
  - mouse drag で corner = resize、 center = move、 normalized 0-1
    座標系で更新 → `Edit::mutate` で AppData に流す
  - 既存 preview window は単なる出力 widget だったが、 mouse event
    を扱える hook を gui_01 に要望 (= 必要なら別 conversation 要望)
- `cargo build` / `cargo clippy --workspace -- -D warnings` /
  `cargo test --workspace` clean
- smoke test: `cargo run -p daw_gui -- --smoke-test
  daw_gui/tests/fixtures/smoke_test.mp4` exit 0
- 実機: 動画 import + image import → image clip の PiP 配置 → preview
  で重なって表示される、 export mp4 にも反映、 fade in/out 効く

## 5. gui_01 への要望リスト

### 要望 1: preview window で mouse event を扱う API

- 現状: `Runner` の preview window は `winit::WindowEvent` を受信して
  redraw + resize + close 処理だけ、 mouse event は破棄
- 用途: PiP の drag handle UI (= 4 corner + center handle で resize /
  move)
- 最終形態: preview window 側で `InputAccumulator::ingest` を呼んで
  pointer move / pointer down/up を捕捉、 daw_gui 側でカスタム
  hit-test → AppEvent 発火可能
- 関連仕様: `docs/plan_image_overlay.md` §4 P5

### 要望 2: arrangement view に image clip 種別

- 現状: arrangement widget は audio / video / midi / automation の 4 種
- 用途: `ClipContent::Image` の clip を arrangement 内で thumbnail +
  rect 表示
- 最終形態: 既存 row API に clip_kind hint、 既存の `push_video_thumbnail`
  API を image にも流用可能なら共通化、 別途必要なら `push_image_thumbnail`
- 関連仕様: `docs/plan_image_overlay.md` §4 P4

## 6. Out-of-scope (post-MVP に回す)

- Animated GIF / APNG (= 静止画像のみ MVP)
- Animation (= 時間で位置・サイズが変化する keyframe automation、 image
  の x/y/w/h を automation lane に乗せる、 と等価。 P5 完了後の拡張)
- Image effects (= blur / sharpen / color grading / chromakey)
- 回転 (= 任意角度の rotate、 normalized rect + rotation_radians 等)
- 16bit color depth / HDR / wide color gamut
- ICC profile 対応
- Per-image text overlay (= 画像内に text を書き込む、 これは title
  generator 系の別機能)
- 1 frame 単位の細かい timing control (= seconds 単位、 micros 単位の
  trim handle)
- Linux 対応 (= image crate は cross-platform なので将来追加は容易、
  ただし daw_01 全体が Windows 優先のため MVP は Windows のみ)

## 7. 未確定事項

- **GPU 上限**: 4K PNG ×100 個 import で 3GB は許容範囲か、 もしくは
  `image_textures` cache サイズ上限を設けて LRU で drop? MVP は
  「無制限 + 終了時破棄」 で進める
- **PNG alpha の premultiplied vs straight**: image crate は straight
  alpha を返す (= 公式 docs)、 wgpu の `Bgra8UnormSrgb` texture は
  straight alpha のまま fragment shader で blend、 問題なし
- **drag handle の hit-test 半径**: 8px 程度を想定、 normalized 換算で
  preview window サイズ依存、 実装時に実機で調整
- **複数 image event 同 clip 内**: ClipContent::Image.events: Vec で
  N 個 OK、 ただし PiP の rect / opacity / fade が独立。 これで
  「同 clip で 5 個の画像を順次切り替え」 が可能 (= image event の
  start/length が時間的に並ぶ)
- **JPEG 透過**: JPEG は alpha 無し、 image crate が opaque RGBA8 を
  返す (= alpha=255)、 fade / opacity だけが効く、 問題なし
- **WebP animated**: WebP container は static / animated 両対応、 image
  crate の default は static のみ、 animated は別 feature flag が必要、
  MVP は static only

## 8. 関連 plan / 参照

- `docs/plan_video.md` — Video clip pipeline、 image は video track 内
  に混在する設計
- `docs/plan_video_perf.md` — zero-copy preview pipeline、 image は
  worker decode 不要 (= 静止のため preview composite に直接 layer push)
- `docs/plan_audio_clip.md` — Clip ⊃ Event 階層、 image にも踏襲
- `docs/gui_01_conversation.md` — gui_01 への要望 / 回答ファイル、
  §5 の要望をここに entry 化
