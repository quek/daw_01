# Global Sampler / MIDI Capture — 設計正本

REAPER 用スクリプト **Global Sampler** (BirdBird、
<https://forum.cockos.com/showthread.php?p=2506514>、ソース
`Bird-Bird/ReaScript_Testing/Global Sampler/BirdBird_Global Sampler.{lua,jsfx}`) の
「常に録り続けて、後から欲しい部分を切り出す」を本 DAW に組み込む。**MIDI 版** (Ableton Live の
Capture MIDI 相当) も同じ思想で足す。

UI は 2026-09-05 の grill-me で確定 (Q1〜Q8)。この文書がそれ以降の SSoT。

## 1. 原典の挙動 (一次情報)

| 項目 | Global Sampler (jsfx + lua) |
|---|---|
| 録音源 | JSFX を挿した位置の出力 (master / monitor FX / 任意 track)。1 インスタンスのみ |
| バッファ | `len_in_secs = 60; buf_len = len_in_secs*srate*2` (stereo interleaved ring)。SR 変更で再確保 |
| 一時停止 | `gmem[13]` フラグで write head を止める |
| 表示 | リング全体の波形、write head が右へ動く。中ドラッグでオフセット、Ctrl+Shift+ドラッグで縦ズーム |
| 選択 | 左ドラッグで範囲 (`draw_crop_region`) |
| 挿入 | 選択範囲をアレンジへドラッグ (`GetTrackFromPoint` でマウス下 track、`SnapToGrid`) / 右クリック「Insert at edit cursor」 |
| 試聴 | Alt+click で `gmem[11]` に位置を書き JSFX が再生 |
| 再生区間 | `play_state` の変化で `play_start` / `play_len` を記録 → 「Sample Last Playthrough」 |

Rolling Sampler (商用版) は 45 秒既定・最大 10 分、右クリック設定で長さ変更。

## 2. 確定した UI (grill-me 2026-09-05)

| # | 決定 |
|---|---|
| Q1 | **下部パネルのタブ** (Mixer / Piano Roll と並ぶ)。floating window / 別 OS 窓ではない |
| Q2 | タブは **2 つ**: 「Sampler」(音声) と「MIDI Capture」。時間軸は共有しない |
| Q3 | 録音源は **タブ内ドロップダウン**: Master (既定) / 各 track × Pre-FX / Post-FX / Post-Fader (= `AudioTap` の語彙)。切替でリングはクリア |
| Q4 | 選択範囲は **ドラッグ**でアレンジの track レーン / ランチャーのセルへ落とす。右クリックメニュー・「直前の再生区間」ボタンは作らない |
| Q5 | MIDI Capture は **MIDI 入力の全ノートを常時**溜める (arm 不問) |
| Q6 | 停止中に弾いた MIDI は **現在テンポで wall-clock → 拍**。テンポ推定・フレーズ長推定はしない |
| Q7 | 溜める長さは **ヘッダの秒数入力 (既定 60、上限 600)**。音声・MIDI 共通。**app_config に永続**。変更でリング再確保 (中身は消える) |
| Q8 | タブ内操作は **一時停止 / 試聴** の 2 つ。スクロール・ズーム無し = **常にリング全体を表示** (右端 = 今) |

## 3. アーキテクチャ

### 3.1 音声リング (`common/src/sampler_ring.rs`)

`ScopeBridge` と同じ流儀 (単一 RT writer / 単一 reader / 上書きリング / atomic のみ) を
**長さ可変**にしたもの。GUI が `NamedShmem::create`、daw_audio が `open`。

```text
#[repr(C)] SamplerRingHeader {
    write_frames:   AtomicU64   // 累積書き込みフレーム (Release store で公開)
    capacity:       AtomicU64   // フレーム数 (create 時に固定)
    sample_rate:    AtomicU32
    paused:         AtomicU32   // GUI が書く。1 なら RT は書かない
    seg_write:      AtomicU64   // 走行セグメント ring の書き込み数
    segments: [Segment; 256]    // 下記
}
samples: [[AtomicU32; 2]; capacity]   // header 直後、f32::to_bits interleaved
```

- **走行セグメント**: 「リングの何フレーム目から、曲のどこ (playhead_samples) を、どの
  bpm で再生していたか」。RT は `playing` の変化・seek・loop wrap (= 前 block の予測と
  playhead が食い違った) のときだけ 1 件 push。停止中は `playhead = None` のセグメント。
  GUI はこれで「リング座標 → 曲の拍」を引く (ルーラーに小節線を重ねる / drop 先の拍を
  決める)。
- **wall-clock**: 各セグメントに `SystemTime` (UNIX ns) も持つ。MIDI Capture の
  時刻と同じ時計 (`GetSystemTimePreciseAsFileTime`)。
