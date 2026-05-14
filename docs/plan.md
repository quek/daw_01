# daw_01 master plan

このファイルは daw_01 の中期作業計画。`/clear` 後でもこの 1 枚を読めば「次に何をやるか」が分かるように維持する。

## 運用ルール

- このファイル = master plan。日々の作業順を決める起点。
- 大きい個別タスクは `docs/plan_<feature>.md` に切り出し、本ファイルからリンク。
- 各 Phase が完了したら本ファイル末尾の「進捗ログ」に commit hash + 1 行サマリを追記。
- gui_01 への要望は `docs/gui_01_conversation.md` にエントリ追加 → 返信受領 → 対応完了で `_archive_NNN.md` へ。
- スコープが変わったらこのファイルを書き換える。古い計画を残して別 phase を増やすより、上書きで always-current を保つ。

## 大方針

**Phase 1-5 (UI を gui_01 widget で再構築) と Phase 6 (M1 完成) はすべて完了**。
M1 で着手予定だった VST3 / sidechain / PDC / グループトラック / Undo/Redo / plugin
GUI embed もすべて実装済。 次は **M2 = DAW としての表現力** (オートメーション /
録音 / メトロノーム / Linux 対応) を進める。

これまでに達成済みの土台:
- 3 プロセス分離 + IPC (named pipe + shared memory)
- CLAP / VST3 統一 plugin host (scan / load / activate / process / GUI embed / sidechain / PDC)
- track-parallel スレッドプール + MMCSS + RT 違反検出 (A2)
- gui_01 widget による全 view 構築 (arrangement / piano_roll / mixer / inspector / transport)
- VOICEVOX 統合 (engine 自動起動 + per-track speaker + WAV cache + 拗音結合)
- WAV export (freewheel offline render)
- autosave + 起動時 recovery modal
- Undo/Redo (plugin instance reconcile + state snapshot 同期)
- plugin load 失敗通知 (`SlotPluginLoadFailed`、 A8)

## Phase 6: M1 完成 (完了)

DESIGN.md M1 残項目を Axx タスクに分解 + 派生で routing_graph / Undo/Redo plugin
sync chain を実装。 着手順マップ (実態):

```
A2 audio engine        ─┐
A3 WAV export          ─┤
A6 tempo/timesig       ─┼─→ A4 autosave ─┬─→ A5 lyric UI ─→ A1 VOICEVOX ─┐
A7 plugin load 同期    ─┘                │                                 ├─→ M1 完成
                                          ↓                                 │
                              routing_graph PR1-4 (group / PDC / sidechain / paraout)
                              + plugin_host stability (UAF / crash fix)
                              + Undo/Redo plugin sync chain
                              + A8 plugin load 失敗通知                    ─┘
```

各タスクの詳細は `docs/plan_<feature>.md` を参照:
- [plan_a1_voicevox.md](plan_a1_voicevox.md) (A1)
- [plan_a2_audio_engine.md](plan_a2_audio_engine.md) (A2)
- [plan_a3_wav_export.md](plan_a3_wav_export.md) (A3)
- [plan_a4_autosave_recovery.md](plan_a4_autosave_recovery.md) (A4)
- [plan_a6_tempo_timesig.md](plan_a6_tempo_timesig.md) (A6)
- [plan_a7_plugin_load_sync.md](plan_a7_plugin_load_sync.md) (A7)
- [plan_a8_plugin_load_failure.md](plan_a8_plugin_load_failure.md) (A8)
- [plan_routing_graph.md](plan_routing_graph.md) (group / PDC / sidechain / paraout)
- [plan_group_track.md](plan_group_track.md) (routing_graph PR2 詳細)
- [plan_piano_roll_widget_rewrite.md](plan_piano_roll_widget_rewrite.md)
- [plan_smart_note_length.md](plan_smart_note_length.md)
- [plan_audio_followup.md](plan_audio_followup.md) (Phase 2 polish + §3.8/§3.10 後段)

### 既知の残 bug

無し (blocking なし)。 MSoundFactory の GUI 黒画面 + 無音は VST3 bus enumeration /
setBusArrangements 改善で解消、 再現性なし。

### 完了タスクの要約

### A2: 責務分担正常化 + track-parallel スレッドプール化 [完了]

詳細は [docs/plan_a2_audio_engine.md](plan_a2_audio_engine.md)。

**完了内容 (PR1-7 + cleanup PR、 build / clippy / test clean、 ユーザー smoke test OK)**:

1. **責務分担の正常化** — DESIGN.md の規定どおり `daw_audio = シーケンサー / mixer / オーディオ出力`、`daw_plugin_host = プラグインのロード / process` に整理
2. **新しい IPC layer** — per-plugin `ProcessData` shmem (16 KB / plugin)、per-worker `WorkerBridge` shmem + Win32 named auto-reset events
3. **track-parallel スレッドプール (両プロセス N worker、 1:1 ペア)** — `audio_worker::AudioWorkerPool` が work-stealing で track を fan-out、`process_server::WorkerPool` が各 plugin を wake/done event で dispatch
4. **plugin_id registry** — plugin_host が SetSlotPlugin で発行 + shmem create + `Arc<ArcSwap<Vec<Option<PluginEntry>>>>` で publish、daw_audio は `slot_to_plugin_id` map で引く
5. **TIME_CRITICAL priority + MMCSS** — 両プロセスの worker が `SetThreadPriority(TIME_CRITICAL)` + `AvSetMmThreadCharacteristicsW("Pro Audio")` (Drop で revert)
6. **CLAP `thread_check` ext** — host 側で `clap_host_thread_check` を提供、TLS フラグで is_audio_thread を判定
7. **assert_no_alloc** — `[features] rt-assert` 追加、`cargo test --features rt-assert` で RT 違反検出可能
8. **旧 audio thread コード完全撤去** — `run_audio`, `collect_events_for_buffer`, `Tracks::params/vocal`, `TrackAudioParams (旧)`, `VocalAudio (旧)`, `PerTrackState`, `start_audio_legacy`, `AudioThread`, `TrackRouting`, `AudioRouting` を全削除 (~1500 行減)、tests を `daw_audio/src/sequencer.rs` に移植

**未完: WAV export** — A2 残タスクとしてあったが、daw_audio 側 LocalState 共有設計が大規模変更のため A3 で本格実装に切り出し。本フェーズでは plugin_host の旧 export_wav_offline + GUI Export メニューを削除のみ (機能は一時停止状態で A3 まで持ち越し)。

### A6: tempo / time_sig 変更 UI [完了]

詳細は [docs/plan_a6_tempo_timesig.md](plan_a6_tempo_timesig.md)。

**完了内容 (build / clippy / test --features rt-assert all clean、 ユーザー smoke test OK)**:

