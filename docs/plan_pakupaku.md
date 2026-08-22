<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_pakupaku — 口パク (lip-sync) 画像自動生成

VOICEVOX の phoneme タイミングから、立ち絵の口画像を歌唱に合わせて
自動配置する機能。REAPER 版スクリプト
(`%APPDATA%\REAPER\Scripts\<user>\voicevox\pakupaku.lua`、作者ローカルの
自作 Lua) の daw_01 移植。

## 1. 概要 / 目的

vocal track の notes + per-note 歌詞から VOICEVOX で phoneme 列とその尺
(frame_length) を取得し、各 phoneme を口形状画像 (a/i/u/e/o/N/閉口) に対応
させて、立ち絵 group の「口」子 track 上に時間配置した `ImageEvent` 列として
**焼き込む**。連続同形はマージ、隙間は閉口で埋める。

REAPER 版との差分: daw_01 は per-note 歌詞 (`Note.lyric`) を SSoT に持ち、
音声合成 (`sing_frame_audio_query`) が既に動いているので、**口は別途歌詞を
持たず、音声と同一のクエリ由来 phoneme から生成** する (完全同期)。

## 2. 参照

- REAPER `pakupaku.lua` — 移植元。MOUTH_MAP / 子音→次母音 / merge / gap-fill /
  REST_FRAMES lead-in の仕様。
- `common/src/voicevox.rs` — sing 合成 (`synthesize_sing_clip`, `build_sing_query`)、
  定数 `REST_FRAMES=10`, `FRAME_RATE=93.75`。phoneme は現状捨てている。
- `common/src/model.rs` — `Track` / `Clip` / `ClipContent::Image` /
  `ImageEvent` / `ImageContent` / `InstrumentSource::Vocal` / 各 alloc。
- `daw_gui/src/image_compose.rs`, `group_compose.rs`, `view/preview_window.rs` —
  立ち絵 group 合成 (preview / export 共通経路)。子 image レイヤーは
  `active_image_sources_at` が再生位置で active な `ImageEvent` を拾う。
- `daw_gui/src/import_image.rs` — 画像 import (project/images/ へコピー、
  content-address、`Song.image_sources` 登録)。
- `daw_gui/src/app.rs` — `sync_vocal_metadata` (notes/歌詞→builtin plugin flush)、
  `AppEvent` / `handle_event` / undo。

## 3. 設計判断 (grill 結果、2026-06-03)

| # | 判断 | 内容 |
|---|------|------|
| 1 | 視覚 | 口は **立ち絵 group の子 image レイヤー**。body の `group_transform` に追従。 |
| 2 | 表現 | **`ImageEvent` に焼き込み**。ただし「自動再生成される純粋な派生物」。 |
| 3 | 起動/入力 | **選択 vocal clip に明示アクション** (= binding 設定) で確立 + 初回 bake。 |
| 4 | 生成先 | **vocal track に `lipsync_target_track: Option<u32>`** で口 track を紐づけ。 |
| 5 | mapping | **口 track に `mouth_map`** (7形 → ImageSourceId)、inspector で割当。 |
| 6 | モード | **歌唱のみ** (talk は UI ごと未実装なので対象外)。 |
| 7 | 古び | **自動再生成** (notes/歌詞/mapping 変更で debounce + 背景スレッド)。 |
| 8 | 手編集 | **保持しない** (口 track の生成済み clip は派生物、再生成で置換)。 |

SSoT = vocal の notes+lyric + 口 track の `mouth_map` + vocal→口 binding。
口画像実体は `Song.image_sources` プール 1 箇所 (mapping は id 参照のみ、
通常の image import 経由で project/images/ へコピー = WAV と同様、自己完結)。

## 4. データモデル (protocol 追加 — `cargo build --workspace` 必須)

`common/src/model.rs`:

```rust
/// 口形状クラス。VOICEVOX phoneme をこの 7 種へ畳む。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash,
         Serialize, Deserialize, Encode, Decode)]
pub enum MouthShape { A, I, U, E, O, N, Closed }

/// 口形状 → ImageSourceId のマッピング。`0` = 未割当 sentinel。
/// 口 track (= 立ち絵 group の子 image track) に持たせる。
#[derive(Debug, Clone, Default, PartialEq, Eq,
         Serialize, Deserialize, Encode, Decode)]
pub struct MouthMap {
    pub a: ImageSourceId,
    pub i: ImageSourceId,
    pub u: ImageSourceId,
    pub e: ImageSourceId,
    pub o: ImageSourceId,
    pub n: ImageSourceId,
    pub closed: ImageSourceId,
}
impl MouthMap {
    pub fn get(&self, s: MouthShape) -> ImageSourceId { /* match */ }
    /// 未割当なら closed へ、closed も未割当なら 0。
    pub fn resolve(&self, s: MouthShape) -> ImageSourceId { /* fallback */ }
}
```