- shmem 名は **世代込み** `daw_01_sampler_{pid}_{gen}` ([[project_shmem_name_reuse_race]])。
  長さ変更 / 録音源変更 = 新世代を create → `OpenSamplerRing` → 旧世代 drop。
- `common/build.rs` の `WIRE_SOURCES` に追加 (不変条件 7)。

### 3.2 engine 側 (daw_audio)

- `AudioCommand::OpenSamplerRing { shmem_id, source: SamplerSource }` /
  `CloseSamplerRing` / `SetSamplerPaused(bool)` は **shmem flag** (command 不要)。
- `SamplerSource { Master, Track(AudioTap) }`。Master は `scope.write_block` と同じ
  タップ点 (metronome 前の `master_l/r`)。Track は `resolve_tap_buffers` で
  `TrackScratch` / `PreFaderScratch` / `PreFxScratch` を読む (track index は
  `refresh_bundle` 後の song snapshot から id で毎 block 解決、無ければ無音を書く)。
  PreFx tap は schedule の `any_pre_fx_tap` に sampler 分も含める (compile 側で
  `SamplerSource` を見て snapshot を要求)。
- **試聴**: `AudioCommand::SamplerPreview { start_frame, end_frame }` /
  `SamplerPreviewStop`。RT はリング (自分が書いた shmem) を読んで master に加算する。
  **リングへの書き込みの後**に加算する (試聴音を再録しない)。範囲がリングから
  押し出されたら停止。試聴の頭・尻に 5ms のフェード。
- RT 制約: 事前確保済み shmem への store / load のみ。command 受信は既存の
  `pump_commands` 経路。

### 3.3 GUI 側 (daw_gui)

- `state/sampler.rs` (`SamplerState`): 世代 / handle / 選択範囲 (リング絶対フレーム
  `[start, end)`) / 一時停止 / 試聴中 / **波形オーバービュー** (512 frame ごとの
  `(min, max)` バケツの ring、`capacity/512` 件)。
- オーバービューは **playhead poller スレッド**が 33ms ごとにリングを読み進めて
  作り、`AppEvent::SamplerAdvance { buckets, write_frames, segments }` で GUI へ
  送る (数バケツ/tick)。省電力 (250ms) でも 1 秒上限で追従。
- 選択範囲は絶対フレームなので時間とともに左へ流れ、左端から出たら消える。
- **切り出し**: drop 時に GUI が shmem から `[start, end)` を読む (読んだ後に
  `write_frames - capacity <= start` を再検証、失敗ならステータスに「流れてしまった」)。
  32-bit float WAV を scratch に書き `import_audio::import_one` へ渡す (hash 付きで
  `samples/` or 未保存キャッシュへ複製、save 時の移動も既存規約に乗る)。
  clip 名 = `Sampler <source> <HH-MM-SS>`。
- **配置**: `action_import_audio` の「取り込み済み `ImportedAudio` を
  `ImportTrackTarget` + `target_beat` に置く」部分を `place_imported_audio` に切り出し、
  ファイル取り込みと Sampler drop の両方がそこを通る (経路ごとの手写し禁止)。
- **ドラッグ**: daw-ui の `begin_drag(SAMPLER_DRAG_KIND, SamplerDragPayload)`。
  drop 側は `arrangement_view` の file drop と同じ場所で `take_drag_payload` し、
  `file_drop_target` + 同じ pixel→beat + snap で `AppEvent::SamplerDrop`。ドラッグ中は
  選択範囲の波形サムネイルをポインタに追従させて描く。
- タブ index: `bottom_panel = Some(2)` = Sampler / `Some(3)` = MIDI Capture。View
  メニューに「Sampler」「MIDI Capture」を追加 (ミキサーと同じ toggle 経路)。
- app_config: `sampler_seconds: u32` (既定 60、範囲 1..=600)。録音源は session-only
  (track id はプロジェクト依存)、既定 Master。

### 3.4 MIDI Capture

- `state/midi_capture.rs`: `CapturedNote { on_ns, off_ns: Option, pitch, velocity,
  channel, on_beat: Option<f64>, off_beat: Option<f64> }` の `VecDeque`。`on_ns` は
  midir コールバックで取る `SystemTime`。`on_beat` は note-on 到着時に **再生中なら**
  `transport.playhead_beat`。`sampler_seconds` より古いノートは落とす。押しっぱなしの
  ノートは `off_ns = None` で伸び続ける。
- 溜める場所は `handle_midi_note_on/off` の先頭 (ランチャー binding に飲まれる前)。
  arm / 録音状態は見ない (Q5)。
