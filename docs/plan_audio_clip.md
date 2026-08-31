# WAV / Audio Clip 機能仕様 (Bitwig 流 + 共有コピー対応)

ステータス: **Phase 1 完了** (2026-05-08)。 Phase 2 (編集機能拡張 + Audio Editor)
着手前。 段階分けは §11、 未確定事項は §13。

Phase 1 完了内容: AudioSource pool / `ClipContent` enum 化 / .daw v6→v7 migration、
drag&drop + File menu からの WAV import + dedup + project bundle 化、 Clip 移動 /
右端 trim (= length 縮め時に audio event を追従 clamp) / Delete、 共有コピー /
独立コピー / `D` / `Alt+D` / Make Unique、 Repitch (linear interp)、 Raw mode
playback + sample rate auto-resample、 arrangement 内波形描画 (gui_01
WaveformView)、 `E` Split / `J` Glue (MIDI / Audio / Vocal 共通)、 VOICEVOX
経路を `SetGeneratedAudio` に移行 (clip-keyed gen_id で multi-clip Vocal
track 独立化)。

Phase 1 残課題 (Phase 2 へ持ち越し): 左端 trim は gui_01 arrangement widget
が `ResizeClips` delta に `next_start` を持たないため未対応 (gui_01 への要望
が必要)。 Alt+drag time-stretch handle (§3.2) も同様。

## 0. 動機と現状

- アレンジメントへの WAV 貼り付けは現状 status_message にファイル名を出すだけ
  ([daw_gui/src/view/arrangement_view.rs:267-281](../daw_gui/src/view/arrangement_view.rs)) 。
- データモデル `ClipContent` ([common/src/model.rs:530](../common/src/model.rs)) は MIDI notes
  専用、 WAV / オーディオデータの参照を持たない。
- 既存の audio 経路は VOICEVOX 合成結果のみ ([daw_audio/src/vocal.rs](../daw_audio/src/vocal.rs)
  の `VocalAudio` を `vocal_store` で持ち、 ArcSwap 経由で track へ流す)。 汎用 audio clip 経路はない。

ユーザー要件:
- 一般的な DAW 相当の WAV 処理機能、 採用方針は **Bitwig Studio**
- audio clip も MIDI 同様 **共有コピー / 独立コピー可能** (REAPER pooled MIDI と等価な
  audio 版、 [docs/plan_clip_share_clone.md](plan_clip_share_clone.md) を踏襲)

## 1. 採用方針 (一次情報根拠)