`Track` に追加 (両方 `#[serde(default, skip_serializing_if = "Option::is_none")]`):
- `lipsync_target_track: Option<u32>` — vocal track 用。口 track の `id`。
- `mouth_map: Option<MouthMap>` — 口 track 用。

`Clip` に追加:
- `#[serde(default)] pub auto_lipsync: bool` — 自動生成 clip 印。再生成時に
  口 track 上の `auto_lipsync == true` clip を全削除 → 再構築 (手編集保持しない)。

migration: 全て serde default で forward-migrate。`ProjectFile.version` を bump
(現行 +1) し、コメントに口パク field 追加を記録。bincode は workspace 同時
ビルドで整合 (IPC は同一バイナリ世代)。

## 5. VOICEVOX phoneme 取得

`common/src/voicevox.rs`:

```rust
#[derive(Debug, Clone)]
pub struct Phoneme { pub phoneme: String, pub frame_length: u32 }

/// `sing_frame_audio_query` のみ叩いて phoneme 列を取得 (frame_synthesis
/// 不要 = 軽い)。`build_sing_query` を共用するので音声と同一 phoneme。
pub fn query_phonemes(notes: &[Note], bpm: f32) -> Result<Vec<Phoneme>>;
```

- `build_sing_query(notes, bpm)` → `POST /sing_frame_audio_query?speaker=QUERY_SPEAKER`。
- 応答 JSON の `phonemes: [{phoneme, frame_length}, ...]` を serde_json で parse。
  (REAPER の `parse_phonemes` と同構造。実コードで応答に phonemes 配列が
  含まれることを確認済み。)
- daw_gui の背景スレッドで実行 (HTTP は `fetch_singers` と同経路、IPC 不要)。

## 6. 生成アルゴリズム (pure fn, `common/src/lipsync.rs`, unit test 付き)

```rust
pub fn build_mouth_events(
    phonemes: &[Phoneme],
    mouth_map: &MouthMap,
    bpm: f32,
    first_phoneme_local_beat: f64,  // phoneme 列 frame 0 が来る clip-local beat
    clip_len_beats: f64,
) -> Vec<ImageEvent>;
```

