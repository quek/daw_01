# audio clip フォローアップ計画

ステータス: **着手中** (2026-05-08)。 Phase 2 (PR1-9 + #025 wire) で audio
clip 編集機能の中心は完成。 残るのは spec §3.8 後半 (plugin 効果込み Bounce)、
§3.10 後半 (Audio Editor の event 単位編集)、 および設計負債の DRY 化など
中-長期の polish。 Phase 3 (= Stretch / Slice / Normalize / peaks サイドカー
等の本格実装) には踏み込まず、 Phase 2 の延長線にある最小スコープに集中する。

優先順位の判断: **DRY 化を先**にやる (= 後段 PR の参照基盤になる) → cosmetic
な Undo polish → 中規模機能 (plugin-FX Bounce → Audio Editor event 編集) → 後回し
items。

## PR 計画

### PR-A: `audio_clip_renderer` の共通化 (DRY 化)

**動機**: Phase 2 PR9 (Bounce In Place) で `daw_audio::audio_clip_renderer
::render_audio_events` のロジックを `daw_gui::AppData::bounce_clip_in_place`
に portion-wise port した (= 重複定義)。 同じく `fade_envelope` も
`bounce_fade_env` として AppData 側に重複。 後段 PR (plugin-FX Bounce、
Audio Editor event 編集) でも同 logic を再利用したいので、 早めに共通 crate
に切り出す。

**スコープ**:
- `common/src/audio_render.rs` (新規) に以下を移植:
  - `fade_envelope(t, fade_len, curve) -> f32` — 0..=1 envelope
  - `pitch_ratio(stretch_mode, source_sr, engine_sr, semitones) -> f64`
  - `mix_event_mono_to_stereo(...)` — 1 event の mix を `(track_l, track_r)`
    に加算する pure function (= fade × gain × pan × pitch_ratio × reversed)
- `daw_audio::audio_clip_renderer::render_audio_events` を新 helper を呼ぶ
  形にリファクタ (= ロジック差分なしで signature 変えない)
- `daw_gui::AppData::bounce_clip_in_place` も同 helper に置き換え (= 重複
  解消、 `bounce_fade_env` は削除して `common::audio_render::fade_envelope`
  使用)
- `common::audio_render` に unit test を移植 (既存の audio_clip_renderer
  test があれば共通化、 無ければ新規 fade_envelope 値域 / pitch_ratio /
  mix output の境界 test)

**受け入れ基準**:
- `cargo test --workspace --features rt-assert` 全 pass
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- 既存 PDC / sidechain integration test (pdc_mcenter / pdc_mcompressor_sidechain
  / split_glue_smoke) が pass (= ロジック不変の証明)
- `daw_audio::audio_clip_renderer` と `daw_gui::AppData::bounce_*` から
  fade / pitch / mix のロジック重複が消える

**規模**: ~150-300 行 (純リファクタ、 機能追加なし)

---

### PR-B: multi-clip drag の Undo step 統合 (cosmetic)

**動機**: gui_01 #025 の wire 後、 multi-clip 一括 drag で
`SetClipGainDb` / `SetClipFade` 等が delta 数だけ Undo step を積む (= 1
release 1 step ではない)。 既存 `MoveClips` / `SetNotePositions` 等は
Vec 引数の単一 AppEvent で 1 step に揃えているので、 同 pattern に揃える。

**スコープ**:
- 新 AppEvent (batch 系):
  - `SetClipGainDbBatch(Vec<(ClipRef, f32)>)`
  - `SetClipFadeBeatsBatch(Vec<(ClipRef, FadeEdge, f64)>)` *
  - `SetClipFadeCurveBatch(Vec<(ClipRef, FadeEdge, FadeCurve)>)` *
  - (* `FadeEdge` は AppEvent module 内で再定義 or `common::model` に追加)
- `arrangement_view::make_edit::SetClipGainDb` / `SetClipFade` /
  `SetClipFadeCurve` を batch AppEvent 1 発に変換 (= delta 群を Vec で
  まとめて発火)
- handler (`set_clip_audio_event_*` 系) はループに変更、 1 PR で全 entries
  を処理 + 最後に sync 1 回
- `is_undoable` に batch 系 3 つを登録、 単発 `SetClipGainDb` 等は keep
  (= JS test API / 単発呼び出しから直接使う)

**受け入れ基準**:
- multi-clip 選択して dB / fade / curve を drag → release 後 Ctrl+Z 1 回で
  全 clip 元に戻る
- 単発 (1 clip) drag は既存どおり 1 step
- build / clippy / test 全 pass

**規模**: ~100-150 行

---

### PR-C: plugin 効果込み Bounce (= 新 Clip + 新 track)

**動機**: spec §3.8 の "Bounce" (Pre-FX の "Bounce In Place" ではない)。
clip の plugin chain (instrument + fx_chain) を **通した** 結果を render
し、 結果を **新 track / 新 Clip** に書き出す (元 clip は残る)。
Phase 2 PR9 の `Bounce In Place` (Pre-FX) と並ぶ姉妹機能。

**スコープ**:
- `protocol`: `MainToChild::BounceClipFxOnline { source_track: u32, source_clip:
  u32, out_path: PathBuf }` を audio へ追加。 既存 `ExportWav` は song 全体
  だが、 こちらは特定 clip range のみ。 完了通知は `ChildToMain
  ::BounceClipComplete { error: Option<String>, frames: u64 }`
- `daw_audio::export.rs` を拡張: `run_export` のループ範囲引数化 (= 既存は
  song.length_beats まで、 新 path は `(start_beat, end_beat)` 引数)。
  既存の WAV 書き出しはそのまま使える (= freewheel render の本体は不変)
- `daw_gui::AppEvent::BounceClipWithFx(ClipRef)` + handler:
  - 出力先 path (`<project_dir>/bounce/<name>_fx_<ts>.wav`、 cache fallback)
  - audio engine に `BounceClipFxOnline` IPC 送信
  - 完了通知で AudioSource 採番 + 新 track 作成 + 新 Clip 配置 (元 clip
    と同 start_beat、 length_beats も同等)
  - `is_undoable` 登録
- 右クリックメニューに「Bounce (with FX)」 を追加 (6 項目目)

**設計判断**:
- **新 track** に置く理由 (spec §3.8): 元 track の plugin 効果込み output
  を新 track に切り出すと、 元 track は plugin のまま残り、 新 track は
  effect-free の audio clip となる。 「pre-mixed stem を作る」 用途
- 新 track の plugin chain は空 (= 既に baked された audio なので fx 不要)、
  parent_group_id / volume / pan は元 track からコピーする?  → 一旦コピー
  しない (= top-level 新 track、 user が手動で配置)

**受け入れ基準**:
- audio clip 右クリック → "Bounce (with FX)" → 新 track + 新 audio clip が
  作成される
- 元 clip は変更されず残る
- 新 clip を再生すると plugin 効果込みの音が出る (= mute された元 clip と
  比較して同 audio が再生される)
- export 中の status_message 進捗表示
- failure path (= IPC 失敗 / WAV write 失敗) で status_message + cleanup

**規模**: ~400-600 行 + IPC variant + protocol bincode 維持

---

### PR-D: Audio Editor の event 単位編集

**動機**: spec §3.10.2 の Audio Editor 内操作 (= clip 内に複数 event を
並べて編集)。 Phase 2 PR6 で minimal viewer (= 波形 + playhead 線) は実装
済みだが、 event 単位の操作 (Duplicate / 移動 / trim / 削除) は未着手。
multi-event clip を作成する手段がないとテストもできないので、 まず
**Duplicate** から実装して multi-event 化する path を作る。

**スコープ** (3 段階):

**段階 1: event 単位 selection + Duplicate**
- `AppData.audio_editor_selected_event: Option<usize>` (= clip 内 event
  index)
- Audio Editor 内で event の rect を click → `selected_event` 更新 (= 描画は
  選択強調)
- `Ctrl+D` shortcut で `AppEvent::DuplicateAudioEvent { clip: ClipRef,
  event_idx: usize }` 発火
- handler: events.push(events[idx].clone())、 新 event の
  `event_start_in_clip_beats` を「元 event の終端」 に配置 (元 event と
  重ならない)、 `is_undoable`

**段階 2: event drag 操作**
- audio_editor.rs に自前 hit test + drag handler を追加 (= gui_01 widget で
  なく self-managed)
- event 中央 drag → `event_start_in_clip_beats` 移動 (snap あり)
- event 左右端 drag → `source_start/end_frames` + `event_length_beats`
  連動 trim
- AppEvent: `SetAudioEventStart` / `SetAudioEventTrim` を新設、 全部
  undoable

**段階 3: event 追加 / 削除**
- 空白領域への file system drag&drop → `AppEvent::AddAudioEventFromFile {
  clip, path, beat }` (= 新 source decode + event 追加)
- 既存 event を選択 + Delete → `AppEvent::DeleteAudioEvent { clip, idx }`
- 右クリック → "Duplicate" / "Delete" / "Add From Source..." メニュー

**受け入れ基準**:
- multi-event clip が作成 / 編集できる (= clip 内に 2+ event)
- 各 event が個別に位置 / trim / gain (= Inspector で個別 selected_event
  対応) 編集可能
- Undo / Redo で全操作が戻る
- audio engine 側の compile_audio_schedule が multi-event を正しく schedule
  に積む (= 既存実装で済むはず、 PR4 PR8 PR9 と同様の検証)

**規模**: ~600-1000 行 (UI hit test + drag handler + 3 段階分の AppEvent)。
段階 1 を独立 PR、 段階 2 / 3 は別 PR に分割可能。

---

### 後回し (= 他 PR で必要になったら着手)

1. ~~**bounce_cache migration helper**~~ — **済 (本 plan の追加 PR-E)**:
   `import_audio::migrate_unsaved_bounce_sources_into` + `unsaved_bounce
   _cache_dir` を新設、 `save_to` で import_cache migration の直後に呼ぶ
   ように。 Bounce In Place / Bounce (with FX) 両方の fallback path
   組み立ても helper 経由に DRY 化。

2. **VOICEVOX → ClipContent::Audio 統合**: PR8 commit message で予告した
   後続 PR。 現状 `process_track_owned` の専用 vocal block で再生して
   いるが、 audio_clip_renderer 経由に統合したい。 ただし MIDI clip (=
   歌詞 / note) と audio buffer の併存をどう model 化するかの設計議論が
   必要。 急ぎでないので Phase 3+ の Stretch / Slice 着手前に再検討。

   #### 設計選択肢 (= 着手前にユーザー判断が要る):

   - **(a) `ClipContent::VocalAudio { events, notes }`** (新 variant):
     既存 `Audio { events }` と並列に 1 variant 増やし、 vocal clip だけ
     notes を持つ。 利点: track.kind = Vocal の流れと素直、 既存
     `clip_contents` の pool 構造を変えない。 欠点: ClipContent のデータ
     スキーマ拡張で bincode / serde 互換性に注意 (= migration 必要)。

   - **(b) `ClipContent::Audio { events, notes: Vec<Note> }`** に notes
     を埋め込む (既存 variant 拡張): 同 ClipContent で notes を共存。
     利点: variant 数を増やさない、 instrument track に audio overlay
     する高度な使い方にも転用可能。 欠点: `notes.is_empty()` の
     instrument 系 audio clip と vocal clip が型上区別できなくなる、
     audio editor / piano roll の責務分離が曖昧化。

   - **(c) `clip.notes: Vec<Note>` (=既存) + `clip.content_id ->
     ClipContent::Audio`** で audio buffer を保持 (= clip レベルで
     notes と audio events を持つ二重構造): 利点: 既存 model の
     `Clip.notes` は Phase 0 から存在、 `clip_contents.get(content_id)`
     で audio events を取れる構造に整合。 欠点: instrument MIDI clip も
     `Clip.notes` を使っているので、 vocal track だけ「audio events も
     持つ」 状態にする条件分岐が piano_roll / audio_editor / VOICEVOX
     synth 経路に散らばる。

   - **(d) Vocal track は MIDI clip と Audio clip の 2 系統を track 上に
     並列配置**: VOICEVOX 合成結果を AudioClip として MIDI clip と隣に
     置く。 利点: 既存の Audio clip pipeline をそのまま使える、 MIDI と
     audio の対応関係は metadata で管理。 欠点: 「歌詞編集 → 自動的に
     bounced audio が再生される」 という UX が崩れる (= manual sync
     必要)。

   #### 推奨

   現状の `Track.kind == Vocal` + `Clip.notes` (= 歌詞 + note) +
   `process_track_owned` で VOICEVOX 合成済 audio buffer を `EngineShared
   ::generated_audio_store` から引いてくる仕組みは、 既に「audio buffer
   が clip 単位で対応」 していて、 内部実装は (c) に近い。 統合方針として
   は **(c) を明示化** する方向が破壊的変更最小: `Clip.audio_buffer_id`
   (= `AudioSourceId` を VOICEVOX 合成結果として割り当てる) を持たせて、
   `audio_clip_renderer` が vocal track の MIDI clip 描画時に
   `clip.audio_buffer_id` を参照する形に統一する。 model 変更は小、
   audio engine の専用 vocal block は削除可能。

   ただし、 これは既存 PR8 「VOICEVOX 経路を SetGeneratedAudio に移行」
   commit `a04cc3b` の後で再検討する話なので、 **本 plan の範囲外** と
   して Phase 3+ (Stretch / Slice) 着手前にユーザーと一緒に再設計。

## 進行状況 (2026-05-08 更新)

着手済み:
- ✅ **PR-A**: `common::audio_render` 共通化 (commit `3a3826e`)
- ✅ **PR-B**: multi-clip drag Undo 統合 (commit `4401d59`)
- ✅ **PR-D 段階 1**: Audio Editor event selection + Duplicate (commit `38b0a9b`)
- ✅ **gui_01 #026 [Closed]**: rect-based pointer hit-test API 要望 (commit
  `2675218`、 gui_01 側 M14 Phase 63l で `take_primary_press_in_rect` +
  `take_drag_in_rect` 実装、 daw_01 path 依存再ビルドで取り込み完了)