### 1.1 Bitwig 階層モデル
> "The clip is the parent in this relationship, and the children (audio events, in this case)
> can exist only where the parent is there to allow it."
> — [Bitwig User Guide / Working with Audio Events](https://www.bitwig.com/userguide/latest/working_with_audio_events/)

つまり Clip ⊃ Audio Event(s)。 1 Clip 内に複数 audio event が並び、 各 event が source 内
range と再生パラメータを持つ。 Slice In Place で event 数が増える (Clip は 1 つのまま)。

### 1.2 Audio Event の core expression (Bitwig)
- **Gain** (dB)
- **Pan** (-100 % .. +100 %)
- **Pitch** (semitones, -96.0 .. +96.0)
- **Formant** (semitones)
- **Stretch** (beat markers — 「locked な点」 を打ち、 間を伸縮)

Bitwig はすべて時間変化する point 列 (= clip 内 automation 相当)。 daw_01 では M1
単一値、 M3+ で point 列対応 (§3.6 / §11)。

### 1.3 Stretch Mode (Bitwig 4 種)
> "Stretch is an optimized algorithm that time-stretches audio to match the project's
> tempo. Repitch ties pitch and playback speed together (as a tape recorder would).
> Slice divides audio into chunks and then stretches those chunks. Raw ignores all
> stretch expression data."

### 1.4 Clip menu 機能 (採用するもの)
- **Consolidate (Glue)**: 隣接 clip を merge (= `J` shortcut)
- **Reverse**: 逆再生 (audio event の `reversed` flag)
- **Bounce In Place (Pre-FX)** / **Bounce**: 処理結果を新 audio clip に書き出し
- **Normalize**: 0 dB 近傍まで gain を持ち上げる (non-destructive、 Phase 3)
- **Auto-Fade** / **Auto-Crossfade**: 全選択 / 隣接間に短 fade (Phase 2)

採用しない (ユーザー判断):
- **Slice In Place**: 不要 (= 1 clip 内 event 編集は §3.10 Audio Editor で行う)
- **Slice to Drum Machine** / **Slice to Multisample**: 不要

### 1.5 共有コピー (daw_01 既存 MIDI 仕様、踏襲)
- Ctrl+drag = 共有コピー (= linked clip、 content 編集が同期)
- Ctrl+Shift+drag = 独立コピー (= deep clone)
- 視覚区別 (アクセント色 + ⇌ link icon)
- D / Alt+D shortcut、 右クリック Make Unique

詳細は [docs/plan_clip_share_clone.md](plan_clip_share_clone.md) §1。 **audio clip でも
完全に同じ操作仕様**。

---

## 2. 用語と階層

```
Song
 ├ AudioSource (Song-global pool, AudioSourceId 参照)
 │  └ path / sample_rate / channels / frames / original_bpm / root_key
 │
 ├ ClipContent (既存 pool, ContentId 参照、 共有可能)
 │  ├ Midi(MidiContent { notes })            -- 既存 (enum 化)
 │  └ Audio(AudioContent { events })         -- 新規
 │     └ AudioEvent (1 ClipContent::Audio に複数並ぶ)
 │        └ source_id (AudioSourceId), 切り出し範囲, gain/pan/pitch/formant,
 │          stretch_mode, fade, reversed, muted, onsets, beat_markers
 │
 └ Track
    └ Clip (track 配置)
       └ start_beat / length_beats / content_id
```

- **AudioSource**: 1 WAV ファイル = 1 AudioSource。 同 path は dedup されて 1 つ
- **ClipContent::Audio**: 共有可能な「内容」。 同 ContentId を複数 Clip が参照すると linked
- **AudioEvent**: ClipContent::Audio 内の 1 単位 (Bitwig audio event 相当)
- **Clip**: track 上の時間軸ブロック。 ClipContent への参照のみ持つ

「audio clip 共有コピー」 = **ClipContent::Audio が共有される**。 AudioSource (sample buffer)
は元から共有されているので、 「共有 / 独立」 の差は events 構成 (= event ごとの切り出し
範囲・gain・pitch 等) を 1 実体にするか deep clone するか。

---

## 3. 操作仕様 (UX)

### 3.1 Import

| 入口 | 動作 |
|---|---|
| arrangement の lane area へ drag&drop | drop 座標 (x, y) → (start_beat, track) に解決 → §3.1.1 の copy & dedup → ClipContent::Audio { events: [event 1 つ] } 採番 → Clip 配置 |
| File menu → Import Audio... | File dialog → 選択 track の playhead 位置に配置 (drag&drop と同じ後段処理) |

#### 3.1.1 copy & dedup

import するファイルは **`<project_dir>/samples/<basename>_<hash8>.<ext>`** にコピーされ、
それ以降はコピー先を参照する (元 WAV が消えてもプロジェクトは壊れない、 §13 Q2)。

1. `<project_dir>/samples/` を作成 (なければ)
2. 元ファイルの SHA-256 prefix 8 文字を hash として算出
3. `samples/<basename>_<hash>.<ext>` が既に存在 (= 同 hash) → コピー省略 (dedup)
4. 同 hash の `Song.audio_sources` entry が既に存在 → 既存 `AudioSourceId` を再利用 (Clip
   だけ追加)、 重複 decode しない
5. `AudioSourcePath::ProjectRelative(PathBuf::from("samples").join(filename))` で記録

未保存プロジェクトの場合は user cache に一時コピー、 save 時に `samples/` へ移動 (§13 Q2)。

- 1 つの drop で複数ファイル: drop 座標から **下方向に各 track へ縦に配置**。 既存 track が
  足りなければ新規 track を作成
- decode は **background thread**、 進捗は status_message に "decoding foo.wav... (2/12 MB)"。
  完了で `EventLoopProxy::send_event(AppEvent::AudioImported { ... })` (§7.4)

### 3.2 配置・移動・リサイズ

| 操作 | 動作 |
|---|---|
| drag (clip 中央) | `Clip.start_beat` 変更 (snap あり) |
| drag (左端、 trim) | `start_beat += d`, `length_beats -= d`, `event.source_start_frames += d` (= source 内の見える範囲を縮める)。 source 越え不可 |
| drag (右端、 trim) | `length_beats` 変更。 source 範囲は変えない (= source より長く伸ばすと残り無音) |
| Alt+drag (右端) | **time-stretch handle**: `event_length_beats` を変えつつ source range 固定。 stretch_mode = Stretch (M2-3) / Repitch (M1) を event の現 mode で適用 |
| Delete | Clip 削除 (refcount=0 で ClipContent / AudioSource も GC) |

### 3.3 Split / Glue (MIDI / Audio / Vocal 共通)

これらは MIDI clip / Audio clip / Vocal (= VOICEVOX) clip の **すべての kind に共通** の
操作。 既存 MIDI clip にも今回 (Phase 1) で同時導入する。

| 操作 | shortcut | 動作 |
|---|---|---|
| **Split** | `E` | cursor (= playhead) 位置で選択 Clip を 2 つに分割。 後半は新 ContentId を採番 (= 元の共有グループから離れる、 Make Unique 相当の挙動を伴う) |
| **Glue / Consolidate** | `J` | 選択中の隣接 Clip を 1 つに merge。 結果 Clip は新 ContentId、 元 Clip と同 track 上で時間順に位置 |

右クリックメニューには出さない (shortcut のみ)。

#### 3.3.1 Split の clip kind 別挙動

| kind | 後半 ClipContent の構築 |
|---|---|
| MIDI / Vocal (`Midi(MidiContent { notes })`) | notes をフィルタ: split_beat より後の note は `start_beat -= split_offset` で再配置。 split_beat をまたぐ note は 2 つに分割 (前半 note は前 Clip、 後半 note は後 Clip、 各々 lyric は前 Clip にのみ残す) |
| Audio (`Audio(AudioContent { events })`) | events をフィルタ: split_beat より後の event は `event_start_in_clip_beats -= split_offset`。 split_beat をまたぐ event は 2 つに分割 (前 event は前 Clip、 後 event は `source_start_frames` を split 位置に対応する frame まで進める) |

#### 3.3.2 Glue の clip kind 別挙動

選択中の Clip 群が **同 kind** であることが必須 (異 kind 混在は Glue reject + status_message)。

| kind | 結合 ClipContent の構築 |
|---|---|
| MIDI / Vocal | 全 Clip の notes を時間軸で順次連結、 `start_beat` を各 Clip の元 `start_beat` 起点で再計算。 lyric は note ごとに保持 |
| Audio | **焼き込み** — 選択範囲を 1 本の WAV へ offline render して 1 clip / 1 event に置換。正本は [plan_glue_bake.md](plan_glue_bake.md) |

連続していない (= 隙間がある) Clip 群を Glue した場合、 隙間は **無音 / 空 ClipContent
範囲** として扱う (= MIDI なら notes 無し、 audio なら event 無し)。 結果 Clip の長さは
最初の Clip 始点から最後の Clip 終点まで。

### 3.4 共有コピー / 独立コピー (audio 版)

[plan_clip_share_clone.md](plan_clip_share_clone.md) §1 と完全に同一の操作:

| 操作 | 動作 |
|---|---|
| drag (release) | 移動 |
| **Ctrl+drag** (release, ≥4 px) | **共有コピー** — 元 Clip を残し、 drop 位置に同 ContentId のコピーを配置 (linked) |
| **Ctrl+Shift+drag** (release, ≥4 px) | **独立コピー** — ClipContent::Audio を deep clone + 新 ContentId 採番 |
| Alt+drag | snap 一時無効 (現状維持) |
| **D** | 選択 Clip の末尾直後に共有コピー連打 |
| **Alt+D** | 末尾直後に独立コピー連打 |
| 右クリック → **Make Unique** | refcount≥2 の linked clip を独立化 |

**重要**:
- AudioSource (sample buffer) は共有 / 独立いずれでも同じものを参照する (= sample buffer の
  deep clone は不要)。 「独立」 になるのは ClipContent::Audio (events 構成)
- 視覚区別 (アクセント色 / ⇌ link icon) は MIDI と同一 ([plan_clip_share_clone.md §1.3](plan_clip_share_clone.md))。
  audio / midi で badge 色を変えない (linked であることが視覚化されればいい)

### 3.5 Fade / Crossfade

| 操作 | 動作 |
|---|---|
| clip 上端 角 drag (左) | Fade In length 変更 (beats 単位) |
| clip 上端 角 drag (右) | Fade Out length |
| 角 上下方向 drag | curve 切替 (Linear ⇔ Exponential ⇔ SCurve、 段階トグル) |
| 右クリック → **Auto-Fade** | 全選択 clip に短 (≒4 ms 相当) fade を適用 |
| 右クリック → **Auto-Crossfade** | 隣接 clip 間で重なり区間に crossfade を作成 |

### 3.6 Gain / Pan / Pitch / Formant

| 操作 | 場所 | M1 仕様 | 将来仕様 |
|---|---|---|---|
| Gain | Inspector + clip 中央の dB handle (上下 drag、 ±24 dB 範囲) | 単一値 (event ごと) | point 列 (時間変化、 M3+) |
| Pan | Inspector | 単一値 | point 列 |
| Pitch | Inspector (semitones, -96..+96) | 単一値 (全 mode で有効) | point 列 |
| Formant | Inspector (semitones, -48..+48) | 単一値 (全 mode で有効、 r.md #40) | point 列 |

### 3.7 Stretch Mode (event ごと)

| Mode | 説明 | M1 実装 | 後続実装 |
|---|---|---|---|
| **Raw** | tempo / project BPM を無視、 source 元速度で再生 | ○ default | — |
| **Repitch** | playback speed = pitch ratio (tape 風)。 Pitch field と project tempo 比に従って source frame stride を変える | ○ (linear interp) | sinc / Lanczos M3+ |
| **Stretch** | スペクトル (位相ボコーダ)。 tempo に合わせ伸縮、 pitch / formant を独立に制御 | ○ (Signalsmith Stretch を vendoring、 r.md #40) | 時間変化 point 列 |
| **Slice** | transient で割って各 slice を native rate 再生 | ○ (onset 自動検出 = r.md #8 B1) | beat markers 手編集 |

Formant は全 mode で有効 (Stretch は「0 = 原音の声質を保持」、 テープ系は「0 = 素通し」)。
詳細は `docs/plan_clip_stretch.md` §7。 grain size 等の Bitwig 詳細パラメータは未対応。

### 3.8 Reverse / Normalize / Bounce

| 操作 | 動作 |
|---|---|
| **Reverse** (右クリック) | `event.reversed` 反転 (再生時に source を逆方向走査、 destructive ではない) |
| **Normalize** (右クリック) | event の peak を解析し、 0 dB に達する係数を `event.gain_db` に設定 (non-destructive、 Bitwig 流) |
| **Bounce In Place** (右クリック) | clip 内 events を offline render → 1 つの新 AudioSource (.wav) に書き込み → ClipContent を 1 event 構成に置換。 Bitwig "Pre-FX" は plugin chain 通さず |
| **Bounce** (右クリック) | bounce 結果を **新 Clip** / **新 track** に書き出し (元 clip は残る) |

### 3.9 Inspector

audio clip / event 選択時に右 panel (or 下部) に表示。 Bitwig "Inspector Panel" 相当。

#### Clip 選択時
- Name (text)
- Position / Length (beats、 readonly + 数値編集)
- Color (palette)
- ContentId (read-only, debug)
- Refcount (linked clip 数、 read-only)

#### AudioEvent 選択時
- **Timing**: Start (clip 内 beats) / Length / Mute (toggle)
- **Source**: 該当 AudioSource の path / sample_rate / channels / source_start_frames / source_end_frames
- **Stretch**: Mode (Raw / Repitch / Stretch / Slice) / Original BPM / Formant Auto-shift (M3+)
- **Fades**: Fade In length + curve / Fade Out length + curve
- **Expressions**: Gain (dB) / Pan / Pitch (semitones) / Formant (semitones、 M3+)
- **Reverse** (toggle)

### 3.10 Audio Editor (clip ダブルクリックで開く)

Bitwig の Detail Editor (Audio Editor) と同じ思想。 既存の MIDI clip ダブルクリックで
piano_roll が開くのと並列で、 audio clip ダブルクリックで Audio Editor を開く。

| 起点 | 開くもの |
|---|---|
| MIDI clip ダブルクリック | piano_roll (既存) |
| Audio clip ダブルクリック | **Audio Editor** (新規) |
| Vocal (VOICEVOX) clip ダブルクリック | piano_roll (lyric 編集付き、 既存) |

#### 3.10.1 Audio Editor の表示

- arrangement の下半分 (or 別 panel) に切替表示、 piano_roll の領域を流用
- 横軸: clip 内 beat 位置 (0 から `Clip.length_beats` まで)
- 縦軸: source の channel 別波形 (mono は 1 段、 stereo は 2 段)
- **複数 audio event を 1 clip 内で並べて表示・編集できる**

#### 3.10.2 audio event の操作 (Audio Editor 内)

| 操作 | 動作 |
|---|---|
| event 中央 drag | event の clip 内位置 (`event_start_in_clip_beats`) を変更 |
| event 左端 drag | event 左 trim (`source_start_frames` 連動) |
| event 右端 drag | event 右 trim (`source_end_frames` / `event_length_beats`) |
| 空白領域へ file system からファイル drag&drop | 新規 event 追加 (drop 位置 = `event_start_in_clip_beats`) |
| event 右クリック → Duplicate / `Ctrl+D` | 同 source の event を直後に複製 |
| event を選択 → `Delete` | event 削除 |
| event 角 drag | Fade In/Out length / curve (arrangement の clip と同じ操作) |
| event 中央 dB handle drag | Gain |
| event ダブルクリック | Inspector の event フィールドにフォーカス |

複数 event の重なりは許容 (= 同時再生で mix される)。 重なり区間で fade を切れば
crossfade になる。

#### 3.10.3 audio event の追加経路

1. **空白領域への drag&drop** (file system from outside / Audio Editor 内で別位置へ複製)
2. **AudioSource pool から既存 source を流用** (右クリック → Add Event from Source... → pool list 表示)
3. **既存 event の Duplicate** (`Ctrl+D`)

新規 event の default:
- `source_start_frames = 0`、 `source_end_frames = source.frames`
- `gain_db = 0`, `pan = 0`, `pitch_semitones = 0`, `stretch_mode = Raw`
- `fade_*_beats = 0`, `reversed = false`, `muted = false`

#### 3.10.4 共有 clip の Audio Editor 編集

同 ContentId を持つ linked clip は、 Audio Editor で event 編集すると **全 linked clip
に即時反映** (§13 Q7 の通り)。 arrangement 側の波形描画も次フレームで refresh。

#### 3.10.5 close / 切替

- `Esc` で close → arrangement のみ表示
- 別 clip ダブルクリックで切替 (MIDI clip → piano_roll、 audio clip → Audio Editor 自動)

---

## 4. データモデル

### 4.1 AudioSource (Song-global pool)

```rust
// common/src/model.rs

pub type AudioSourceId = u32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioSource {
    pub path: AudioSourcePath,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    /// import 時に WAV header / sidecar から推定。 Stretch mode で project BPM 換算に使う
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bpm: Option<f32>,
    /// MIDI key (Sampler 用、 M3+ で活用)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_key: Option<u8>,
}

/// 解決規則は §5.2。 通常の import 経路は `ProjectRelative` のみを生成する
/// (= import 時に `<project_dir>/samples/` へコピー、 §13 Q2)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum AudioSourcePath {
    /// project file dir からの相対 (= 通常の import 結果、 Bitwig "project bundle" 相当)。
    /// `<project_dir>/samples/<basename>_<hash8>.<ext>` を典型形とする
    ProjectRelative(PathBuf),
    /// 絶対 path。 未保存 project の import_cache fallback / 将来の "link to external sample"
    /// 用に型を残す (通常 import では生成しない)
    Absolute(PathBuf),
    /// VOICEVOX 等、 in-memory generated audio。 file は無く、 IPC で直接配信
    Generated { id: u64 },
}

pub struct Song {
    // ... 既存
    /// AudioSource pool. AudioSourceId → AudioSource. refcount=0 entry は
    /// `gc_audio_sources` で save 前に GC.
    #[serde(default)]
    pub audio_sources: HashMap<AudioSourceId, AudioSource>,
    #[serde(default)]
    pub next_audio_source_id: AudioSourceId,
}
```

設計判断:
- **sample buffer は Song に乗せない**: 大きすぎる (10 MB の WAV を JSON serialize すると
  base64 で 13 MB)。 path 解決 + 各プロセス独自 decode
- **peaks (波形描画用) も Song に乗せない**: §5.3 の peaks サイドカー or 起動時再計算
- **AudioSource refcount**: ClipContent::Audio 内の AudioEvent.source_id を全走査して計算
  (`Song::audio_source_refcount(id) -> usize`)。 GC は save 前

### 4.2 ClipContent enum 化

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ClipContent {
    Midi(MidiContent),
    Audio(AudioContent),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MidiContent {
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioContent {
    pub events: Vec<AudioEvent>,
}

impl Default for ClipContent {
    fn default() -> Self {
        ClipContent::Midi(MidiContent::default())
    }
}
```

既存 `ClipContent { notes: Vec<Note> }` から `ClipContent::Midi(MidiContent { notes })` への
migration は §5.1。

### 4.3 AudioEvent

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioEvent {
    pub source_id: AudioSourceId,
    /// この event が clip 内のどこから始まり、 どれだけ長いか (clip 内ローカル beats)
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    /// source 内の切り出し範囲 (sample frames)
    pub source_start_frames: u64,
    pub source_end_frames: u64,

    pub gain_db: f32,
    pub pan: f32,
    pub pitch_semitones: f32,
    pub formant_semitones: f32,

    pub stretch_mode: StretchMode,

    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,

    pub reversed: bool,
    pub muted: bool,

    /// auto-detected transient frames (sample 単位)。 M3+、 M1 は空
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub onsets: Vec<u64>,
    /// user-placed beat markers for Stretch mode。 M3+、 M1 は空
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beat_markers: Vec<BeatMarker>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum StretchMode {
    Raw,
    Repitch,
    Stretch,
    Slice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum FadeCurve {
    Linear,
    Exponential,
    SCurve,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct BeatMarker {
    pub source_frame: u64,    // source 内の位置
    pub locked_beat: f64,     // event 内 beat 位置 (= 再生時 ここに来る)
}
```

### 4.4 Clip との関係 (変更なし)

```rust
pub struct Clip {
    pub id: u32,
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,
    pub content_id: ContentId,  // → Song.clip_contents[content_id] (= Midi or Audio)
    // ... 既存 fields
}
```

`ClipContent::Midi` を持つ Clip = MIDI clip、 `ClipContent::Audio` = audio clip。 1 Track 内に
両 kind が混在することは技術的には可能だが、 UX 上は track kind で出し分ける (§13 Q1)。

---

## 5. 永続化

### 5.1 .daw v6 → v7 migration

`CURRENT_VERSION` を 6 → 7 にバンプ。 v6 file は load 時に migrate:

| v6 → v7 変換 | 詳細 |
|---|---|
| `ClipContent { notes }` → `ClipContent::Midi(MidiContent { notes })` | enum tag を補う。 `serde` の `untagged` 表現 or 二段 deserialize で吸収 |
| `Song.audio_sources` 初期化 | v6 file には無いので空 HashMap で default |
| `Song.next_audio_source_id` 初期化 | 1 で default |

bincode の Encode/Decode は enum / struct migration を直接サポートしないので、
**JSON (project file) の load パスのみで legacy struct → enum 変換**を行う。 IPC は v7 形式
(enum) のみ。

### 5.2 AudioSourcePath 解決規則

1. **save 時**:
   - project file 内の path に書ける場合 → `ProjectRelative(rel)` で書く (project file dir
     からの相対)
   - project 外の path → `Absolute(abs)`
   - generated → `Generated { id }`
2. **load 時**:
   1. `ProjectRelative(rel)` → `<project_dir>/rel` で実 path を組み立て、 `metadata` で存在確認
   2. 見つからない場合 → ユーザーに「missing source」 status_message + 当該 Clip は赤 outline
      で警告表示、 再生は無音
   3. **ファイル探索ダイアログ** (M3+): missing source を一括で探す UI
3. **"Pack into bundle" / "Collect external samples"** (Bitwig 相当、 M3+): project 移植時に
   sample を `<project_dir>/samples/` にコピーして全 path を `ProjectRelative` 化

### 5.3 peaks サイドカー (.peaks)

- 各 AudioSource に対し `<wav_path>.peaks` を同 dir に作成 (read-only project の場合は
  user cache dir `%LOCALAPPDATA%/daw_01/peaks_cache/<sha256>.peaks`)
- Format: 自前バイナリ (multi-resolution mip pyramid)
  - 1/64, 1/256, 1/1024, 1/4096 frames per peak
  - 各レベルに min / max / rms_sum_sq の f32 を保持
- 起動時に `<wav_path>.peaks` の存在 + sample_rate / frames / mtime 一致を確認、 invalid なら
  background thread で再計算 → 完了後 EventLoopProxy で view 更新
- 詳細仕様: M2-3 で別 plan (`docs/plan_audio_peaks.md`)

---

## 6. 再生 (audio engine)

### 6.1 AudioClipRenderer per song

既存 `vocal_store: HashMap<TrackId, Arc<ArcSwapOption<VocalAudio>>>` ([daw_audio/src/engine.rs](../daw_audio/src/engine.rs)
EngineShared) を一般化:

```rust
// daw_audio/src/audio_clip_renderer.rs (新規)

pub struct AudioClipRenderer {
    /// 各 track の audio events を、 frame 開始時刻 (= absolute samples) でソートしたリスト。
    /// LoadSong 時に compile_audio_schedule で構築。
    pub schedule: Vec<RenderedEvent>,
    /// AudioSource の sample buffer 共有領域。 path-based / generated 両方。
    pub sources: HashMap<AudioSourceId, Arc<AudioSourceBuffer>>,
}

pub struct RenderedEvent {
    pub track_idx: usize,
    pub clip_idx: usize,         // debug / inspector 用
    pub start_frame: u64,        // song 上の絶対 sample 位置
    pub end_frame: u64,          // exclusive
    pub source_id: AudioSourceId,
    pub source_start_frames: u64,
    pub source_end_frames: u64,
    pub gain_lin: f32,
    pub pan: f32,
    pub pitch_ratio: f64,        // Repitch mode で source frame stride
    pub fade_in_frames: u64,
    pub fade_out_frames: u64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
    pub reversed: bool,
    pub stretch_mode: StretchMode,
}

pub struct AudioSourceBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    /// Planar storage: samples[ch][frame_idx]
    pub samples: Vec<Vec<f32>>,
}
```

- `AudioSourceBuffer` は `Arc` で共有、 audio thread / export thread / decode thread から同
  buffer を読む (= write は decode thread のみで `Arc::new` の swap で更新)
- `EngineShared.audio_clip_renderer: ArcSwap<AudioClipRenderer>` で wait-free 共有
- 再生時: `process_buffer` 内で `current_frame .. current_frame+block_size` に重なる
  RenderedEvent を schedule から `binary_search` で抜き出し、 source から resample して
  track scratch に加算

### 6.2 stretch mode 実装 (M1 / M2 / M3 マッピング)

| mode | M1 | M2-3 |
|---|---|---|
| Raw | source frames を frame stride 1 で読み出し (source SR と engine SR 不一致は §6.3) | — |
| Repitch | `pitch_ratio = 2^(pitch_semitones/12) * (source_sr / engine_sr) * (project_bpm / event_original_bpm)` で source frame stride を変える (linear interp) | sinc / Lanczos resampler |
| Stretch | Raw 同等 + warn ログ | granular / phase vocoder |
| Slice | Raw 同等 + warn ログ | chunk 単位 stretch + crossfade |

### 6.3 sample rate 不一致

- AudioSource.sample_rate と engine sample_rate (cpal) が違う場合、 Raw mode でも
  `pitch_ratio = source_sr / engine_sr` で linear-interp resample (= 速度を engine SR に合わせる)
- engine 起動時 / LoadSong 時に schedule compile で全 RenderedEvent に対して係数を pre-compute

### 6.4 fade / gain / pan 適用

- 各 frame で fade envelope を計算
  - Linear: `t / fade_len`
  - Exponential: `(t / fade_len)^2`
  - SCurve: `0.5 * (1 - cos(pi * t / fade_len))`
- `gain_lin = 10^(gain_db / 20)`
- pan (equal-power、 stereo 出力):
  - mono source → `L = sample * cos(pan_rad), R = sample * sin(pan_rad)`、 `pan_rad = (pan + 1) * pi / 4`
  - stereo source → balance pan (L 削減 / R 削減のみ)

### 6.5 multi-channel 対応

- M1: mono / stereo のみ
- M2: 4ch / 5.1ch source は各 ch を最初の 2 ch に downmix + warn
- engine track 出力は常に stereo (既存仕様維持)

### 6.6 RT 制約 (CLAUDE.md 違反回避)

- audio thread で禁止: `Vec::new()`, lock, file I/O, `format!()`
- `process_buffer` 内では `AudioClipRenderer` を `ArcSwap::load()` で snapshot 取得 (atomic ptr swap のみ)
- AudioSourceBuffer は `Arc` 経由の read-only 共有
- decode は **必ず別スレッド** (decode thread)、 完了後 `Arc::new(AudioClipRenderer::new(...))` を
  `engine_shared.audio_clip_renderer.store(...)` で swap-in

---

## 7. import 処理

### 7.1 デコーダー

- M1: `hound` のみ (= WAV PCM 16/24 + float 32/64)。 既存 workspace の `hound` 依存を流用
- M2: `symphonia` 導入で FLAC / AIFF / MP3 / Ogg Vorbis / Opus 対応

### 7.2 サイズ上限と streaming

- M1: 全 sample 一括メモリ展開、 上限 **4 GB / file**。 超過時は import 失敗 +
  status_message でエラー
- 4 GB 超 (long-form recording / field recording 等) は memory-mapped read or chunked
  streaming で対応 (Phase 5+)

### 7.3 sample rate 不一致

- import 時には変換しない (source の sample_rate を保持)
- 再生時に on-the-fly resample (§6.3)

### 7.4 import 進捗とイベント

```
GUI thread                       Decoder thread
─────────                        ──────────────
file drop                  →     spawn(move || {
status_message "decoding..."         hound::WavReader::open(path)
                                     全 sample を Vec<Vec<f32>> に load
                                     → AudioSourceBuffer 構築
                                     → daw_gui の AudioSourceCache に insert
                                     → daw_audio へ LoadSong 再送 (audio_sources update)
                                  })
                                   ↓ EventLoopProxy::send_event
AppEvent::AudioImported
{ source_id, target_track,
  target_beat, frames, sr, ch }   ←
↓
ClipContent::Audio { events: [event] } 採番
Clip 配置
LoadSong → daw_audio へ再送
```

---

## 8. 波形描画

### 8.1 gui_01 既存 API

[F:\dev\gui_01\crates\ui\src\widgets\waveform.rs:73](F:/dev/gui_01/crates/ui/src/widgets/waveform.rs):

```rust
pub struct WaveformSource<'s> {
    pub samples: &'s [Vec<f32>],   // planar
    pub sample_rate: u32,
    pub valid_len: usize,          // frames
    pub channels: ChannelLayout,
    // ...
}

pub struct WaveformView { /* style + viewport */ }
```

`Ui::heavy(...)` 内で `hctx.waveform(view, source)` を呼ぶと LOD ピラミッド (mip-mapped
peaks) を内部キャッシュして描画。 Render mode は zoom 自動切替 (PeakLines / SamplePolyline /
RmsBars)。

参照: [F:\dev\gui_01\crates\examples\sample_editor\src\main.rs:300](F:/dev/gui_01/crates/examples/sample_editor/src/main.rs)

### 8.2 daw_01 側の使い方

arrangement の clip rect 内で heavy block を埋める:

```rust
ui.heavy(("audio_clip_waveform", clip.id), |hctx| {
    hctx.cached(viewport_key, |hctx| {
        let buffer = self.audio_source_cache.get(event.source_id);
        hctx.waveform(WaveformView { ... }, WaveformSource {
            samples: &buffer.samples,
            sample_rate: buffer.sample_rate,
            valid_len: (event.source_end_frames - event.source_start_frames) as usize,
            channels: ChannelLayout::from_count(buffer.channels),
        });
    })
});
```

### 8.3 source buffer の二重保持

- audio engine と GUI で別々に同 path を decode = メモリ 2 倍消費。 容認 (M1)
- M2: peaks サイドカー対応 → GUI は peaks のみ load、 サンプル本体は audio engine のみ
- それでも巨大 source (1 GB+) では peaks も巨大 → mmap 検討 (M3+)

---

## 9. IPC 影響

### 9.1 自動で乗るもの

`MainToChild::LoadSong(Song)` で Song 全体を送るので、 `audio_sources` map / `clip_contents`
の `Audio` variant も自動で audio engine / plugin host へ伝播 (bincode Encode/Decode 必須)。

### 9.2 path 解決のため project_dir を audio engine へ通知

audio engine 側でも `AudioSourcePath::ProjectRelative` を解決するため、 project_dir が必要:

```rust
// MainToChild に追加
SetProjectDir(Option<PathBuf>),
```

- 新規プロジェクト (未保存) は `None` → `ProjectRelative` は使用不可、 import は `Absolute` のみ
- save / load 時に project_dir 更新 → 全プロセスへ broadcast

### 9.3 generated audio (VOICEVOX 等) の IPC 一般化

既存 `MainToChild::SetVocalAudio { ... }` (mono 専用) を一般化:

```rust
// 既存
SetVocalAudio { track_id, samples: Vec<f32>, ... }

// 新
SetGeneratedAudio { id: u64, samples: Vec<Vec<f32>>, sample_rate: u32, channels: u16 }
```

- `id` は `AudioSourcePath::Generated { id }` と一致
- audio engine 側で `HashMap<u64, Arc<AudioSourceBuffer>>` に保持、 `AudioClipRenderer` から
  参照
- VOICEVOX 合成完了 → `SetGeneratedAudio` で配信 → AudioSource pool には
  `Generated { id }` で参照

### 9.4 大きい sample buffer の IPC

- Generated audio の sample buffer は IPC で送る → 既存の bincode + lock-free ring
  buffer (CLAUDE.md memory: project_ipc_architecture)
- 巨大 (>16 MB) は chunked send。 ring buffer 上限を超える場合は別 shmem 領域 (M3+)
- File-based AudioSource は IPC で sample 自体を送らず、 各プロセスが path を独立 decode

---

## 10. ファイル変更点 (案)

### common/

- [common/src/model.rs](../common/src/model.rs)
  - `AudioSource / AudioSourceId / AudioSourcePath` 追加
  - `AudioContent / AudioEvent / StretchMode / FadeCurve / BeatMarker` 追加
  - `ClipContent` を struct → enum (`Midi(MidiContent) / Audio(AudioContent)`)
  - `Song.audio_sources / next_audio_source_id` 追加
  - helper: `Song::alloc_audio_source_id() / ensure_audio_source_ids() / gc_audio_sources() / audio_source_refcount()`
  - helper: `Song::clip_audio_events(&Clip) -> &[AudioEvent]` / `audio_events_in_clip_mut(&mut self, t, c) -> Option<&mut Vec<AudioEvent>>`
  - `CURRENT_VERSION` 6 → 7、 v6 → v7 migration test (`load_v6_migrates_clip_content_to_enum`)
- [common/src/protocol.rs](../common/src/protocol.rs)
  - `MainToChild::SetGeneratedAudio { id, samples, sample_rate, channels }` 追加
  - `MainToChild::SetProjectDir(Option<PathBuf>)` 追加
  - `MainToChild::SetVocalAudio` 削除 (= `SetGeneratedAudio` に集約)
- [common/src/voicevox.rs](../common/src/voicevox.rs) / `voicevox_cache.rs`
  - 合成出力先を `AudioSourcePath::Generated { id }` に変更、 `SetGeneratedAudio` で配信

### daw_gui/

- 新規 [daw_gui/src/audio_source_cache.rs](../daw_gui/src/audio_source_cache.rs)
  - `Arc<AudioSourceBuffer>` を AudioSourceId キーで保持
  - decode 結果の GUI 側キャッシュ (波形描画専用)
- 新規 [daw_gui/src/import_audio.rs](../daw_gui/src/import_audio.rs)
  - import file の SHA-256 hash 算出 + `<project_dir>/samples/` への copy + dedup
  - 未保存 project の import_cache fallback
- 新規 [daw_gui/src/view/inspector.rs](../daw_gui/src/view/inspector.rs)
  - audio event 編集 UI (Bitwig Inspector 相当)
- 新規 [daw_gui/src/view/audio_editor.rs](../daw_gui/src/view/audio_editor.rs) (Phase 2)
  - clip ダブルクリックで開く Audio Editor (§3.10)
  - 既存 [daw_gui/src/view/piano_roll_view.rs](../daw_gui/src/view/piano_roll_view.rs) と並列の構造
  - heavy block 内で event 単位の波形描画 + drag handler (event 追加 / trim / 移動 / 削除)
- [daw_gui/src/view/arrangement_view.rs](../daw_gui/src/view/arrangement_view.rs)
  - `take_file_drop_in_rect` で取った paths を `AppEvent::ImportAudio { paths, target_track, target_beat }` に変換 (status_message を出すだけの現状を置換)
  - audio clip 描画 (heavy block 内 `WaveformView`)
  - 角 drag (fade) / 中央 drag (gain) ハンドラ追加
  - **clip ダブルクリック**: kind に応じて piano_roll / Audio Editor 切替の AppEvent 発火
- [daw_gui/src/app.rs](../daw_gui/src/app.rs)
  - 新 AppEvent: `ImportAudio / AudioImported / SplitClipAtPlayhead / GlueSelectedClips / SetAudioEvent / AddAudioEvent / DeleteAudioEvent / DuplicateAudioEvent / AudioEventReverse / Normalize / BounceClip / SetStretchMode / SetFadeIn / SetFadeOut / SetClipGain / SetClipPitch`
  - import handler: background thread で copy + decode → `AudioSourceCache` 更新 → AppEvent::AudioImported
  - Split (`E`) / Glue (`J`) は MIDI / Audio / Vocal すべての kind に対し同 handler で分岐
  - `is_undoable` 登録
- [daw_gui/src/view/runner.rs](../daw_gui/src/view/runner.rs) / `shortcuts.rs`
  - **`E` (Split) / `J` (Glue)** shortcut wire (text input フォーカス時除外)
  - `Ctrl+B` (Bounce In Place) / `Ctrl+R` (Reverse) wire
  - clip ダブルクリック検出 (gui_01 #018 で AppData.last_click を活用、 既存パターン)
- [daw_gui/src/view/root.rs](../daw_gui/src/view/root.rs)
  - File menu に "Import Audio..."
  - Audio Editor open 時の panel 切替 logic

### daw_audio/

- 新規 [daw_audio/src/audio_clip_renderer.rs](../daw_audio/src/audio_clip_renderer.rs)
  - `AudioClipRenderer / RenderedEvent / AudioSourceBuffer`
  - `compile_audio_schedule(&Song, &project_dir, sample_rate, decoder_pool) -> AudioClipRenderer`
  - resampler / fade / gain / pan の inline helper
- 新規 [daw_audio/src/decoder.rs](../daw_audio/src/decoder.rs)
  - `decode_wav_to_buffer(path, target_sr) -> Result<AudioSourceBuffer>` (hound)
  - decode thread pool (LoadSong 受信時に並列 decode)
- [daw_audio/src/engine.rs](../daw_audio/src/engine.rs)
  - `EngineShared.vocal_store` を撤去 → `audio_clip_renderer: ArcSwap<AudioClipRenderer>` に置換
  - `process_buffer` 内で active RenderedEvent を mix
- [daw_audio/src/main.rs](../daw_audio/src/main.rs)
  - `MainToChild::LoadSong` 受信時:
    1. project_dir で path 解決
    2. background thread で全 AudioSource を decode
    3. compile_audio_schedule → ArcSwap::store
  - `MainToChild::SetGeneratedAudio` 受信時に in-memory AudioSourceBuffer を sources に登録 + recompile
  - `MainToChild::SetProjectDir` で project_dir を保持
- [daw_audio/src/export.rs](../daw_audio/src/export.rs)
  - export thread でも `AudioClipRenderer` を使うので変更最小 (compile_schedule 流用)
- [daw_audio/src/vocal.rs](../daw_audio/src/vocal.rs)
  - 削除 or `AudioSourceBuffer` 経由で吸収

### daw_plugin_host/

- 変更不要 (audio clip は audio engine の責務、 plugin host は plugin 経由のみ)

---

## 11. Phase 区切り (実装順)

memory「機能復旧 > 新機能」 (feedback_recovery_priority) に従い、 export (A3) 等の機能消失分が
片付いた後に着手。 各 Phase は独立 PR、 各 Phase ごとに `cargo build / clippy / test` clean を
確認してから次へ。

### Phase 1: import + 基本配置 + 単純再生 (Raw + Repitch) + Split / Glue
- AudioSource pool / ClipContent enum 化 / .daw v6 → v7 migration
- drag&drop import (WAV のみ、 hound)
- File menu "Import Audio..."
- **import 時に project dir 内 `samples/` へコピー** (§13 Q2)
- Clip 移動 / 左右端 trim / Delete
- audio engine の Raw mode 再生 + sample rate 自動 resample
- arrangement の波形描画 (gui_01 WaveformView 連携、 LOD は WaveformView 内蔵)
- 共有コピー (Ctrl+drag) / 独立コピー (Ctrl+Shift+drag) / D / Alt+D
- Make Unique (audio 版)
- Pitch (Repitch mode、 単一値、 linear interp)
- **Split (`E`) / Glue (`J`)** — MIDI / Audio / Vocal すべての clip kind に同時実装
- 既存 VOICEVOX 経路の SetGeneratedAudio 移行 (互換性維持)

### Phase 2: 編集機能拡張 + Audio Editor
- Fade In/Out (角 drag + Linear/Exponential/SCurve)
- Auto-Fade / Auto-Crossfade
- Gain (clip 中央 handle + Inspector)
- Pan
- Reverse
- Mute (event 単位)
- Inspector 基本 UI (timing / source / fade / gain / pitch / pan / mode)
- **Audio Editor (§3.10)**: clip ダブルクリックで開く、 event 編集・追加・削除
- Bounce / Bounce In Place

### Phase 3: stretch + multi-format + peaks
- Stretch mode (granular / phase vocoder)
- Normalize (peak 解析 + gain 設定)
- **peaks サイドカー (.peaks file)** — 4 GB 想定では起動毎の LOD build を回避するため Phase 3 で導入
- symphonia 導入 (FLAC / AIFF / MP3 / Ogg Vorbis / Opus)

### Phase 4: 想定外スコープ
- Comping (cycle record + take folder)
- Expression point 列 (時間変化 gain / pan / pitch / formant、 Bitwig 流の curve 編集)
- Project bundle ("Pack" / "Collect external samples" の保存先選択 UI)
- > 4 GB sample の memory-mapped streaming
- 高品質 sinc / Lanczos resampler
- Formant shift
- Onsets auto-detection (Audio Editor で event 自動分割の補助)

---

## 12. CLAUDE.md / 既存設計との整合

- **bincode derive 必須**: `AudioSource / AudioContent / AudioEvent / StretchMode / FadeCurve /
  BeatMarker / AudioSourcePath` すべてに `#[derive(Encode, Decode)]`
- **RT 制約**: audio engine の `process_buffer` 内で `Vec::new()` / lock / I/O 禁止
  → `AudioClipRenderer` は事前 compile、 sample buffer は `Arc` 共有、 decode は必ず別スレッド
- **3 プロセス分離**: file-based sample buffer は path 解決 + 各プロセス独自 decode (メモリ
  重複だが IPC は path のみ)。 generated audio のみ shmem / ring buffer 経由
- **gui_01 path 依存**: WaveformView は `daw-ui-core` で既に export 済 ([gui_01/crates/ui/src/lib.rs:75](F:/dev/gui_01/crates/ui/src/lib.rs))。
  追加 dep 不要
- **Single Source of Truth**: AudioSource は Song-global pool 1 箇所、 各プロセスは独自 cache
  (decode 結果) を持つが、 これは「path → buffer」 の純粋な derived data。 Source 自体の
  metadata (sample_rate / channels / frames) は Song 内 1 箇所
- **gui_01 への要望が必要な場面**: Phase 4 で audio event 単位の Inspector / Slice marker 描画
  / 角 drag fade を gui_01 widget API として要望する可能性あり (現状 widget 越えの可能性あり)

---

## 13. 上流決定事項 (確定)

仕様確定にあたり、 未確定だった選択肢を一次情報 (Bitwig User Guide / 既存 daw_01 設計 /
既存 DAW 慣行) に基づき以下のとおり確定する。

### Q1 Track の kind 区別 → **持たない** (Bitwig 5.x Hybrid Track 流)

1 track 内に MIDI clip / Audio clip を**混在配置可能**にする。 Bitwig 5.x の Hybrid
Track と同じ思想で、 daw_01 では Track に kind を持たせない (= 既存の kind-less Track を
維持)。

判断根拠:
- Bitwig 5.x で Hybrid Track が導入され、 1 track で MIDI + audio 両対応が標準化
- ユーザー指定 (本会話)
- 既存 daw_01 Track は kind-less ([common/src/model.rs](../common/src/model.rs))。 変更不要

挙動:
- 1 track 内で MIDI clip と Audio clip が時間軸上に混在配置できる
- Instrument plugin chain は MIDI clip 由来の note 信号にのみ作用 (= MIDI clip → instrument → effect chain)
- Audio clip は instrument plugin chain を **bypass** し、 effect chain だけ通る
- VOICEVOX (= Vocal) は既存通り `InstrumentSource::Voicevox` で識別、 Track 種別ではなく
  Track が「VOICEVOX を instrument として使っているか」 で判定 (既存仕様維持)
- Group / Bus 等の特殊 track 判定は既存 routing graph の構造に従う ([plan_routing_graph.md](plan_routing_graph.md))

migration 不要 (Track 構造変更なし)。

### Q2 sample copy 方針 → **import 時に project dir 内へコピー**

#### Project = bundle directory (Bitwig / Ableton / Logic 流)

`File → Save As` の UI は **「名前を付けて保存」 ダイアログ** (`rfd::FileDialog
::save_file` with `.daw` filter)。 ユーザーは普通に `<parent>/wav03.daw` のように
プロジェクト名を入力し、 daw_01 は親フォルダ内に **同名のフォルダを自動作成** して
中に project file (`wav03.daw`) と `samples/` などを配置する:

```
<parent_dir>/
└── wav03/                       -- daw_01 が自動作成した bundle directory
    ├── wav03.daw                -- project file (= フォルダ名と同じ)
    ├── samples/                 -- import した audio file の copy 置き場
    │   ├── kick_a3f4b912.wav
    │   └── lead_5e1c2d04.wav
    └── bounce/                  -- Phase 2: Bounce In Place 出力先
```

つまりユーザー入力 `<parent>/wav03.daw` → 実際の保存先は `<parent>/wav03/wav03.daw`。
これにより「ファイル名だけ選んだら samples/ がどこに作られるか分からない」 旧挙動と、
「`pick_folder` dialog では Windows の input 欄に新フォルダ名を入れても "パスが存在
しません" エラーで先に進めない」 という UI 問題の両方を回避できる (Bitwig / Ableton
の Save As と同じ流れ)。

既存 `<parent>/<name>/<name>.daw` の上書きは確認ダイアログ (`rfd::MessageDialog`) を
出してから。 `<parent>/<name>/` フォルダが既に存在しても daw_01 は (空であれ非空であれ)
そのまま使い、 `samples/` 等のサブフォルダだけ `create_dir_all` で確保する。

`File → Save` (=`Ctrl+S`) は既存 `file_path` があればそのまま上書き、 無ければ
`action_save_as` (= 上記の名前ダイアログ) にフォールバック。

#### import file の配置

import した WAV は `<project_dir>/samples/<sanitized_name>_<short_hash>.wav` にコピーし、

詳細:
- **import 時**:
  1. `<project_dir>/samples/` を作成 (なければ)
  2. ファイル名衝突回避: `<basename>_<8 文字 hash>.<ext>` (hash は元 path の SHA-256 prefix)
  3. 既存 `samples/` 内に同 hash のファイルがあれば dedup (再 copy しない)
  4. `AudioSourcePath::ProjectRelative(PathBuf::from("samples").join(filename))` で記録
- **未保存プロジェクトの場合**:
  - 一時的に `%LOCALAPPDATA%/daw_01/import_cache/<session_id>/<filename>` にコピー
  - `AudioSourcePath::Absolute(cache_path)` で記録
  - save 時に project dir の `samples/` に移動 + `ProjectRelative` に変換 (autosave も同じ)
- **save as / move project**:
  - 旧 project dir の `samples/` を新 project dir にコピー (= project 全体 portable)
- **GC**: save 時に未参照の `samples/<file>` は削除 (`Song::audio_sources` に存在しない物のみ)
  - 安全側に倒し、 「project_dir/samples/ 内で AudioSource pool に無いもの」 だけ remove

`AudioSourcePath::Absolute` は型として保持 (未保存プロジェクトの import_cache 用 / 将来の
"link to external sample" 用)、 通常の import 経路では生成しない。

### Q3 Pitch (Repitch mode) → Phase 1 に含める

linear-interp resample は実装容易、 Bitwig audio event の core 機能なので Phase 1 範囲内
で完結させる。 Phase 1 で:
- `AudioEvent.pitch_semitones` field 編集 (Inspector or shortcut)
- `StretchMode::Repitch` 再生 (linear interp)
- `pitch_ratio = 2^(pitch_semitones/12) * (source_sr / engine_sr)` で source frame stride

### Q4 mono → stereo upmix → L=R 複製 + pan 適用

mono source を stereo bus に流す際は L=R に同じ sample を複製、 その後 pan で equal-power
attenuation。 stereo source は balance pan (L 削減 / R 削減のみ、 sample 自体は触らない)。
Bitwig / Ableton 標準。

### Q5 Fade curve → Linear / Exponential / SCurve の 3 種

採用。 SCurve は Auto-Crossfade の equal-power に使う。 角 上下方向 drag で 3 種を
段階トグル。

### Q6 Audio clip と plugin chain の関係 → MIDI 信号のみ instrument を通す

Q1 で 1 track 混在 OK としたため Audio track 概念は無い。 1 track の plugin chain は:
- **MIDI clip** 由来の note → instrument plugin (chain 先頭) → effect plugin chain → 出力
- **Audio clip** 由来の audio → instrument plugin を **bypass** → effect plugin chain → 出力

つまり instrument plugin は MIDI 信号のみ受け取り、 audio clip は effect chain の入口に
直接合流する。 Bitwig Hybrid Track の挙動と一致。

実装: `compile_schedule` ([daw_audio/src/graph.rs](../daw_audio/src/graph.rs)) で audio
clip の出力サンプルを instrument plugin の output buffer に **加算** する形で合流させる
(= effect chain は instrument output を input にしているので、 そこに足せば自然に流れる)。

### Q7 共有 audio clip の event 編集の伝播 → 即時、 全 linked clip に反映

MIDI 編集と同一 ([plan_clip_share_clone.md §4](plan_clip_share_clone.md))。
- Inspector 編集 / 角 fade drag / 中央 gain drag / Reverse 等は AppEvent → AppData が
  `clip_contents[content_id]` を直接書き換え
- 同 content_id を持つ他 Clip は次フレーム再描画 (波形描画も refresh)
- batch (commit ボタン) は採用しない

### Q8 Bounce In Place の出力先 → project dir 内 `bounce/` サブフォルダ

`<project_dir>/bounce/<source_name>_<ts>.wav`。 未保存 project の場合は user cache dir
(`%LOCALAPPDATA%/daw_01/bounce_cache/<random>.wav`) にフォールバック、 save 時に
`bounce/` へ移動。

### Q9 Slice In Place → **採用しない**

ユーザー指定により採用しない。 audio event の追加 / 分割 / 削除は **Audio Editor** (clip
ダブルクリックで開くエディタ、 §3.10) で行う。

`Slice to Drum Machine` / `Slice to Multisample` も同様に採用しない (Bitwig 固有機能で
daw_01 には不要との判断)。

### Q10 gui_01 widget の audio clip 描画 → heavy block 内で自前描画

Phase 1-3 は heavy block 内で `WaveformView` を直接呼ぶ自前描画 (Ui::arrangement 拡張
要望は出さない)。 Phase 4 で audio event 単位の描画 / Slice marker 等で必要が生じた
段階で gui_01 widget 拡張要望を投げる ([docs/gui_01_conversation.md](gui_01_conversation.md)
への新 entry)。

---

## 14. Keyboard shortcut 一覧

audio clip 関連の shortcut。 既存 MIDI clip 系 ([daw_gui/src/view/shortcuts.rs](../daw_gui/src/view/shortcuts.rs))
と整合。 text input フォーカス中はすべて除外 (gui_01 自動対応)。

| Shortcut | 動作 | スコープ |
|---|---|---|
| `E` | **Split** clip at cursor (= playhead) — MIDI / Audio / Vocal 共通 | arrangement focus |
| `J` | **Glue (Consolidate)** selected clips — MIDI / Audio / Vocal 共通 | arrangement focus |
| `Delete` | 選択 clip 削除 (既存 MIDI 共通) | arrangement focus |
| `D` | 選択 clip の末尾に共有コピー (既存) | arrangement focus |
| `Alt+D` | 末尾に独立コピー (既存) | arrangement focus |
| `Ctrl+B` | Bounce In Place | arrangement focus、 audio clip 選択中 |
| `Ctrl+R` | Reverse selected event | arrangement focus、 audio clip 選択中 |
| `Ctrl+N` | Normalize selected event | arrangement focus、 audio clip 選択中 (Phase 3) |
| `Ctrl+drag` | 共有コピー (linked) — 既存 | arrangement |
| `Ctrl+Shift+drag` | 独立コピー (deep clone) — 既存 | arrangement |
| `Alt+drag` (clip 中央) | snap 一時無効 — 既存 | arrangement |
| `Alt+drag` (clip 右端) | time-stretch handle (Phase 3) | arrangement |
| `Drag` (clip 角) | Fade In/Out length | arrangement |
| `Drag` (clip 角 上下方向) | Fade curve トグル (Linear → Exp → SCurve → Linear) | arrangement |
| `Drag` (clip 中央 dB handle) | Gain (±24 dB) | arrangement |
| ダブルクリック (audio clip) | **Audio Editor** を開く (§3.10) | arrangement |
| `Esc` (Audio Editor 内) | Audio Editor を閉じる | Audio Editor focus |
| `Ctrl+D` (event 選択中) | event を Duplicate (同 source で直後に複製) | Audio Editor focus |

`Ctrl+Z` / `Ctrl+Y` (Undo / Redo) は既存。 1 操作 = 1 step (§16)。

---

## 15. Selection model

audio clip の選択は MIDI clip と同一 model ([daw_gui/src/app.rs](../daw_gui/src/app.rs) の
`selected_clip` / `selected_clips`):
- 単一: クリックで選択、 同一 ContentId 共有 clip でも各 Clip は別個に選択管理
- 複数: Shift+click で範囲 / Ctrl+click で toggle (audio / MIDI 混在選択も可)
- 全選択: `Ctrl+A` (arrangement focus)

audio event 単位の選択は Phase 4 (Slice In Place) で初めて必要になる。 M1-3 は「1 Clip
= 1 event」 なので Clip 選択 = event 選択と同義。

Inspector の表示挙動 (Bitwig 流):
- 0 選択: 何も表示しない
- 1 clip 選択: Clip フィールド + 唯一の event のフィールド両方を 1 panel で
- 複数 clip 選択: 共通 field のみ表示、 値が分散する field は `—` 表示、 編集すると全選択に
  一括適用

audio / MIDI 混在選択時:
- 共通 field (Position / Length / Color / ContentId 表示) のみ表示
- audio 専用 field (Gain / Pitch / Stretch Mode 等) は audio clip 選択数を hint で表示

---

## 16. Undo / Redo

既存の Song snapshot ベース Undo (`is_undoable` 列挙、 [daw_gui/src/app.rs](../daw_gui/src/app.rs))
で audio clip 操作も自然に対応。 1 操作 1 step を原則:

| 操作 | step 単位 |
|---|---|
| Import (file drop / dialog) | 1 step (= AudioSource pool 追加 + Clip 配置) |
| 移動 / 左右端 trim | drag release で 1 step |
| Delete | 1 step |
| 共有コピー / 独立コピー / D / Alt+D | 1 step |
| Make Unique | 1 step |
| Split / Glue | 1 step |
| Inspector field 編集 | field ごとに 1 step |
| 角 fade drag / 中央 gain drag | drag release で 1 step (中間値は記録しない) |
| Reverse / Normalize | 1 step |
| Bounce In Place / Bounce | 1 step (ClipContent 置換 + 新 AudioSource 追加が atomic) |
| Slice In Place (Phase 4) | 1 step |

AudioSource pool への追加 / GC は単独の Undo 対象ではなく、 引き起こした Clip 操作と
同じ step に紐付く (= Undo で Clip が消えると同時に未参照 AudioSource も snapshot 復元
で消える)。

---

## 17. エラーハンドリング

| エラー | 検出箇所 | 挙動 |
|---|---|---|
| 未対応フォーマット (M1: WAV 以外) | import 時 | status_message "Unsupported format: foo.flac (Phase 3+ で対応)" + import abort |
| WAV header 異常 / 破損 | hound decode | status_message + import abort |
| サイズ上限超 (>4 GB) | decode 前の metadata 確認 | status_message + import abort |
| missing source (load 時 file 不在) | load 時 path 解決 | AudioSource metadata は保持、 Clip は赤 outline + 再生時無音 |
| sample_rate=0 / channels=0 | decode 後 validate | import abort、 AudioSource 登録せず |
| project_dir 未設定 + ProjectRelative path | audio engine 側 | warn ログ + 当該 source skip (= 該当 Clip 無音) |
| decode thread panic | decode 関数 catch_unwind | status_message + AudioSource 登録せず |
| AudioSourceBuffer メモリ確保失敗 | decode 時 Vec 確保 | status_message + import abort |
| Glue で異 kind 混在選択 | Glue handler | reject + status_message "MIDI / Audio / Vocal clip が混在しているため Glue できません" |
| Audio Editor で source pool に無い event を参照 | Audio Editor render | event を赤 outline + "MISSING SOURCE" badge、 再生時無音 |
| samples/ ディレクトリ作成失敗 | import handler | status_message + import abort (= 元 wav には触れずに失敗) |
| samples/ への copy 失敗 (ディスク容量不足等) | import handler | status_message + import abort、 部分 copy file は削除 |
| Bounce 中の plugin error | offline render | bounce abort + status_message、 partial wav 削除 |
| Bounce 出力先 dir 作成失敗 | bounce thread | status_message + bounce abort |

ユーザーに見せるエラーはすべて `AppData.status_message` に出力 + 該当 Clip / Source は
視覚警告 (赤 outline / "MISSING" badge)。 開発者向け詳細は `tracing::error!` ログ。

---

## 18. テスト戦略

### Unit test (common / daw_audio)
- `Song::audio_source_refcount(id)` — 0 / 1 / 複数 Clip 参照
- `Song::gc_audio_sources()` — 未参照 source のみ remove、 参照中は keep
- `Song::ensure_audio_source_ids()` — 0 sentinel の再採番
- `compile_audio_schedule` — 重なり events / 隣接 events / fade overlap / Repitch ratio
- `migrate_v6_to_v7_clip_content_enum` — `ClipContent { notes }` → `Midi(MidiContent { notes })`
- `decode_wav_to_buffer` — 16/24/32-bit PCM + f32/f64、 mono/stereo
- `audio_event_resample_repitch` — pitch_semitones=12 で出力 frame stride が 2x
- `split_clip_midi` / `split_clip_audio` / `split_clip_vocal` — 各 kind の split で notes / events が正しく分割される
- `glue_clips_midi` / `glue_clips_audio` — 隣接 / 隙間あり / 同 ContentId / 異 ContentId の各組合せ
- `glue_rejects_mixed_kinds` — MIDI + Audio 混在選択は reject される
- `import_copies_to_samples_dir` — import 時に `<project_dir>/samples/` へコピーされ、 元ファイルを削除しても AudioSource は load 可能
- `import_dedup_by_hash` — 同 hash の wav 2 回 import で AudioSource は 1 つ

### Integration test (daw_gui/tests)
- `audio_clip_import_smoke.rs` — drag&drop simulate → samples/ へ copy → AudioSource 採番 → Clip 配置 → LoadSong 到達
- `audio_clip_playback.rs` — script で audio clip 配置 → freewheel export → 出力 wav が source と frame-level 一致
- `audio_clip_pitch.rs` — `pitch_semitones=12` で出力 wav の長さが半分
- `audio_clip_share_clone.rs` — Ctrl+drag で linked → 一方の event 編集 (Reverse 等) が他方に反映
- `audio_clip_fade.rs` — fade_in_beats=4 → 出力 wav 先頭 4 beats の RMS が curve に従って増加
- `audio_clip_persistence.rs` — save → 元 wav を削除 → 別プロセスで load → samples/ から AudioSource が復元、 再生も成功
- `audio_clip_split_glue.rs` — `E` で split → `J` で glue で元に戻る (= round trip 等価) を MIDI / Audio それぞれで
- `audio_clip_audio_editor.rs` — clip ダブルクリック → Audio Editor open → event 追加 / 削除 / 移動 → arrangement 側の波形が反映
- `audio_clip_voicevox_compat.rs` — 既存 VOICEVOX 経路 (SetGeneratedAudio に移行後) が動作
- `audio_clip_track_mixed.rs` — 1 track 内に MIDI clip + Audio clip 混在配置で再生時に正しく合流 (instrument plugin chain は MIDI 由来のみ通り、 audio は bypass される)

### 実機 smoke test (cargo run -p daw_gui)
1. WAV を arrangement に drag&drop → `<project_dir>/samples/` にコピー → 配置 → Play で鳴る
2. 同 WAV を 2 clip 配置 → AudioSource refcount=2 (samples/ には 1 ファイルのみ)
3. import 後に元 wav を削除 → save → reload で再生が継続 (= portable 性確認)
4. Ctrl+drag で linked clip → 親 event 編集 (gain / pitch) が子に反映
5. Ctrl+Shift+drag で independent clip → 親編集が子に **反映されない**
6. 複数 file drop で複数 track に縦配置 (足りなければ track 自動作成)
7. 巨大 file (3 GB OK / 5 GB abort) のサイズ上限挙動
8. save → 別プロセスで load → AudioSource pool / ClipContent::Audio が round trip
9. VOICEVOX 経路の合成が引き続き動作 (SetGeneratedAudio 移行後)
10. Repitch mode で pitch=+12 → 1 octave 上 + 半分の長さ (聴感確認)
11. **`E` Split / `J` Glue**: MIDI clip と Audio clip 両方で動作確認
12. **audio clip ダブルクリックで Audio Editor open**、 event 追加 / 移動 / 削除が arrangement に反映
13. 1 track 内 MIDI + Audio 混在配置で正しく鳴る (Bitwig Hybrid Track 相当)

---

## 19. 参照

### 一次情報 (Bitwig User Guide)
- [Working with Audio Events](https://www.bitwig.com/userguide/latest/working_with_audio_events/)
- [Inspecting Audio Clips](https://www.bitwig.com/userguide/latest/inspecting_audio_clips/)
- [Clip Menu Functions](https://www.bitwig.com/userguide/latest/clip_menu_functions/)
- [Slice to Drum Machine](https://www.bitwig.com/userguide/latest/slicing_to_notes/)
- [Bitwig User Guide PDF](https://www.bitwig.com/media/bitwig_userguide/pdf/Bitwig_Studio_User_Guide_English_XfuP7Nz.pdf)

### daw_01 既存仕様
- [docs/plan_clip_share_clone.md](plan_clip_share_clone.md) — MIDI 共有コピー仕様 (audio で踏襲)
- [docs/plan_a3_wav_export.md](plan_a3_wav_export.md) — freewheel offline render の構造
- [docs/plan_a2_audio_engine.md](plan_a2_audio_engine.md) — audio engine の現状
- [docs/plan_a1_voicevox.md](plan_a1_voicevox.md) — VOICEVOX 経路 (SetGeneratedAudio で一般化)

### 参考実装
- gui_01 WaveformView: [F:\dev\gui_01\crates\ui\src\widgets\waveform.rs](F:/dev/gui_01/crates/ui/src/widgets/waveform.rs)
- gui_01 sample_editor example: [F:\dev\gui_01\crates\examples\sample_editor\src\main.rs](F:/dev/gui_01/crates/examples/sample_editor/src/main.rs)
- 既存 vocal pre-render: [daw_audio/src/vocal.rs](../daw_audio/src/vocal.rs)
- 既存 freewheel render: [daw_audio/src/export.rs](../daw_audio/src/export.rs)
