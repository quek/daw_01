# B4: MIDI 録音 / MIDI export 計画

ステータス: **着手中** (2026-05-13)。 Phase 7 (M2) B4 の minimum スコープを
実装する。 詳細は [plan.md §B4](plan.md) と本ファイル参照。

## 1. 全体方針

業界標準 (Bitwig / Live / Reaper) に倣う minimum:

- **MIDI input**: `midir` crate で cross-platform device 列挙 + listener。 GUI
  process が listener thread を持ち、 受信 event を `EventLoopProxy<AppEvent>`
  経由で AppData に流す (= timing は ~10ms order、 audio thread 経由は不要)。
- **track-arm**: `Track.armed: bool` で「録音対象」 track を user が選択
  (track header の R button)。 armed track のみが MIDI input を受け取る。
- **録音 trigger**: transport bar の `Record` button + `Play` で開始。 armed
  track の現 playhead 位置に note を書き込む。
- **count-in**: B3 残として本フェーズで実装。 録音 trigger 時に `count_in_bars
  ∈ {0, 1, 2}` を見て、 該当 bars 分 playhead を負方向に preroll → click のみ
  → count-in 終了で実際の record + plagback 開始。
- **MIDI export**: `midly` crate で SMF format 1 出力。 全 MIDI track を 1 file
  に。 punch range / per-track 出力は extended で対応。

## 2. 機能スコープ (A minimum)

| 機能 | 仕様 |
|---|---|
| MIDI input | `midir` 自動 connect (1 個目 available device、 device 選択 UI は extended) |
| track-arm | `Track.armed: bool` + track header の R button |
| 録音 trigger | transport の Record toggle + Play |
| 録音書き込み | armed track の **既存 MIDI clip があればそこに append、 無ければ新規 clip 作成** (clip 範囲超え= length 自動延長) |
| count-in | `AppData.count_in_bars: u8 ∈ {0, 1, 2}` + transport bar dropdown |
| MIDI export | File menu "Export MIDI..." → SMF1 / 全 MIDI track |

**範囲外** (extended で別途):

- MIDI device 選択 dropdown (現状 1 個目 fix)
- punch in / out (任意 range 録音)
- per-track export
- B1-M (VST3 IMidiMapping) — B4 完了後に別フェーズ

## 3. データモデル変更

### 3.1 `Track.armed: bool`

```rust
pub struct Track {
    // 既存
    pub armed: bool,  // default false
}
```

CURRENT_VERSION 8 → 9 に bump、 v8 forward-migrate で `armed: false`。

### 3.2 `AppData` 追加 field (session-only)

```rust
pub struct AppData {
    // 既存

    /// Phase 7 B4: MIDI 録音 enabled (transport bar Record toggle)。 false で
    /// 通常再生、 true で armed track に MIDI input が書き込まれる。
    pub midi_recording: bool,

    /// Phase 7 B3 残 / B4: count-in bars。 0 = no count-in、 1 / 2 = 1/2 小節
    /// 待機。 transport bar dropdown で設定。
    pub count_in_bars: u8,

    /// Phase 7 B4: 直近 note_on event の `(track_id, key) → start_beat` cache。
    /// note_off 受信時に `length_beats = playhead_beat - start_beat` を計算
    /// する。 stop / count-in 中 / armed 解除で clear。
    pub midi_recording_active_notes: HashMap<(u32, u8), f64>,
}
```

### 3.3 MIDI clip auto-extend

録音中、 playhead が clip 末尾を超えたら `clip.length_beats = playhead - clip.start_beat` で延長。

## 4. MIDI input 経路

```
[MIDI device]
     │ (midir thread)
     ▼
EventLoopProxy<AppEvent::MidiNoteOn { key, velocity }>
EventLoopProxy<AppEvent::MidiNoteOff { key }>
     │
     ▼
AppData::handle_event
     │
     ▼
midi_recording && self.armed_track() で armed track の MIDI clip に書き込み
```

**timing 精度**: midir → EventLoopProxy → AppData は ~5-15ms latency。 user 体感
での「録音 timing ずれ」 は許容範囲 (= REAPER / Live でも GUI 経路の MIDI 録音
は同 order)。 sample-accurate が要るなら audio thread に MIDI を渡す path が
必要だが、 minimum では不要。

## 5. MIDI export 経路