- ✅ **PR-D 段階 2**: Inspector multi-event + Ctrl+] / Ctrl+[ navigation (commit `24dd71d`)
- ✅ **PR-D 段階 3**: rect-based drag UI = 中央 drag 移動 / 左右端 trim
  / 空白 drop で event 追加 / Delete shortcut / 右クリック context menu
  (Duplicate / Delete / Add From Source...) (commit `7d77fb9`)
- ✅ **PR-C 段階 1**: protocol `BounceClipFxOnline` /
  `BounceClipFxComplete` + `daw_audio::export::run_export` に range
  arg 追加、 walk は frame 0 から / write は `[start, end)` のみ +
  tail silence cutoff (commit `0e5e2c6`)
- ✅ **PR-C 段階 2**: `AppEvent::BounceClipWithFx` /
  `BounceClipFxComplete` + `pending_clip_fx_bounce` state +
  `bounce_clip_with_fx` / `handle_bounce_clip_fx_complete` handler。
  完了通知で 新 audio source (decode → cache) + 新 track ("(FX)" suffix)
  + 新 clip 配置 + Undo snapshot。 右クリックメニューに
  「Bounce (with FX)」 を追加。

すべての PR (PR-A / PR-B / PR-C / PR-D) と gui_01 #026 が完了。

追加 (本 plan 完成後):
- ✅ **PR-E (bounce_cache migration helper)**: `import_audio::migrate_unsaved
  _bounce_sources_into` + `unsaved_bounce_cache_dir` を新設、 `save_to`
  に組み込み。 Bounce In Place / Bounce (with FX) 両方の fallback path
  組み立ても helper 経由に DRY 化。

設計議論待ち (= ユーザー判断要):
- 🟡 **VOICEVOX → ClipContent::Audio 統合**: 上記「設計選択肢」 (a)〜(d) の
  どれを採用するかでき決め次第着手。 推奨は (c) 明示化方針。 Phase 3+
  着手前に user と確認。

各 PR ごとに `cargo build / clippy / test --features rt-assert` clean を
確認、 `docs/plan.md` 進捗ログにエントリ追加 + commit する運用は維持。