- transport bar に BPM `text_input` (1.0..=400.0 clamp、 commit で audio engine に LoadSong 再送)
- 同じく time_sig numerator `text_input` (1..=32 clamp) と denominator `dropdown` (2/4/8/16)
- AppData に `bpm_edit_text` / `time_sig_num_edit_text` 編集 buffer、 `AppEvent::BpmEditChanged` / `CommitBpmEdit` / `TimeSigNumEditChanged` / `CommitTimeSigNumEdit` / `SetSongTimeSigDenominator` を追加
- commit 系 3 種を `is_undoable` に登録 → handle_event 冒頭の `push_undo_snapshot` で自動 undo 対応
- `resync_song_edit_texts` helper で song 差し替え時 (after_undo_redo / action_new / action_open_path) に表示 buffer を現値に書き戻す
- 再生テンポは sequencer の `samples_per_beat = sample_rate * 60 / song.bpm` (sequencer.rs:71) が ArcSwap publish された Song を毎フレーム参照 → 変更が次フレームから即時反映

**スコープ外 (M2 範囲)**: CLAP/VST3 plugin への transport / tempo 通知 (`clap_event_transport_t`)、 tempo automation、 拍子 automation、 bar 番号 offset。

### A4: autosave + 起動時復元 [完了]

詳細は [docs/plan_a4_autosave_recovery.md](plan_a4_autosave_recovery.md)。

**完了内容 (build / clippy / test --workspace clean、 ユーザー smoke test OK)**:

- 新規 `common/src/recovery.rs`: recovery_dir / sidecar / session_id / scan helpers + uuid v4 生成 (5 unit test)
- AppData に `recovery_session_id` (uuid v4) / `recovery_candidates` / `show_recovery_modal` 追加
- `maybe_autosave` を改修: file_path Some なら sidecar (`<file>.daw.autosave.daw`)、 None なら `%LOCALAPPDATA%\daw_01\recovery\<session_id>.autosave.daw` に save
- `AppEvent::RecoveryRestore / RecoveryDiscard / RecoveryDismiss` + handler 実装。 復元時は sidecar なら元 .daw を file_path に、 recovery_dir 内 file なら file_path=None で新規プロジェクト扱い
- `action_open_path`: Open 時に sidecar 存在チェック → 候補に push
- 新規 `daw_gui/src/view/recovery_modal.rs`: gui_01 `Ui::modal` + `button_at_clicked + close_modal` パターン (modal 内で各候補に「復元 / 破棄」、 下部に「閉じる」)
- `runner.rs::CloseRequested`: `AppData::on_shutdown` で当セッションの recovery file (recovery_dir / sidecar 両方) を削除
- smoke 確認: × 閉じで cleanup ✅、 PowerShell `Stop-Process -Force` 経由の真の kill 後に file 残存 ✅、 再起動で modal 表示 ✅、 「復元 / 破棄 / 閉じる」 全て expected

**スコープ外 (将来課題)**:
- 古い recovery file の自動 GC (現状ユーザー操作で破棄)
- multi-instance での同時 recovery 衝突対策 (uuid v4 で衝突確率は実用上ゼロ)
- conflict 解決 UI (sidecar と元 file の自動 diff / merge)

### A5: piano_roll note 歌詞編集 UI [完了]

詳細は本ファイル「### 完了タスクの要約」 と進捗ログ参照。 以下は元計画。


**現状**: `Note { lyric: Option<String> }` schema あり、piano_roll widget の表示も M9 Phase 44c で済 (歌詞文字を note 上にレイアウト)。**入力 UI が無い**。

**やること**:
1. **gui_01 conversation #015 起こす**: piano_roll widget 内で note を選択 → F2 / Enter / dbl-click で text_input overlay 起動 → Enter commit / Esc cancel / Tab で次 note へ。編集中は drag/resize/wheel zoom を抑制 (modal-ish)
2. gui_01 reply 受領後、daw_01 側で `NotesEditRequest::SetLyric { id, text }` ハンドリング、`AppEvent::SetNoteLyric { clip_ref, note_id, text }` 追加
3. IME 入力対応 (CJK モーラ単位、既存 `text_input_at` の IME 機構に乗る)

**主な変更ファイル**:
- `docs/gui_01_conversation.md` #015 起こし
- gui_01 reply 後: [daw_gui/src/view/piano_roll_view.rs](daw_gui/src/view/piano_roll_view.rs)、[daw_gui/src/app.rs](daw_gui/src/app.rs)

**受け入れ基準**:
- piano_roll で note を選択 → F2 / Enter で inline 編集
- Tab で次の note (start_beat 順) へ移動
- 入力した歌詞が JSON プロジェクトファイルに保存される
- IME 入力で CJK が正しく入る (commit 時のみ反映、preedit は表示)

### A1: VOICEVOX 統合 [完了]

詳細は [plan_a1_voicevox.md](plan_a1_voicevox.md) と進捗ログ参照。 以下は元計画。


**現状**: `daw_audio/voicevox.rs` プレースホルダ ([common/src/model.rs](common/src/model.rs) の `InstrumentSource::Vocal` schema は完備)。