```rust
use midly::{Smf, Header, Format, Timing, Track, TrackEvent, MidiMessage};

fn export_midi(song: &Song, path: &Path) -> Result<()> {
    let ppq = 480u16;  // 業界標準
    let mut smf = Smf::new(Header::new(Format::Parallel, Timing::Metrical(ppq.into())));
    // track 0: tempo / time_sig
    // track 1..N: 各 MIDI track (Track.clips の MIDI events を SMF tick に換算)
    smf.write_std(File::create(path)?)?;
    Ok(())
}
```

## 6. count-in (B3 残 / B4 一体)

`AppData::play_with_record()` で:

```rust
fn start_recording(&mut self) {
    let bars = self.count_in_bars;
    let beats_per_bar = self.song.time_sig.0 as f64;
    if bars > 0 {
        // playhead を負方向に preroll、 click のみ鳴らす
        let preroll_beats = bars as f64 * beats_per_bar;
        self.send_audio(MainToChild::SeekTo { beat: -preroll_beats });
        self.midi_recording_pending = true;  // 0 拍に達したら録音開始
    } else {
        self.midi_recording = true;
    }
    self.send_audio(MainToChild::Play);
}
```

audio engine 側: playhead 0 通過時に `ChildToMain::CountInDone` を発火 → GUI で
`midi_recording_pending → midi_recording` 遷移。 click 音は B3 minimum でカバー
済 (= `metronome_enabled` が ON である前提)。 count-in 中は metronome を強制 ON
にする。

**SeekTo 拡張**: 現状 `MainToChild::SeekTo { beat: f64 }` は非負前提。 負値も
受け入れるよう extension が必要。

## 7. UI

### 7.1 track header の R button

既存 mute (M) / solo (S) ボタンの隣に R (Record-arm) を追加。 既存 `ArrangementTrack`
schema に R button が無ければ gui_01 #040 で要望、 既存ならそのまま使用。

### 7.2 transport bar の Record button

既存 Click toggle の隣に Record toggle を追加 (`STYLE_REC_MODE` 流用、 active
時 赤系)。

### 7.3 count-in dropdown

transport bar に "Count-in: [Off / 1 bar / 2 bars]" dropdown を追加。

### 7.4 File menu "Export MIDI..."

既存 "Export WAV..." の隣に追加。 `rfd::pick_save_file_dialog` で保存先選択。

## 8. State machine

```
   ┌──[Stop]──────────────────────────────────────┐
   ▼                                              │
[Idle] ──[Record toggle ON + Play]──> [Count-in ─[0 拍到達]──> [Recording]
                                          │ ↑                        │
                                          │ │ [count_in_bars > 0]   │
                                          ▼ │                        │
                                       [Click only]                  │
                                                                     │
                                          [Record toggle OFF / Stop]─┘
                                                  │
                                                  ▼
                                              [Idle]
```

実装は `AppData` の field 4 つ (`midi_recording: bool` / `midi_recording_pending:
bool` / `count_in_bars: u8` / `midi_recording_active_notes`) で表現。

## 9. RT 安全性

GUI 経路 (= midir → EventLoopProxy → AppData) なので audio thread には影響
なし。 audio engine 側で既存 sequencer が arrangement の MIDI clip から note
を読む経路は変えない (= 録音書き込みは GUI が ArcSwap で song を publish、
audio thread は次 buffer で snapshot を見る)。

count-in 中の audio thread: 既存 `playhead < 0` を `playing && !cmd_to_render`
として扱い、 metronome のみ render。 sequencer は `playhead < 0` で no-op。

## 10. 段階リリース計画

各 Step landing 後に user 目視確認を挟む。 build / clippy / test all clean を
各 Step で確認、 progress を本ファイル §13 に記載。

### Step A: Track.armed schema + R button UI

- [ ] `common::model::Track.armed: bool` 追加 (default false)
- [ ] `CURRENT_VERSION 8 → 9` 移行 (v8 forward-migrate で `armed: false`)
- [ ] `ArrangementTrack` schema 確認 — R button 用 field の有無
  - 無ければ gui_01 #040 で要望、 ある (= mute / solo と同 idiom) なら caller wire のみ
- [ ] `ArrangementEditRequest` で R button click event を受ける
- [ ] `AppEvent::SetTrackArmed { track_id, armed: bool }` + handler
- [ ] track header に R button 描画 + click → `SetTrackArmed`

