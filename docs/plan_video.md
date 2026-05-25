# Video 編集機能 計画 (REAPER 流 multi-track + GPU composite + FFmpeg)

ステータス: **設計完了** (2026-05-25)、 着手前。 grilling session
([grill-me 履歴](#))  でユーザーと shared understanding に到達した設計事項を SSoT 化。

## 0. 動機と現状

- ユーザーは VOICEVOX 歌唱・録音した楽器・カメラ映像を組み合わせた **MV / 動画
  作品を daw_01 だけで完結** させたい (M0 = すぐ着手したい状態)。
- 想定編集規模: 1 本あたり 10-30 cut + crossfade + 数本 multi-track。
  REAPER の video 機能 (split / trim / crossfade / FFmpeg render) と同等を target。
  本格 NLE 機能 (色補正 / 複雑 transition / VFX / multi-cam sync) は post-MVP。
- 現状 `Track` は volume/pan/instrument/fx_chain を持つ audio 専用 struct
  ([common/src/model.rs:676-746](../common/src/model.rs))、 video を表現するデータ型は無い。
- 現状 daw_gui は単一 top-level winit window ([daw_gui/src/view/runner.rs](../daw_gui/src/view/runner.rs))。
  Preview を別 window で出すには gui_01 の `WindowBackend` 周辺を多 surface 対応に拡張する必要。

## 1. 採用方針 (一次情報根拠)

### 1.1 REAPER の video 機能 (target スコープ)
- 一次情報: [REAPER User Guide / Video Editing](https://www.reaper.fm/userguide.php) ch.18
- Track に video item / audio item を混在可能 (item の type は source file で決まる)
- Video item: split / trim / fade / crossfade
- 複数 video track の重なり = top track wins (default)、 video processor で override 可
- Preview window = floating + dockable
- Render = FFmpeg、 H.264/H.265/ProRes/その他

### 1.2 daw_01 で踏襲する範囲 (MVP)
- **Multi-track interleave** (audio/video 両方を同 `tracks: Vec<Track>` に並べる、
  `Track.kind: TrackKind { Audio, Video }` で discriminate)
- **別 top-level window で preview** (REAPER 方式、 `plugin_embed.rs` と同 idiom)
- **FFmpeg decode / encode** (`ffmpeg-next` crate)
- **Cut / split / trim** (既存 audio clip の `E`/`J` shortcut を踏襲)
- **Crossfade** (隣接 clip overlap で alpha ramp)
- **Multi-track composite** (wgpu render pass で blend)
- **Render to mp4** (sequential 2-pass: audio WAV → video encode → mux)

### 1.3 採用しない (post-MVP)
- Video processor / per-clip FX
- 色補正 / LUT / トランジション effect
- Text overlay / title generator
- Time-stretch video
- Hardware decode (DXVA / NVDEC) — software で MVP は十分
- Picture-in-picture / multi-cam sync
- Proxy file (低解像度 cache)
- Color management (sRGB / Rec.709)
- Variable framerate (VFR) handling

## 2. データモデル変更 (v11 → v12 migration)

### 2.1 `Track.kind` discriminator
```rust
#[derive(Default, ..)]
pub enum TrackKind {
    #[default]
    Audio,
    Video,
}

pub struct Track {
    #[serde(default)]
    pub kind: TrackKind,
    // 既存 fields
}
```
Audio track は既存 schema そのまま。 Video track は同じ struct を share するが、
`instrument` / `midi_fx_chain` / `fx_chain` / `volume` / `pan` / `armed` /
`source` を無視、 `muted` / `parent_group_id` / `clips` のみ意味を持つ。
v11 file は `kind: Audio` で forward-migrate (= `#[serde(default)]`)。

### 2.2 `ClipContent::Video` variant 追加
```rust
#[serde(untagged)]
pub enum ClipContent {
    Midi(MidiContent),
    Audio(AudioContent),
    Automation(AutomationContent),
    Video(VideoContent),  // 新規
}

#[derive(.., deny_unknown_fields)]
pub struct VideoContent {
    pub events: Vec<VideoEvent>,
}

pub struct VideoEvent {
    pub source_id: VideoSourceId,
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    pub source_start_micros: u64,  // mp4 timestamp は μs ベース
    pub source_end_micros: u64,
    pub gain_db: f32,         // post-MVP では opacity に使い回す可能性
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}
```
`#[serde(untagged)]` 互換: 既存 3 variant とは disjoint な field set (`events`
in `Audio` と `Video` で名前重複するため、 `Video` 側は別 marker field を持た
せるか、 もしくは `#[serde(tag = "kind")]` への移行を検討する。 v12 移行コス
トと天秤、 §7 参照)。

### 2.3 `Song.video_sources` プール
```rust
pub type VideoSourceId = u32;

pub struct Song {
    // 既存 fields
    #[serde(default)]
    pub video_sources: HashMap<VideoSourceId, VideoSource>,
    #[serde(default)]
    pub next_video_source_id: VideoSourceId,
    #[serde(default = "default_video_resolution")]
    pub video_resolution: (u32, u32),  // (1920, 1080) default
    #[serde(default = "default_video_framerate")]
    pub video_framerate: f32,  // 30.0 default
}

pub struct VideoSource {
    pub path: VideoSourcePath,
    pub width: u32,
    pub height: u32,
    pub framerate: f32,         // 元 video の framerate (project とは別概念)
    pub duration_micros: u64,
    pub codec: String,           // "h264" / "hevc" / "vp9" / "av1"
    pub audio_source_id: Option<AudioSourceId>,  // 抽出した audio への bind
}

pub enum VideoSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
}
```

### 2.4 Migration (`CURRENT_VERSION: u32 = 12`)
- v11 file は全 default で v12 へ forward-migrate
- `Song::ensure_video_sources()` を `ensure_audio_source_ids` と同 idiom で追加
- `Song::gc_video_sources()` で `Video event.source_id` 参照を集計し未参照を drop

## 3. プロセス境界 & threading

```
┌──────────────────────────────────────────────────────────────────┐
│ daw_gui (主役)                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ main thread (winit event loop + wgpu present)               │ │
│  │  - arrangement view (audio + video row)                     │ │
│  │  - preview window 駆動 (frame → wgpu render → present)      │ │
│  └─────────────────────────────────────────────────────────────┘ │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ video worker thread (lookahead decode)                      │ │
│  │  - audio playhead を atomic で poll                         │ │
│  │  - 各 active video clip の frame を 200-500ms 先読み         │ │
│  │  - decoded RGBA frame を ring buffer に push                │ │
│  └─────────────────────────────────────────────────────────────┘ │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ import worker thread (per import)                           │ │
│  │  - FFmpeg で video metadata + audio extract                 │ │
│  │  - .wav を `<project>/samples/<hash>.wav` に書き出す        │ │
│  └─────────────────────────────────────────────────────────────┘ │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ render thread (export 中だけ起動)                            │ │
│  │  - pass 1: 既存 audio WAV freewheel を kick                  │ │
│  │  - pass 2: video freewheel → wgpu composite → H.264 encode   │ │
│  │  - pass 3: ffmpeg-next muxer で WAV + H.264 → mp4            │ │
│  └─────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────────┐
│ daw_audio (変更なし)                                              │
│  - 抽出された WAV を既存 AudioSource pipeline で再生               │
│  - video format を知らない                                         │
└──────────────────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────────────────┐
│ daw_plugin_host (変更なし)                                        │
└──────────────────────────────────────────────────────────────────┘
```

- IPC 追加なし (= video は GUI ↔ disk のみで完結、 daw_audio は WAV だけ見る)
- 既存 `AudioPlayhead` atomic ([daw_audio/src/...] 要確認) を video worker thread から poll

## 4. Phase 分け (= 実装順序)

### P1. データモデル + migration (基盤)
- `common/src/model.rs`: `TrackKind`, `ClipContent::Video`, `VideoContent`,
  `VideoEvent`, `VideoSource`, `VideoSourceId`, `VideoSourcePath`、 `Song`
  fields 追加
- `CURRENT_VERSION = 12`、 docs comment 更新
- `Song::ensure_video_sources` / `gc_video_sources` / `alloc_video_source_id`
  + `Song::video_source_refcount` 実装
- bincode `Encode`/`Decode` derive 確認 (IPC 境界の MainToChild / Song / Track
  / Clip / ClipContent / VideoContent / VideoEvent / VideoSource)
- 単体テスト:
  - v11 file をシリアル → v12 で deserialize roundtrip
  - `ensure_video_sources` が orphan を drop
  - `Song::clip_content_refcount` が Video variant でも動く

### P2. Video import path
- daw_gui の File menu / drag&drop で .mp4/.mov/.mkv/.webm を受け取る
- `import_video.rs` 新設:
  - FFmpeg で metadata 取得 (width/height/duration/framerate/codec)
  - audio stream demux + decode → `<project>/samples/<basename>_<hash8>.wav`
  - `VideoSource` 登録 + `AudioSource` 登録 (`audio_source_id` で bind)
  - 自動で pair audio track (`TrackKind::Audio`) を video track の下に追加
  - 自動で video track (`TrackKind::Video`) を最上段に追加
- Error path: 非対応 codec / 破損 file / extract 失敗 → status_message に表示
- `Cargo.toml`: `ffmpeg-next = "8.x"` 追加 (要 FFmpeg DLL 同梱、 LGPL 配布要件確認)

### P3. Arrangement view 上の Video track 表現
- `daw_gui/src/view/arrangement_view.rs`: `track.kind` で row 描画分岐
- Video clip 描画 = 中間 frame の thumbnail (=  P2 import 時に 1 frame decode して
  PNG / RGBA8 を cache)
- ドラッグでの移動 / 右トリム handle は既存 audio clip の機構を踏襲
- gui_01 要望 (§5): video row 描画 API、 thumbnail render API

### P4. Preview window (第二 top-level winit window)
- `daw_gui/src/view/preview_window.rs` 新設
- `winit::Window` をもう 1 枚作って `WindowBackend` でラップ
- 別 wgpu surface + 別 winit window_id
- ESC / close button で hide (project に dock state を保存)
- gui_01 要望 (§5): `Runner` の多 window 対応、 multi-surface 対応

### P5. Playback sync + lookahead decode
- `daw_gui/src/video_worker.rs` (仮)
  - `Arc<AtomicU64>` で audio playhead (μs) を poll
  - 各 active video clip について `event_start_in_clip_beats` + clip 位置から
    現在 source μs を逆算 → FFmpeg で seek + decode
  - decoded RGBA frame を `VecDeque<DecodedFrame>` (per clip, ~500ms 分) に push
- main thread の frame redraw:
  - 「now に最も近い frame」 を ring から pick → wgpu texture upload →
    preview window に present
  - frame miss は直前 frame を hold (drop indicator は debug overlay のみ)
- Seek (transport.set_position) 時は ring を flush + 先頭 keyframe から re-decode

### P6. Cut / Split / Trim (既存 audio clip pattern を踏襲)
- `E` Split (既存 `plan_audio_clip` §3.10 と同) を Video clip にも適用
- `J` Glue (隣接 video clip merge)
- 右端 trim (既存 ResizeClips delta で動く)
- 左端 trim (`plan_audio_clip` §11 の gui_01 要望と並行待ち)
- Delete / Move (既存機構を再利用)

### P7. Crossfade + Multi-track composite
- 隣接 clip の overlap area で alpha ramp (linear 固定 / s-curve は post-MVP)
- 複数 video track: 上の track が下を覆う (alpha blend with top wins by default)
- wgpu render pass:
  - 各 video clip の current frame を texture として bind
  - per-clip alpha (crossfade ramp) を uniform で push
  - fragment shader で `out = a.rgb * a_alpha + b.rgb * (1.0 - a_alpha)`
  - 全 track composite 後に project resolution へ scale (= preview window size)
- gui_01 要望 (§5): texture bind + uniform 設定が可能な custom render API

### P8. Render (export to mp4)
- Render dialog (output path / codec選択 = MVP は H.264 only / bitrate or CRF)
- Pass 1: 既存の WAV freewheel export を呼ぶ (= `daw_audio` の render mode)
- Pass 2: video freewheel
  - `1.0 / project_framerate` 秒刻みで playhead を進める
  - 各刻みで active video clips を decode + composite (P7 と同じ wgpu pass)
  - rendered RGBA frame を ffmpeg-next encoder (H.264) に push
  - encoded stream を一時 .mp4 (video-only) に書く
- Pass 3: ffmpeg-next muxer で video + WAV を 1 つの mp4 にまとめる
- Progress callback で GUI に進捗 bar (= 既存 `Worker::report_progress` 経路)

## 5. gui_01 への要望リスト (= 別紙 docs/gui_01_conversation.md に entry を起こす)

### 要望: 第二 top-level window + multi-surface 対応
- 現状: `Runner` は単一 `WindowBackend` 前提
- 用途: daw_gui の preview window (= 動画 preview を独立 window で表示)
- 最終形態: `Runner` が複数 `WindowBackend` を保持し、 user_event で個別 window
  に redraw 駆動。 winit `WindowId` で識別、 各 window に独立した wgpu surface
- 関連仕様: `docs/plan_video.md` §3 / §4

### 要望: arrangement view に video track 種別
- 現状: arrangement widget は track 単一種 (audio 前提)
- 用途: `TrackKind::Video` の row を audio と区別して描画 (背景 / clip 表現 /
  操作)。 thumbnail 描画も含む
- 最終形態: 既存 row API に track_kind hint、 `push_video_thumbnail(rect,
  texture_handle)` API
- 関連仕様: `docs/plan_video.md` §3

### 要望: video frame texture upload + custom render pass
- 現状: `HeavyCtx::push_rect / push_text / push_lines` は CPU rasterize、
  GPU texture upload なし、 fragment shader を直接書く方法なし
- 用途: decoded video frame を GPU texture に upload して preview window /
  arrangement thumbnail に描画、 composite には custom fragment shader が必要
- 最終形態: `HeavyCtx::push_texture(rect, texture_handle)` + texture 登録 /
  解放 API + composite-style 用 custom render pass API
- 関連仕様: `docs/plan_video.md` §4 / §7

## 6. Out-of-scope (post-MVP に回す)

- 色補正 / LUT / トーン調整
- Text overlay / title generator
- Effects (blur / sharpen / chromakey / glitch / ...)
- Time-stretch video (`StretchMode::Repitch` 相当)
- Hardware decode (DXVA on Windows / VAAPI on Linux / NVDEC)
- Picture-in-picture / multi-cam sync (= 複数 video を同 source 扱い)
- Proxy file (低解像度 cache)
- Color management (sRGB / Rec.709 / Rec.2020)
- Audio extraction の lazy mode (現状: import 時に eager extract)
- Variable framerate (VFR) handling — MVP は CFR 仮定
- Per-track video FX chain (`fx_chain` のような plugin 列)

## 7. 未確定事項

- **ClipContent::Video の serde 互換性**: `#[serde(untagged)]` の field-set
  disjoint 性を維持するには、 `VideoContent` の `events` 名が `AudioContent.
  events` と衝突しないか確認が必要。 衝突するなら field 名を変える
  (`video_events`) か、 untagged を捨てて `#[serde(tag = "kind")]` に移行
  (= v6 以降全 ClipContent file の migration が必要、 大事になる)
- **VideoEvent の time unit**: μs (mp4 timestamp の native) vs frame index。
  μs 採択予定だが、 seek 精度との trade-off
- **Render codec**: MVP は H.264 only、 H.265 / ProRes は post-MVP
- **Frame cache size**: 200ms / 500ms / project setting? defaults 300ms 案
- **Crossfade curve**: MVP は linear 固定、 s-curve / exp は post-MVP
- **Clip の timebase**: `start_beat` + `length_beats` (= 既存 Clip 流) で揃え
  るが、 tempo 変化時の挙動は要詰め (= MV は fixed tempo 前提と仮置き)
- **Preview window の独立した play/pause**: REAPER は preview だけ scrub 可、
  daw_01 は transport 一元で MVP 進める想定 (= preview window は表示のみ)
- **FFmpeg DLL 配布**: LGPL 動的リンクで shared DLL を `target/release/` に
  同梱する必要、 Windows installer 段階で対応

## 8. 関連 plan / 参照

- `docs/plan_audio_clip.md` — Audio clip の Bitwig 階層モデル (Clip ⊃ Event)、
  これを Video に踏襲
- `docs/plan_clip_share_clone.md` — 共有コピー仕様、 video clip も同じ操作で
  揃える (post-MVP)
- `docs/plan_a3_wav_export.md` — freewheel WAV export pipeline、 video render
  pass 1 で再利用
- `docs/gui_01_conversation.md` — gui_01 要望 / 回答ファイル、 §5 の要望を
  ここに entry 化