> **不変条件 (r.md #39)**: `first_phoneme_local_beat` は **phoneme 列の frame 0 が来る
> 位置** = 合成 wav の先頭が来る位置。歌なら
> `voicevox::sing_head_beat(sing_base_beat(notes), bpm)`、talk なら
> 「発話開始 − `talk_pre_silence_frames()` 相当 beats」を呼び出し側が渡す。
> `build_mouth_events` の内部で経路別の補正 (先頭 pau を引く等) は **しない**
> — 音声側の配置式と口側の配置式を 1 本に保つため (多重 SSoT を作らない)。
>
> **配置ルールを変えたら `common::lipsync::PLACEMENT_GEN` を +1 する。** 口パク event は
> project に永続化される派生データで、通常の再生成トリガは入力 fingerprint の差分だけ。
> 配置ルールを変えても入力は同じままなので、生成時に `Clip::lipsync_gen` へ焼き込んだ
> 世代と照合する `vocal_tracks_with_outdated_lipsync` が「開いたときに一度だけ作り直す」
> 唯一の経路になる (合成 WAV 側の `CACHE_SCHEMA_VERSION` と対)。現行世代しか無い project
> では何もしないので、r.md #9 の dirty-on-open contract とは両立する。

手順 (REAPER 忠実):
1. `cursor = first_phoneme_local_beat` (= phoneme 列 frame 0 = wav 先頭)。
2. 各 phoneme:
   - shape 判定: 母音 a/i/u/e/o → 対応形 / `N` → N / `cl`/`pau` → Closed /
     子音 → **次の母音** (pau/cl で打ち切り、無ければ Closed) の形を借用。
   - `dur_beats = frame_length / FRAME_RATE * (bpm/60)`。
   - `source_id = mouth_map.resolve(shape)`。
   - `[cursor, cursor+dur]` の event を emit。直前 event と `source_id` が同じ
     なら延長 (= 連続同形マージ)。
   - `cursor += dur`。
3. gap-fill: `[0, first_event_start]` と `[last_event_end, clip_len]` を Closed で
   埋める (phoneme 列は VOICEVOX が pau で内部の間を埋めるので、隙間は前後端のみ)。
4. `[0, clip_len_beats]` でクランプ。範囲外 event は drop、跨ぎは clip。
5. `ImageEvent` は `Default` (全画面 rect、opacity 1、fade 0) ベース、
   `source_id` / `event_start_in_clip_beats` / `event_length_beats` のみ設定。

タイミング検証: phoneme 列 frame 0 と合成 wav 先頭が **同じ beat** に置かれ、
どちらも「frame/sample を積むだけ」で進むので、口と音声は構造的に同期する
(r.md #39 でこの契約に統一。旧実装は talk だけ別式で置いていたため口が
106.67ms 遅れていた)。

## 7. GUI wiring (`daw_gui`)

### 7.1 inspector (`view/track_inspector.rs`)
- vocal track 選択時: 「口パク出力先」picker (立ち絵 group 内の子 image track
  の dropdown)。選択 = `lipsync_target_track` 設定 = 口パク arm。
- 口 track 選択時: 7 スロットの口画像 mapping (既存 image inspector idiom 流用、
  各スロットは ImageSource picker)。
- ※ picker / slot UI が既存 daw-ui widget で組めるか着手時に確認。新規 widget が
  要れば gui_01 へ要望 (interim を作らない)。

### 7.2 生成ジョブ (背景スレッド)
- `AppData` に lipsync ジョブを追加 (既存 `voicevox_job` JobDispatcher idiom)。
- dirty な vocal track について `query_phonemes` を背景実行 → 結果を
  `EventLoopProxy<AppEvent>` で `AppEvent::LipsyncPhonemesReady { vocal_track_id,
  phonemes }` として main thread へ。
- debounce: notes/歌詞/mapping 変更で dirty mark、quiet period 後に 1 回発火
  (rapid 編集を coalesce)。

### 7.3 適用 (`app.rs` handler)
- `LipsyncPhonemesReady`: vocal track の各 clip について
  `build_mouth_events` → 口 track 上の対応 `auto_lipsync` clip を置換
  (なければ新規)。clip は vocal clip の `start_beat`/`length_beats` に揃え、
  `auto_lipsync = true`、`ClipContent::Image(ImageContent{events})`、
  `alloc_content_id`。undo 1 step。
- 再生成トリガ: `sync_vocal_metadata` と同じ変更点 (notes/lyric) + mapping 変更 +
  binding 設定時に dirty mark。

### 7.4 明示コマンド
- 「口パク再生成」を shortcut / context-menu に (force refresh)。

## 8. preview / export
追加描画コード不要。口 track は子 image レイヤーなので
`active_image_sources_at` → group 合成 → preview / `render_video.rs` の既存経路で
自動的に出る。export も byte-parity。

## 9. 依存・留意
- 立ち絵 group transform は進行中 (gui_01 #063/#064 landing 待ち)。口の body 追従は
  そこに依存するが、**子 image レイヤー描画自体は独立して動く** ので core は
  ブロックされない (group 無しでも口 track 単体で描画はされる)。
- inspector の picker / 7 スロット UI に gui_01 widget 拡張が要るか着手時に判定。
- HTTP は背景スレッド限定 (real-time audio 制約とは無関係だが UI block を避ける)。

## 10. 実装ステップ順
1. **model**: `MouthShape` / `MouthMap` / `Track.lipsync_target_track` /
   `Track.mouth_map` / `Clip.auto_lipsync` + version bump。`build --workspace`。
2. **voicevox**: `Phoneme` + `query_phonemes` (serde_json parse)。
3. **lipsync**: `build_mouth_events` pure fn + unit test。`cargo test -p common`。
4. **GUI 適用**: `AppEvent::LipsyncPhonemesReady` + handler + 背景ジョブ + debounce。
5. **inspector**: binding picker + 7 スロット mapping。
6. **コマンド**: 再生成 shortcut/menu。
7. **検証**: 実機で vocal clip + 立ち絵 + mapping → 自動口パク → preview/export 確認。

## 11. 検証
- `cargo test -p common` (build_mouth_events: merge / gap-fill / 子音→次母音 /
  REST offset / クランプ)。
- `cargo build --workspace` / `cargo clippy --workspace -- -D warnings`。
- 実機: vocal track + VOICEVOX instrument + 歌詞、立ち絵 group + 口 track + mapping、
  binding 設定 → 自動生成 → 再生で口が歌唱に同期、notes 編集で自動再生成、
  WAV/MP4 export で preview と一致。