- 表示: 横 = 時間 (右端 = 今、幅 = `sampler_seconds`)、縦 = ピッチ (捕捉ノートの
  min..max に 1 オクターブの余白、最低 2 オクターブ)。ノートは矩形、押しっぱなしは
  右端まで。再生していた区間はセグメント (音声リングと同じ wall-clock) で小節線を重ねる。
- 選択 → ドラッグ → drop (`MIDI_CAPTURE_DRAG_KIND`)。clip 化の規則:
  - 選択内の全ノートが `on_beat` を持つ (= 再生中に弾いた) なら **その拍**を使う。
    clip 原点 = 選択開始の拍。
  - それ以外は wall-clock: `beat = (t - sel_start_ns) * bpm / 60` (Q6)。
  - clip 長 = 選択長 (拍) を小節に切り上げ。ランチャーのセルは既存
    `cell_place_len` 規則。
  - `place_imported_clip` (media.rs) に載せる = 音声と同じ配置経路。
- 試聴: 選択ノートを **cursor track** のインストへ。`AudioCommand::PreviewSequence
  { track_id, notes: Vec<PreviewNote { offset_frames, duration_frames, pitch, velocity }> }`
  / `PreviewSequenceStop` を足し、engine が `pending_preview` へフレーム精度で注入する
  (GUI の 33ms tick で on/off を撃つのは精度不足)。
- 一時停止: `MidiCaptureState.paused` (GUI 内)。

### 3.5 アーキテクチャ不変条件との照合

| 不変条件 | 対応 |
|---|---|
| 1 安定 id | 録音源は `AudioTap` (track id)、drop 先は `ImportTrackTarget` (id / 表示順列) |
| 2 blob-less | PCM は shmem リングと WAV materialize。protocol に `Vec<f32>` は載せない |
| 3 宛先型 | `AudioCommand` に variant 追加、`AudioEvent` は不要 (状態は shmem で観測) |
| 4 RT | store/load のみ。試聴もリング読み + 加算 |
| 5 edit_song | clip 作成は `place_imported_audio` / `place_imported_clip` 経由 |
| 7 fingerprint | `sampler_ring.rs` を `WIRE_SOURCES` へ |
| 8 daw-ui | 波形 / ノート描画は `daw_gui/src/view/sampler_tab.rs` / `midi_capture_tab.rs` |
| 9 budget | 新ファイルは各 < 1,000 ncloc |

## 4. ファイル一覧

| ファイル | 内容 |
|---|---|
| `common/src/sampler_ring.rs` | 新規: リング shmem + セグメント + reader |
| `common/src/protocol.rs` | `SamplerSource` / `OpenSamplerRing` / `CloseSamplerRing` / `SamplerPreview*` / `PreviewSequence*` |
| `common/build.rs` | `WIRE_SOURCES` 追加 |
| `daw_audio/src/main.rs` / `engine.rs` | リング open / 毎 block 書き込み / セグメント / 試聴加算 / PreviewSequence |
| `daw_audio/src/graph/compile.rs` | PreFx tap 要求に sampler source を含める |
| `daw_gui/src/state/sampler.rs` / `midi_capture.rs` | 状態 (+ poller 側のバケツ化 / 切り出し規則) |
| `daw_gui/src/event_sampler.rs` | `AppEvent::Sampler(SamplerEvent)` (`Launcher` と同じ「1 arm = 1 サブ enum」) |
| `daw_gui/src/handler/sampler.rs` | 世代管理 / drop → WAV → clip / 試聴 / MIDI drop |
| `daw_gui/src/view/sampler_tab.rs` / `midi_capture_tab.rs` | タブ描画 + 選択 + drag 開始 |
| `daw_gui/src/view/capture_drop.rs` | アレンジ / セルでの drop 受け (ファイル drop と同じ着地解決) |
| `daw_gui/src/view/bottom_panel.rs` / `menu_bar.rs` / `root.rs` | タブ追加 / メニュー / 運搬チップ |
| `daw_gui/src/main.rs` | poller で overview 生成、midir で `SystemTime` 付与 |
| `daw_gui/src/app_config.rs` | `sampler_seconds` |
| `daw_gui/src/handler/media.rs` | `place_imported_audio` 切り出し |

## 5. テスト

- `sampler_ring`: write/read 往復、上書き後の読み出し失敗検出、セグメント push の
  条件 (play 開始 / 停止 / seek / loop wrap)。
- MIDI Capture → clip 化: 再生中ノート (拍) / 停止中ノート (wall-clock) / 混在、
  clip 長の小節切り上げ、押しっぱなしノートの終端。
- overview バケツ化の境界 (512 の倍数でない読み出し量)。
- 配置は `place_imported_audio` の既存 import テストで担保 (経路共有)。