**やること** (REAPER VOICEVOX スクリプト = `%APPDATA%\REAPER\Scripts\yoshino\voicevox\` を参照実装に):
1. **HTTP client** (`reqwest` async): `/sing_frame_audio_query` / `/frame_synthesis` / `/audio_query` / `/synthesis` / `/speakers`
2. **Engine 自動起動**: 設定 (`%APPDATA%/daw_01/voicevox_engine_path`) から実行ファイルパスを読み、subprocess + Job Object で寿命管理 (DESIGN.md の手法を流用)
3. **歌詞分割**: 小書きかな (ぁぃぅぇぉゃゅょっ) は直前と結合して 1 モーラ化
4. **Sing pipeline**:
   - Clip notes → `{"notes":[{"id","key","frame_length","lyric"}, ...]}` JSON
   - `POST /sing_frame_audio_query?speaker=6000` (波音リツ固定) → audio_query
   - `outputSamplingRate=48000` 書き換え
   - `POST /frame_synthesis?speaker={singer_id}` → WAV bytes
5. **WAV cache**: `clip_id + content_hash → Vec<f32>` を `daw_audio` 側で保持。同 clip 同内容は再合成しない
6. **Vocal track の audio mix**: audio thread は cache から該当時刻の f32 を読んで master へ mix。track の plugin chain (`fx_chain`) は通すが `instrument` は使わない (Vocal は WAV 直挿し)
7. **Track Inspector の Vocal source UI**: speaker / style 選択 dropdown ([speakers] API レスポンス + cache)
8. **失敗時の resilience**: Engine 起動失敗 / HTTP 失敗時は track を mute 扱い、エラーメッセージ表示、他 track は影響受けず再生続行

**主な変更ファイル**:
- [daw_audio/src/voicevox.rs](daw_audio/src/voicevox.rs) (現状プレースホルダ → 実装)
- 新規 `daw_audio/src/voicevox_cache.rs` (clip_id → Vec<f32>)
- 新規 `daw_audio/src/voicevox_engine.rs` (subprocess 起動 + ヘルスチェック)
- [daw_audio/src/engine.rs](daw_audio/src/engine.rs) (Vocal track の mix-in)
- [daw_gui/src/view/track_inspector.rs](daw_gui/src/view/track_inspector.rs) (Vocal source 編集)
- [common/src/protocol.rs](common/src/protocol.rs) (Vocal track 内容変更を audio に伝達するメッセージが必要なら追加)

**受け入れ基準**:
- 新規 Vocal track 作成 → speaker 選択 → clip 内 note に歌詞入力 → 再生で歌う
- 同 clip の 2 度目の再生はキャッシュヒット (合成 1 回のみ)
- Engine 起動失敗時はエラー表示 + 他 track は再生継続
- Vocal track の `volume` / `pan` / `muted` / `solo` が反映 (A2 完了が前提)

着手時 `docs/plan_a1_voicevox.md` を切り出し必須 (大規模)。

### A3: WAV 書き出し / mixdown (freewheel offline render) [完了]

詳細は [docs/plan_a3_wav_export.md](plan_a3_wav_export.md)。

**完了内容 (PR1-5、 build / clippy / test --features rt-assert all clean)**:

- engine resource 共有化: `LocalState` の `worker_bridge` / `worker_syncs` / `plugin_refs` / `slot_to_plugin_id` / `vocal_store` / `worker_pool` を `Arc<EngineShared>` に移管 (ArcSwap で wait-free 共有)
- CLAP `clap_plugin_render` ext: `LoadedPlugin::set_render_mode(mode)` を追加、 ClapPlugin で `clap_plugin_render::set` 呼び出し
- protocol: `MainToChild::ExportWav { path }` / `SetRenderMode(RenderMode)` 追加、 `ChildToMain::ExportWavComplete { error }` 復活
- daw_audio: `daw_audio/src/export.rs` で freewheel offline render を実装。 同 AudioWorkerPool で track-parallel + plugin handshake を駆動、 hound::WavWriter で出力。 `export_running` フラグで CPAL callback を silence
- IPC: audio pipe を tokio::io::split で双方向化、 export 完了通知を ChildToMain::ExportWavComplete で送信
- GUI: File → Export WAV / Ctrl+E / status bar 表示復活、 SetRenderMode(Offline/Realtime) を export bookend で plugin host に送る

**残課題 (M1 完成後の検討、 必要なら別 plan)**:

- 進捗 modal (cancel button、 export thread を中断する mechanism)
- Selection / Loop range のみの export
- VST3 の render mode サポート (現状 no-op、 IComponent::setIoMode へのマップは M2)
- has_hard_realtime_requirement() を返す plugin (hardware proxy 等) のサポートは M2 で realtime export 検討

---

### A3 (旧版): WAV 書き出し / mixdown [優先度 6 — 完了済、 以下は元計画]

**現状**: A2 で plugin_host 側の旧 export_wav_offline を削除し、GUI Export メニューも一旦無効化済。daw_audio 側に新規実装する。

**設計方針** (Ardour [Export Dialog manual](https://manual.ardour.org/exporting/export-dialog/) と CLAP [render ext spec](https://github.com/free-audio/clap/blob/main/include/clap/ext/render.h) を一次情報根拠):
- **freewheel mode 採用**: export 中は通常再生を停止、CPAL callback は無音、export thread が同じ AudioWorkerPool / plugin shmem を使って CPU 限界速度で render
- **plugin instance は複製しない**: Ardour / REAPER とも標準は同 instance 流用、複製案はメモリ 2 倍化で非現実的
- **CLAP `clap_plugin_render` ext**: export 開始前に全 plugin に `set(CLAP_RENDER_OFFLINE)`、完了で `CLAP_RENDER_REALTIME`

**やること**:
1. **engine の resource 共有化**: `LocalState` の `plugin_refs` / `slot_to_plugin_id` / `vocal_store` / `worker_syncs` / `worker_pool` を `SharedState` に Arc 化して移管 (CPAL closure と export thread で共有可能に)
2. **新規 `daw_audio/src/export.rs`**: offline render loop + WAV writer。export thread が `process_track_owned` を `Song.length_beats` 分ループ呼び出し、 `hound::WavWriter` で出力
3. CPAL callback は `shared.export_running` 中、`process_buffer` を skip し無音出力
4. `MainToChild::ExportWav { path }` を再導入 (or `MainToAudio::ExportWav`)、daw_audio で受信
5. `ChildToMain::ExportWavComplete { error }` を audio から送信
6. `LoadedPlugin` trait に `set_render_mode(mode)` 追加、ClapPlugin で `clap_plugin_render` ext を取得 / set 呼び出し
7. GUI File menu / `Ctrl+E` shortcut / `action_export_wav` 復活
8. UI: export 中は status bar に「Export 中」表示、Play ボタンを無効化

**主な変更ファイル**:
- 新規 `daw_audio/src/export.rs`
- [daw_audio/src/engine.rs](daw_audio/src/engine.rs) (resource 共有化、export hook)
- [daw_audio/src/main.rs](daw_audio/src/main.rs) (ExportWav handler + writer thread)
- [daw_plugin_host/src/plugin_instance.rs](daw_plugin_host/src/plugin_instance.rs) (`set_render_mode`)
- [daw_plugin_host/src/clap_plugin.rs](daw_plugin_host/src/clap_plugin.rs) (`clap_plugin_render` 実装)
- [common/src/protocol.rs](common/src/protocol.rs) (ExportWav / ExportWavComplete 再追加)
- [daw_gui/src/app.rs](daw_gui/src/app.rs) (action_export_wav 復活)
- [daw_gui/src/view/root.rs](daw_gui/src/view/root.rs) (File menu / shortcut)

**受け入れ基準**:
- File → Export WAV → パス選択 → 再生中は自動停止 → freewheel offline render → WAV ファイルが書き出される
- 書き出した WAV を別アプリで再生 → DAW 内の再生と聴感上一致
- export 中は CPAL は無音、 完了後通常再生に復帰可能
- VOICEVOX cache を併用 (再合成しない、A1 完成後)

着手時 `docs/plan_a3_wav_export.md` を切り出し推奨 (engine の resource 共有化が大規模変更のため)。

## Phase 7: M2 = DAW としての表現力 (進行中)

M1 で土台 (3 プロセス + IPC + CLAP/VST3 host + sidechain/PDC + Undo/Redo + VOICEVOX
+ WAV export + autosave) が揃ったので、 M2 は **再生できる DAW から制作できる
DAW へ** のステップ。

進捗サマリ:

- **B1** (オートメーション + transport 通知): **大半完了**。 残: VST3 経路 3 項目のみ
- **B2** (オーディオ録音): 未着手
- **B3** (メトロノーム / count-in): 未着手
- **B4** (MIDI 録音 / export): 未着手
- **B5** (Linux 対応): 未着手
- **Undo/Redo 残リスク (B/D/E)**: クッションタスク、 未着手

### B1: オートメーション + transport 通知 (大半完了)

plugin パラメータの時間変化を扱う M2 最大の機能追加。 [`plan_automation.md`](plan_automation.md)
で詳細管理、 Phase 1〜5 が landing 済 (本 plan.md 進捗ログの 2026-05-08 〜
2026-05-13 行を参照)。

**完了**:

- パラメータオートメーション (lane 表示 / clip 同期 / `Hold` / `Linear` / `Bezier` / `Exponential` curve / lasso / multi-clip drag) → Phase 1〜3
- recording mode (Read / Touch / Latch / Write) + thinning algorithm + plugin GUI gesture sync → Phase 4
- tempo / time_sig オートメーション (`Song.song_lanes` + master row UI + audio engine tempo eval + sequencer / audio_clip_renderer beat-domain refactor + granular DSP / Slice) → Phase 5 Step 5.0〜5.2
- CLAP `clap_event_param_value` (param 送信) → Phase 2
- CLAP `clap_event_transport_t` (transport / tempo 通知) → Phase 5 Step 5.3
- SongTempo lane recording 中の engine 側 curve bypass (drag 値 / curve point 二重反映抑止) → Phase 5 Step 5.2 follow-up

**残** (VST3 経路補完のみ):

- VST3 `IAudioProcessor::ProcessData::processContext` で transport / tempo を送る
  (= CLAP `clap_event_transport_t` の VST3 版、 現状 transport 情報が VST3
  plugin に届いていない = tempo-sync 系 VST3 plugin が host テンポ追随しない)
- VST3 `IMidiMapping` (MIDI controller → plugin parameter、 = MIDI 経由の
  パラメータ自動マップ。 別途 MIDI input が必要なので B4 と一部 overlap)
- VST3 `IComponent::setIoMode(kOfflineProcessing)` (export 高品質モード切替、
  現状 `Vst3Plugin::set_render_mode` は no-op = export 中の VST3 plugin が
  realtime mode のまま動く)

着手時 `docs/plan_b1_vst3_completion.md` を切り出し。

### B2: オーディオ録音

- CPAL input stream (WASAPI 共有モード)
- オーディオクリップ (.wav 直挿し) + arrangement 表示
- 録音 → クリップ書き込み → ディスクに `.daw` と並んで save

### B3: メトロノーム / count-in

B2 録音の前提 (録音時のテンポ guide)。 既存 sequencer に乗せる軽量実装。

- 内蔵 click 音 (短い square wave)
- count-in 1 / 2 小節 オプション
- transport bar に on/off ボタン

### B4: MIDI 録音 / export

- MIDI input から note を arrangement の MIDI clip に録音
- `.mid` 形式 (midly) で export

### B5: Linux 対応

- CPAL ALSA backend
- X11 / Wayland window (winit が対応済)
- CLAP Linux GUI embed (`gtk` API、 現状 Win32 専用)
- VST3 Linux GUI embed (`X11EmbedWindow`、 同様)
- Job Object 相当 (Linux は prctl `PR_SET_PDEATHSIG`)

### Undo/Redo plugin sync の残リスク (B/D/E)

A8 で A (load 失敗 → pending stuck) は解消したが、 reconcile 経由で残るリスク:

- **B** 連続 deferred edit の race: `pending_state_request.is_some()` 即時 fallback
  で 2 番目以降の knob 値が Undo で復元されない (`app.rs:2225,2699,3684`)
- **D** Test カバレッジ不足: 4dc982c (slot-level diff) の integration test なし
- **E** 多段 Undo パフォーマンス未検証: `after_undo_redo` で reconcile が毎 step
  走る (機能正しさは OK、 連続 load/unload のコスト未測定)

着手時 `docs/plan_undo_reconcile_polish.md` を切り出し。 規模が小さいので B5
着手前のクッションタスクとしても可。

## 進捗ログ

| 日付 | commit | Phase | 内容 |
|---|---|---|---|
| 2026-05-03 | (plan.md 初版) | - | master plan 作成 |
| 2026-05-03 | 2625255 | Phase 0 | 要望 #005-#009 を gui_01_conversation.md に投稿 |
| 2026-05-03 | c46df37 | Phase 1-1 | bottom_panel を `tab_view_with_state` 化 (98 → 49 LOC) |
| 2026-05-03 | f280274 | Phase 1-2 | plugin_picker のリストを `scroll_area` 化 (truncation 廃止) |
| 2026-05-03 | (gui_01) | Phase 0 | gui_01 から #005-#009 全件 [Replied] 受領、API 確定 |
| 2026-05-03 | 2d6d70e | Phase 1-3 | mixer/inspector を scroll_area 化、scrollbar drag bug を gui_01 #010 で報告 |
| 2026-05-04 | (gui_01) | Phase 0 | gui_01 から #006 (Phase 45c API) と #010 (Phase 45g 修正済) の返信受領 |
| 2026-05-04 | 8aebba3 | Phase 3c | piano_roll の velocity / playhead を gui_01 widget 内蔵に移譲 (#006 解決、320 → ~210 LOC) |
| 2026-05-04 | ad26af5 | Phase 0 | gui_01 #006 / #010 を Resolved 化、archive_001.md へ移動 |
| 2026-05-04 | 51ff532 | Phase 3a | 背景塗りを Ui::panel / panel_with_border に置換 (10 箇所、heavy+cached boilerplate を 1 行化) |
| 2026-05-04 | bbda445 | Phase 3b | mixer の M/S を Ui::toggle_button_at に置換 (button + 自前 hint band の 2 段構えを 1 呼び出しに) |
| 2026-05-04 | 59c93ec | Phase 3d | plugin_picker.rs を Ui::modal + Ui::list_view で rewrite (167 → 145 LOC) |
| 2026-05-04 | 73e3504 | Phase 2  | Track / Clip に stable id を追加 (next_*_id 採番、ensure_ids、CURRENT_VERSION 2→3) |
| 2026-05-04 | eda8954 | Phase 3e | arrangement_view を Ui::arrangement widget で rewrite (614 → 322 LOC、id ↔ index 変換層) |
| 2026-05-04 | b7b9def | M10 取り込み | gui_01 #010 (Phase 46-48 + 47b/c) build 追従 + #011 (UX 非対称 2 件) を gui_01 に相談 |
| 2026-05-04 | (gui_01) | -            | gui_01 が #011 に Phase 49 (volume live update) + Phase 50 (reorder optimistic preview) で対応、daw_01 側は make_edit が fn 自由関数なので追従不要 |
| 2026-05-04 | da0bdf5 | Phase 5 | dead_code 清掃 (ClipBox/NoteBox/TrackHeader 構造体 + 派生メソッド削除、AppEvent に `#[allow(dead_code)]`) + clippy fix 4 件 |
| 2026-05-04 | b12adff | Phase 4 | track rename / chain reorder / reorder IPC 取り込み |
| 2026-05-04 | d832627 | Phase 4 仕上げ | gui_01 #013 (M11 Phase 52) `text_input_at_focused` 取り込み、track Rename → 即タイプ可能 |
| 2026-05-04 | b9a3fe2 | Phase 0 | gui_01 #013 を Resolved 化、archive へ移動 |
| 2026-05-04 | ce08095 | bug fix | `UiHost::with_window` で cursor 配線、hover/drag のカーソル形状が変化しない問題を解消 |
| 2026-05-04 | 22ffa56 | Phase 0 | gui_01 #014 [Open] ルーラー & time_sig 対応 grid 要望 |
| 2026-05-04 | a2117cb | chore  | .gitignore に /.claude/scheduled_tasks.lock を追加 |
| 2026-05-04 | af44c46 | M13 取り込み | gui_01 #014 (M13 Phase 55) を取り込み — アレンジビュー小節番号 ruler + ピアノロール ruler 領域 + time_sig 対応 grid |
| 2026-05-04 | 12be490 | Phase 0 | gui_01 #014 を Resolved 化、archive へ移動 |
| 2026-05-04 | 8954bed | A2 PR1-7 | 責務分担正常化 + track-parallel スレッドプール化 (build/clippy clean): plan_a2_audio_engine.md 参照 |
| 2026-05-05 | d7a7575 | A2 完了 | 残 cleanup + audio quality: 旧 audio thread コード全削除 (~1500 行)、tests を daw_audio/src/sequencer.rs に移植、MMCSS / CLAP thread_check ext / assert_no_alloc 追加。 CLAUDE.md に「まず調べる」 ルール追記。 WAV export は A3 で本格実装に切り出し |
| 2026-05-05 | 82f7e54 | A3 PR1 | LocalState から EngineShared を抽出 (Arc + ArcSwap で wait-free 共有)、 機能変化なし |
| 2026-05-05 | c10141d | A3 PR2 | LoadedPlugin::set_render_mode trait method 追加、 ClapPlugin で clap_plugin_render::set 呼び出し、 Vst3Plugin は no-op |
| 2026-05-05 | 4ace2a5 | A3 PR3 | protocol 拡張: MainToChild::ExportWav / SetRenderMode + ChildToMain::ExportWavComplete 復活、 plugin_host で全 plugin に render mode broadcast |
| 2026-05-05 | 9607660 | A3 PR4 | daw_audio/src/export.rs 新規: freewheel offline render + WavWriter。 export_running フラグで CPAL callback を silence、 同 worker pool / plugin shmem を export thread が独占駆動 |
| 2026-05-05 | e8b3d6e | A3 完了 | GUI で File→Export WAV / Ctrl+E / status bar 復活。 audio pipe を read/write 双方向化し、 export 完了通知を ChildToMain::ExportWavComplete で daw_gui へ。 SetRenderMode(Offline/Realtime) bookend で plugin に CLAP render hint |
| 2026-05-05 | 469acd7 | A3 smoke fix | smoke test で発見した 3 件 (Play/Stop/SetLoop の plugin_host 重複送信、 Ctrl+E shortcut 漏れ、 メーター peak の publish 漏れ) を修正 |
| 2026-05-05 | 02fe061 | A7 完了 | plugin ロード race の同期化: AppData::pending_plugin_loads + track_pending_load helper で SetSlotPlugin 送信時に再生中なら自動 Stop、 全 SlotPluginLoaded 受信完了で自動 Play 再開 |
| 2026-05-05 | 77cc7c5 | A6 完了 | transport bar に BPM / time_sig 編集 UI: text_input + dropdown、 commit で song 更新 + LoadSong 再送 + Undo/Redo 対応。 numpad Enter 不対応は gui_01 #016 で対応依頼 |
| 2026-05-05 | 4211315 | gui_01 #015 解決 | gui_01 M14 Phase 56 (button_at_clicked + take_*_in_rect の modal 透過抑制) を取り込み、 plugin_picker.rs の ✕ ボタンを button_at_clicked + close_modal に置換。 ✕ click / wheel scroll / Esc / outside click 全 expected。 plan.md 既知 bug クリア |
| 2026-05-05 | 9648aba | gui_01 #016 解決 | gui_01 M14 Phase 57 (PhysicalKey::NumpadEnter 追加 + text_input commit 拡張) を取り込み + daw_gui/src/view/runner.rs::map_phys_key にも NumpadEnter マッピング追加 (gui_01 winit_backend と二重実装の都合)。 BPM 入力欄でテンキー Enter による commit を実機確認 |
| 2026-05-05 | (A4 commit) | A4 完了 | autosave 拡充 (file_path None でも recovery_dir に save) + 起動時 recovery_modal + 「復元 / 破棄 / 閉じる」 + 正常終了時 cleanup + Open 時 sidecar 検出。 真の kill (PowerShell Stop-Process -Force) 後の file 残存 + 再起動 modal 表示まで実機確認 |
| 2026-05-05 | 6df6f55 ... e23ecd7 | A1 完了 | VOICEVOX 統合 Phase A/B/C/D 全完了: engine 自動起動 + JobObject + per-track speaker dropdown + /singers fetch + WAV cache + split_into_morae (拗音結合) |
| 2026-05-05 | 5311564 | A5 完了 | piano_roll L キー歌詞編集 wire (gui_01 #017 取り込み) + 旧 lyric_panel 削除 |
| 2026-05-06 | f3d35e1 / ef8588c / 70ff180 / 4992d38 | piano_roll polish | ノートのコピペ + 量子化、 velocity lane、 任意トラック削除/並び替え、 Undo/Redo (Ctrl+Z) を Song snapshot で実装 |
| 2026-05-06 | 52cab33 | piano_roll widget | piano_roll_view を gui_01 Ui::piano_roll widget で書き換え |
| 2026-05-06 | 6e4e558 / a359f7b / 85b705d | piano_roll snap | snap toolbar (Bars / Straight) + auto-fit zoom + smart note length + AHE 自律改善ループ稼働 |
| 2026-05-06 | d454bd6 | routing_graph 基盤 | 共通 schedule + group track (Reaper folder 流) + nest 無制限 + gui_01 #016 追従 |
| 2026-05-06 | 9b44d2a | PR2.1 | plugin_host を track_id ベース化 + GUI lifecycle 修正 (delete/ungroup race 対策) |
| 2026-05-06 | 9816b15 / af63880 / 4828d27 | PR3 PDC 完成 | graph layer に PDC 補償 + JS scripting + headless mode + 実 VST3 (MCenter) integration test + plugin auto-latency IPC (CLAP + VST3) |
| 2026-05-06 | 134cc9f / e8c0b3d / b37354d / 19870b0 / e94dd7e | PR4 sidechain | compile_schedule に sidechain edge + ProcessData::buffer_aux_in + engine SidechainTap + plugin_host で aux input を CLAP/VST3 process() に渡す + PDC × sidechain integration |
| 2026-05-06 | a4c8d51 / 9fa9810 / 1d58167 / 8fc09d2 / 6bb8a98 | PR4.5 follow-up | track_inspector に Sidechain section + plugin-internal main vs aux alignment + ensure_ids() の sidechain 参照 dangle 修正 + reload で sidechain_sources 保持 + duplicate SetSlotPlugin で SlotPluginLoaded 再 emit |
| 2026-05-06 | d049060 / 9ee2dca | plugin_host 安定化 | VST3/CLAP の `_library` を struct 末尾に移動 (Drop 順序 crash fix) + DispatchCounter + WorkerPool::quiesce で plugin Drop を audio worker と同期 (UAF fix) |
| 2026-05-07 | 22d7a9e / 5a9df06 / 4dc982c | Undo/Redo plugin sync | track-level reconcile (削除 → Undo で plugin 再 load) + Undo snapshot 直前に最新 state を Song に書き戻し (knob 値復元) + slot 粒度の reconcile (同 track 内 plugin 追加/削除/切替を同期) |
| 2026-05-07 | 88bf3dc | A8 完了 | plugin load 失敗通知 (`SlotPluginLoadFailed`) を新設、 plugin_host 2 失敗 path で emit (orphan cleanup 含む)、 daw_gui で pending 解放 + queue Play flush + status 表示。 integration test 2 件 |
| 2026-05-07 | aa0bb7a | M1 達成 | DESIGN.md M1 マイルストーンの全項目完了。 plan.md / DESIGN.md を実態に追従更新 (VST3 GUI embed / sidechain / PDC / グループトラック / Undo/Redo を M1 に取り込み、 M2 候補を細分化) |
| 2026-05-07 | 2f60f2e | gui_01 #018 [Open] | piano_roll velocity lane の drag 編集を内蔵要望 (NotesEditRequest::SetVelocity 新設、 multi-select 一括変更、 click<3px no-op、 release frame で 1 batch) |
| 2026-05-07 | (this commit) | gui_01 #018 [Resolved] | gui_01 main を fast-forward (5632b41 = M14 Phase 64) で取り込み、 daw_01 側で `NotesEditRequest::SetVelocity` arm + `AppEvent::SetNoteVelocities(Vec<(u32, u8)>)` + handler `set_note_velocities` を追加。 1 drag = 1 Undo step (`is_undoable` に登録)。 conversation.md の #018 entry は archive_001.md に移動 |
| 2026-05-08 | a04cc3b | Phase 1 PR8 | VOICEVOX 経路を `SetGeneratedAudio` に移行: protocol から `SetVocalAudio` 削除、 `vocal_store` / `VocalAudio` 廃止、 `EngineShared::generated_audio_store` を vocal の唯一の sink に。 `vocal_gen_id(track_id, clip_id) = (track_id << 32) \| clip_id` で per-clip 独立 buffer (= multi-clip Vocal track が overwrite し合わない)。 `process_track_owned` の vocal block は `song_track.clips` を walk して該当 buffer を clip range で mix。 daw_gui の `finish_vocal_synth` / JS `setGeneratedAudio` API も移行、 PDC / sidechain integration tests (pdc_mcenter / pdc_mcompressor_sidechain) も新 API に書き換え (clips: [{...}] を test song に追加)。 build / clippy / test --features rt-assert all clean。 audio_clip_renderer 経由の本格統合 (Vocal clip → ClipContent::Audio 化) は後続 PR |
| 2026-05-08 | 9692c72 | Phase 1 仕上げ | File menu に "Import Audio..." エントリ追加 (`AppEvent::OpenImportAudioDialog` + rfd `pick_files` で multi-select WAV → 既存 `action_import_audio` に転送)。 `resize_clip` を audio clip 対応化 — 右端 trim で `clip.length_beats` 縮小時に各 `AudioEvent.event_length_beats` を clip 内 beats 軸でクランプ (compile_audio_schedule が clip 範囲を超えて event を render し続ける問題を解消)。 plan_audio_clip.md ステータスを「仕様策定中」 → 「Phase 1 完了」 に更新。 左端 trim と Alt+drag time-stretch handle は arrangement widget が `ResizeClips` delta に `next_start` を持たないので Phase 2 で gui_01 #025 として要望予定 |
| 2026-05-08 | 9d1a0c0 | Phase 2 PR1 | Inspector に audio event 編集 section を追加。 selected_clip が `ClipContent::Audio` のとき "Audio Event" 見出しと共に Reverse / Mute toggle (gui_01 `toggle_button_at` + 専用 `ToggleButtonStyle`) と Stretch Mode dropdown (Raw/Repitch/Stretch/Slice) を表示。 AppEvent: `SetClipReversed` / `SetClipMuted` / `SetClipStretchMode` を追加し、 handler は当該 ClipContent::Audio の全 event に値を broadcast (Phase 1 で 1 clip 1 event 前提なので first event = clip 全体)。 `is_undoable` 登録済 = 1 操作 1 Undo step。 AppData::`inspector_audio_event_summary` で view 側 read snapshot を分離。 数値 field (Gain / Pan / Pitch) は edit buffer 設計が要るので別 PR。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 4b5c7dd | Phase 2 PR2 | Inspector の audio event section に数値 field 編集 (Gain dB / Pan / Pitch semitones) を追加。 既存 `bpm_edit_text` パターンを踏襲: AppData に `clip_edit_buffer_target` / `clip_*_edit_text` 4 field、 AppEvent: `Clip*EditChanged(String)` / `CommitClip*Edit` / `SetClip*` (3 種 × 3 = 9 variants) + `ResyncClipEditBuffers(target)`。 handler は Bitwig spec §3.6 の値範囲で clamp (Gain -80..+24 dB / Pan -1..1 / Pitch -96..+96 semitones)、 commit 失敗時は status_message + buffer を formatted な現値に書き戻し。 `resync_song_edit_texts` を audio event buffer も含むよう拡張 (open / new / undo / redo で resync)、 view 側でも buffer.target が selected_clip と違ければ `Edit::mutate` で `ResyncClipEditBuffers` を発火 (1 frame だけ古い buffer 表示後に書き戻る)。 `CommitClip*Edit` / `SetClip*` を `is_undoable` 登録 = 1 commit 1 Undo step。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 507be91 | Phase 2 PR3 | Inspector の audio event section に Fade In / Fade Out 編集 (length + curve) を追加。 audio_clip_renderer の fade envelope は既存 (PR4 で実装済) なので即音に効く。 AppEvent: `ClipFadeIn/OutEditChanged(String)` / `CommitClipFadeIn/OutEdit` (2 + 2 = 4)、 `SetClipFadeIn/OutBeats { target, beats }` (2)、 `SetClipFadeIn/OutCurve { target, curve }` (2) の合計 8 variants。 handler は length を `0..clip.length_beats` で clamp (= fade が clip より長くならない、 spec §3.5)、 curve は Linear / Exponential / SCurve dropdown 選択。 全 event broadcast。 `clip_fade_in/out_edit_text` 2 buffer を AppData に追加、 `resync_clip_audio_event_edit_buffers` も追従。 InspectorAudioEventSummary に `fade_in_curve` / `fade_out_curve` 追加。 Inspector view で「Fade In: length text_input + curve dropdown」 「Fade Out: length text_input + curve dropdown」 の 2 行表示。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | dcf23dd | Phase 2 PR4 | 左端 trim 対応 (Bitwig spec §3.2)。 `AppEvent::ResizeClip { target, length }` を `{ target, start_beat, length }` に拡張、 `arrangement_view::make_edit::ResizeClips` で `ResizeClipDelta.next_start` も読んで AppEvent に流す (gui_01 widget は左端 / 右端 grip drag を別 delta で emit していたが、 daw_01 は `next_len` だけ読んで左端 trim を捨てていた問題)。 handler は `delta_start = next_start - prev_start` から audio event の追従計算: 左端 trim (delta_start>0) は event を delta 手前にスライド + event が削れた場合 source_start_frames を進める / event_length_beats を縮める、 左端を伸ばす (delta_start<0) は event を後方スライド (source は触らず追加範囲は無音)、 右端 trim (delta_start==0) は既存どおり length clamp。 Phase 2 PR4 では Raw mode 前提 (Repitch 中の左端 trim は pitch_ratio 補正必要だが将来 PR スコープ)。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | a277191 | Phase 2 PR5 | Auto-Fade / Auto-Crossfade (`docs/plan_audio_clip.md` §3.5) を実装。 AppEvent: `AutoFadeSelectedClips` / `AutoCrossfadeSelectedClips` を追加 (両方 undoable)、 `arrangement_view` の clip 右クリックメニューに「Make Unique」 「Auto-Fade」 「Auto-Crossfade」 の 3 項目を表示。 Auto-Fade: 選択 audio clip 全部の fade_in/out_beats を `0.004 * bpm / 60` beats (= 4 ms 相当) に上書き、 業界標準のクリック除去 fade。 Auto-Crossfade: 選択 audio clip を track 別に start_beat sort し、 隣接ペアで `prev_end > next_start` を判定 → overlap 長を prev の fade_out + next の fade_in に設定 (隙間ペアと内包ペアは skip)。 status_message で適用件数を report。 inspector の clip_edit_buffer も追従 resync。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 57e89f9 | Phase 2 PR6 | Audio Editor (`docs/plan_audio_clip.md` §3.10) の minimal viewer を新設。 audio clip ダブルクリックで `audio_editor_clip = Some(target)` 化、 bottom_panel の Piano Roll タブが Audio Editor view に切り替わる (= タブ名も "Audio Editor"、 §3.10「piano_roll の領域を流用」)。 Esc で Close (root.rs::dispatch_shortcuts に分岐追加)。 view/audio_editor.rs 新規: ヘッダ (clip 名 + length + Close ボタン)、 first event の波形を `Ui::waveform` で全幅描画 (`ChannelLayout::Stack` で channel 別表示)、 source 情報メタラベル。 Phase 2 PR6 は read-only viewer のみ — event 単位 trim / 移動 / 追加 / 削除 / dB handle / 角 fade drag は後続 PR。 編集はそれまで Inspector 経由 (PR1-3)。 AppEvent: `OpenAudioEditor(ClipRef)` / `CloseAudioEditor` を追加 (どちらも非 undoable: view state)。 `is_audio_clip` helper を pub 化して `arrangement_view::DoubleClickClip` から利用、 audio / MIDI で分岐。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 3666f10 | Phase 2 PR7 | (1) Reverse 右クリックメニュー追加 + (2) Audio Editor に再生 playhead 線表示。 (1) `AppEvent::ToggleClipReversed(ClipRef)` を新設 (undoable)、 first event の reversed を読んで `set_clip_audio_event_reversed(target, !cur)` で全 event broadcast。 arrangement の clip 右クリックメニューに「Reverse」 を 4 項目目として追加 (Make Unique / Auto-Fade / Auto-Crossfade / Reverse)、 Auto-Fade と違って selection 全体ではなく当該 clip のみ toggle (Bitwig clip メニュー流)。 (2) audio_editor.rs に push_lines (`LineBatch`) で playhead を vertical 線として overlay、 clip range 内のときのみ表示 (= 曲全体のどこを再生中か Audio Editor 内で視認可能)、 wf_area で clip_rect 切り。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | d22d5c6 | gui_01 #025 [Open] | arrangement の audio clip に dB / fade 直接編集 gesture 要望投稿 (Bitwig spec §3.5 / §3.6)。 `ArrangementClip` に `audio_edit: Option<ArrangementClipAudioEdit>` field、 新 `ArrangementEditRequest` variant 3 つ (`SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve`)、 中央帯縦 drag = Gain、 角横 drag = fade length、 角縦 drag (>10 px) = curve トグル、 sticky direction で振り分け。 daw_01 側は AppEvent (`SetClipGainDb` / `SetClipFadeIn/OutBeats` / `SetClipFadeIn/OutCurve`) を Phase 2 PR2-3 で既に持っているので、 main マージ後は `arrangement_view::make_edit` に 3 arm 追加するだけで wire 完了 |
| 2026-05-08 | 3d1664e | Phase 2 PR8 | audio clip rect 上に gain_db / fade_in / fade_out 値の overlay label を small font で表示 (read-only)。 grip drag UI (gui_01 #025 マージ待ち) が来るまでの視覚 feedback として、 ユーザーが Inspector を開かなくても値が確認できるようにする。 default 値 (0 dB / 0 fade) の clip ではラベル無し (= 視覚ノイズ抑制)、 clip 幅 60 px 未満 / 高さ 24 px 未満も無描画。 描画位置は clip rect の右下 (= name は左上、 重ならない)、 右から `Fo / Fi / Gain` の順。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 0f2409e | Phase 2 PR9 | Bounce In Place (Pre-FX、 `docs/plan_audio_clip.md` §3.8 / §13 Q8) を実装。 audio clip 右クリックメニューに「Bounce In Place」 項目を追加 (5 項目目)、 selected clip 内の全 events を engine sample_rate (48 kHz) で stereo 32-bit float mix → WAV ファイルに書き出し → 新 `AudioSource` 採番 + `Song.audio_sources` insert + `audio_source_cache` 登録 + `ClipContent::Audio { events: [新 1 event] }` で置換 + `sync_song_to_plugin_host` で audio engine に LoadSong 再送。 出力先: project_dir があれば `<project_dir>/bounce/<safe_name>_<ts8>.wav`、 未保存 project は `%LOCALAPPDATA%/daw_01/bounce_cache/...` にフォールバック (= import_cache と同じ pattern、 save 時 migration helper は将来 PR)。 mix loop は audio_clip_renderer のロジックを portion-wise port (fade / gain / pan / pitch_ratio / reversed)、 fade_envelope は AppData 側に bounce_fade_env として重複定義 (= Phase 3+ で共通 crate に切り出し予定)。 Pre-FX なので plugin chain は通さない (Bitwig spec 通り)。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | dd7f6b9 | gui_01 #025 [Resolved] | gui_01 main を fast-forward (M14 Phase 63k = audio_edit + 3 EditRequest variant + grip drag handler) で取り込み、 daw_01 側で `ArrangementClip.audio_edit` を `ClipContent::Audio` の first event 値で詰め、 `arrangement_view::make_edit` に `SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve` 3 arm を追加。 各 delta は既存 AppEvent (`SetClipGainDb` / `SetClipFadeIn/OutBeats` / `SetClipFadeIn/OutCurve`、 Phase 2 PR2-3) に変換 → handler が全 event broadcast + Undo step を取る。 widget 側 FadeCurve / FadeEdge と daw_01 model FadeCurve の 1:1 変換 helper (`widget_curve_from_model` / `model_curve_from_widget`) を追加。 conversation.md の #025 entry は `[Resolved]` 化して archive_001.md に移動。 これで Bitwig 流の clip 中央 dB drag / 角 fade drag / 角縦 drag curve トグルが動く |
| 2026-05-08 | 2e58cd2 | docs cleanup | `docs/gui_01_conversation.md` に [Replied] / [Resolved] のまま残っていた 16 件 (#005 / #007-#015 / #017 / #019-#024) を監査 → daw_01 で取り込み済確認 → archive_001.md 末尾へ移動。 conversation.md は header + テンプレートのみ。 残作業エントリは [Open] / [Replied] 共に 0 件 |
| 2026-05-08 | c164391 | docs cleanup | gui_01 #015 (snap mode 単位修正) もユーザー実機確認で完了判断、 archive へ移動 |
| 2026-05-08 | (main merge) | release | `c70194f..c164391` を main へ fast-forward merge (16 commits)。 Phase 1 PR8 + Phase 1 仕上げ + Phase 2 PR1-9 + gui_01 #025 wire + conversation 整理が main にランディング |
| 2026-05-08 | b050d81 | docs | [plan_audio_followup.md](plan_audio_followup.md) 新設。 Phase 2 完了後に残る audio clip 関連の polish / 機能追加を 4 PR (DRY 化 → multi-clip Undo 統合 → plugin-FX Bounce → Audio Editor event 編集) + 後回し 2 件 (bounce_cache migration / VOICEVOX ClipContent::Audio 統合) で計画 |
| 2026-05-08 | 3a3826e | PR-A | `common/src/audio_render.rs` を新設し、 `fade_envelope` / `pitch_ratio_for` を共通化。 `daw_audio::audio_clip_renderer` の 2 関数 (fade_envelope ローカル定義 + compile_audio_schedule 内 pitch 計算) と `daw_gui::AppData` の bounce_fade_env / bounce_clip_in_place 内 pitch 計算をすべて共通 helper に置換 (= PR9 で portion-wise port した重複を解消)。 12 件の unit test を `common::audio_render::tests` に追加 (Linear / Exp / SCurve fade、 Raw / Repitch / Stretch / Slice pitch、 endpoint / SR-mismatch 境界)。 build / clippy / test --features rt-assert all clean (= 既存 PDC / sidechain / split-glue integration test が pass、 ロジック不変の証明) |
| 2026-05-08 | 4401d59 | PR-B | multi-clip drag の Undo step 統合。 batch AppEvent (`SetClipGainDbBatch(Vec<(ClipRef, f32)>)` / `SetClipFadeBeatsBatch(Vec<(ClipRef, FadeEdgeKind, f64)>)` / `SetClipFadeCurveBatch(Vec<(ClipRef, FadeEdgeKind, FadeCurve)>)`) を新設、 `is_undoable` 登録 = 1 release 1 Undo step。 `arrangement_view::make_edit::SetClipGainDb / SetClipFade / SetClipFadeCurve` を delta 列を Vec で集めて batch AppEvent 1 発に変換するよう書き換え。 単発 AppEvent (`SetClipGainDb` / `SetClipFadeIn/OutBeats` / `SetClipFadeIn/OutCurve`) は Inspector commit 経路で引き続き使用 (= 別系統)。 `FadeEdgeKind` enum を AppEvent module に追加 (widget 側 `FadeEdge` と 1:1)。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 38b0a9b | PR-D 段階 1 | Audio Editor の event 単位編集の基盤 (= multi-event clip を作る path)。 AppData に `audio_editor_selected_event: Option<usize>` 追加 (= clip 内 events Vec への index)、 AppEvent: `SelectAudioEditorEvent(Option<usize>)` (非 undoable view state) と `DuplicateAudioEditorEvent` (undoable) を追加。 handler `duplicate_audio_editor_event` は選択中 event を `event_start + event_length` の位置に複製、 同 source / 同パラメータ、 clip.length_beats を必要に応じて拡張 + selection を新 event に進める。 audio_editor.rs を multi-event 対応に書き換え (events.iter().enumerate() で各 event の rect を時間比で分割 + 選択中は `LineBatch` で 4 辺 border highlight)、 footer の source meta も選択中 event 参照 + event 数表示。 shortcuts.rs に `daw.duplicate_audio_event = Ctrl+D` を追加、 root.rs::dispatch_shortcuts で audio_editor_clip is Some のときだけ消費 (= 既存 D / Alt+D の clip duplicate と紛らわしくないよう gate)。 close_audio_editor は `audio_editor_selected_event = None` も clear。 段階 2 (event drag) / 段階 3 (event add / delete + context menu) は別 PR。 build / clippy / test --features rt-assert all clean |
| 2026-05-08 | 2675218 | gui_01 #026 [Open] | caller 側 view 用 rect-based pointer hit-test API 要望投稿。 `Ui::take_primary_press_in_rect(rect)` (= single click 検出) と `Ui::take_drag_in_rect(id, rect)` (= drag session、 anchor / current / delta + Started/Continuing/Released kind) の 2 つを既存 `take_double_click_in_rect` の並びに追加してほしい。 Audio Editor の event click 選択 / 中央 drag 移動 / 左右端 drag trim / 空白領域 file drop で event 追加に必要、 PR-D 段階 2 / 3 のうち drag UI 部分は本要望マージ後に再開予定 |
| 2026-05-08 | (this commit) | PR-D 段階 2 | Inspector multi-event 対応 + event 選択 keyboard navigation。 `inspector_audio_event_summary` を「audio_editor_clip == selected_clip なら audio_editor_selected_event idx の event を返す、 さもなくば first event」 に拡張。 `audio_event_target_indices(target, n_events) -> Range<usize>` helper + `mutate_audio_events_in_clip(target, f)` 集約 helper を新設、 既存 10 件の `set_clip_audio_event_*` (reversed / muted / stretch_mode / gain_db / pan / pitch_semitones / fade_in/out_beats / fade_in/out_curve) を helper 経由に書き換え (= audio_editor で event 選択中は 1 event のみ更新、 さもなくば全 event broadcast = 既存挙動互換)。 `next_audio_editor_event_idx(delta)` helper + shortcut `daw.next_audio_event = Ctrl+]` / `daw.prev_audio_event = Ctrl+[` を追加 (audio_editor_clip is Some 時のみ消費)、 wrap-around で event 選択を巡回。 これで Inspector 経由の個別 event 編集が動く。 drag UI 系は gui_01 #026 マージ後の段階 3 で。 build / clippy / test --features rt-assert all clean |
| 2026-05-13 | 8f8f730 | Phase 5 完結 | Step 5.2 SongTempo lane recording bypass wire — `daw_audio::engine::process_buffer` の `current_bpm` 計算で `recording_lanes.contains(&(MASTER_TRACK_ID, AutomationTarget::SongTempo))` を判定、 該当中は `evaluate_song_tempo` を skip して `song.bpm` constant fallback。 transport BPM input drag 時の curve / drag 値の二重反映を抑止、 mixer fader Volume / Pan の `fill_track_param_ramps` bypass と同 idiom を Song-level に展開。 plan_automation.md §10 Phase 5 の残 smoke test 4 件 + bypass wire 1 件を整理 (smoke は user 判断で skip)、 Step 5.0 / 5.1 / 5.2 / 5.3 の全項目 [x] 化で Phase 5 (Tempo / TimeSig / Transport event) 完結 |