### Step B: midir input listener + AppEvent

- [ ] `Cargo.toml` に `midir = "0.10"` 追加 (workspace deps)
- [ ] `daw_gui::midi_input` 新規 module: midir thread + EventLoopProxy
- [ ] `AppEvent::MidiNoteOn { key, velocity }` / `MidiNoteOff { key }` 追加
- [ ] handler: `if midi_recording && self.armed_track()` で書き込み

### Step C: count-in + transport Record button

- [ ] `AppData.count_in_bars: u8` field + transport bar dropdown
- [ ] `AppData.midi_recording: bool` + `midi_recording_pending: bool` field
- [ ] transport bar に Record toggle button (active 赤系)
- [ ] `AppEvent::ToggleRecording` + handler (count-in preroll → Play)
- [ ] `MainToChild::SeekTo { beat: f64 }` を負値対応に拡張
- [ ] `ChildToMain::CountInDone` 通知 (audio engine が 0 拍到達で送る)
- [ ] audio engine の sequencer / render で `playhead < 0` を「playing but no note dispatch」 として扱う
- [ ] count-in 中は `metronome_enabled` 強制 ON

### Step D: MIDI 録音書き込み

- [ ] `MidiNoteOn/Off` handler で armed track の MIDI clip に書き込み
- [ ] clip 自動拡張 (`clip.length_beats = max(prev, playhead - clip.start_beat)`)
- [ ] 既存 clip が無い場合は新規作成 (start_beat = playhead)
- [ ] `is_undoable` 登録 (= 1 録音セッション 1 Undo step、 stop 時に snapshot)
- [ ] live preview: 録音中の note は piano_roll で見える (= `Song::handle_event` 経由で arrangement / piano_roll 描画自動更新)

### Step E: MIDI export

- [ ] `Cargo.toml` に `midly = "0.5"` 追加
- [ ] `daw_gui::midi_export` 新規 module: `export_midi(song, path)`
- [ ] File menu "Export MIDI..." 追加
- [ ] `AppEvent::OpenExportMidiDialog` + handler (rfd::pick_save_file)
- [ ] SMF format 1: track 0 = tempo / time_sig、 track 1..N = 各 MIDI track の
      events (NoteOn / NoteOff / 必要なら CC / Pitch Bend)

## 11. リスク / 未解決事項

- **MIDI device 選択**: minimum では「1 個目 available」 fix。 user の主 device
  が 2 個目以降だと使えない → 早期 extended で dropdown 必要
- **timing 精度**: midir → EventLoopProxy 経由は ~5-15ms latency。 「sample
  accurate 録音」 が要件になったら audio thread 経由に refactor (大規模)
- **MIDI clip auto-extend と share group**: linked clip 録音時の挙動 (= 共有
  content だから書き込みが他 clip にも反映される) は思想 OK だが要 smoke
- **count-in 中の sequencer no-op**: 既存 sequencer は `playhead: u64` 前提。
  i64 化 or sentinel (= u64::MAX を「pre-roll」 扱い) のどちらかで対応

## 12. 進捗

- [ ] Step A (Track.armed + R button UI)
- [ ] Step B (midir input listener)
- [ ] Step C (count-in + Record button + audio engine 拡張)
- [ ] Step D (録音書き込み)
- [ ] Step E (MIDI export)

## 13. 主要ファイル変更点

| 層 | ファイル | Step |
|---|---|---|
| Cargo | `Cargo.toml` (workspace) | B (midir), E (midly) |
| Model | `common/src/model.rs` | A (Track.armed, v9 migrate) |
| Protocol | `common/src/protocol.rs` | C (SeekTo 負値、 CountInDone) |
| Audio | `daw_audio/src/sequencer.rs`, `engine.rs` | C (playhead < 0 の no-op) |
| GUI | `daw_gui/src/midi_input.rs` (新規) | B |
| GUI | `daw_gui/src/midi_export.rs` (新規) | E |
| GUI | `daw_gui/src/app.rs` | A-E (AppData / AppEvent / handler) |
| GUI | `daw_gui/src/view/transport.rs` | C (Record button, count-in dropdown) |
| GUI | `daw_gui/src/view/arrangement_view.rs` | A (R button wire) |
| GUI | `daw_gui/src/view/menu_bar.rs` | E (Export MIDI menu item) |
